// SPDX-License-Identifier: BSD-2-Clause
//! The display microcontroller (the DMCUB, "display micro-controller unit B")
//! of display core next 3.1.5: the reset-and-start sequence of specification
//! 08 section 3.2, split into steps the caller can run one at a time and
//! log between.
//!
//! The order the caller follows:
//!
//! 1. [`stop`]: ask the firmware that is running to stop (a courtesy; the
//!    platform's firmware may not answer).
//! 2. [`hold`]: put the unit in reset.
//! 3. [`clean`]: zero the mailboxes' pointers and the status registers.
//! 4. The security processor loads the image (type [`IMAGE_TYPE`]) and
//!    writes windows 0 and 1 itself.
//! 5. [`configure`]: windows 2 to 6, region 5, the mailboxes, boot options.
//! 6. [`release`]: let the unit run.
//! 7. [`wait_running`]: until the display firmware says it is up.

use crate::psp::Memory;
use crate::regs::Window;

/// The security processor's image type for the display microcontroller
/// (03 section 5, 02 section 9).
pub const IMAGE_TYPE: u32 = 51;

/// `DMCUB_CNTL`: bit 16 enable, bit 19 trace port, bit 20 "stopped".
pub const CNTL: usize = 0xdad8;
/// `DMCUB_CNTL2` (3.1.x only): bit 0 soft reset.
pub const CNTL2: usize = 0xdb00;
/// `DMCUB_SCRATCH0`: bit 0 display firmware running, bit 1 mailbox ready.
pub const SCRATCH0: usize = 0xda8c;
/// `DMCUB_SCRATCH7`: the firmware's answer to a get-version or stop request.
pub const SCRATCH7: usize = 0xdaa8;
/// `DMCUB_SCRATCH14`: boot options.
pub const SCRATCH14: usize = 0xdac4;
/// `DMCUB_SCRATCH15`: the security processor's version, masked.
pub const SCRATCH15: usize = 0xdac8;
/// `DMCUB_GPINT_DATAIN1`: the general-purpose interrupt request.
pub const GPINT_DATAIN1: usize = 0xdae0;
/// `MMHUBBUB_SOFT_RESET` (3.1.5: seg2 + `0x342`): bit 8 resets the unit's
/// interface to the memory hub.
pub const MMHUBBUB_SOFT_RESET: usize = 0xe008;
/// `DMCUB_INBOX1_BASE_ADDRESS`, then `_SIZE`, `_WPTR`, `_RPTR`.
pub const INBOX1: usize = 0xda50;
/// `DMCUB_OUTBOX0_BASE_ADDRESS`, then `_SIZE`, `_WPTR`, `_RPTR`.
pub const OUTBOX0: usize = 0xda60;
/// `DMCUB_OUTBOX1_BASE_ADDRESS`, then `_SIZE`, `_WPTR`, `_RPTR`.
pub const OUTBOX1: usize = 0xda70;
/// `DMCUB_REGION5_OFFSET` (then `_HIGH` at +4).
pub const REGION5_OFFSET: usize = 0xd960;
/// `DMCUB_REGION5_TOP_ADDRESS`.
pub const REGION5_TOP: usize = 0xd988;
/// `DMCUB_REGION3_CW0_BASE_ADDRESS`; window `n` is `+ 4 n`.
pub const CW_BASE: usize = 0xd994;
/// `DMCUB_REGION3_CW0_TOP_ADDRESS`; window `n` is `+ 4 n`.
pub const CW_TOP: usize = 0xd9b4;
/// `DMCUB_REGION3_CW0_OFFSET`; window `n` is `+ 8 n`, the high word 4 later.
pub const CW_OFFSET: usize = 0xd9d4;
/// `CC_DC_PIPE_DIS`: bit 16 the unit is present.
pub const CC_DC_PIPE_DIS: usize = 0xd628;

/// The enable bit of a window's or region's top-address register.
pub const ENABLE: u32 = 1 << 31;
/// Where the unit sees window 0 (the instructions) and window 1 (the stack):
/// the security processor writes these two windows itself.
pub const CW0_BASE: u32 = 0x6000_0000;
/// Window 1's base.
pub const CW1_BASE: u32 = 0x6100_0000;
/// Window 3 (the copy of the video BIOS).
pub const CW3_BASE: u32 = 0x6300_0000;
/// Window 4 (the mailboxes).
pub const CW4_BASE: u32 = 0x6400_0000;
/// Window 5 (the trace buffer).
pub const CW5_BASE: u32 = 0x6500_0000;
/// Window 6 (the firmware's state).
pub const CW6_BASE: u32 = 0x6600_0000;
/// Region 5's base, where outbox 0 lives (after a 16-byte header).
pub const REGION5_BASE: u32 = 0xa000_0000;
/// Each mailbox is 8 KiB: inbox 1 first in window 4, outbox 1 after it.
pub const MAILBOX_BYTES: u32 = 0x2000;

/// The firmware's metadata magic, "DMUB".
const METADATA_MAGIC: u32 = 0x444d_5542;
/// How far from the end of the instruction region the metadata is looked for
/// (02 section 6: after a 256-byte footer, maybe shifted back by up to 15
/// bytes, or a 512-byte footer).
const METADATA_SEARCH_BYTES: usize = 1024;

/// What the firmware's metadata asks the driver to provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Needs {
    /// The state window's size.
    pub state_bytes: u32,
    /// The trace buffer's size.
    pub trace_bytes: u32,
    /// The firmware's version.
    pub version: u32,
}

/// Finds the metadata block in the instruction region (the bytes after the
/// file's 256-byte header, `region` long) and reads what it asks for.
#[must_use]
pub fn needs(region: &[u8]) -> Option<Needs> {
    let end = region.len();
    let from = end.saturating_sub(METADATA_SEARCH_BYTES);
    let mut at = end.checked_sub(16)? & !3;
    loop {
        if at < from {
            return None;
        }
        let w = |o: usize| -> Option<u32> {
            let b = region.get(at + o..at + o + 4)?;
            Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        if w(0) == Some(METADATA_MAGIC) && region.get(at + 0x10) == Some(&1) {
            return Some(Needs {
                state_bytes: w(4)?,
                trace_bytes: w(8)?,
                version: w(0xc)?,
            });
        }
        at = at.checked_sub(4)?;
    }
}

/// Where the driver's windows 3 to 6 are in video memory (GPU addresses,
/// 1 MiB-aligned) and how big the firmware wants them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The copy of the video BIOS (window 3).
    pub vbios: u64,
    /// Its size.
    pub vbios_bytes: u32,
    /// The mailboxes (window 4).
    pub mailboxes: u64,
    /// The trace buffer (window 5 and region 5).
    pub trace: u64,
    /// Its size.
    pub trace_bytes: u32,
    /// The firmware's state (window 6).
    pub state: u64,
    /// Its size.
    pub state_bytes: u32,
    /// The size of the image's instructions, for window 2's base.
    pub code_bytes: u32,
}

fn lo(v: u64) -> u32 {
    (v & 0xffff_ffff) as u32
}

fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

/// Whether the unit is present (`CC_DC_PIPE_DIS` bit 16).
#[must_use]
pub fn present<W: Window + ?Sized>(w: &W) -> bool {
    w.read32(CC_DC_PIPE_DIS) & (1 << 16) != 0
}

/// Whether the unit's display firmware has come up: `SCRATCH0` bits 0 and 1.
#[must_use]
pub fn running<W: Window + ?Sized>(w: &W) -> bool {
    w.read32(SCRATCH0) & 3 == 3
}

/// Asks the firmware to stop (08 section 3.2, "Stop"): the general-purpose
/// interrupt request, command 2. Gives up on each of its three conditions
/// after `polls` tries of `pause_us`. Returns whether the firmware said it
/// stopped; a platform firmware that does not answer is not an error.
pub fn stop<W: Window + ?Sized>(w: &W, polls: u32, pause_us: u32, pause: &dyn Fn(u32)) -> bool {
    if w.read32(CNTL2) & 1 != 0 {
        // Already in soft reset: nothing runs.
        return true;
    }
    w.write32(GPINT_DATAIN1, (1 << 28) | (2 << 16));
    let wait = |ok: &dyn Fn() -> bool| {
        for _ in 0..polls {
            if ok() {
                return true;
            }
            pause(pause_us);
        }
        false
    };
    let a = wait(&|| w.read32(GPINT_DATAIN1) >> 28 == 0);
    let b = wait(&|| w.read32(SCRATCH7) == 0xdead_dead);
    let c = wait(&|| w.read32(CNTL) & (1 << 20) != 0);
    a && b && c
}

/// Puts the unit in reset (08 section 3.2, "Hold"): soft reset on, the
/// memory-hub interface reset on, the enable off. Does nothing if the unit
/// is not enabled.
pub fn hold<W: Window + ?Sized>(w: &W) {
    let cntl = w.read32(CNTL);
    if cntl & (1 << 16) == 0 {
        return;
    }
    w.write32(CNTL2, w.read32(CNTL2) | 1);
    w.write32(MMHUBBUB_SOFT_RESET, w.read32(MMHUBBUB_SOFT_RESET) | (1 << 8));
    w.write32(CNTL, cntl & !(1 << 16));
}

/// Zeroes the inbox 1, outbox 1 and outbox 0 pointers, `SCRATCH0` and the
/// general-purpose interrupt request (08 section 3.2, "Clean").
pub fn clean<W: Window + ?Sized>(w: &W) {
    for base in [INBOX1, OUTBOX1, OUTBOX0] {
        w.write32(base + 8, 0);
        w.write32(base + 12, 0);
    }
    w.write32(SCRATCH0, 0);
    w.write32(GPINT_DATAIN1, 0);
}

/// Programs windows 2 to 6, region 5, the mailboxes and the boot options
/// (08 section 3.2, "Configure"). Windows 0 and 1 are the security
/// processor's. Window 2 holds the image's data after its instructions; the
/// images have none, so it is left disabled.
pub fn configure<W: Window + ?Sized>(w: &W, l: &Layout) {
    let window = |n: usize, base: u32, gpu: u64, bytes: u32| {
        w.write32(CW_OFFSET + 8 * n, lo(gpu));
        w.write32(CW_OFFSET + 8 * n + 4, hi(gpu));
        w.write32(CW_BASE + 4 * n, base);
        w.write32(CW_TOP + 4 * n, (base + bytes - 1) | ENABLE);
    };
    // Window 2: the base after the instructions, no size, off.
    w.write32(CW_BASE + 8, CW0_BASE + l.code_bytes);
    w.write32(CW_TOP + 8, 0);
    window(3, CW3_BASE, l.vbios, l.vbios_bytes);
    window(4, CW4_BASE, l.mailboxes, 2 * MAILBOX_BYTES);
    window(5, CW5_BASE, l.trace, l.trace_bytes);
    window(6, CW6_BASE, l.state, l.state_bytes);
    // Region 5: the same trace memory, at the address outbox 0 uses.
    w.write32(REGION5_OFFSET, lo(l.trace));
    w.write32(REGION5_OFFSET + 4, hi(l.trace));
    w.write32(REGION5_TOP, (l.trace_bytes - 1) | ENABLE);
    // The mailboxes: inbox 1 and outbox 1 in window 4, outbox 0 in region 5
    // after its 16-byte header.
    w.write32(INBOX1, CW4_BASE);
    w.write32(INBOX1 + 4, MAILBOX_BYTES);
    w.write32(OUTBOX1, CW4_BASE + MAILBOX_BYTES);
    w.write32(OUTBOX1 + 4, MAILBOX_BYTES);
    w.write32(OUTBOX0, REGION5_BASE + 16);
    w.write32(OUTBOX0 + 4, l.trace_bytes - 16);
    w.write32(SCRATCH14, 0);
}

/// Lets the unit run (08 section 3.2, "Release"): the memory-hub interface
/// out of reset, the security processor's version for the firmware, the
/// enable and trace port on, the soft reset off. `psp_version` is the
/// security processor's version register.
pub fn release<W: Window + ?Sized>(w: &W, psp_version: u32) {
    w.write32(MMHUBBUB_SOFT_RESET, w.read32(MMHUBBUB_SOFT_RESET) & !(1 << 8));
    w.write32(SCRATCH15, psp_version & 0x0011_00ff);
    w.write32(CNTL, w.read32(CNTL) | (1 << 16) | (1 << 19));
    w.write32(CNTL2, w.read32(CNTL2) & !1);
}

/// Asks the running firmware its version through the general-purpose
/// interrupt register (command 1; the answer is in `SCRATCH7`). `None` if
/// the request is not taken within `polls` tries of `pause_us`.
pub fn version<W: Window + ?Sized>(
    w: &W,
    polls: u32,
    pause_us: u32,
    pause: &dyn Fn(u32),
) -> Option<u32> {
    w.write32(GPINT_DATAIN1, (1 << 28) | (1 << 16));
    for _ in 0..polls {
        if w.read32(GPINT_DATAIN1) >> 28 == 0 {
            return Some(w.read32(SCRATCH7));
        }
        pause(pause_us);
    }
    None
}

/// One command in inbox 1 is 64 bytes; the ring is 8 KiB (08 section 3.3).
pub const COMMAND_BYTES: u32 = 64;

/// Command type: no operation.
pub const CMD_NOP: u8 = 0;

/// The first four bytes of a command (08 section 3.3): the type, the
/// subtype, whether a reply is wanted, and the payload size.
#[must_use]
pub fn header(kind: u8, subtype: u8, reply: bool, payload_bytes: u8) -> u32 {
    u32::from(kind)
        | u32::from(subtype) << 8
        | u32::from(reply) << 16
        | (u32::from(payload_bytes) & 0x3f) << 24
}

/// Why a command did not get through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The ring is full.
    Full,
    /// The firmware did not take the command in time (the read pointer).
    Timeout(u32),
}

/// Inbox 1: the host's end of the command ring.
#[derive(Debug, Clone, Copy)]
pub struct Inbox {
    /// The ring's GPU address (window 4's memory).
    ring: u64,
}

impl Inbox {
    /// The inbox in the mailbox window at GPU address `mailboxes`.
    #[must_use]
    pub const fn new(mailboxes: u64) -> Self {
        Self { ring: mailboxes }
    }

    /// Writes `command` (a header and payload, at most 64 bytes) at the
    /// write pointer, flushes it, moves the pointer on, and waits for the
    /// firmware's read pointer to reach it (08 section 3.3).
    ///
    /// # Errors
    ///
    /// [`Error::Full`] or [`Error::Timeout`] (with the last read pointer).
    pub fn submit<W: Window + ?Sized, M: Memory + ?Sized>(
        &self,
        w: &W,
        m: &M,
        command: &[u8],
        polls: u32,
        pause_us: u32,
        pause: &dyn Fn(u32),
    ) -> Result<(), Error> {
        let size = w.read32(INBOX1 + 4);
        let wptr = w.read32(INBOX1 + 8);
        let rptr = w.read32(INBOX1 + 12);
        let next = (wptr + COMMAND_BYTES) % size.max(COMMAND_BYTES);
        if next == rptr {
            return Err(Error::Full);
        }
        let mut slot = [0u8; COMMAND_BYTES as usize];
        let n = command.len().min(slot.len());
        slot[..n].copy_from_slice(&command[..n]);
        m.write(self.ring + u64::from(wptr), &slot);
        m.flush();
        w.write32(INBOX1 + 8, next);
        let mut last = rptr;
        for _ in 0..polls {
            last = w.read32(INBOX1 + 12);
            if last == next {
                return Ok(());
            }
            pause(pause_us);
        }
        Err(Error::Timeout(last))
    }
}

/// Waits for the display firmware (`SCRATCH0` bits 0 and 1): `polls` tries
/// of `pause_us`. Returns the last `SCRATCH0`.
pub fn wait_running<W: Window + ?Sized>(
    w: &W,
    polls: u32,
    pause_us: u32,
    pause: &dyn Fn(u32),
) -> Result<u32, u32> {
    let mut v = 0;
    for _ in 0..polls {
        v = w.read32(SCRATCH0);
        if v & 3 == 3 {
            return Ok(v);
        }
        pause(pause_us);
    }
    Err(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::vec;
    use std::vec::Vec;

    /// Registers, with a log of writes in order.
    struct Regs {
        regs: RefCell<BTreeMap<usize, u32>>,
        log: RefCell<Vec<(usize, u32)>>,
        /// What the unit does once released: sets scratch 0.
        boots: bool,
    }

    impl Regs {
        fn new() -> Self {
            Self {
                regs: RefCell::new(BTreeMap::new()),
                log: RefCell::new(Vec::new()),
                boots: true,
            }
        }
        fn get(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
        fn wrote(&self, o: usize) -> Vec<u32> {
            self.log
                .borrow()
                .iter()
                .filter(|(r, _)| *r == o)
                .map(|(_, v)| *v)
                .collect()
        }
    }

    impl Window for Regs {
        fn read32(&self, o: usize) -> u32 {
            self.get(o)
        }
        fn write32(&self, o: usize, v: u32) {
            self.log.borrow_mut().push((o, v));
            self.regs.borrow_mut().insert(o, v);
            // Releasing: enable on and soft reset off starts the firmware.
            if o == CNTL2 && v & 1 == 0 && self.boots && self.get(CNTL) & (1 << 16) != 0 {
                self.regs.borrow_mut().insert(SCRATCH0, 0x3);
            }
        }
    }

    fn layout() -> Layout {
        Layout {
            vbios: 0xf4_0a20_0000,
            vbios_bytes: 0x2_0000,
            mailboxes: 0xf4_0a30_0000,
            trace: 0xf4_0a40_0000,
            trace_bytes: 0x1_0010,
            state: 0xf4_0a50_0000,
            state_bytes: 0xc880,
            code_bytes: 0x3a520,
        }
    }

    #[test]
    fn the_register_addresses_are_the_specifications() {
        assert_eq!(CNTL, 0xdad8);
        assert_eq!(SCRATCH0 + 7 * 4, SCRATCH7);
        assert_eq!(SCRATCH0 + 14 * 4, SCRATCH14);
        assert_eq!(SCRATCH0 + 15 * 4, SCRATCH15);
        // Windows 3 to 6 are where the display register list has them.
        assert_eq!(CW_BASE + 4 * 3, 0xd9a0);
        assert_eq!(CW_TOP + 4 * 6, 0xd9cc);
        assert_eq!(CW_OFFSET + 8 * 3, 0xd9ec);
        assert_eq!(CW_OFFSET + 8 * 6 + 4, 0xda08);
        assert_eq!(CW_OFFSET + 8 * 5 + 4, 0xda00);
    }

    #[test]
    fn the_metadata_is_found_where_the_specification_puts_it() {
        // Raphael: the region is 0x3a720 bytes, the metadata at 0x3a4dc.
        let mut region = vec![0u8; 0x3a720];
        let at = 0x3a4dc;
        region[at..at + 4].copy_from_slice(&METADATA_MAGIC.to_le_bytes());
        region[at + 4..at + 8].copy_from_slice(&0xc880u32.to_le_bytes());
        region[at + 8..at + 12].copy_from_slice(&0x1_0010u32.to_le_bytes());
        region[at + 12..at + 16].copy_from_slice(&0x0500_3700u32.to_le_bytes());
        region[at + 0x10] = 1;
        assert_eq!(
            needs(&region),
            Some(Needs {
                state_bytes: 0xc880,
                trace_bytes: 0x1_0010,
                version: 0x0500_3700
            })
        );
        // No block, no answer; a block that is not for the display is skipped.
        assert_eq!(needs(&vec![0u8; 4096]), None);
        region[at + 0x10] = 0;
        assert_eq!(needs(&region), None);
    }

    #[test]
    fn hold_resets_an_enabled_unit_and_leaves_a_disabled_one_alone() {
        let r = Regs::new();
        r.regs.borrow_mut().insert(CNTL, 0x0019_00c6);
        hold(&r);
        assert_eq!(r.get(CNTL2) & 1, 1);
        assert_eq!(r.get(MMHUBBUB_SOFT_RESET) & 0x100, 0x100);
        assert_eq!(r.get(CNTL) & (1 << 16), 0);
        // The other bits of CNTL stay (the wake delay).
        assert_eq!(r.get(CNTL) & 0xff, 0xc6);
        let r = Regs::new();
        r.regs.borrow_mut().insert(CNTL, 0x0008_00c6);
        hold(&r);
        assert!(r.log.borrow().is_empty());
    }

    #[test]
    fn clean_zeroes_the_pointers_and_the_status() {
        let r = Regs::new();
        for o in [INBOX1 + 8, INBOX1 + 12, OUTBOX0 + 8, OUTBOX1 + 12, SCRATCH0] {
            r.regs.borrow_mut().insert(o, 0x400);
        }
        clean(&r);
        for o in [INBOX1 + 8, INBOX1 + 12, OUTBOX0 + 8, OUTBOX1 + 12, SCRATCH0] {
            assert_eq!(r.get(o), 0, "{o:#x}");
        }
        // The bases and sizes are not the cleaning's business.
        assert!(r.wrote(INBOX1).is_empty());
    }

    #[test]
    fn configure_programs_windows_three_to_six_and_the_mailboxes() {
        let r = Regs::new();
        configure(&r, &layout());
        // Window 4: the mailboxes, 16 KiB, at the GPU address.
        assert_eq!(r.get(CW_OFFSET + 8 * 4), 0x0a30_0000);
        assert_eq!(r.get(CW_OFFSET + 8 * 4 + 4), 0xf4);
        assert_eq!(r.get(CW_BASE + 16), CW4_BASE);
        assert_eq!(r.get(CW_TOP + 16), (CW4_BASE + 0x3fff) | ENABLE);
        // Window 6: the state.
        assert_eq!(r.get(CW_TOP + 24), (CW6_BASE + 0xc87f) | ENABLE);
        // Window 2 is off; windows 0 and 1 are not touched.
        assert_eq!(r.get(CW_TOP + 8), 0);
        assert_eq!(r.get(CW_BASE + 8), CW0_BASE + 0x3a520);
        for n in [0usize, 1] {
            assert!(r.wrote(CW_BASE + 4 * n).is_empty());
            assert!(r.wrote(CW_TOP + 4 * n).is_empty());
            assert!(r.wrote(CW_OFFSET + 8 * n).is_empty());
        }
        // Region 5 is the trace memory; outbox 0 follows its header.
        assert_eq!(r.get(REGION5_OFFSET), 0x0a40_0000);
        assert_eq!(r.get(REGION5_TOP), 0x1_000f | ENABLE);
        assert_eq!(r.get(OUTBOX0), 0xa000_0010);
        assert_eq!(r.get(OUTBOX0 + 4), 0x1_0000);
        // The inbox and outbox 1.
        assert_eq!(r.get(INBOX1), 0x6400_0000);
        assert_eq!(r.get(INBOX1 + 4), 0x2000);
        assert_eq!(r.get(OUTBOX1), 0x6400_2000);
        assert_eq!(r.get(OUTBOX1 + 4), 0x2000);
        assert_eq!(r.get(SCRATCH14), 0);
    }

    #[test]
    fn release_then_wait_sees_the_firmware_come_up() {
        let r = Regs::new();
        r.regs.borrow_mut().insert(CNTL, 0x0009_00c6);
        r.regs.borrow_mut().insert(CNTL2, 1);
        r.regs.borrow_mut().insert(MMHUBBUB_SOFT_RESET, 0x100);
        // Held: enable off, soft reset on.
        r.regs.borrow_mut().insert(CNTL, 0x0001_00c6 & !(1 << 16));
        release(&r, 0x0042_0016);
        assert_eq!(r.get(MMHUBBUB_SOFT_RESET) & 0x100, 0);
        assert_eq!(r.get(SCRATCH15), 0x16);
        assert_eq!(r.get(CNTL) & ((1 << 16) | (1 << 19)), (1 << 16) | (1 << 19));
        assert_eq!(r.get(CNTL2) & 1, 0);
        assert_eq!(wait_running(&r, 3, 10, &|_| {}), Ok(3));
        assert!(running(&r));
    }

    /// Memory for the inbox tests: remembers writes.
    struct Mem(RefCell<Vec<(u64, Vec<u8>)>>);

    impl Memory for Mem {
        fn write(&self, gpu: u64, data: &[u8]) {
            self.0.borrow_mut().push((gpu, data.to_vec()));
        }
        fn read32(&self, _: u64) -> u32 {
            0
        }
        fn flush(&self) {}
        fn invalidate(&self) {}
    }

    #[test]
    fn the_header_packs_as_the_specification_says() {
        assert_eq!(header(0, 0, false, 0), 0);
        assert_eq!(header(128, 2, true, 16), 128 | 2 << 8 | 1 << 16 | 16 << 24);
        // The payload size is 6 bits.
        assert_eq!(header(0, 0, false, 0xff), 0x3f << 24);
    }

    #[test]
    fn a_command_goes_in_the_slot_at_the_write_pointer() {
        let r = Regs::new();
        r.regs.borrow_mut().insert(INBOX1 + 4, 0x2000);
        let m = Mem(RefCell::new(Vec::new()));
        let inbox = Inbox::new(0xf4_0a30_0000);
        // The firmware takes it: the read pointer follows the write pointer.
        struct Follows<'a>(&'a Regs);
        impl Window for Follows<'_> {
            fn read32(&self, o: usize) -> u32 {
                if o == INBOX1 + 12 {
                    return self.0.get(INBOX1 + 8);
                }
                self.0.read32(o)
            }
            fn write32(&self, o: usize, v: u32) {
                self.0.write32(o, v);
            }
        }
        let cmd = header(CMD_NOP, 0, false, 0).to_le_bytes();
        assert_eq!(inbox.submit(&Follows(&r), &m, &cmd, 3, 1, &|_| {}), Ok(()));
        assert_eq!(r.get(INBOX1 + 8), 64);
        let w = m.0.borrow();
        assert_eq!(w[0].0, 0xf4_0a30_0000);
        assert_eq!(w[0].1.len(), 64);
        // A second one lands in the next slot.
        drop(w);
        assert_eq!(inbox.submit(&Follows(&r), &m, &cmd, 3, 1, &|_| {}), Ok(()));
        assert_eq!(m.0.borrow()[1].0, 0xf4_0a30_0040);
    }

    #[test]
    fn a_firmware_that_does_not_read_times_out_and_the_ring_fills() {
        let r = Regs::new();
        r.regs.borrow_mut().insert(INBOX1 + 4, 0x2000);
        let m = Mem(RefCell::new(Vec::new()));
        let inbox = Inbox::new(0x1000);
        assert_eq!(
            inbox.submit(&r, &m, &[0; 4], 2, 1, &|_| {}),
            Err(Error::Timeout(0))
        );
        // Wrap: the write pointer one slot behind the read pointer is full.
        r.regs.borrow_mut().insert(INBOX1 + 8, 0x1fc0);
        r.regs.borrow_mut().insert(INBOX1 + 12, 0);
        assert_eq!(inbox.submit(&r, &m, &[0; 4], 2, 1, &|_| {}), Err(Error::Full));
    }

    #[test]
    fn the_version_request_returns_scratch7() {
        let r = Regs::new();
        r.regs.borrow_mut().insert(SCRATCH7, 0x0500_3700);
        // The model never clears the status nibble itself: write-through
        // leaves it set, so the request times out...
        assert_eq!(version(&r, 2, 1, &|_| {}), None);
        assert_eq!(r.wrote(GPINT_DATAIN1), vec![(1 << 28) | (1 << 16)]);
        // ...and a firmware that takes it clears the nibble.
        r.regs.borrow_mut().insert(GPINT_DATAIN1, 0);
        struct Takes<'a>(&'a Regs);
        impl Window for Takes<'_> {
            fn read32(&self, o: usize) -> u32 {
                if o == GPINT_DATAIN1 {
                    return 0;
                }
                self.0.read32(o)
            }
            fn write32(&self, o: usize, v: u32) {
                self.0.write32(o, v);
            }
        }
        assert_eq!(version(&Takes(&r), 2, 1, &|_| {}), Some(0x0500_3700));
    }

    #[test]
    fn a_unit_that_never_comes_up_times_out_with_its_status() {
        let mut r = Regs::new();
        r.boots = false;
        r.regs.borrow_mut().insert(SCRATCH0, 0x40);
        assert_eq!(wait_running(&r, 5, 10, &|_| {}), Err(0x40));
    }

    #[test]
    fn stop_reports_a_firmware_that_does_not_answer() {
        let r = Regs::new();
        assert!(!stop(&r, 2, 1, &|_| {}));
        assert_eq!(r.wrote(GPINT_DATAIN1), vec![(1 << 28) | (2 << 16)]);
        // A unit already in soft reset is stopped.
        let r = Regs::new();
        r.regs.borrow_mut().insert(CNTL2, 1);
        assert!(stop(&r, 2, 1, &|_| {}));
        assert!(r.log.borrow().is_empty());
    }
}
