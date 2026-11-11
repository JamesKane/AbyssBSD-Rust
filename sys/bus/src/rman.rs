// SPDX-License-Identifier: BSD-2-Clause
//! The resource spaces as FreeBSD's rmans (DRIVER-MODEL.md §8.1, §11.1):
//! what `hw.bus.rman` lists for `devinfo -u` and `devinfo -r`.
//!
//! - The platform declares the root's rmans (`I/O memory addresses`,
//!   `Interrupts`), and buses declare theirs as they start (`acpi0`'s
//!   system-resource ranges, a bridge's windows and bus numbers), each
//!   with the regions it manages. An rman goes when its bus detaches.
//! - What each node holds is mostly what its bus described: every memory
//!   and I/O-port resource of every node, attached or not, as FreeBSD's
//!   ACPI and PCI buses reserve their children's ranges when they add
//!   them. A bus may account for a child's ranges itself (`acpi0`
//!   allocates the system-resource devices' ranges, merged, and manages
//!   them).
//! - Buses also record ranges that are no child's resource ([`Hold`]): a
//!   bridge's windows and bus numbers, a host bridge's configuration
//!   space. Installed interrupt handlers record their lines.
//! - Which rman a range is in, the free spans, and the order are
//!   `devtree::rman`'s.

use alloc::string::String;
use alloc::vec::Vec;

pub use devtree::rman::Kind as RmanKind;

use crate::types::DeviceId;

/// An rman as its bus declares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RmanSpec {
    /// Its description (`rm_descr`): `pcib1 memory window`.
    pub descr: String,
    /// What it holds.
    pub kind: RmanKind,
    /// The space it covers, inclusive (`rm_start`, `rm_end`).
    pub start: u64,
    /// See `start`.
    pub end: u64,
    /// The regions it manages, inclusive.
    pub regions: Vec<(u64, u64)>,
    /// Each range allocated from it is managed as it is allocated (x86's
    /// MSI interrupts).
    pub grows: bool,
}

impl RmanSpec {
    /// An rman over `start..=end` managing `regions`.
    #[must_use]
    pub fn new(descr: impl Into<String>, kind: RmanKind, start: u64, end: u64) -> Self {
        Self {
            descr: descr.into(),
            kind,
            start,
            end,
            regions: Vec::new(),
            grows: false,
        }
    }

    /// The same, managing `regions`.
    #[must_use]
    pub fn managing(mut self, regions: Vec<(u64, u64)>) -> Self {
        self.regions = regions;
        self
    }

    /// The same, managing each range as it is allocated.
    #[must_use]
    pub fn growing(mut self) -> Self {
        self.grows = true;
        self
    }
}

/// An rman, by handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RmanId(pub(crate) usize);

pub(crate) struct Rec {
    pub(crate) handle: usize,
    /// The bus whose children allocate from it; `None` for the platform's.
    pub(crate) owner: Option<DeviceId>,
    /// The node whose start made it (tree order); `None`: before any.
    pub(crate) anchor: Option<DeviceId>,
    pub(crate) spec: RmanSpec,
}

/// A range a node holds that is not one of its resources.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Hold {
    pub(crate) dev: DeviceId,
    pub(crate) kind: RmanKind,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) flags: u32,
    pub(crate) rman: Option<usize>,
    /// Who recorded it: it goes when that node detaches.
    pub(crate) by: DeviceId,
}

/// Every rman, in creation order, and the ranges recorded.
#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) rmans: Vec<Rec>,
    pub(crate) holds: Vec<Hold>,
    /// Nodes whose resources their bus accounts for (system-resource
    /// devices), with the bus.
    pub(crate) held_by_bus: Vec<(DeviceId, DeviceId)>,
    next: usize,
}

impl Registry {
    pub(crate) const fn new() -> Self {
        Self {
            rmans: Vec::new(),
            holds: Vec::new(),
            held_by_bus: Vec::new(),
            next: 0,
        }
    }

    pub(crate) fn add(
        &mut self,
        owner: Option<DeviceId>,
        anchor: Option<DeviceId>,
        spec: RmanSpec,
    ) -> RmanId {
        self.next += 1;
        let handle = self.next;
        self.rmans.push(Rec {
            handle,
            owner,
            anchor,
            spec,
        });
        RmanId(handle)
    }

    /// The platform's rman described `descr`, made (anchored at `anchor`)
    /// if there is none yet: a PCI domain's bus numbers, made when its
    /// first bus takes its number.
    pub(crate) fn platform(&mut self, anchor: DeviceId, spec: RmanSpec) -> RmanId {
        if let Some(r) = self
            .rmans
            .iter()
            .find(|r| r.owner.is_none() && r.spec.kind == spec.kind && r.spec.descr == spec.descr)
        {
            return RmanId(r.handle);
        }
        self.add(None, Some(anchor), spec)
    }

    /// Drops what `id` made and holds: it detached.
    pub(crate) fn forget(&mut self, id: DeviceId) {
        self.rmans.retain(|r| r.owner != Some(id));
        self.holds.retain(|h| h.by != id);
        self.held_by_bus.retain(|&(_, by)| by != id);
    }
}
