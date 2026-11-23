// SPDX-License-Identifier: BSD-2-Clause
//! `hostbN`: a host bridge function on a PCI bus (FreeBSD's `pci_hostb`),
//! "Host to PCI bridge".
//!
//! It has little to drive; it holds the function so that the tree and
//! `pciconf` name it as FreeBSD does, and it is the parent of the drivers
//! the platform adds under a host bridge ([`set_children`]: `amdsmn` and
//! `amdtemp` on an AMD processor, whose host bridge is the door to the
//! System Management Network), as FreeBSD's `identify` routines add them
//! to `hostb`. As there, it probes below every other driver, is quiet at
//! boot (`device_quiet`), and declines VIA's VT82C596 power-management
//! function, which shares the class. FreeBSD adds an `agp` child for a
//! bridge with an AGP capability; there is no `agp` driver here, so none is
//! added.
//!
//! Behavior reference: sys/dev/pci/hostb_pci.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    Announce, AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use devtree::graph::EdgeKind;
use devtree::ids::PciId;
use devtree::matching::Rank;
use sync::Once;

use crate::bus::PciChild;

/// A driver the platform adds under every host bridge.
#[derive(Clone, Copy, Debug)]
pub struct Child {
    /// Its class (`amdsmn`).
    pub class: &'static str,
    /// Its description.
    pub description: &'static str,
    /// The class of an earlier entry it needs attached first
    /// (`device_find_child(parent, "amdsmn", -1)` finds it there).
    pub after: Option<&'static str>,
}

/// What the platform names.
static CHILDREN: Once<&'static [Child]> = Once::new();

/// Names the drivers `hostb` adds under every host bridge, in the order to
/// add them; the kernel calls it once, for its ISA. Each declines a bridge
/// that is not the one it drives.
pub fn set_children(children: &'static [Child]) {
    let _ = CHILDREN.set(children);
}

/// What `hostb` gives each driver it adds.
#[derive(Debug)]
pub struct HostbChild {
    /// The host bridge function: its identity and configuration space.
    pub bridge: Arc<PciChild>,
}

/// `hostbN`.
#[derive(Debug)]
pub struct HostBridge {
    bridge: Arc<PciChild>,
}

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
            Ident::Pci(..) => Ok(Self {
                bridge: cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?,
            }),
            _ => Err(AttachError::Decline),
        }
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let mut added: Vec<(&'static str, bus::DeviceId)> = Vec::new();
        for c in CHILDREN.get().copied().unwrap_or(&[]) {
            let deps: Vec<_> = c
                .after
                .and_then(|class| added.iter().find(|(n, _)| *n == class))
                .map(|&(_, id)| (id, EdgeKind::Required))
                .into_iter()
                .collect();
            let id = cx.add_child_with_deps(
                Ident::Child {
                    class: c.class,
                    desc: c.description,
                },
                Some(Arc::new(HostbChild {
                    bridge: Arc::clone(&self.bridge),
                })),
                Vec::new(),
                &deps,
            );
            added.push((c.class, id));
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for every ISA's driver table (`optional pci`).
pub const DRIVER: DriverDecl = DriverDecl::of::<HostBridge>();
