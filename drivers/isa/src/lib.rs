// SPDX-License-Identifier: BSD-2-Clause
//! The PCI-ISA (LPC) bridge (`isab`) and the ISA bus behind it (`isa`).
//!
//! ISA devices themselves come from ACPI here; what the bus adds is what
//! FreeBSD's identify routines add on chipsets that have it: the Intel
//! TCO watchdog (`ichwd`), at the TCO registers in the ACPI PM I/O block,
//! with the chipset's GCS register for its NO_REBOOT bit.
//!
//! Behavior reference: sys/dev/pci/isa_pci.c, sys/isa/isa_common.c,
//! sys/dev/ichwd/ichwd.c (identify).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::ids::PciId;
use devtree::matching::Rank;
use pci::bus::PciChild;

/// The LPC bridge's configuration: ACPI PM base (bits 15:7), and the root
/// complex base (bits 31:14, bit 0 enable).
const PMBASE: u16 = 0x40;
const RCBA: u16 = 0xf0;
/// The TCO registers in the PM block, and GCS in the root complex block.
const TCO_OFFSET: u64 = 0x60;
const TCO_LEN: u16 = 0x20;
const GCS_OFFSET: u64 = 0x3410;

/// `isabN`.
#[derive(Debug)]
pub struct Isab {
    bridge: Arc<PciChild>,
}

impl Driver for Isab {
    const NAME: &'static str = "isab";
    // FreeBSD probes any PCI-ISA bridge at -10000.
    const IDS: Ids = Ids::Pci(&[PciId::subclass(0x06, 0x01).rank(Rank(-10_000))]);

    fn describe(_: &Ident) -> String {
        "PCI-ISA bridge".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let bridge = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        Ok(Self { bridge })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.add_child(
            Ident::Nexus("isa"),
            Some(Arc::clone(&self.bridge) as _),
            Vec::new(),
        );
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// `isaN`.
#[derive(Debug)]
pub struct Isa {
    bridge: Arc<PciChild>,
}

impl Isa {
    /// The TCO watchdog's resources, on Intel chipsets that have one.
    fn ichwd(&self) -> Option<(&'static str, Vec<Resource>)> {
        let id = self.bridge.function.header.ident;
        if id.vendor != 0x8086 {
            return None;
        }
        let (_, desc) = ichwd::CHIPSETS.iter().find(|(d, _)| *d == id.device)?;
        let pm = u64::from(self.bridge.read32(PMBASE) & 0xff80);
        if pm == 0 {
            return None;
        }
        let mut res = alloc::vec![Resource::IoPort {
            base: u16::try_from(pm + TCO_OFFSET).ok()?,
            len: TCO_LEN,
        }];
        let rcba = self.bridge.read32(RCBA);
        if rcba & 1 != 0 {
            res.push(Resource::Memory {
                base: u64::from(rcba & !0x3fff) + GCS_OFFSET,
                len: 4,
                prefetchable: false,
            });
        }
        Some((desc, res))
    }
}

impl Driver for Isa {
    const NAME: &'static str = "isa";
    const IDS: Ids = Ids::Nexus(&["isa"]);

    fn describe(_: &Ident) -> String {
        "ISA bus".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let bridge = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        Ok(Self { bridge })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        if cfg!(target_arch = "x86_64")
            && let Some((desc, res)) = self.ichwd()
        {
            cx.add_child(
                Ident::Child {
                    class: "ichwd",
                    desc,
                },
                None,
                res,
            );
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declarations for the build's driver table.
pub const ISAB: DriverDecl = DriverDecl::of::<Isab>();
/// See [`ISAB`].
pub const ISA: DriverDecl = DriverDecl::of::<Isa>();
