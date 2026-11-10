// SPDX-License-Identifier: BSD-2-Clause
//! Requester IDs: the ID a function's DMA carries when it reaches the
//! root complex, which an IOMMU sees it by (`%iommu`'s `rid=`).
//!
//! FreeBSD's `iommu_get_requester` walks from the function up through
//! the PCI-PCI bridges:
//!
//! - a PCIe function is seen by its own ID, through PCIe bridges;
//! - a conventional function behind a PCIe-to-PCI bridge is seen as
//!   (secondary bus, 0, 0), and the walk goes on from the bridge above
//!   that one, which must itself be a PCI function;
//! - one behind a conventional bridge is seen as the bridge, and the
//!   walk goes on from the bridge.
//!
//! Each bus carries what the rest of that walk does above it
//! ([`Upstream`]), so a function's ID needs only its own bus's record:
//! the walk from a bridge up is the same for every function behind it.
//!
//! Behavior reference: sys/dev/iommu/busdma_iommu.c (`iommu_get_requester`).

use devtree::strings::PciAddress;

use crate::caps::{Capability, find, id};
use crate::config::ConfigSpace;

/// The PCI Express Capabilities register's device/port type of a PCI
/// Express to PCI/PCI-X bridge (`PCIEM_TYPE_PCI_BRIDGE`).
pub const TYPE_PCI_BRIDGE: u8 = 0x7;

/// The requester ID of the function at `a`: bus, device, function.
#[must_use]
pub fn rid(a: PciAddress) -> u16 {
    u16::from(a.bus) << 8 | u16::from(a.slot & 0x1f) << 3 | u16::from(a.function & 7)
}

/// A function's PCI Express device/port type, if it has the capability.
pub fn express_type(cfg: &dyn ConfigSpace, a: PciAddress, caps: &[Capability]) -> Option<u8> {
    let c = find(caps, id::PCIE)?;
    let flags = cfg.read16(a, u16::from(c.offset) + 2);
    Some(((flags >> 4) & 0xf).to_le_bytes()[0])
}

/// What the walk does from some point up: the ID it ends with, given the
/// one it carries there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Keeps the ID it has (it reaches a host bridge with it).
    Keep,
    /// Ends with this ID, whatever it carries.
    Fixed(u16),
    /// Fails: it reaches something that is not a PCI function (FreeBSD's
    /// "non-pci ancestor", `ENXIO`).
    Fail,
}

impl Step {
    /// This step, entered carrying `id`.
    #[must_use]
    pub fn from(self, id: u16) -> Self {
        match self {
            Self::Keep => Self::Fixed(id),
            s => s,
        }
    }

    /// The ID the walk ends with, entered carrying `id`.
    #[must_use]
    pub fn apply(self, id: u16) -> Option<u16> {
        match self {
            Self::Keep => Some(id),
            Self::Fixed(v) => Some(v),
            Self::Fail => None,
        }
    }
}

/// What is above a bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upstream {
    /// A host bridge: the root bus.
    Host,
    /// A PCI-PCI bridge.
    Bridge(Bridge),
}

/// A PCI-PCI bridge above a bus, as the walk sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bridge {
    /// Its own requester ID.
    pub rid: u16,
    /// Its PCI Express device/port type, if it has the capability.
    pub express: Option<u8>,
    /// The walk from the bridge up.
    pub step: Step,
    /// What is above the bridge's own bus.
    pub above: Above,
}

/// What is above a bridge's own bus, as far as the walk looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Above {
    /// A host bridge.
    Host,
    /// Another PCI-PCI bridge: its PCI Express type, if any, and the walk
    /// from it up.
    Bridge {
        /// Its PCI Express device/port type, if it has the capability.
        express: Option<u8>,
        /// The walk from it up.
        step: Step,
    },
}

impl Upstream {
    /// The walk from a function on this bus (on bus `bus`, PCI Express if
    /// `express`) up.
    #[must_use]
    pub fn step(&self, bus: u8, express: bool) -> Step {
        let b = match self {
            Self::Host => return Step::Keep,
            Self::Bridge(b) => b,
        };
        if express {
            // Seen by its own ID through the bridge: the walk goes on
            // from the bridge.
            return b.step;
        }
        // A conventional function: is the bridge above it PCI Express to
        // PCI? It is if it says so, or if the bridge above it is PCI
        // Express but not a PCIe-to-PCI bridge (a bridge that does not
        // report its capability).
        let pcie_to_pci = b.express.is_some()
            || matches!(b.above, Above::Bridge { express: Some(t), .. } if t != TYPE_PCI_BRIDGE);
        if pcie_to_pci {
            match b.above {
                // The walk goes on from the bridge above, which is not a
                // PCI function.
                Above::Host => Step::Fail,
                Above::Bridge { step, .. } => step.from(u16::from(bus) << 8),
            }
        } else {
            b.step.from(b.rid)
        }
    }

    /// The requester ID of the function at `a` on this bus, PCI Express if
    /// `express`: `None` where FreeBSD's walk fails.
    #[must_use]
    pub fn requester(&self, a: PciAddress, express: bool) -> Option<u16> {
        self.step(a.bus, express).apply(rid(a))
    }

    /// The record for the bus behind bridge `a` (with PCI Express type
    /// `express`), which sits on this bus.
    #[must_use]
    pub fn behind(&self, a: PciAddress, express: Option<u8>) -> Self {
        Self::Bridge(Bridge {
            rid: rid(a),
            express,
            step: self.step(a.bus, express.is_some()),
            above: match self {
                Self::Host => Above::Host,
                Self::Bridge(b) => Above::Bridge {
                    express: b.express,
                    step: b.step,
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(bus: u8, slot: u8, function: u8) -> PciAddress {
        PciAddress {
            domain: 0,
            bus,
            slot,
            function,
        }
    }

    /// PCIe port types.
    const ROOT_PORT: u8 = 0x4;
    const DOWNSTREAM: u8 = 0x6;
    const UPSTREAM: u8 = 0x5;

    #[test]
    fn root_bus_functions_are_themselves() {
        // bhyve's devices, all on bus 0 behind the host bridge.
        let host = Upstream::Host;
        for (slot, want) in [
            (0, 0),
            (2, 0x10),
            (5, 0x28),
            (9, 0x48),
            (29, 0xe8),
            (31, 0xf8),
        ] {
            assert_eq!(host.requester(at(0, slot, 0), false), Some(want));
            assert_eq!(host.requester(at(0, slot, 0), true), Some(want));
        }
    }

    #[test]
    fn pcie_behind_root_ports() {
        // The Orange Pi 6 Plus: root ports on the root buses, an NVMe or
        // a NIC behind each.
        let host = Upstream::Host;
        for (root_bus, nvme) in [(0xc0, 0xc100), (0x90, 0x9100), (0x30, 0x3100)] {
            let port = at(root_bus, 0, 0);
            assert_eq!(host.requester(port, true), Some(rid(port)));
            let below = host.behind(port, Some(ROOT_PORT));
            assert_eq!(below.requester(at(root_bus + 1, 0, 0), true), Some(nvme));
            // A switch below: still its own ID.
            let up = below.behind(at(root_bus + 1, 0, 0), Some(UPSTREAM));
            let down = up.behind(at(root_bus + 2, 1, 0), Some(DOWNSTREAM));
            let f = at(root_bus + 3, 0, 2);
            assert_eq!(down.requester(f, true), Some(rid(f)));
        }
    }

    #[test]
    fn conventional_functions() {
        let host = Upstream::Host;
        // Behind a PCIe-to-PCI bridge on a root port: (secondary, 0, 0).
        let port = host.behind(at(0, 1, 0), Some(ROOT_PORT));
        let bridge = port.behind(at(1, 0, 0), Some(TYPE_PCI_BRIDGE));
        assert_eq!(bridge.requester(at(2, 3, 1), false), Some(0x0200));
        // A PCIe-to-PCI bridge right on the root bus: the walk reaches the
        // host bridge as "the bridge above", not a PCI function, and
        // fails, as FreeBSD's does.
        let bridge0 = host.behind(at(0, 4, 0), Some(TYPE_PCI_BRIDGE));
        assert_eq!(bridge0.requester(at(3, 0, 0), false), None);
        // One that does not report its capability, under a root port: the
        // root port says what it is.
        let quiet = port.behind(at(1, 0, 0), None);
        assert_eq!(quiet.requester(at(2, 5, 0), false), Some(0x0200));
        // A conventional bridge under a conventional bridge on the root
        // bus: seen as the topmost bridge.
        let pci = host.behind(at(0, 6, 0), None);
        let pci2 = pci.behind(at(4, 2, 0), None);
        assert_eq!(pci.requester(at(4, 7, 0), false), Some(rid(at(0, 6, 0))));
        assert_eq!(pci2.requester(at(5, 1, 0), false), Some(rid(at(0, 6, 0))));
        // A conventional function behind a PCIe switch's downstream port
        // whose capability says what it is.
        let up = port.behind(at(1, 0, 0), Some(UPSTREAM));
        let down = up.behind(at(2, 0, 0), Some(DOWNSTREAM));
        assert_eq!(down.requester(at(3, 1, 0), false), Some(0x0300));
    }
}
