// SPDX-License-Identifier: BSD-2-Clause
//! UARTs (`uart`) from their ACPI devices: 16550-compatible COM ports and
//! the PL011, named and described as FreeBSD names them.
//!
//! The kernel's console keeps driving its UART itself (the early console,
//! from SPCR or the loader). The `uart` on the same registers takes the
//! port's interrupt for it: the filter has the console take received bytes
//! off the UART, and the interrupt thread passes them to the console's
//! terminal (`ttyu0` for `uart0`). The console says which port is its own
//! through [`CONSOLE`]. Other ports claim nothing yet.
//!
//! Behavior reference: sys/dev/uart/uart_bus_acpi.c, uart_dev_ns8250.c,
//! uart_dev_pl011.c, uart_tty.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;

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

/// A `uartN` instance.
#[derive(Debug)]
pub struct Uart {
    /// The console port's interrupt, until `start` installs it.
    line: SpinLock<Option<IrqLine>>,
    unit: u32,
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

impl Driver for Uart {
    const NAME: &'static str = "uart";
    const IDS: Ids = Ids::Acpi(&[
        AcpiId::new("PNP0500"),
        AcpiId::new("PNP0501"),
        AcpiId::new("PNP0502"),
        AcpiId::new("ARMH0011"),
        AcpiId::new("ARMHB000"),
    ]);

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
        let line = if CONSOLE.get().is_some_and(|c| c.is(cx.resources())) {
            cx.irq(0).ok()
        } else {
            None
        };
        Ok(Self {
            line: SpinLock::new(line),
            unit: cx.unit(),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        // Taken in its own statement: installing may sleep.
        let line = self.line.lock().take();
        if let (Some(line), Some(port)) = (line, CONSOLE.get()) {
            cx.interrupt(line, Some(console_filter), Some(console_thread), 0)?;
            port.enable_input(self.unit);
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Uart>();
