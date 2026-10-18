// SPDX-License-Identifier: BSD-2-Clause
//! The early console: the serial port and the framebuffer the loader found,
//! written together, and mirrored into the persistent log ring that the
//! loader saves to the ESP on the next boot.

use core::fmt::{self, Write};

use bootproto::{BootInfo, PixelFormat, Serial};
use fb_console::PixelLayout;
use regs::{AccessWidth, Mmio, StridedMmio};
use sync::SpinLock;

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

struct Sinks {
    serial: Option<SerialSink>,
    fb: Option<fb_console::Console<'static>>,
    ring: Option<logring::Ring<'static>>,
}

impl Write for Sinks {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match &mut self.serial {
            #[cfg(target_arch = "x86_64")]
            Some(SerialSink::Port(u)) => u.write_str(s)?,
            Some(SerialSink::Mmio16550(u)) => u.write_str(s)?,
            Some(SerialSink::Pl011(u)) => u.write_str(s)?,
            None => {}
        }
        if let Some(fb) = &mut self.fb {
            fb.write_str(s)?;
        }
        if let Some(ring) = &mut self.ring {
            ring.write(s.as_bytes());
        }
        Ok(())
    }
}

static CONSOLE: SpinLock<Sinks> = SpinLock::new(Sinks {
    serial: None,
    fb: None,
    ring: None,
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
    let fb = info.framebuffer.get().and_then(|f| {
        let layout = match f.format {
            PixelFormat::Rgbx => PixelLayout::Rgbx,
            PixelFormat::Bgrx => PixelLayout::Bgrx,
            PixelFormat::Masks { red, green, blue } => PixelLayout::Masks { red, green, blue },
        };
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
    *CONSOLE.lock() = Sinks { serial, fb, ring };
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

/// Prints on the panic path, breaking the console lock if its holder is
/// the code that panicked.
pub(crate) fn print_panic(args: fmt::Arguments<'_>) {
    match CONSOLE.try_lock() {
        Some(mut c) => {
            let _ = c.write_fmt(args);
        }
        None => {
            // SAFETY: interrupts are off and there is one CPU in Phase 1, so
            // the holder is the interrupted code on this CPU, which will not
            // run again.
            let _ = unsafe { CONSOLE.force() }.write_fmt(args);
        }
    }
}

/// A `fmt::Write` sink for code that takes one (the test harness).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
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
