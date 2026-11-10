// SPDX-License-Identifier: BSD-2-Clause
//! `sky1_syscon`: the CIX Sky1's shared register windows (`CIXHA018`),
//! the ACPI form of a devicetree `syscon` node. Each instance is a range
//! of registers other devices program through it, which they name by a
//! `_DSD` reference: the FCH CRU (`\_SB.UCRU`, the UARTs'
//! `sky1,fch_cru`), the audio subsystem's CRU (`audss_cru`), and others
//! (clean-room `sky1-audio` specification, §0.6.1).
//!
//! An instance has no behavior of its own and must not be touched when
//! bound: the audio CRU is unpowered until its power resource is on. So
//! attach claims nothing and reads nothing; binding releases the devices
//! that wait for it. Leasing fields of the window to those devices, with
//! the read-modify-write under one lock (DRIVER-MODEL.md §8.1, §9.3's
//! `RegisterRegion`), comes with the first of them that programs one.
//!
//! FreeBSD has no driver for it; the name is recorded in
//! docs/compat-deviations.md.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StopCx, StopReason};
use devtree::ids::AcpiId;

/// `sky1_sysconN`.
#[derive(Debug)]
pub struct Sky1Syscon;

impl Driver for Sky1Syscon {
    const NAME: &'static str = "sky1_syscon";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXHA018")]);

    fn describe(_: &Ident) -> String {
        "CIX Sky1 shared register window".into()
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Sky1Syscon>();
