// SPDX-License-Identifier: BSD-2-Clause
//! UARTs (`uart`) from their ACPI devices: 16550-compatible COM ports and
//! the PL011, named and described as FreeBSD names them.
//!
//! The kernel's console keeps driving its UART itself (the early console,
//! from SPCR or the loader). The `uart` on the same registers takes the
//! port's interrupt for it: the filter has the console take received bytes
//! off the UART, and the interrupt thread passes them to the console's
//! terminal (`ttyu0` for `uart0`). The console says which port is its own
//! through [`CONSOLE`].
//!
//! Every other 16550 is a terminal of its own (`ttyu1`, `cuau1`, ...),
//! made by the kernel's terminal layer through [`TERMINALS`]: the port is
//! the terminal's device ([`Port`]), interrupt-driven both ways. The
//! filter moves received bytes to a buffer and notes room to transmit and
//! modem changes; the interrupt thread types the bytes, sends more output,
//! and reports the carrier ([`Line`]). Other PL011s claim nothing yet.
//!
//! Behavior reference: sys/dev/uart/uart_bus_acpi.c, uart_dev_ns8250.c,
//! uart_dev_pl011.c, uart_tty.c, uart_core.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, Resource, StartCx, StopCx,
    StopReason,
};
use devtree::ids::AcpiId;
use irq::Filter;
use sched::{IrqCx, Sleepable};
use sync::{Once, SpinLock};

/// PL011s by ACPI ID.
const PL011: &[&str] = &["ARMH0011", "ARMHB000"];

/// The kernel console's UART, as the `uart` driver needs it.
pub trait ConsolePort: Sync {
    /// Whether the console's UART is the device with `resources`.
    fn is(&self, resources: &[Resource]) -> bool;
    /// Turns on interrupts for received data; `unit` is the `uart` unit.
    fn enable_input(&self, unit: u32);
    /// In interrupt context: takes what the UART received; whether there
    /// was anything.
    fn receive(&self) -> bool;
    /// In the interrupt thread: hands what was received to the terminal.
    fn deliver(&self, s: &Sleepable);
}

/// The console's port, set by the kernel before devices attach.
pub static CONSOLE: Once<&'static dyn ConsolePort> = Once::new();

/// The modem lines (FreeBSD's `SER_*`), as [`Port::modem`] sets and
/// reports them.
pub const SER_DTR: i32 = 0x01;
pub const SER_RTS: i32 = 0x02;
pub const SER_CTS: i32 = 0x10;
pub const SER_DCD: i32 = 0x20;
pub const SER_RI: i32 = 0x40;
pub const SER_DSR: i32 = 0x80;

/// How the kernel makes a port a terminal (`uart_tty_attach`).
pub trait Terminals: Sync {
    /// Makes `ttyu<unit>` (and its other nodes) with `port` as its device;
    /// returns what the port's interrupts tell.
    fn attach(&self, unit: u32, port: Arc<dyn Port>) -> Arc<dyn Line>;
}

/// The kernel's terminal layer, set before devices attach.
pub static TERMINALS: Once<&'static dyn Terminals> = Once::new();

/// A port, as its terminal drives it (`uart_tty_class`).
pub trait Port: Send + Sync {
    /// Sends what fits of `getc`'s output now (`uart_tty_outwakeup`); with
    /// some sent, the port interrupts when it has room again.
    fn transmit(&self, getc: &mut dyn FnMut(&mut [u8]) -> usize);
    /// Sets the speed, data bits, two stop bits, and parity (`Some(even)`);
    /// `false` if the port cannot.
    fn param(&self, speed: u32, bits: u8, stop2: bool, parity: Option<bool>) -> bool;
    /// Raises the lines in `on`, drops those in `off`; the lines' state
    /// (`SER_*`).
    fn modem(&self, on: i32, off: i32) -> i32;
    /// Sends a break while `on`.
    fn set_break(&self, on: bool);
    /// Loops what the port sends back to its receiver while `on` (for
    /// tests of a port's paths).
    fn set_loopback(&self, on: bool);
}

/// The terminal, as its port's interrupt thread tells it.
pub trait Line: Send + Sync {
    /// Bytes received; returns how many it took.
    fn input(&self, s: &Sleepable, bytes: &[u8]) -> usize;
    /// The port has room to transmit.
    fn transmit(&self, s: &Sleepable);
    /// The carrier came (`true`) or went.
    fn carrier(&self, s: &Sleepable, on: bool);
    /// The port went (`tty_rel_gone`).
    fn gone(&self, s: &Sleepable);
}

/// A 16550's registers in port space.
#[derive(Debug)]
struct Io(bus::IoGrant);

impl regs::RegisterIo for Io {
    fn read(&self, n: usize) -> u8 {
        self.0.read8(u16::try_from(n).unwrap_or(0))
    }
    fn write(&self, n: usize, v: u8) {
        self.0.write8(u16::try_from(n).unwrap_or(0), v);
    }
}

/// A 16550's registers, in port space or memory (with the mapping kept).
#[derive(Debug)]
enum Regs {
    Io(uart_16550::Uart<Io>),
    Mmio {
        uart: uart_16550::Uart<regs::StridedMmio>,
        /// Unmapped when dropped.
        _mapping: bus::MmioGrant<regs::Bytes>,
    },
}

macro_rules! with_uart {
    ($regs:expr, $u:ident => $e:expr) => {
        match $regs {
            Regs::Io($u) => $e,
            Regs::Mmio { uart: $u, .. } => $e,
        }
    };
}

/// [`with_uart!`] on a terminal port's registers, which a stopped port
/// has given back: then `$none`.
macro_rules! with_port {
    ($port:expr, $u:ident => $e:expr, $none:expr) => {{
        let r = $port.regs.lock();
        match r.as_ref() {
            Some(regs) => with_uart!(regs, $u => $e),
            None => $none,
        }
    }};
}

/// What the filter saw, for the thread (`sc_ttypend`).
const PEND_RX: u8 = 1 << 0;
const PEND_TX: u8 = 1 << 1;
const PEND_SIG: u8 = 1 << 2;

/// Bytes received, between the filter and the thread (`sc_rxbuf`).
#[derive(Debug)]
struct Rx {
    buf: [u8; 256],
    len: usize,
}

/// A port that is a terminal.
#[derive(Debug)]
struct TermPort {
    /// Its registers, until the port stops.
    regs: SpinLock<Option<Regs>>,
    rx: SpinLock<Rx>,
    pend: AtomicU8,
    /// Output is in the FIFO (`sc_txbusy`).
    txbusy: AtomicBool,
    /// The carrier as last seen.
    dcd: AtomicBool,
    line: Once<Arc<dyn Line>>,
}

impl core::fmt::Debug for dyn Line {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Line")
    }
}

impl Port for TermPort {
    /// Under the registers' lock, as the filter decides the transmitter
    /// is done under it: a burst started outside it could be marked done
    /// by the filter before its bytes were in the FIFO.
    fn transmit(&self, getc: &mut dyn FnMut(&mut [u8]) -> usize) {
        with_port!(self, u => {
            if self.txbusy.load(Ordering::Acquire) {
                return;
            }
            let mut buf = [0u8; uart_16550::TX_FIFO];
            let n = getc(&mut buf);
            if n == 0 {
                return;
            }
            self.txbusy.store(true, Ordering::Release);
            for &b in &buf[..n] {
                u.put_now(b);
            }
            u.set_interrupts(true, true, true);
        }, ());
    }

    fn param(&self, speed: u32, bits: u8, stop2: bool, parity: Option<bool>) -> bool {
        with_port!(self, u => u.set_line(uart_16550::RCLK, speed, bits, stop2, parity), false)
    }

    fn modem(&self, on: i32, off: i32) -> i32 {
        with_port!(self, u => {
            if on != 0 || off != 0 {
                let (dtr, rts) = u.modem_out();
                let dtr = if on & SER_DTR != 0 { true } else { dtr && off & SER_DTR == 0 };
                let rts = if on & SER_RTS != 0 { true } else { rts && off & SER_RTS == 0 };
                u.set_modem(dtr, rts);
            }
            let (dtr, rts) = u.modem_out();
            let msr = u.modem_status();
            let mut sig = 0;
            for (have, bit) in [
                (dtr, SER_DTR),
                (rts, SER_RTS),
                (msr & uart_16550::MSR_CTS != 0, SER_CTS),
                (msr & uart_16550::MSR_DCD != 0, SER_DCD),
                (msr & uart_16550::MSR_RI != 0, SER_RI),
                (msr & uart_16550::MSR_DSR != 0, SER_DSR),
            ] {
                if have {
                    sig |= bit;
                }
            }
            sig
        }, 0)
    }

    fn set_break(&self, on: bool) {
        with_port!(self, u => u.set_break(on), ());
    }

    fn set_loopback(&self, on: bool) {
        with_port!(self, u => u.set_loopback(on), ());
    }
}

impl TermPort {
    /// In the filter: takes what came, and notes room and modem changes
    /// (`ns8250_bus_ipend`, `ns8250_bus_receive`).
    fn filter(&self) -> bool {
        let r = self.regs.lock();
        let Some(regs) = r.as_ref() else {
            return false;
        };
        let mut pend = 0;
        with_uart!(regs, u => {
            if !u.pending() && !u.tx_empty() {
                return false;
            }
            let mut rx = self.rx.lock();
            while rx.len < rx.buf.len() {
                let Some(b) = u.get() else { break };
                let at = rx.len;
                rx.buf[at] = b;
                rx.len += 1;
                pend |= PEND_RX;
            }
            if rx.len == rx.buf.len() {
                // Full: the rest waits in the FIFO.
                u.set_interrupts(false, self.txbusy.load(Ordering::Acquire), true);
            }
            drop(rx);
            if self.txbusy.load(Ordering::Acquire) && u.tx_empty() {
                self.txbusy.store(false, Ordering::Release);
                pend |= PEND_TX;
                u.set_interrupts(true, false, true);
            }
            let msr = u.modem_status();
            if msr & uart_16550::MSR_DELTA != 0 {
                pend |= PEND_SIG;
            }
        });
        if pend == 0 {
            return false;
        }
        self.pend.fetch_or(pend, Ordering::AcqRel);
        true
    }

    /// In the interrupt thread (`uart_tty_intr`).
    fn deliver(&self, s: &Sleepable) {
        let pend = self.pend.swap(0, Ordering::AcqRel);
        let Some(line) = self.line.get() else {
            return;
        };
        if pend & PEND_RX != 0 {
            loop {
                let mut buf = [0u8; 256];
                let n = {
                    let mut rx = self.rx.lock();
                    let n = rx.len;
                    buf[..n].copy_from_slice(&rx.buf[..n]);
                    rx.len = 0;
                    n
                };
                if n == 0 {
                    break;
                }
                // What the terminal refuses is dropped (its queue is
                // full); the receive interrupt is back on either way.
                let _ = line.input(s, &buf[..n]);
                // Whether a burst is out is read under the registers' lock:
                // read before it, a burst started meanwhile would have its
                // interrupt turned off, and output would wait for input.
                with_port!(self, u => {
                    u.set_interrupts(true, self.txbusy.load(Ordering::Acquire), true);
                }, ());
            }
        }
        if pend & PEND_SIG != 0 {
            let dcd = self.modem(0, 0) & SER_DCD != 0;
            if self.dcd.swap(dcd, Ordering::AcqRel) != dcd {
                line.carrier(s, dcd);
            }
        }
        if pend & (PEND_TX | PEND_SIG) != 0 {
            line.transmit(s);
        }
    }
}

/// A `uartN` instance.
#[derive(Debug)]
pub struct Uart {
    /// The port's interrupt, until `start` installs it.
    line: SpinLock<Option<IrqLine>>,
    unit: u32,
    /// Not the console's: a terminal of its own.
    term: Option<Arc<TermPort>>,
}

fn console_filter(_: &Uart, _: usize, _: &IrqCx) -> Filter {
    match CONSOLE.get() {
        Some(c) if c.receive() => Filter::Schedule,
        _ => Filter::Stray,
    }
}

fn console_thread(_: &Uart, _: usize, s: &Sleepable) {
    if let Some(c) = CONSOLE.get() {
        c.deliver(s);
    }
}

fn term_filter(u: &Uart, _: usize, _: &IrqCx) -> Filter {
    match &u.term {
        Some(t) if t.filter() => Filter::Schedule,
        _ => Filter::Stray,
    }
}

fn term_thread(u: &Uart, _: usize, s: &Sleepable) {
    if let Some(t) = &u.term {
        t.deliver(s);
    }
}

impl Driver for Uart {
    const NAME: &'static str = "uart";
    const IDS: Ids = Ids::Acpi(&[
        AcpiId::new("PNP0500"),
        AcpiId::new("PNP0501"),
        AcpiId::new("PNP0502"),
        AcpiId::new("ARMH0011"),
        AcpiId::new("ARMHB000"),
    ]);

    /// GENERIC's hints (`sys/amd64/conf/GENERIC.hints`): COM1 and COM2.
    /// arm64 has none.
    const HINTS: &'static [bus::Hint] = if cfg!(target_arch = "x86_64") {
        &[
            bus::Hint {
                unit: 0,
                port: 0x3f8,
            },
            bus::Hint {
                unit: 1,
                port: 0x2f8,
            },
        ]
    } else {
        &[]
    };

    fn describe(ident: &Ident) -> String {
        let pl011 = matches!(ident, Ident::Acpi(a, _)
            if a.hid.as_deref().is_some_and(|h| PL011.contains(&h))
                || a.cids.iter().any(|c| PL011.contains(&c.as_str())));
        // FreeBSD probes the FIFO; every emulated and modern 16550 has one.
        if pl011 {
            "PrimeCell UART (PL011)"
        } else {
            "16550 or compatible"
        }
        .into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        // The console's port takes its interrupt for input. Without one it
        // still prints; it just cannot be typed at.
        if CONSOLE.get().is_some_and(|c| c.is(cx.resources())) {
            return Ok(Self {
                line: SpinLock::new(cx.irq(0).ok()),
                unit: cx.unit(),
                term: None,
            });
        }
        // Another 16550 in port space or memory is a terminal: its
        // registers, reset to a known state (`ns8250_bus_attach`).
        let is_pl011 = matches!(cx.ident(), Ident::Acpi(a, _)
            if a.hid.as_deref().is_some_and(|h| PL011.contains(&h)));
        let has_io = cx
            .resources()
            .iter()
            .any(|r| matches!(r, Resource::IoPort { .. }));
        let regs = if is_pl011 || TERMINALS.get().is_none() {
            None
        } else if has_io {
            Some(Regs::Io(uart_16550::Uart::init(Io(cx.io_ports(0)?))))
        } else {
            cx.mmio_bytes(0).ok().map(|m| {
                let u =
                    uart_16550::Uart::init(regs::StridedMmio::new(*m, 0, regs::AccessWidth::Byte));
                Regs::Mmio {
                    uart: u,
                    _mapping: m,
                }
            })
        };
        let term = regs.map(|r| {
            with_uart!(&r, u => {
                u.reset_fifos();
                u.set_line(uart_16550::RCLK, 9600, 8, false, None);
            });
            Arc::new(TermPort {
                regs: SpinLock::new(Some(r)),
                rx: SpinLock::new(Rx {
                    buf: [0; 256],
                    len: 0,
                }),
                pend: AtomicU8::new(0),
                txbusy: AtomicBool::new(false),
                dcd: AtomicBool::new(false),
                line: Once::new(),
            })
        });
        let line = if term.is_some() { cx.irq(0).ok() } else { None };
        Ok(Self {
            line: SpinLock::new(line),
            unit: cx.unit(),
            term,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        // Taken in its own statement: installing may sleep.
        let line = self.line.lock().take();
        if let Some(t) = &self.term {
            // Without its interrupt (a line another port has, as COM3
            // shares COM1's), it stays a device only.
            let (Some(host), Some(line)) = (TERMINALS.get(), line) else {
                return Ok(());
            };
            if cx
                .interrupt(line, Some(term_filter), Some(term_thread), 0)
                .is_err()
            {
                return Ok(());
            }
            let tty_line = host.attach(self.unit, Arc::clone(t) as Arc<dyn Port>);
            let _ = t.line.set(tty_line);
            t.dcd.store(t.modem(0, 0) & SER_DCD != 0, Ordering::Release);
            with_port!(t, u => u.set_interrupts(true, false, true), ());
            return Ok(());
        }
        if let (Some(line), Some(port)) = (line, CONSOLE.get()) {
            cx.interrupt(line, Some(console_filter), Some(console_thread), 0)?;
            port.enable_input(self.unit);
        }
        Ok(())
    }

    /// A terminal port's terminal goes (`uart_tty_detach`) and its
    /// registers go back.
    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        if let Some(t) = &self.term {
            with_port!(t, u => u.set_interrupts(false, false, false), ());
            if let Some(line) = t.line.get() {
                line.gone(cx.sleepable());
            }
            drop(t.regs.lock().take());
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Uart>();
