// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The platform security processor (PSP 13.0.5; specification 03): its
//! mailbox registers, the kernel-mode command ring, and the commands the
//! driver sends it. The secure OS is already running on Raphael (the
//! platform firmware starts it), so the driver only creates the ring.
//!
//! Everything works on a register window and a [`Memory`] for the ring, the
//! command buffer and the fence, so the whole exchange (a frame, a fence, a
//! response) is tested on the host against a model of the PSP.

use crate::regs::Window;

/// `C2PMSG_33`: bit 31, the PSP's boot-time firmware is initialised.
pub const C2PMSG_33: usize = 0x58184;
/// `C2PMSG_58`: the running secure OS's version.
pub const C2PMSG_58: usize = 0x581e8;
/// `C2PMSG_64`: ring control, command out and response back.
pub const C2PMSG_64: usize = 0x58200;
/// `C2PMSG_67`: the kernel-mode ring's write pointer, in dwords.
pub const C2PMSG_67: usize = 0x5820c;
/// `C2PMSG_69`: the ring's address, bits 31:0.
pub const C2PMSG_69: usize = 0x58214;
/// `C2PMSG_70`: bits 63:32.
pub const C2PMSG_70: usize = 0x58218;
/// `C2PMSG_71`: the ring's size in bytes.
pub const C2PMSG_71: usize = 0x5821c;
/// `C2PMSG_81`: nonzero once the secure OS runs.
pub const C2PMSG_81: usize = 0x58244;

/// `C2PMSG_64`: create the kernel-mode ring.
pub const CREATE_RING: u32 = 0x0002_0000;
/// `C2PMSG_64`: destroy the rings.
pub const DESTROY_RINGS: u32 = 0x0003_0000;
/// The ring is 4 KiB: 64 frames of 64 bytes.
pub const RING_BYTES: usize = 4096;
/// A frame's size.
pub const FRAME_BYTES: usize = 64;
/// The write pointer wraps at this many dwords.
pub const RING_DWORDS: u32 = 1024;
/// A frame is this many dwords.
pub const FRAME_DWORDS: u32 = 16;
/// The command buffer's size.
pub const COMMAND_BYTES: usize = 1024;
/// Where the response block starts in the command buffer.
pub const RESPONSE_AT: usize = 0x360;

/// Command ids (03 section 5.3).
pub mod cmd {
    /// Load a trusted application.
    pub const LOAD_TA: u32 = 0x01;
    /// Load the application security driver.
    pub const LOAD_ASD: u32 = 0x04;
    /// Set up the trusted memory region.
    pub const SETUP_TMR: u32 = 0x05;
    /// Load an engine's firmware.
    pub const LOAD_IP_FW: u32 = 0x06;
    /// Tell the security processor every graphics image has been delivered,
    /// so the RLC brings the engines up from the trusted memory region
    /// (command `0x21`, no arguments).
    #[must_use]
    pub fn start_rlc_autoload() -> Self {
        Self {
            id: cmd::START_RLC_AUTOLOAD,
            args: [0; 8],
        }
    }

    /// Destroy the trusted memory region.
    pub const DESTROY_TMR: u32 = 0x07;
    /// Load the table of contents.
    pub const LOAD_TOC: u32 = 0x20;
    /// Start the RLC's autoload.
    pub const START_RLC_AUTOLOAD: u32 = 0x21;
}

/// Whether `C2PMSG_64`'s value means "ready, status 0": bit 31 set and the
/// status field clear (the same test sees a completed ring command).
#[must_use]
pub const fn ready(v: u32) -> bool {
    v & 0x8000_ffff == 0x8000_0000
}

/// The memory the PSP and the driver share: the ring, the command buffer
/// and the fence, addressed by GPU address. The driver's writes are made
/// visible with [`Memory::flush`], and a read after the PSP's write is
/// preceded by [`Memory::invalidate`].
pub trait Memory {
    /// Writes `data` at GPU address `gpu`.
    fn write(&self, gpu: u64, data: &[u8]);
    /// Reads the dword at GPU address `gpu`.
    fn read32(&self, gpu: u64) -> u32;
    /// Makes the driver's writes visible to the PSP.
    fn flush(&self);
    /// Makes the PSP's writes visible to the driver.
    fn invalidate(&self);
}

/// Why something with the PSP did not work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// `C2PMSG_81` reads zero: the secure OS is not running.
    NoSecureOs,
    /// `C2PMSG_64` did not show ready in time.
    NotReady,
    /// The ring was not created (a status in `C2PMSG_64`).
    Ring(u32),
    /// The fence did not reach its value in time.
    Timeout,
}

/// Where the ring, command buffer and fence are, by GPU address (each
/// 4 KiB-aligned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffers {
    /// The ring.
    pub ring: u64,
    /// The command buffer.
    pub command: u64,
    /// The fence: the PSP writes the value at its first dword.
    pub fence: u64,
}

impl Buffers {
    /// Whether the buffers are 4 KiB-aligned.
    #[must_use]
    pub const fn aligned(&self) -> bool {
        self.ring.is_multiple_of(4096)
            && self.command.is_multiple_of(4096)
            && self.fence.is_multiple_of(4096)
    }
}

/// How long to wait: polls and the pause between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wait {
    /// How many polls.
    pub polls: u32,
    /// Microseconds between them.
    pub pause_us: u32,
}

/// The reference's wait for a command: about 150 to 300 microseconds, up to
/// 20,000 times.
pub const COMMAND_WAIT: Wait = Wait {
    polls: 20_000,
    pause_us: 200,
};

/// What a command answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Response {
    /// The status: 0 is success.
    pub status: u32,
    /// The session id, after loading a trusted application.
    pub session: u32,
    /// The trusted memory region size the PSP needs, after loading the table
    /// of contents.
    pub tmr_size: u32,
}

/// A command, ready for the command buffer: its id and the words from
/// offset `0x1c`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The command id.
    pub id: u32,
    /// The command-specific area, dwords from `+0x1c`.
    pub args: [u32; 8],
}

fn lo(v: u64) -> u32 {
    u32::try_from(v & 0xffff_ffff).unwrap_or(0)
}

fn hi(v: u64) -> u32 {
    u32::try_from(v >> 32).unwrap_or(0)
}

impl Command {
    /// Load the table of contents staged at `address`, `size` bytes.
    #[must_use]
    pub fn load_toc(address: u64, size: u32) -> Self {
        Self {
            id: cmd::LOAD_TOC,
            args: [lo(address), hi(address), size, 0, 0, 0, 0, 0],
        }
    }

    /// Set up the trusted memory region at GPU address `address`, `size`
    /// bytes, whose physical address is `physical`.
    #[must_use]
    pub fn setup_tmr(address: u64, size: u32, physical: u64) -> Self {
        Self {
            id: cmd::SETUP_TMR,
            // Flags bit 1: a physical address follows.
            args: [
                lo(address),
                hi(address),
                size,
                2,
                lo(physical),
                hi(physical),
                0,
                0,
            ],
        }
    }

    /// Load the engine firmware of type `kind` (02 section 9) staged at
    /// `address`, `size` bytes.
    #[must_use]
    pub fn load_ip_firmware(address: u64, size: u32, kind: u32) -> Self {
        Self {
            id: cmd::LOAD_IP_FW,
            args: [lo(address), hi(address), size, kind, 0, 0, 0, 0],
        }
    }

    /// Destroy the trusted memory region.
    #[must_use]
    pub fn destroy_tmr() -> Self {
        Self {
            id: cmd::DESTROY_TMR,
            args: [0; 8],
        }
    }

    /// The command buffer's bytes: the header (size and version left 0, as
    /// the reference does), the command id, the arguments, and zeros to 1 KiB.
    #[must_use]
    pub fn buffer(&self) -> [u8; COMMAND_BYTES] {
        let mut b = [0u8; COMMAND_BYTES];
        b[8..12].copy_from_slice(&self.id.to_le_bytes());
        for (i, a) in self.args.iter().enumerate() {
            b[0x1c + 4 * i..0x20 + 4 * i].copy_from_slice(&a.to_le_bytes());
        }
        b
    }
}

/// The 64-byte ring frame (03 section 4.3).
#[must_use]
pub fn frame(command: u64, fence: u64, fence_value: u32) -> [u8; FRAME_BYTES] {
    let mut f = [0u8; FRAME_BYTES];
    f[0..4].copy_from_slice(&lo(command).to_le_bytes());
    f[4..8].copy_from_slice(&hi(command).to_le_bytes());
    // The command buffer's size field stays 0, as the reference leaves it.
    f[0x0c..0x10].copy_from_slice(&lo(fence).to_le_bytes());
    f[0x10..0x14].copy_from_slice(&hi(fence).to_le_bytes());
    f[0x14..0x18].copy_from_slice(&fence_value.to_le_bytes());
    f
}

/// Reads the response block of the command buffer at `command`.
pub fn response<M: Memory + ?Sized>(m: &M, command: u64) -> Response {
    let at = command + RESPONSE_AT as u64;
    Response {
        status: m.read32(at),
        session: m.read32(at + 4),
        tmr_size: m.read32(at + 0x10),
    }
}

/// A ring and its commands.
#[derive(Debug)]
pub struct Psp {
    buffers: Buffers,
    next_fence: u32,
}

fn wait_for<W: Window + ?Sized>(
    w: &W,
    at: usize,
    wait: Wait,
    pause: &dyn Fn(u32),
) -> Result<u32, u32> {
    let mut v = 0;
    for _ in 0..wait.polls {
        v = w.read32(at);
        if ready(v) {
            return Ok(v);
        }
        pause(wait.pause_us);
    }
    Err(v)
}

impl Psp {
    /// Checks the secure OS runs and creates the kernel-mode ring at
    /// `buffers.ring` (03 section 4.2). The fence is zeroed first.
    ///
    /// # Errors
    ///
    /// [`Error`] if the secure OS does not run, the PSP does not show ready,
    /// or it refuses the ring.
    pub fn create_ring<W: Window + ?Sized, M: Memory + ?Sized>(
        w: &W,
        m: &M,
        buffers: Buffers,
        wait: Wait,
        pause: &dyn Fn(u32),
    ) -> Result<Self, Error> {
        if w.read32(C2PMSG_81) == 0 {
            return Err(Error::NoSecureOs);
        }
        // The fence and the ring start out zero.
        m.write(buffers.fence, &[0u8; 8]);
        m.write(buffers.ring, &[0u8; RING_BYTES]);
        m.flush();
        wait_for(w, C2PMSG_64, wait, pause).map_err(|_| Error::NotReady)?;
        w.write32(C2PMSG_69, lo(buffers.ring));
        w.write32(C2PMSG_70, hi(buffers.ring));
        w.write32(C2PMSG_71, u32::try_from(RING_BYTES).unwrap_or(0));
        w.write32(C2PMSG_64, CREATE_RING);
        match wait_for(w, C2PMSG_64, wait, pause) {
            Ok(_) => Ok(Self {
                buffers,
                next_fence: 1,
            }),
            Err(v) => Err(Error::Ring(v)),
        }
    }

    /// Sends one command and waits for it (03 section 4.4): the command
    /// buffer, a frame at the write pointer, the pointer moved on, the
    /// fence polled, the response read. Only one command is ever
    /// outstanding.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] if the fence does not reach its value. A
    /// non-zero status in the [`Response`] is for the caller (some
    /// firmware leaves one on success).
    pub fn submit<W: Window + ?Sized, M: Memory + ?Sized>(
        &mut self,
        w: &W,
        m: &M,
        command: &Command,
        wait: Wait,
        pause: &dyn Fn(u32),
    ) -> Result<Response, Error> {
        let b = self.buffers;
        m.write(b.command, &command.buffer());
        m.flush();
        let wptr = w.read32(C2PMSG_67) % RING_DWORDS;
        let slot = u64::from(wptr / FRAME_DWORDS);
        let value = self.next_fence;
        self.next_fence = self.next_fence.wrapping_add(1).max(1);
        m.write(
            b.ring + slot * FRAME_BYTES as u64,
            &frame(b.command, b.fence, value),
        );
        m.flush();
        w.write32(C2PMSG_67, (wptr + FRAME_DWORDS) % RING_DWORDS);
        for _ in 0..wait.polls {
            m.invalidate();
            if m.read32(b.fence) == value {
                m.invalidate();
                return Ok(response(m, b.command));
            }
            pause(wait.pause_us);
        }
        Err(Error::Timeout)
    }

    /// Destroys the rings.
    ///
    /// # Errors
    ///
    /// [`Error::NotReady`] if the PSP does not answer.
    pub fn destroy<W: Window + ?Sized>(
        self,
        w: &W,
        wait: Wait,
        pause: &dyn Fn(u32),
    ) -> Result<(), Error> {
        w.write32(C2PMSG_64, DESTROY_RINGS);
        wait_for(w, C2PMSG_64, wait, pause)
            .map(|_| ())
            .map_err(|_| Error::NotReady)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    const BASE: u64 = 0xf4_0a00_0000;
    const B: Buffers = Buffers {
        ring: BASE,
        command: BASE + 0x1000,
        fence: BASE + 0x2000,
    };

    /// The PSP and its memory, in one model: registers, memory by byte, and a PSP that
    /// takes a frame when the write pointer moves.
    struct Model {
        regs: RefCell<BTreeMap<usize, u32>>,
        mem: RefCell<BTreeMap<u64, u8>>,
        flushed: RefCell<u32>,
        /// What the PSP answers to load-TOC.
        tmr_size: u32,
        /// Whether it ever completes a command.
        deaf: bool,
        log: RefCell<Vec<(usize, u32)>>,
        commands: RefCell<Vec<u32>>,
    }

    impl Model {
        fn new() -> Self {
            let m = Self {
                regs: RefCell::new(BTreeMap::new()),
                mem: RefCell::new(BTreeMap::new()),
                flushed: RefCell::new(0),
                tmr_size: 0xa0_0000,
                deaf: false,
                log: RefCell::new(Vec::new()),
                commands: RefCell::new(Vec::new()),
            };
            // The secure OS runs, and is ready for a ring.
            m.regs.borrow_mut().insert(C2PMSG_81, 0x1234);
            m.regs.borrow_mut().insert(C2PMSG_64, 0x8000_0000);
            m
        }
        fn rd(&self, gpu: u64) -> u32 {
            let mem = self.mem.borrow();
            let b = |i: u64| *mem.get(&(gpu + i)).unwrap_or(&0);
            u32::from_le_bytes([b(0), b(1), b(2), b(3)])
        }
        fn put(&self, gpu: u64, v: &[u8]) {
            let mut mem = self.mem.borrow_mut();
            for (i, x) in v.iter().enumerate() {
                mem.insert(gpu + i as u64, *x);
            }
        }
        /// The PSP takes the frame at the old write pointer.
        fn process(&self, old: u32) {
            if self.deaf {
                return;
            }
            let ring = u64::from(self.regs.borrow().get(&C2PMSG_69).copied().unwrap_or(0))
                | u64::from(self.regs.borrow().get(&C2PMSG_70).copied().unwrap_or(0)) << 32;
            let at = ring + u64::from(old / FRAME_DWORDS) * FRAME_BYTES as u64;
            let command = u64::from(self.rd(at)) | u64::from(self.rd(at + 4)) << 32;
            let fence = u64::from(self.rd(at + 0x0c)) | u64::from(self.rd(at + 0x10)) << 32;
            let value = self.rd(at + 0x14);
            let id = self.rd(command + 8);
            self.commands.borrow_mut().push(id);
            let mut resp = [0u8; 0x14];
            if id == cmd::LOAD_TOC {
                resp[0x10..0x14].copy_from_slice(&self.tmr_size.to_le_bytes());
            }
            self.put(command + RESPONSE_AT as u64, &resp);
            self.put(fence, &value.to_le_bytes());
        }
    }

    impl Window for Model {
        fn read32(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
        fn write32(&self, o: usize, v: u32) {
            self.log.borrow_mut().push((o, v));
            let old = Window::read32(self, o);
            self.regs.borrow_mut().insert(o, v);
            if o == C2PMSG_64 && v == CREATE_RING {
                self.regs.borrow_mut().insert(C2PMSG_64, 0x8000_0000);
            }
            if o == C2PMSG_67 {
                self.process(old);
            }
        }
    }

    impl Memory for Model {
        fn write(&self, gpu: u64, data: &[u8]) {
            self.put(gpu, data);
        }
        fn read32(&self, gpu: u64) -> u32 {
            self.rd(gpu)
        }
        fn flush(&self) {
            *self.flushed.borrow_mut() += 1;
        }
        fn invalidate(&self) {}
    }

    const WAIT: Wait = Wait {
        polls: 5,
        pause_us: 1,
    };

    #[test]
    fn registers_are_at_the_specifications_bytes() {
        // 03 section 1: segment base 0x16000, byte = 4 x (base + index).
        assert_eq!(C2PMSG_64, 4 * (0x16000 + 0x80));
        assert_eq!(C2PMSG_67, 4 * (0x16000 + 0x83));
        assert_eq!(C2PMSG_71, 4 * (0x16000 + 0x87));
        assert_eq!(C2PMSG_81, 4 * (0x16000 + 0x91));
        assert!(ready(0x8000_0000));
        assert!(!ready(0x8000_0001), "a status");
        assert!(!ready(0x0000_0000), "not yet");
        assert!(ready(0x8001_0000), "the command id bits do not matter");
    }

    #[test]
    fn the_command_buffer_and_frame_are_laid_out_as_specified() {
        let c = Command::load_toc(0xf4_0a10_0000, 1536);
        let b = c.buffer();
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 0x20);
        assert_eq!(
            u32::from_le_bytes(b[0x1c..0x20].try_into().unwrap()),
            0x0a10_0000
        );
        assert_eq!(u32::from_le_bytes(b[0x20..0x24].try_into().unwrap()), 0xf4);
        assert_eq!(u32::from_le_bytes(b[0x24..0x28].try_into().unwrap()), 1536);
        assert_eq!(&b[0..8], &[0; 8], "size and version stay 0");
        let t = Command::setup_tmr(0xf4_1e00_0000, 0xa0_0000, 0x18_5e00_0000).buffer();
        assert_eq!(
            u32::from_le_bytes(t[0x28..0x2c].try_into().unwrap()),
            2,
            "flags: physical follows"
        );
        assert_eq!(
            u32::from_le_bytes(t[0x2c..0x30].try_into().unwrap()),
            0x5e00_0000
        );
        assert_eq!(u32::from_le_bytes(t[0x30..0x34].try_into().unwrap()), 0x18);
        let f = Command::load_ip_firmware(0xf4_0b00_0000, 4096, 9).buffer();
        assert_eq!(u32::from_le_bytes(f[0x28..0x2c].try_into().unwrap()), 9);
        // Start RLC autoload: command 0x21, no arguments.
        let a = Command::start_rlc_autoload().buffer();
        assert_eq!(u32::from_le_bytes(a[8..12].try_into().unwrap()), 0x21);
        assert!(a[0x1c..0x3c].iter().all(|b| *b == 0));
        let fr = frame(0xf4_0a00_1000, 0xf4_0a00_2000, 7);
        assert_eq!(
            u32::from_le_bytes(fr[0..4].try_into().unwrap()),
            0x0a00_1000
        );
        assert_eq!(u32::from_le_bytes(fr[4..8].try_into().unwrap()), 0xf4);
        assert_eq!(
            u32::from_le_bytes(fr[8..12].try_into().unwrap()),
            0,
            "size left 0"
        );
        assert_eq!(
            u32::from_le_bytes(fr[0x0c..0x10].try_into().unwrap()),
            0x0a00_2000
        );
        assert_eq!(u32::from_le_bytes(fr[0x14..0x18].try_into().unwrap()), 7);
        assert_eq!(&fr[0x18..], &[0u8; 40][..], "the rest is zero");
    }

    #[test]
    fn the_ring_is_created_only_if_the_secure_os_runs_and_is_ready() {
        let m = Model::new();
        m.regs.borrow_mut().insert(C2PMSG_81, 0);
        assert_eq!(
            Psp::create_ring(&m, &m, B, WAIT, &|_| {}).unwrap_err(),
            Error::NoSecureOs
        );
        assert!(
            m.log.borrow().is_empty(),
            "nothing is written to a PSP that is not there"
        );
        let m = Model::new();
        m.regs.borrow_mut().insert(C2PMSG_64, 0);
        assert_eq!(
            Psp::create_ring(&m, &m, B, WAIT, &|_| {}).unwrap_err(),
            Error::NotReady
        );
        assert!(m.log.borrow().iter().all(|w| w.0 != C2PMSG_64));
    }

    #[test]
    fn the_ring_is_created_in_the_specified_order() {
        let m = Model::new();
        Psp::create_ring(&m, &m, B, WAIT, &|_| {}).unwrap();
        let log = m.log.borrow();
        let order: Vec<usize> = log.iter().map(|w| w.0).collect();
        assert_eq!(order, [C2PMSG_69, C2PMSG_70, C2PMSG_71, C2PMSG_64]);
        assert_eq!(log[0].1, 0x0a00_0000);
        assert_eq!(log[1].1, 0xf4);
        assert_eq!(log[2].1, 4096);
        assert_eq!(log[3].1, CREATE_RING);
        assert!(*m.flushed.borrow() >= 1, "the zeroed ring is flushed first");
    }

    #[test]
    fn a_ring_the_psp_refuses_reports_its_status() {
        // After the create command the PSP shows a status, never ready.
        struct Refuses<'a>(&'a Model);
        impl Window for Refuses<'_> {
            fn read32(&self, o: usize) -> u32 {
                if o == C2PMSG_64 && self.0.log.borrow().iter().any(|w| w.1 == CREATE_RING) {
                    return 0x8000_0007;
                }
                Window::read32(self.0, o)
            }
            fn write32(&self, o: usize, v: u32) {
                self.0.log.borrow_mut().push((o, v));
            }
        }
        let m = Model::new();
        let w = Refuses(&m);
        assert_eq!(
            Psp::create_ring(&w, &m, B, WAIT, &|_| {}).unwrap_err(),
            Error::Ring(0x8000_0007)
        );
    }

    #[test]
    fn a_command_is_answered_through_the_ring_and_its_fence() {
        let m = Model::new();
        let mut psp = Psp::create_ring(&m, &m, B, WAIT, &|_| {}).unwrap();
        let r = psp
            .submit(
                &m,
                &m,
                &Command::load_toc(0xf4_0a10_0000, 1536),
                WAIT,
                &|_| {},
            )
            .unwrap();
        assert_eq!(
            r,
            Response {
                status: 0,
                session: 0,
                tmr_size: 0xa0_0000
            }
        );
        assert_eq!(*m.commands.borrow(), [cmd::LOAD_TOC]);
        // The write pointer moved on by one frame.
        assert_eq!(m.read32_reg(C2PMSG_67), 16);
        // The next command goes in the next slot, with the next fence value.
        let r = psp
            .submit(
                &m,
                &m,
                &Command::setup_tmr(0xf4_1e00_0000, 0xa0_0000, 0x18_5e00_0000),
                WAIT,
                &|_| {},
            )
            .unwrap();
        assert_eq!(r.status, 0);
        assert_eq!(m.read32_reg(C2PMSG_67), 32);
        assert_eq!(m.rd(B.fence), 2);
        assert_eq!(*m.commands.borrow(), [cmd::LOAD_TOC, cmd::SETUP_TMR]);
        // The command was flushed before the pointer moved.
        assert!(*m.flushed.borrow() >= 3);
    }

    impl Model {
        fn read32_reg(&self, o: usize) -> u32 {
            Window::read32(self, o)
        }
    }

    #[test]
    fn the_write_pointer_wraps_after_sixty_four_frames() {
        let m = Model::new();
        let mut psp = Psp::create_ring(&m, &m, B, WAIT, &|_| {}).unwrap();
        for _ in 0..64 {
            psp.submit(&m, &m, &Command::destroy_tmr(), WAIT, &|_| {})
                .unwrap();
        }
        assert_eq!(m.read32_reg(C2PMSG_67), 0, "back to the start of the ring");
        psp.submit(&m, &m, &Command::destroy_tmr(), WAIT, &|_| {})
            .unwrap();
        assert_eq!(m.read32_reg(C2PMSG_67), 16);
    }

    #[test]
    fn a_psp_that_never_answers_times_out() {
        let mut m = Model::new();
        let mut psp = Psp::create_ring(&m, &m, B, WAIT, &|_| {}).unwrap();
        m.deaf = true;
        assert_eq!(
            psp.submit(
                &m,
                &m,
                &Command::load_toc(0xf4_0a10_0000, 1536),
                WAIT,
                &|_| {}
            )
            .unwrap_err(),
            Error::Timeout
        );
    }

    #[test]
    fn the_fence_value_never_wraps_to_zero() {
        let mut psp = Psp {
            buffers: B,
            next_fence: u32::MAX,
        };
        let m = Model::new();
        psp.submit(&m, &m, &Command::destroy_tmr(), WAIT, &|_| {})
            .ok();
        assert_eq!(
            psp.next_fence, 1,
            "zero is the fence's initial value, and is skipped"
        );
    }

    #[test]
    fn buffers_must_be_page_aligned() {
        assert!(B.aligned());
        assert!(
            !Buffers {
                ring: BASE + 8,
                ..B
            }
            .aligned()
        );
    }
}
