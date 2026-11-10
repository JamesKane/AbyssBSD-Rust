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
//! Nothing here translates yet: the records are what the broker will
//! choose an `Iommu` DMA backend from (IOMMU.md §8, stage 3).
//!
//! Behavior reference: sys/dev/iommu/busdma_iommu.c (`iommu_get_requester`),
//! sys/arm64/acpica/acpi_iort.c.

use alloc::vec::Vec;
use core::ops::RangeInclusive;

use crate::types::DeviceId;

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
