// SPDX-License-Identifier: BSD-2-Clause
//! What firmware says about a device's DMA and an IOMMU: the unit that
//! translates it and the streams it uses there (docs/IOMMU.md §4.4).
//!
//! A bus records a [`Translation`] on a node with
//! [`crate::StartCx::set_dma_translation`], as it records `_CCA` with
//! `set_dma_constraints`; the node's descendants inherit it. A PCI bus
//! records each function's requester ID with
//! [`crate::StartCx::set_requester_id`], which `%iommu` shows and which
//! the root complex's mapping turns into a stream.
//!
//! An attached unit publishes an [`Iommu`] provider
//! ([`crate::StartCx::provide_iommu`]). When a translated device first
//! needs DMA (its DMA tag, or MSI, whichever comes first, as FreeBSD's
//! `iommu_get_dev_ctx`), the broker asks the unit for a context for its
//! stream ([`crate::AttachCx::iommu_context`]). A translated context
//! gives the device's tags the IOMMU backend over its domain
//! ([`Context::Translated`]); a pass-through one (FreeBSD's disabled
//! requesters, `IOMMU_CTX_DISABLED`; a virtio device that does not
//! accept platform translation) leaves its stream bypassing and its tag
//! direct or bounce. A stream no device asked for stays invalid: its DMA
//! aborts. When a PCI function's requester first gets a translated
//! context, the function's bus hears of it ([`RequesterHook`]): PCIe
//! relaxed ordering goes off (docs/IOMMU.md §4.4).
//!
//! Behavior reference: sys/dev/iommu/busdma_iommu.c (`iommu_get_requester`,
//! `iommu_get_dev_ctx`), sys/arm64/acpica/acpi_iort.c.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::RangeInclusive;
use core::sync::atomic::{AtomicBool, Ordering};

use vm::dma::IoDomain;

use crate::types::DeviceId;

/// Why a unit could not give a stream a context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IommuError {
    /// The stream is beyond the unit's stream table.
    NoStream,
    /// Out of memory for the unit's tables.
    NoMemory,
    /// The unit did not complete the change (a command timed out).
    Hardware,
    /// The unit's driver is detaching.
    Revoked,
}

/// What a device asks of its context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// Translation, if the unit translates.
    Translate,
    /// Physical addresses: a virtio device that does not accept platform
    /// translation (`VIRTIO_F_ACCESS_PLATFORM`) uses them whatever the
    /// platform does.
    PassThrough,
}

/// A device's request for its stream's context.
#[derive(Clone, Copy, Debug)]
pub struct ContextRequest<'a> {
    /// What it wants.
    pub want: Want,
    /// The name of the node holding the requester ID (the PCI function)
    /// for the unit's event lines: its driver's name and unit, or its PCI
    /// selector (`pci0:0:3:0`) when no driver has it.
    pub name: &'a str,

    /// The requester ID.
    pub rid: u16,
    /// The PCI segment.
    pub segment: u32,
    /// The device's accesses snoop the CPU's caches.
    pub coherent: bool,
    /// Bus address ranges the domain must never hand out: the host
    /// bridge's windows (start, length).
    pub reserved: &'a [(u64, u64)],
}

/// A stream's context, as a device's tag uses it.
#[derive(Clone)]
pub enum Context {
    /// The stream bypasses translation: bus address = physical address.
    PassThrough,
    /// The stream is translated through this domain.
    Translated(Arc<dyn IoDomain>),
}

impl core::fmt::Debug for Context {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::PassThrough => "PassThrough",
            Self::Translated(_) => "Translated",
        })
    }
}

/// An IOMMU unit, as the broker uses it.
pub trait Iommu: Send + Sync {
    /// The context of `stream`, made on its first request (FreeBSD's
    /// `iommu_get_ctx`): translated if the unit translates and the
    /// request wants it, pass-through otherwise. A later request gets the
    /// same context, except that one wanting physical addresses gets
    /// [`Context::PassThrough`] whatever the stream does.
    ///
    /// # Errors
    ///
    /// Why the stream has no context: its DMA still aborts.
    fn context(&self, stream: u32, req: &ContextRequest<'_>) -> Result<Context, IommuError>;
}

/// The bus of a node with a requester ID, told when its requester gets a
/// translated context (the PCI bus: relaxed ordering off).
pub trait RequesterHook: Send + Sync {
    /// The requester's context translates. Returns whether the device
    /// changed in a way to report (relaxed ordering was on and is now
    /// off).
    fn translated(&self) -> bool;
}

/// A published [`Iommu`], revoked when its driver detaches.
pub struct IommuSource {
    imp: Arc<dyn Iommu>,
    revoked: AtomicBool,
}

impl core::fmt::Debug for IommuSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IommuSource")
            .field("revoked", &self.revoked)
            .finish_non_exhaustive()
    }
}

impl IommuSource {
    /// Wraps a provider.
    #[must_use]
    pub fn new(imp: Arc<dyn Iommu>) -> Arc<Self> {
        Arc::new(Self {
            imp,
            revoked: AtomicBool::new(false),
        })
    }

    /// Stops every request: the driver is detaching.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    /// [`Iommu::context`], unless revoked.
    ///
    /// # Errors
    ///
    /// The provider's, or [`IommuError::Revoked`].
    pub fn context(&self, stream: u32, req: &ContextRequest<'_>) -> Result<Context, IommuError> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(IommuError::Revoked);
        }
        self.imp.context(stream, req)
    }
}

/// One mapping from a range of input IDs (requester IDs) to streams, as
/// an IORT root complex gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdMap {
    /// The input IDs it covers.
    pub input: RangeInclusive<u32>,
    /// The stream of the first input ID (every one's, if `single`).
    pub output: u32,
    /// One stream for every input it covers.
    pub single: bool,
}

impl IdMap {
    /// The stream of input ID `id`, if this mapping covers it.
    #[must_use]
    pub fn map(&self, id: u32) -> Option<u32> {
        if !self.input.contains(&id) {
            return None;
        }
        Some(if self.single {
            self.output
        } else {
            self.output.wrapping_add(id - self.input.start())
        })
    }
}

/// How the DMA of a node and its descendants reaches memory through an
/// IOMMU unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Translation {
    /// The functions below a PCI host bridge: their requester IDs become
    /// streams of `unit` through the root complex's mappings (only those
    /// that go to `unit`).
    Pci {
        /// The IOMMU unit's node.
        unit: DeviceId,
        /// The PCI segment.
        segment: u32,
        /// Requester ID to stream.
        map: Vec<IdMap>,
        /// The host bridge's memory windows at their bus addresses
        /// (start, length): never IOVAs (docs/IOMMU.md §4.6).
        windows: Vec<(u64, u64)>,
    },
    /// A device firmware names (an IORT named component), with the
    /// streams it uses on `unit`.
    Named {
        /// The IOMMU unit's node.
        unit: DeviceId,
        /// Its streams, mapping by mapping.
        streams: Vec<RangeInclusive<u32>>,
    },
}

impl Translation {
    /// The unit that translates.
    #[must_use]
    pub fn unit(&self) -> DeviceId {
        match self {
            Self::Pci { unit, .. } | Self::Named { unit, .. } => *unit,
        }
    }

    /// The PCI segment, for a PCI translation.
    #[must_use]
    pub fn segment(&self) -> Option<u32> {
        match self {
            Self::Pci { segment, .. } => Some(*segment),
            Self::Named { .. } => None,
        }
    }

    /// The bus address ranges a domain of this translation reserves.
    #[must_use]
    pub fn windows(&self) -> &[(u64, u64)] {
        match self {
            Self::Pci { windows, .. } => windows,
            Self::Named { .. } => &[],
        }
    }

    /// The stream requester ID `rid` uses, for a PCI translation that maps
    /// it.
    #[must_use]
    pub fn stream(&self, rid: u16) -> Option<u32> {
        match self {
            Self::Pci { map, .. } => map.iter().find_map(|m| m.map(u32::from(rid))),
            Self::Named { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requester_ids_map_to_streams() {
        let unit = DeviceId {
            index: 3,
            generation: 0,
        };
        // QEMU's: buses 0 and 1 through the SMMU, identity.
        let t = Translation::Pci {
            unit,
            segment: 0,
            map: alloc::vec![IdMap {
                input: 0..=0x1ff,
                output: 0,
                single: false,
            }],
            windows: alloc::vec![(0x1000_0000, 0x2eff_0000)],
        };

        assert_eq!(t.unit(), unit);
        assert_eq!(t.stream(0x100), Some(0x100));
        assert_eq!(t.stream(0x200), None);
        let offset = IdMap {
            input: 0x1000..=0x1fff,
            output: 0x8000,
            single: false,
        };
        assert_eq!(offset.map(0x1010), Some(0x8010));
        let single = IdMap {
            input: 0..=0xff,
            output: 7,
            single: true,
        };
        assert_eq!((single.map(3), single.map(0x100)), (Some(7), None));
        let named = Translation::Named {
            unit,
            streams: alloc::vec![0..=0, 3..=5],
        };
        assert_eq!(named.stream(0), None);
    }
}
