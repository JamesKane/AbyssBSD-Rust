// SPDX-License-Identifier: BSD-2-Clause
//! The PC timers ACPI describes, adopted under FreeBSD's names: the 8254
//! (`attimer`), the HPET (`hpet`), and the ACPI PM timer (`acpi_timer`,
//! which `acpi0` adds from the FADT, as FreeBSD's identify does).
//!
//! The kernel's clocks stay its own (the local APIC timer and the TSC,
//! calibrated against the PM timer); these drivers claim nothing, so the
//! tree names the hardware as FreeBSD's does.
//!
//! Behavior reference: sys/x86/isa/clock.c, sys/dev/acpica/acpi_hpet.c,
//! sys/dev/acpica/acpi_timer.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StopCx, StopReason};
use devtree::ids::AcpiId;

/// An `attimerN` instance.
#[derive(Debug)]
pub struct AtTimer;

impl Driver for AtTimer {
    const NAME: &'static str = "attimer";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("PNP0100")]);

    fn describe(_: &Ident) -> String {
        "AT timer".into()
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// An `hpetN` instance.
#[derive(Debug)]
pub struct Hpet;

impl Driver for Hpet {
    const NAME: &'static str = "hpet";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("PNP0103")]);

    fn describe(_: &Ident) -> String {
        "High Precision Event Timer".into()
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// An `acpi_timerN` instance.
#[derive(Debug)]
pub struct AcpiTimer;

impl Driver for AcpiTimer {
    const NAME: &'static str = "acpi_timer";
    const IDS: Ids = Ids::Nexus(&["acpi_timer"]);

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Child { desc, .. } => (*desc).into(),
            _ => "ACPI timer".into(),
        }
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declarations for the build's driver table.
pub const ATTIMER: DriverDecl = DriverDecl::of::<AtTimer>();
/// See [`ATTIMER`].
pub const HPET: DriverDecl = DriverDecl::of::<Hpet>();
/// See [`ATTIMER`].
pub const ACPI_TIMER: DriverDecl = DriverDecl::of::<AcpiTimer>();
