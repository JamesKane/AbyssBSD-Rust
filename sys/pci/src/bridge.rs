// SPDX-License-Identifier: BSD-2-Clause
//! `pcib` on a PCI bus: a PCI-PCI bridge (FreeBSD's `pci_pci`), with the
//! bus behind it as its `pci` child.
//!
//! Interrupts behind a bridge are routed by the standard swizzle through
//! the bridge's own slot on the parent bus, unless firmware gives the
//! bridge a routing table (the ACPI flavor of the driver, in
//! `sys/acpi`, uses [`secondary`] with its `_PRT`).
//!
//! Bus numbers are firmware's: the secondary bus must be above the
//! bridge's own, so nesting cannot loop, and bridges nest at most
//! [`MAX_DEPTH`] deep.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::ids::PciId;
use devtree::matching::Rank;

use crate::bus::{PciBusData, PciChild};
use crate::header::{CMD_BUS_MASTER, CMD_IO, CMD_MEMORY, COMMAND};

/// Bridges nest at most this deep.
pub const MAX_DEPTH: u8 = 32;

/// FreeBSD's `pcib` probes at -10000, below every other bridge driver.
const RANK: Rank = Rank(-10_000);

/// The routes behind a bridge in slot `bridge_slot` of a bus routed by
/// `parent`: device `slot`'s pin `pin` (0 = INTA) arrives at the bridge's
/// pin `(slot + pin) % 4` (PCI-to-PCI Bridge Architecture §9.1).
#[must_use]
pub fn swizzle(parent: &[(u8, u8, Resource)], bridge_slot: u8) -> Vec<(u8, u8, Resource)> {
    let mut out = Vec::new();
    for slot in 0..32u8 {
        for pin in 0..4u8 {
            let at = (slot + pin) % 4;
            if let Some((_, _, r)) = parent
                .iter()
                .find(|(s, p, _)| *s == bridge_slot && *p == at)
            {
                out.push((slot, pin, *r));
            }
        }
    }
    out
}

/// The bus behind bridge function `child` and the data for its `pci`
/// child: the ACPI devices under the bridge by `_ADR`, and interrupts
/// routed by `intx` if firmware gave a table, else by [`swizzle`]. Turns
/// on the bridge's forwarding (decode and bus mastering).
pub fn secondary(
    child: &PciChild,
    adr_paths: Vec<(u32, String)>,
    intx: Option<Vec<(u8, u8, Resource)>>,
) -> Result<(u8, PciBusData), AttachError> {
    let f = &child.function;
    let b = f
        .bridge
        .ok_or(AttachError::Device("not a PCI-PCI bridge header"))?;
    if b.secondary <= f.addr.bus || b.subordinate < b.secondary {
        return Err(AttachError::Device("bridge bus numbers out of order"));
    }
    if f.depth >= MAX_DEPTH {
        return Err(AttachError::Device("bridges nested too deep"));
    }
    let cmd = child.cfg.read16(f.addr, COMMAND);
    child
        .cfg
        .write16(f.addr, COMMAND, cmd | CMD_IO | CMD_MEMORY | CMD_BUS_MASTER);
    let intx = intx.unwrap_or_else(|| swizzle(&child.bus.intx, f.addr.slot));
    Ok((
        b.secondary,
        PciBusData {
            cfg: child.cfg,
            adr_paths,
            intx,
            depth: f.depth + 1,
        },
    ))
}

/// `pcibN` (PCI-PCI bridge).
#[derive(Debug)]
pub struct PciPciBridge {
    data: Arc<PciBusData>,
    domain: u32,
    bus: u8,
}

impl Driver for PciPciBridge {
    const NAME: &'static str = "pcib";
    const IDS: Ids = Ids::Pci(&[PciId::subclass(0x06, 0x04).rank(RANK)]);

    fn describe(_: &Ident) -> String {
        "PCI-PCI bridge".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let (bus, data) = secondary(&child, Vec::new(), None)?;
        Ok(Self {
            data: Arc::new(data),
            domain: child.function.addr.domain,
            bus,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.add_child(
            Ident::PciBus {
                domain: self.domain,
                bus: self.bus,
                acpi: false,
            },
            Some(Arc::clone(&self.data) as _),
            Vec::new(),
        );
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<PciPciBridge>();
