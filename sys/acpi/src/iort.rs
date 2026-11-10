// SPDX-License-Identifier: BSD-2-Clause
//! What the IORT adds to `acpi0`'s children (docs/IOMMU.md §4.2, §4.3):
//! an `smmu` child per SMMUv3, as FreeBSD's `smmu_acpi_identify` adds
//! them, and the devices each translates.
//!
//! - Each SMMUv3 node, in table order, becomes an `smmu` child with
//!   FreeBSD's resources: memory `0x20000` at its base, then its event,
//!   PRI, sync, and gerror interrupts, edge-triggered and active-high
//!   (ACPI gives no trigger for them; FreeBSD's `smmu_configure_intr`
//!   forces this).
//! - A PCI host bridge (`PNP0A08`, `PNP0A03`) whose segment (`_SEG`, 0
//!   by default) a root complex node serves is translated by each SMMU
//!   that root complex maps requester IDs to: an [`EdgeKind::Settle`]
//!   edge from the `smmu` child, and a [`Translation::Pci`] record with
//!   the mappings to it. PCI functions are the bridge's descendants, so
//!   the tree carries both down.
//! - A named component whose path names a present device is translated
//!   by each SMMU it maps streams to: the same edge, and a
//!   [`Translation::Named`] record with its streams.
//!
//! [`EdgeKind::Settle`]: devtree::graph::EdgeKind::Settle
//! [`Translation::Pci`]: bus::iommu::Translation::Pci
//! [`Translation::Named`]: bus::iommu::Translation::Named
//!
//! Behavior reference: sys/arm64/iommu/smmu_acpi.c,
//! sys/arm64/acpica/acpi_iort.c.

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::RangeInclusive;

use acpi_tables::iort::{Iort, SmmuV3};
use bus::iommu::IdMap;
use devtree::ids::AcpiIdent;

/// FreeBSD's `SMMU_DEVSTR`.
pub const SMMU_DESC: &str = "ARM System Memory Management Unit";

/// The register space FreeBSD gives an SMMUv3 (`MEMORY_RESOURCE_SIZE`):
/// its two 64 KiB pages.
pub const SMMU_MEMORY: u64 = 0x2_0000;

/// An SMMUv3 the IORT lists: the bus data of its `smmu` child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmmuNode {
    /// Its node's offset in the IORT, which mappings name it by.
    pub node: usize,
    /// What the node says.
    pub smmu: SmmuV3,
}

impl SmmuNode {
    /// Its interrupts' GSIVs in FreeBSD's resource order: event, PRI,
    /// sync, gerror.
    #[must_use]
    pub fn gsivs(&self) -> [u32; 4] {
        let s = &self.smmu;
        [s.event_gsiv, s.pri_gsiv, s.sync_gsiv, s.gerr_gsiv]
    }
}

/// How an SMMU translates a device the namespace has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Translated {
    /// A host bridge: its segment, and its root complexes' mappings of
    /// requester IDs to this SMMU's streams.
    HostBridge {
        /// The PCI segment.
        segment: u32,
        /// Requester ID to stream.
        map: Vec<IdMap>,
    },
    /// A named component's streams on the SMMU.
    Named {
        /// Its streams, mapping by mapping.
        streams: Vec<RangeInclusive<u32>>,
    },
}

/// One device an SMMU translates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IortConsumer {
    /// The SMMU, by its index in [`IortPlan::smmus`] (its unit).
    pub smmu: usize,
    /// The device's namespace path.
    pub path: String,
    /// How.
    pub how: Translated,
}

/// The IORT's SMMUs and the devices they translate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IortPlan {
    /// The SMMUv3s, in table order.
    pub smmus: Vec<SmmuNode>,
    /// The devices they translate, host bridges first, in namespace order,
    /// then named components in table order.
    pub consumers: Vec<IortConsumer>,
    /// Named components whose path names no present device.
    pub notes: Vec<String>,
}

/// Whether `ident` is a PCI host bridge.
fn host_bridge(ident: &AcpiIdent) -> bool {
    let is = |id: &str| id == "PNP0A08" || id == "PNP0A03";
    ident.hid.as_deref().is_some_and(is) || ident.cids.iter().any(|c| is(c))
}

/// What the IORT says about the present devices `devices` (each with its
/// identity), in namespace order.
pub fn plan(
    iort: &Iort<'_>,
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    devices: &[(aml::NodeId, &AcpiIdent)],
) -> IortPlan {
    let smmus: Vec<SmmuNode> = iort
        .smmu_v3s()
        .map(|(node, smmu)| SmmuNode { node, smmu })
        .collect();
    let unit = |offset: usize| smmus.iter().position(|s| s.node == offset);
    let mut consumers = Vec::new();
    for &(n, ident) in devices {
        if !host_bridge(ident) {
            continue;
        }
        let segment = crate::pcib::segment(interp, n, host);
        // Every root complex of the segment, in table order, as FreeBSD's
        // `iort_pci_rc_map` tries them; each SMMU gets the mappings that
        // go to it.
        let mut per: Vec<(usize, Vec<IdMap>)> = Vec::new();
        for rc in iort
            .nodes()
            .filter(|r| r.root_complex().is_some_and(|r| r.segment == segment))
        {
            for m in rc.mappings() {
                let Some(u) = unit(m.output) else {
                    continue;
                };
                let map = IdMap {
                    input: m.input_base..=m.input_base.saturating_add(m.count),
                    output: m.output_base,
                    single: m.single,
                };
                match per.iter_mut().find(|(s, _)| *s == u) {
                    Some((_, maps)) => maps.push(map),
                    None => per.push((u, alloc::vec![map])),
                }
            }
        }
        let path = alloc::format!("{}", interp.path(n));
        for (smmu, map) in per {
            consumers.push(IortConsumer {
                smmu,
                path: path.clone(),
                how: Translated::HostBridge { segment, map },
            });
        }
    }
    let mut notes = Vec::new();
    for (at, c) in iort.named_components() {
        let found = aml::AmlPath::parse(c.name)
            .ok()
            .and_then(|p| interp.lookup(&p))
            .filter(|n| devices.iter().any(|(d, _)| d == n));
        let Some(n) = found else {
            notes.push(alloc::format!(
                "acpi0: IORT names {}, which is not present",
                c.name
            ));
            continue;
        };
        let path = alloc::format!("{}", interp.path(n));
        for (smmu, s) in smmus.iter().enumerate() {
            let streams: Vec<RangeInclusive<u32>> = iort.node_streams(at, s.node).collect();
            if !streams.is_empty() {
                consumers.push(IortConsumer {
                    smmu,
                    path: path.clone(),
                    how: Translated::Named { streams },
                });
            }
        }
    }
    IortPlan {
        smmus,
        consumers,
        notes,
    }
}
