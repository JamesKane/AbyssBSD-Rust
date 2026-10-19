// SPDX-License-Identifier: BSD-2-Clause
//! UARTs (`uart`) from their ACPI devices: 16550-compatible COM ports and
//! the PL011, named and described as FreeBSD names them.
//!
//! The kernel's console keeps driving its UART itself (the early console,
//! from SPCR or the loader); this driver claims no registers until the
//! tty class takes UARTs over (P4), so a console port stays the
//! console's.
//!
//! Behavior reference: sys/dev/uart/uart_bus_acpi.c, uart_dev_ns8250.c,
//! uart_dev_pl011.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StopCx, StopReason};
use devtree::ids::AcpiId;

/// PL011s by ACPI ID.
const PL011: &[&str] = &["ARMH0011", "ARMHB000"];

/// A `uartN` instance.
#[derive(Debug)]
pub struct Uart;

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

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Uart>();
