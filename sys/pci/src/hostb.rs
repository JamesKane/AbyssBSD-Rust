// SPDX-License-Identifier: BSD-2-Clause
//! `hostbN`: a host bridge function on a PCI bus (FreeBSD's `pci_hostb`),
//! "Host to PCI bridge".
//!
//! It has nothing to drive; it holds the function so that the tree and
//! `pciconf` name it as FreeBSD does. As there, it probes below every
//! other driver, is quiet at boot (`device_quiet`), and declines VIA's
//! VT82C596 power-management function, which shares the class. FreeBSD
//! adds an `agp` child for a bridge with an AGP capability; there is no
//! `agp` driver here, so none is added.
//!
//! Behavior reference: sys/dev/pci/hostb_pci.c.

use alloc::string::String;

use bus::{
    Announce, AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use devtree::ids::PciId;
use devtree::matching::Rank;

/// `hostbN`.
#[derive(Debug)]
pub struct HostBridge;

impl Driver for HostBridge {
    const NAME: &'static str = "hostb";
    // `pci_hostb_probe` returns -10000.
    const IDS: Ids = Ids::Pci(&[PciId::subclass(0x06, 0x00).rank(Rank(-10_000))]);
    const ANNOUNCE: Announce = Announce::Quiet;

    fn describe(_: &Ident) -> String {
        "Host to PCI bridge".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        match cx.ident() {
            // VIA VT82C596 Power Management Function.
            Ident::Pci(id, ..) if id.vendor == 0x1106 && id.device == 0x3050 => {
                Err(AttachError::Decline)
            }
            Ident::Pci(..) => Ok(Self),
            _ => Err(AttachError::Decline),
        }
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for every ISA's driver table (`optional pci`).
pub const DRIVER: DriverDecl = DriverDecl::of::<HostBridge>();
