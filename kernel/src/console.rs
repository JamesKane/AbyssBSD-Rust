// SPDX-License-Identifier: BSD-2-Clause
//! The early console: the serial port and the framebuffer the loader found,
//! written together, and mirrored into the persistent log ring that the
//! loader saves to the ESP on the next boot.
//!
//! The serial port is also the console terminal's (`ttyu0`): the `uart`
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
}

/// Bytes of framebuffer output held at most.
const HELD: usize = 2048;
/// Bytes of early console output kept for `vt`.
const EARLY: usize = 32 * 1024;

/// Output goes to the log ring only (FreeBSD's `kern.consmute`).
static MUTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Mutes or unmutes the serial and framebuffer consoles.
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub(crate) fn set_muted(muted: bool) {
    MUTED.store(muted, core::sync::atomic::Ordering::Relaxed);
}

impl Write for Sinks {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if MUTED.load(core::sync::atomic::Ordering::Relaxed) {
            if let Some(ring) = &mut self.ring {
                ring.write(s.as_bytes());
            }
            return Ok(());
        }
        match &mut self.serial {
            #[cfg(target_arch = "x86_64")]
            Some(SerialSink::Port(u)) => u.write_str(s)?,
            Some(SerialSink::Mmio16550(u)) => u.write_str(s)?,
            Some(SerialSink::Pl011(u)) => u.write_str(s)?,
            None => {}
        }
        if self.vt {
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
        if let Some(ring) = &mut self.ring {
            ring.write(s.as_bytes());
        }
        Ok(())
    }
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
    c.fb = fb;
    c.ring = ring;
    drop(c);
    if let Some(at) = at {
        let _ = SERIAL_AT.set(at);
        let _ = uart::CONSOLE.set(&PORT);
    }
}

/// Whether the loader found a serial console, which is then the
/// console terminal's (FreeBSD's `comconsole` ahead of `vidconsole`).
pub(crate) fn has_serial() -> bool {
    SERIAL_AT.get().is_some()
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
        let mut r = RECEIVED.lock();
        let mut got = false;
        // Cleared before draining: a byte arriving after the last read
        // raises the interrupt again (cleared after, its receive timeout
        // would be lost and the byte left in the FIFO).
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
        // Full: the rest waits in the FIFO until the thread catches up.
        serial.set_rx_interrupt(false);
        r.held = true;
        true
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
