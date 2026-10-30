// SPDX-License-Identifier: BSD-2-Clause
//! The early console: the serial port and the framebuffer the loader found,
//! written together with `RB_MULTIPLE`, else the best of them alone (see
//! [`howto`]), and mirrored into the persistent log ring that the loader
//! saves to the ESP on the next boot.
//!
//! The serial port is also a terminal (`ttyu0`), the console's unless
//! `vt`'s first window is (see [`howto`]): the `uart`
//! driver on the same registers takes its interrupt, and through
//! [`PORT`] has received bytes moved to a small buffer in the filter and
//! on to the terminal in the interrupt thread. Terminal output is written
//! as it is (the line discipline already turned NL into CR NL).

use alloc::vec::Vec;
use core::fmt::{self, Write};

use bootproto::{BootInfo, PixelFormat, Serial};
use bus::Resource;
use fb_console::PixelLayout;
use regs::{AccessWidth, Mmio, StridedMmio};
use sched::Sleepable;
use sync::{Once, SpinLock};

/// A 16550 in x86 port space.
#[cfg(target_arch = "x86_64")]
#[derive(Debug)]
struct Ports(crate::arch::cpu::PortBlock);

#[cfg(target_arch = "x86_64")]
impl regs::RegisterIo for Ports {
    fn read(&self, n: usize) -> u8 {
        self.0.read(u16::try_from(n).unwrap_or(0))
    }
    fn write(&self, n: usize, v: u8) {
        self.0.write(u16::try_from(n).unwrap_or(0), v);
    }
}

enum SerialSink {
    #[cfg(target_arch = "x86_64")]
    Port(uart_16550::Uart<Ports>),
    Mmio16550(uart_16550::Uart<StridedMmio>),
    Pl011(uart_pl011::Uart),
}

impl SerialSink {
    fn put(&self, b: u8) {
        match self {
            #[cfg(target_arch = "x86_64")]
            Self::Port(u) => u.put(b),
            Self::Mmio16550(u) => u.put(b),
            Self::Pl011(u) => u.put(b),
        }
    }

    fn get(&self) -> Option<u8> {
        match self {
            #[cfg(target_arch = "x86_64")]
            Self::Port(u) => u.get(),
            Self::Mmio16550(u) => u.get(),
            Self::Pl011(u) => u.get(),
        }
    }

    fn enable_rx_interrupt(&self) {
        match self {
            #[cfg(target_arch = "x86_64")]
            Self::Port(u) => u.enable_rx_interrupt(),
            Self::Mmio16550(u) => u.enable_rx_interrupt(),
            Self::Pl011(u) => u.enable_rx_interrupt(),
        }
    }

    fn set_rx_interrupt(&self, on: bool) {
        match self {
            #[cfg(target_arch = "x86_64")]
            Self::Port(u) => u.set_rx_interrupt(on),
            Self::Mmio16550(u) => u.set_rx_interrupt(on),
            Self::Pl011(u) => u.set_rx_interrupt(on),
        }
    }

    #[cfg(feature = "ktest")]
    fn set_loopback(&self, on: bool) {
        match self {
            #[cfg(target_arch = "x86_64")]
            Self::Port(u) => u.set_loopback(on),
            Self::Mmio16550(u) => u.set_loopback(on),
            Self::Pl011(u) => u.set_loopback(on),
        }
    }

    /// Clears the receive interrupt (before the FIFO is read empty).
    fn ack_rx(&self) {
        if let Self::Pl011(u) = self {
            u.ack_rx();
        }
    }
}

/// Where the serial port's registers are, to know its `uart` device by.
#[derive(Clone, Copy, Debug)]
enum SerialAt {
    Port(u16),
    Mmio(u64),
}

static SERIAL_AT: Once<SerialAt> = Once::new();

/// The framebuffer the loader set up, for `vt` to take over.
#[derive(Clone, Copy)]
struct Framebuffer {
    phys: u64,
    virt: usize,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
    layout: PixelLayout,
}

struct Sinks {
    serial: Option<SerialSink>,
    fb: Option<fb_console::Console<'static>>,
    fb_info: Option<Framebuffer>,
    /// What the early console showed, for `vt`'s console window: the
    /// last `early_len` bytes. (Before the heap, so not a `Vec`.)
    early: [u8; EARLY],
    early_len: usize,
    /// `vt` has the framebuffer: the screen is its console window.
    vt: bool,
    ring: Option<logring::Ring<'static>>,
    /// Framebuffer output is held while this is nonzero.
    fb_holds: u32,
    /// Text held back from the framebuffer (the rest is dropped).
    fb_held: [u8; HELD],
    fb_held_len: usize,
    /// The message buffer's last priority (`msg_lastpri`), and whether
    /// its last line is unfinished (`MSGBUF_NEEDNL`).
    log_pri: i32,
    log_open_line: bool,
    /// Where `/dev/klog` reads next (`msg_rseq`).
    log_rseq: u64,
}

/// Bytes of framebuffer output held at most.
const HELD: usize = 2048;
/// Bytes of early console output kept for `vt`.
const EARLY: usize = 32 * 1024;

/// The kernel's messages this boot, oldest first (`kern.msgbuf`): the
/// log ring's contents.
pub(crate) fn msgbuf() -> Vec<u8> {
    let c = CONSOLE.lock();
    let Some(ring) = &c.ring else {
        return Vec::new();
    };
    let (a, b) = ring.contents();
    let mut out = Vec::with_capacity(a.len() + b.len());
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    out
}

/// Output goes to the log ring only (FreeBSD's `kern.consmute`).
static MUTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Mutes or unmutes the serial and framebuffer consoles.
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub(crate) fn set_muted(muted: bool) {
    MUTED.store(muted, core::sync::atomic::Ordering::Relaxed);
}

impl Write for Sinks {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if !MUTED.load(core::sync::atomic::Ordering::Relaxed) {
            self.write_consoles(s)?;
        }
        self.log_ring(-1, s.as_bytes());
        Ok(())
    }
}

/// Set when the message buffer gains text (`msgbuftrigger`), for
/// `/dev/klog`'s periodic wakeup.
static LOG_TRIGGER: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

impl Sinks {
    /// Appends `bytes` to the message buffer with priority `pri` (-1 for
    /// none: `printf`'s), as FreeBSD's `msgbuf_addstr` does: a line of a
    /// priority starts with `<pri>`, a change of priority ends an
    /// unfinished line first, and carriage returns are dropped.
    fn log_ring(&mut self, pri: i32, bytes: &[u8]) {
        let Some(ring) = &mut self.ring else {
            return;
        };
        if bytes.is_empty() {
            return;
        }
        if self.log_pri != pri && self.log_open_line {
            ring.write(b"\n");
            self.log_open_line = false;
        }
        // `<pri>`, written from the right.
        let mut prefix = [0u8; 12];
        let mut at = prefix.len();
        if let Ok(mut v) = u32::try_from(pri) {
            at -= 1;
            prefix[at] = b'>';
            loop {
                at -= 1;
                prefix[at] = b'0' + u8::try_from(v % 10).unwrap_or(0);
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            at -= 1;
            prefix[at] = b'<';
        }
        let prefix = &prefix[at..];
        let mut from = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if !self.log_open_line && !prefix.is_empty() {
                ring.write(&bytes[from..i]);
                ring.write(prefix);
                from = i;
            }
            if b == b'\r' {
                ring.write(&bytes[from..i]);
                from = i + 1;
                continue;
            }
            self.log_open_line = b != b'\n';
        }
        ring.write(&bytes[from..]);
        self.log_pri = pri;
        LOG_TRIGGER.store(true, core::sync::atomic::Ordering::Release);
    }

    /// Writes to the serial port and the screen, not the message buffer.
    fn write_consoles(&mut self, s: &str) -> fmt::Result {
        let (to_serial, to_video) = consoles();
        match &mut self.serial {
            #[cfg(target_arch = "x86_64")]
            Some(SerialSink::Port(u)) if to_serial => u.write_str(s)?,
            Some(SerialSink::Mmio16550(u)) if to_serial => u.write_str(s)?,
            Some(SerialSink::Pl011(u)) if to_serial => u.write_str(s)?,
            _ => {}
        }
        if !to_video {
            // The screen keeps nothing; the log ring below does.
        } else if self.vt {
            vt::write_kernel(s.as_bytes());
        } else if let Some(fb) = &mut self.fb {
            let b = &s.as_bytes()[s.len().saturating_sub(EARLY)..];
            let over = (self.early_len + b.len()).saturating_sub(EARLY);
            self.early.copy_within(over..self.early_len, 0);
            self.early_len -= over;
            self.early[self.early_len..][..b.len()].copy_from_slice(b);
            self.early_len += b.len();
            if self.fb_holds == 0 {
                fb.write_str(s)?;
            } else {
                let n = s.len().min(HELD - self.fb_held_len);
                self.fb_held[self.fb_held_len..][..n].copy_from_slice(&s.as_bytes()[..n]);
                self.fb_held_len += n;
            }
        }
        Ok(())
    }
}

/// A message of priority `pri` (`log(9)`, `vlog`): into the message
/// buffer with its `<pri>`, and on the consoles too when `to_console`
/// (FreeBSD's while no one has `/dev/klog` open).
pub(crate) fn log(pri: i32, to_console: bool, args: fmt::Arguments<'_>) {
    struct Log<'a>(&'a mut Sinks, i32, bool);
    impl Write for Log<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if self.2 && !MUTED.load(core::sync::atomic::Ordering::Relaxed) {
                self.0.write_consoles(s)?;
            }
            self.0.log_ring(self.1, s.as_bytes());
            Ok(())
        }
    }
    let mut c = CONSOLE.lock();
    let _ = Log(&mut c, pri, to_console).write_fmt(args);
}

/// Puts `bytes` in the message buffer only, with priority `pri`
/// (`msglogstr`: console output that `log_console` keeps).
pub(crate) fn log_bytes(pri: i32, bytes: &[u8]) {
    CONSOLE.lock().log_ring(pri, bytes);
}

/// Whether the message buffer gained text since the last call
/// (`msgbuftrigger`, cleared).
pub(crate) fn take_log_trigger() -> bool {
    LOG_TRIGGER.swap(false, core::sync::atomic::Ordering::AcqRel)
}

/// Bytes of the message buffer `/dev/klog` has not read
/// (`msgbuf_getcount`).
pub(crate) fn msgbuf_unread() -> usize {
    let c = CONSOLE.lock();
    let Some(ring) = &c.ring else {
        return 0;
    };
    let (a, b) = ring.contents();
    usize::try_from(ring.written().saturating_sub(c.log_rseq))
        .unwrap_or(usize::MAX)
        .min(a.len() + b.len())
}

/// Takes unread bytes of the message buffer into `buf`
/// (`msgbuf_getbytes`): as many as fit before the end of its storage.
pub(crate) fn msgbuf_read(buf: &mut [u8]) -> usize {
    let mut guard = CONSOLE.lock();
    let c = &mut *guard;
    let rseq = c.log_rseq;
    let Some(ring) = &c.ring else {
        return 0;
    };
    let (at, n) = ring.read_from(rseq, buf);
    c.log_rseq = at + n as u64;
    n
}

/// Holds (`true`) or releases (`false`) framebuffer output; calls nest.
/// Text printed meanwhile still reaches the serial console and the log
/// ring, and reaches the screen on release (up to [`HELD`] bytes). PCI
/// holds it while sizing a function's BARs with its decoding off, since
/// the framebuffer may be that function's memory.
pub(crate) fn hold_framebuffer(hold: bool) {
    vt::hold(hold);
    let mut c = CONSOLE.lock();
    if hold {
        c.fb_holds += 1;
        return;
    }
    c.fb_holds = c.fb_holds.saturating_sub(1);
    if c.fb_holds == 0 && c.fb_held_len > 0 {
        let Sinks {
            fb,
            fb_held,
            fb_held_len,
            ..
        } = &mut *c;
        if let Some(fb) = fb {
            // The cut at `HELD` may split a character: draw what is valid.
            for chunk in fb_held[..*fb_held_len].utf8_chunks() {
                let _ = fb.write_str(chunk.valid());
            }
        }
        *fb_held_len = 0;
    }
}

static CONSOLE: SpinLock<Sinks> = SpinLock::new(Sinks {
    serial: None,
    fb: None,
    fb_info: None,
    early: [0; EARLY],
    early_len: 0,
    vt: false,
    ring: None,
    fb_holds: 0,
    fb_held: [0; HELD],
    fb_held_len: 0,
    log_pri: -1,
    log_open_line: false,
    log_rseq: 0,
});

/// Takes over the devices the loader described.
pub(crate) fn init(info: &BootInfo) {
    let serial = info.serial.get().map(|s| match s {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: the loader found a 16550 at this port.
        Serial::Ns16550Port { port } => SerialSink::Port(uart_16550::Uart::init(Ports(unsafe {
            crate::arch::cpu::PortBlock::new(port)
        }))),
        #[cfg(not(target_arch = "x86_64"))]
        Serial::Ns16550Port { .. } => unreachable!("I/O ports exist only on x86"),
        Serial::Ns16550Mmio(m) => {
            let width = if m.access_width == 4 {
                AccessWidth::Dword
            } else {
                AccessWidth::Byte
            };
            // SAFETY: the loader mapped the UART's registers as device
            // memory at `virt`, and the kernel tables keep that mapping.
            let regs = unsafe { mmio(m) };
            SerialSink::Mmio16550(uart_16550::Uart::init(StridedMmio::new(
                regs,
                m.reg_shift,
                width,
            )))
        }
        // SAFETY: as above.
        Serial::Pl011(m) => SerialSink::Pl011(uart_pl011::Uart::init(unsafe { mmio(m) })),
    });
    let fb_info = info.framebuffer.get().and_then(|f| {
        Some(Framebuffer {
            phys: f.phys.get(),
            virt: usize::try_from(f.virt.get()).ok()?,
            len: usize::try_from(f.size / 4).ok()?,
            width: f.width,
            height: f.height,
            stride: f.stride,
            layout: match f.format {
                PixelFormat::Rgbx => PixelLayout::Rgbx,
                PixelFormat::Bgrx => PixelLayout::Bgrx,
                PixelFormat::Masks { red, green, blue } => PixelLayout::Masks { red, green, blue },
            },
        })
    });
    let fb = info.framebuffer.get().and_then(|f| {
        let layout = fb_info?.layout;
        let len = usize::try_from(f.size / 4).ok()?;
        // SAFETY: the loader mapped the framebuffer write-combining at
        // `virt` for `size` bytes; the console is its only user.
        let pixels = unsafe { core::slice::from_raw_parts_mut(f.virt.get() as *mut u32, len) };
        let dim = |v: u32| usize::try_from(v).ok();
        fb_console::Console::new(
            pixels,
            dim(f.width)?,
            dim(f.height)?,
            dim(f.stride)?,
            layout,
        )
        .ok()
    });
    let ring = info.log.get().and_then(|l| {
        let va = bootproto::layout::dmap(l.region.start)?;
        let len = usize::try_from(l.region.len()).ok()?;
        // SAFETY: the loader reserved the ring as log-ring memory, which is
        // RAM in the DMAP; the console is its only user.
        let buf = unsafe { core::slice::from_raw_parts_mut(va.get() as *mut u8, len) };
        logring::Ring::open(buf).ok()
    });
    let at = info.serial.get().map(|s| match s {
        Serial::Ns16550Port { port } => SerialAt::Port(port),
        Serial::Ns16550Mmio(m) | Serial::Pl011(m) => SerialAt::Mmio(m.phys.get()),
    });
    let mut c = CONSOLE.lock();
    c.serial = serial;
    c.fb_info = fb.is_some().then_some(fb_info).flatten();
    if c.fb_info.is_some() {
        VIDEO.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    c.fb = fb;
    c.ring = ring;
    drop(c);
    if let Some(at) = at {
        let _ = SERIAL_AT.set(at);
        let _ = uart::CONSOLE.set(&PORT);
    }
    CONSOLES_KNOWN.store(true, core::sync::atomic::Ordering::Release);
}

/// Whether the loader found a serial console.
pub(crate) fn has_serial() -> bool {
    SERIAL_AT.get().is_some()
}

/// Whether the loader found a framebuffer, for `vt`.
static VIDEO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The console bits of FreeBSD's `boothowto`: `RB_MULTIPLE` (every
/// console, not just the best) and `RB_SERIAL` (the serial port is the
/// best).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Howto {
    pub(crate) multiple: bool,
    pub(crate) serial: bool,
}

/// The console bits: from the loader variables `boot_multicons` and
/// `boot_serial` if either is given (each set unless "no", as
/// `boot_env_to_howto` reads them), else what FreeBSD's EFI loader infers
/// from the consoles it found (`parse_uefi_con_out`): both, serial first;
/// or the one found.
pub(crate) fn howto() -> Howto {
    let var = |k: &str| {
        crate::boot::cmdline()
            .split_ascii_whitespace()
            .find_map(|w| match w.split_once('=') {
                Some((key, v)) if key == k => Some(!v.eq_ignore_ascii_case("no")),
                None if w == k => Some(true),
                _ => None,
            })
    };
    match (var("boot_multicons"), var("boot_serial")) {
        (None, None) => {
            let video = VIDEO.load(core::sync::atomic::Ordering::Relaxed);
            Howto {
                multiple: video && has_serial(),
                serial: has_serial(),
            }
        }
        (m, s) => Howto {
            multiple: m.unwrap_or(false),
            serial: s.unwrap_or(false),
        },
    }
}

/// The consoles kernel messages go to, serial and video: every one with
/// `RB_MULTIPLE`, else the best alone (FreeBSD's `cninit` registers only
/// it). Both until the console is set up.
fn consoles() -> (bool, bool) {
    static CHOSEN: Once<(bool, bool)> = Once::new();
    if let Some(&c) = CHOSEN.get() {
        return c;
    }
    if !CONSOLES_KNOWN.load(core::sync::atomic::Ordering::Acquire) {
        return (true, true);
    }
    let h = howto();
    let c = if h.multiple {
        (true, true)
    } else if serial_primary() {
        (true, false)
    } else {
        (false, true)
    };
    let _ = CHOSEN.set(c);
    c
}

/// The loader's consoles are known (`init` ran), and with them
/// [`howto`]'s answer.
static CONSOLES_KNOWN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether the serial port's terminal is the console's (`/dev/console`):
/// FreeBSD's best console, `uart` at `CN_REMOTE` with `RB_SERIAL`, else
/// `vt` at `CN_INTERNAL` ahead of `uart` at `CN_NORMAL`, if there is a
/// framebuffer for it.
pub(crate) fn serial_primary() -> bool {
    has_serial() && (howto().serial || !VIDEO.load(core::sync::atomic::Ordering::Relaxed))
}

/// For dual consoles, which is the console's terminal, as FreeBSD reports
/// it once the root is mounted.
pub(crate) fn dual_report() -> Option<&'static str> {
    howto().multiple.then(|| {
        if serial_primary() {
            "Dual Console: Serial Primary, Video Secondary"
        } else {
            "Dual Console: Video Primary, Serial Secondary"
        }
    })
}

/// Gives the framebuffer to `vt`, with what the early console showed
/// and its physical base; the early console stops drawing.
pub(crate) fn take_framebuffer() -> Option<(vt::Display, Vec<u8>, u64)> {
    let mut c = CONSOLE.lock();
    let f = c.fb_info.take()?;
    // The early console's slice of the pixels goes first.
    c.fb = None;
    let early = c.early[..c.early_len].to_vec();
    c.early_len = 0;
    // SAFETY: the loader mapped the framebuffer write-combining at
    // `virt` for `len` pixels; the early console that used it is gone,
    // so `vt` is its only user from here on.
    let pixels = unsafe { core::slice::from_raw_parts_mut(f.virt as *mut u32, f.len) };
    Some((
        vt::Display {
            pixels,
            width: f.width,
            height: f.height,
            stride: f.stride,
            layout: f.layout,
        },
        early,
        f.phys,
    ))
}

/// Kernel output goes to `vt`'s console window from now on.
pub(crate) fn vt_started() {
    CONSOLE.lock().vt = true;
}

/// Writes a terminal's output to the serial port as it is, and to the
/// screen while the early console has it (`vt`'s windows have their
/// own terminals).
fn write_tty(bytes: &[u8]) {
    let mut c = CONSOLE.lock();
    if let Some(serial) = &c.serial {
        for &b in bytes {
            serial.put(b);
        }
    }
    if c.fb_holds == 0
        && let Some(fb) = &mut c.fb
    {
        for chunk in bytes.utf8_chunks() {
            let _ = fb.write_str(chunk.valid());
        }
    }
}

/// Has the serial port receive `b` as if typed, by looping it back (for
/// tests of the input path). Nothing else is written meanwhile: the
/// console lock is held throughout.
#[cfg(feature = "ktest")]
pub(crate) fn loop_back(b: u8) {
    let c = CONSOLE.lock();
    if let Some(serial) = &c.serial {
        serial.set_loopback(true);
        serial.put(b);
        // The byte has gone round once the transmitter is empty again; a
        // newline after it keeps the terminal's echo of both tidy.
        serial.put(b'\n');
        serial.set_loopback(false);
    }
}

/// Bytes received, between the filter and the interrupt thread.
struct Received {
    buf: [u8; 256],
    len: usize,
    /// The buffer filled with bytes still in the FIFO: the receive
    /// interrupt is masked until the thread has taken the buffer, so
    /// nothing is lost (an emulated port sends as fast as its FIFO
    /// empties).
    held: bool,
}

static RECEIVED: SpinLock<Received> = SpinLock::new(Received {
    buf: [0; 256],
    len: 0,
    held: false,
});

/// The console's serial port, for the `uart` driver.
pub(crate) struct Port;

/// The one [`Port`].
pub(crate) static PORT: Port = Port;

impl uart::ConsolePort for Port {
    fn is(&self, resources: &[Resource]) -> bool {
        match SERIAL_AT.get() {
            Some(SerialAt::Port(p)) => resources
                .iter()
                .any(|r| matches!(r, Resource::IoPort { base, .. } if base == p)),
            Some(SerialAt::Mmio(a)) => resources
                .iter()
                .any(|r| matches!(r, Resource::Memory { base, .. } if base == a)),
            None => false,
        }
    }

    fn enable_input(&self, unit: u32) {
        crate::tty::create_console(unit, write_tty);
        if let Some(serial) = &CONSOLE.lock().serial {
            serial.enable_rx_interrupt();
        }
    }

    fn receive(&self) -> bool {
        let c = CONSOLE.lock();
        let Some(serial) = &c.serial else {
            return false;
        };
        drain(serial, &mut RECEIVED.lock())
    }

    fn deliver(&self, s: &Sleepable) {
        loop {
            let mut buf = [0u8; 256];
            let n = {
                let c = CONSOLE.lock();
                let mut r = RECEIVED.lock();
                let n = r.len;
                buf[..n].copy_from_slice(&r.buf[..n]);
                r.len = 0;
                if r.held {
                    r.held = false;
                    if let Some(serial) = &c.serial {
                        serial.set_rx_interrupt(true);
                        // What waited in the FIFO is taken here: a PL011
                        // raises its receive interrupt again only once
                        // the FIFO has been empty, which it never was.
                        drain(serial, &mut r);
                    }
                }
                n
            };
            if n == 0 {
                return;
            }
            if let Some(t) = crate::tty::console() {
                t.input(s, &buf[..n]);
            }
        }
    }
}

/// Takes what the FIFO holds into `r`, whether anything came. The
/// interrupt is cleared before the FIFO is read: a byte arriving after
/// the last read raises it again (cleared after, its receive timeout
/// would be lost and the byte left in the FIFO). With `r` full, the rest
/// waits in the FIFO, the interrupt masked, until the thread catches up.
fn drain(serial: &SerialSink, r: &mut Received) -> bool {
    let mut got = false;
    serial.ack_rx();
    while r.len < r.buf.len() {
        let Some(b) = serial.get() else {
            return got;
        };
        got = true;
        let at = r.len;
        r.buf[at] = b;
        r.len += 1;
    }
    serial.set_rx_interrupt(false);
    r.held = true;
    true
}

/// # Safety
///
/// `m` must describe registers mapped as device memory for the kernel's
/// lifetime.
unsafe fn mmio(m: bootproto::Mmio) -> Mmio {
    let len = usize::try_from(m.size.max(4096)).unwrap_or(4096);
    // SAFETY: per the caller's contract.
    unsafe { Mmio::new(m.virt.get() as *mut u8 as usize, len) }
}

/// Prints to every console.
pub(crate) fn print(args: fmt::Arguments<'_>) {
    let _ = CONSOLE.lock().write_fmt(args);
}

/// Writes bytes from user mode to every console; bytes that are not UTF-8
/// show as U+FFFD (the consoles are text devices until the tty layer).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // processes come in P4.3
pub(crate) fn write_bytes(bytes: &[u8]) {
    let mut c = CONSOLE.lock();
    for chunk in bytes.utf8_chunks() {
        let _ = c.write_str(chunk.valid());
        if !chunk.invalid().is_empty() {
            let _ = c.write_str("\u{fffd}");
        }
    }
}

/// Prints on the panic path, breaking the console lock if its holder is
/// the code that panicked.
pub(crate) fn print_panic(args: fmt::Arguments<'_>) {
    let c = match CONSOLE.try_lock() {
        Some(mut c) => {
            let on_vt = core::mem::replace(&mut c.vt, false);
            let _ = c.write_fmt(args);
            c.vt = on_vt;
            on_vt
        }
        None => {
            // SAFETY: interrupts are off and there is one CPU in Phase 1, so
            // the holder is the interrupted code on this CPU, which will not
            // run again.
            let c = unsafe { CONSOLE.force() };
            let on_vt = core::mem::replace(&mut c.vt, false);
            let _ = c.write_fmt(args);
            c.vt = on_vt;
            on_vt
        }
    };
    // `vt` draws it at once, if its locks are free.
    if c {
        let mut w = PanicVt;
        let _ = w.write_fmt(args);
    }
}

/// `vt`'s panic path, as a `fmt::Write`.
struct PanicVt;

impl Write for PanicVt {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        vt::panic_write(s.as_bytes());
        Ok(())
    }
}

/// A `fmt::Write` sink for code that takes one (the test harness).
pub(crate) struct Writer;

impl Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        CONSOLE.lock().write_str(s)
    }
}

#[allow(unused_macros)]
macro_rules! kprint {
    ($($t:tt)*) => { $crate::console::print(format_args!($($t)*)) };
}

macro_rules! kprintln {
    () => { $crate::console::print(format_args!("\n")) };
    ($($t:tt)*) => { $crate::console::print(format_args!("{}\n", format_args!($($t)*))) };
}

/// `kern.console` (`sysctl_kern_console`): the consoles in use, the best
/// first, then `/` and every console the kernel has: the serial port's
/// named `ttyu0` once it is a console (`uart_tty_attach`), `uart` before.
/// libc's `getttyent` reads it for `/etc/ttys`'s `onifconsole`.
pub(crate) fn sysctl_init(s: &sched::Sleepable) {
    use sysctl::{Number, Value};
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        t.add_value(
            kern,
            Number::Auto,
            "console",
            "Console device control",
            Value::ByteStr(alloc::boxed::Box::new(|out| {
                let (serial, video) = consoles();
                let serial = serial && has_serial();
                let video = video && VIDEO.load(core::sync::atomic::Ordering::Relaxed);
                let mut active: Vec<&str> = Vec::new();
                if serial {
                    active.push("ttyu0");
                }
                if video {
                    active.push("ttyv0");
                }
                if !serial_primary() {
                    active.reverse();
                }
                for c in active {
                    out.extend_from_slice(c.as_bytes());
                    out.push(b',');
                }
                out.extend_from_slice(if serial { b"/ttyu0," } else { b"/uart," });
                out.extend_from_slice(b"ttyv0,");
            })),
        )
        .expect("kern.console");
    });
}
