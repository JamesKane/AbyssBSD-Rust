// SPDX-License-Identifier: BSD-2-Clause
//! `apeiN`: the ACPI Platform Error Interface, as FreeBSD's `apei` bus
//! driver has it: the device that APEI's tables hang their drivers on.
//!
//! FreeBSD adds `apei0` to `acpi0` when the namespace has no `PNP0C33`
//! (ACPI error) device, so it is there on every machine, with or without
//! the tables; `acpi0` does the same here. What FreeBSD hangs on it is
//! `hest`, which parses the Hardware Error Source Table and takes the
//! generic hardware error sources' records (polled, by interrupt, or by
//! NMI). That is not done here: no machine that AbyssBSD runs on has a
//! HEST (the Orange Pi 6 Plus's firmware has none, and neither do the
//! corpora of the X870E, the Z690-A, and the virtual machines), so it
//! could not be tested. docs/compat-deviations.md has it.
//!
//! A real `PNP0C33` device in the namespace is not claimed either.
//!
//! Behavior reference: sys/dev/acpica/acpi_apei_bus.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StopCx, StopReason};

/// The `apeiN` device.
#[derive(Debug)]
pub struct Apei;

impl Driver for Apei {
    const NAME: &'static str = "apei";
    const IDS: Ids = Ids::Nexus(&["apei"]);

    fn describe(_: &Ident) -> String {
        "ACPI Platform Error Interface".into()
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Apei>();
