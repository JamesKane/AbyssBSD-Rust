// SPDX-License-Identifier: BSD-2-Clause
//! I/O Remapping Table (Arm DEN 0049, revision E.e): the SMMUs, the PCI
//! root complexes and named components behind them, the memory firmware
//! reserves for their streams, and how a device's requester ID becomes
//! the stream ID an SMMU sees and the device ID an interrupt translation
//! service (ITS) sees.
//!
//! - [`Iort::nodes`] walks every node in table order; [`Node`] gives the
//!   type-specific views ([`SmmuV3`], [`RootComplex`], [`NamedComponent`],
//!   [`Rmr`]) and the node's [`IdMapping`]s.
//! - [`Iort::stream_for_pci`] maps a PCI RID through its root complex to
//!   an SMMU and stream, and [`Iort::its_for_pci`] on to an ITS.
//! - [`Iort::node_streams`] gives the streams a named component or RMR
//!   names on an SMMU.
//!
//! A mapping matches an ID in `input_base..=input_base + count`; a single
//! mapping's output is its `output_base` whatever the ID, a range's is
//! offset by the ID's. An SMMUv3's own mapping for its MSIs (its "device
//! ID mapping index") is not followed by lookups unless the SMMU uses
//! MSIs, as FreeBSD's `iort_entry_get_id_mapping_index` decides. Every
//! walk is bounded, so a table whose references loop cannot hang it, and
//! a node that does not fit the table ends the node list.
//!
//! Behavior reference: `sys/arm64/acpica/acpi_iort.c`.

use core::ops::RangeInclusive;

use crate::{Error, HEADER_LEN, Sdt, u16_at, u32_at, u64_at};

/// Node types (DEN 0049 table 4).
pub mod kind {
    /// An ITS group.
    pub const ITS_GROUP: u8 = 0;
    /// A named component (an ACPI device).
    pub const NAMED_COMPONENT: u8 = 1;
    /// A PCI root complex.
    pub const ROOT_COMPLEX: u8 = 2;
    /// An SMMUv1 or SMMUv2.
    pub const SMMU: u8 = 3;
    /// An SMMUv3.
    pub const SMMU_V3: u8 = 4;
    /// An SMMUv3 performance monitor counter group.
    pub const PMCG: u8 = 5;
    /// A reserved memory range.
    pub const RMR: u8 = 6;
}

/// The node header: type, length, revision, identifier, mapping count,
/// mapping offset.
const NODE_HEADER: usize = 16;
/// One ID mapping: input base, count minus one, output base, output
/// reference, flags.
const MAPPING_LEN: usize = 20;
/// Mapping flag: one ID for the whole node (not a range).
const SINGLE_MAPPING: u32 = 1;
/// One RMR memory range descriptor: base, length, reserved.
const RMR_RANGE_LEN: usize = 20;

/// References followed at most per lookup.
const MAX_HOPS: usize = 8;

/// A parsed IORT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Iort<'a> {
    bytes: &'a [u8],
    nodes: usize,
    first: usize,
}

/// Parses an IORT table.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table, [`Error::BadLength`] if the
/// node list header is missing.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Iort<'a>, Error> {
    if t.signature() != *b"IORT" {
        return Err(Error::BadSignature);
    }
    let b = t.bytes;
    let nodes = u32_at(b, HEADER_LEN).ok_or(Error::BadLength(*b"IORT"))?;
    let first = u32_at(b, HEADER_LEN + 4).ok_or(Error::BadLength(*b"IORT"))?;
    Ok(Iort {
        bytes: b,
        nodes: nodes as usize,
        first: first as usize,
    })
}

/// A node: its offset in the table (what mappings refer to it by) and
/// its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node<'a> {
    offset: usize,
    body: &'a [u8],
}

/// One ID mapping of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdMapping {
    /// The first input ID.
    pub input_base: u32,
    /// The number of IDs in the range, minus one.
    pub count: u32,
    /// The output ID of `input_base`.
    pub output_base: u32,
    /// The node the IDs go to, by its offset.
    pub output: usize,
    /// A single mapping: one output ID, whatever the input.
    pub single: bool,
}

impl IdMapping {
    /// The output ID for input `id`, if this mapping covers it.
    #[must_use]
    pub fn map(&self, id: u32) -> Option<u32> {
        let last = u64::from(self.input_base) + u64::from(self.count);
        if u64::from(id) < u64::from(self.input_base) || u64::from(id) > last {
            return None;
        }
        Some(if self.single {
            self.output_base
        } else {
            self.output_base.wrapping_add(id - self.input_base)
        })
    }

    /// The output IDs, as a node that is the source of its IDs (a named
    /// component, an RMR) names them: one for a single mapping, the
    /// range's width otherwise, clipped at `u32::MAX`.
    #[must_use]
    pub fn outputs(&self) -> RangeInclusive<u32> {
        let last = if self.single {
            self.output_base
        } else {
            self.output_base.saturating_add(self.count)
        };
        self.output_base..=last
    }
}

/// An SMMUv3 node's data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmmuV3 {
    /// Base of its register frames.
    pub base: u64,
    /// Flags ([`SmmuV3::COHACC_OVERRIDE`], ...).
    pub flags: u32,
    /// The implementation ([`SmmuV3::MODEL_GENERIC`], ...).
    pub model: u32,
    /// The event queue's GSIV (0: it signals by MSI).
    pub event_gsiv: u32,
    /// The PRI queue's GSIV.
    pub pri_gsiv: u32,
    /// The global error interrupt's GSIV.
    pub gerr_gsiv: u32,
    /// The command queue sync interrupt's GSIV.
    pub sync_gsiv: u32,
    /// The index of the mapping for its own MSIs, if the node says.
    pub id_mapping_index: Option<u32>,
}

impl SmmuV3 {
    /// Flag: its table walks and queue accesses are cache coherent,
    /// whatever `SMMU_IDR0.COHACC` says.
    pub const COHACC_OVERRIDE: u32 = 1;
    /// The generic SMMUv3 (the model FreeBSD's `smmu_acpi_probe` takes).
    pub const MODEL_GENERIC: u32 = 0;

    /// Whether firmware says its accesses are coherent.
    #[must_use]
    pub fn coherent(&self) -> bool {
        self.flags & Self::COHACC_OVERRIDE != 0
    }

    /// Whether every control interrupt is wired (a GSIV), so its ID
    /// mapping index means nothing.
    #[must_use]
    pub fn wired(&self) -> bool {
        self.event_gsiv != 0 && self.pri_gsiv != 0 && self.gerr_gsiv != 0 && self.sync_gsiv != 0
    }
}

/// A root complex node's data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootComplex {
    /// Memory access properties: cache coherency (1 coherent).
    pub cca: u32,
    /// Memory access properties: allocation hints.
    pub allocation_hints: u8,
    /// Memory access properties: memory access flags (CPM, DACS).
    pub access_flags: u8,
    /// Whether the root complex supports ATS (bit 0).
    pub ats: u32,
    /// The PCI segment it serves.
    pub segment: u32,
    /// Its memory address size limit, in bits (revision 1 and up; 0 for
    /// none given).
    pub address_limit: u8,
}

/// A named component node: its data and its namespace path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamedComponent<'a> {
    /// Node flags.
    pub flags: u32,
    /// Memory access properties: cache coherency (1 coherent).
    pub cca: u32,
    /// Its device memory address size limit, in bits.
    pub address_limit: u8,
    /// Its namespace path, as written (`\_SB_.DPU0`).
    pub name: &'a str,
}

/// A reserved memory range node's data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rmr<'a> {
    /// Flags ([`Rmr::REMAP_PERMITTED`], the access attributes).
    pub flags: u32,
    ranges: &'a [u8],
}

/// One reserved memory range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemRange {
    /// Its base address.
    pub base: u64,
    /// Its length in bytes.
    pub len: u64,
}

impl Rmr<'_> {
    /// Flag: the OS may remap the range for its streams (rather than only
    /// bypassing them).
    pub const REMAP_PERMITTED: u32 = 1;
    /// Flag: privileged access.
    pub const PRIVILEGED: u32 = 2;

    /// Whether the OS may remap the ranges.
    #[must_use]
    pub fn remap_permitted(&self) -> bool {
        self.flags & Self::REMAP_PERMITTED != 0
    }

    /// The memory access attributes (bits 9:2; 0x04 is Normal inner and
    /// outer non-cacheable).
    #[must_use]
    pub fn attributes(&self) -> u8 {
        (self.flags >> 2).to_le_bytes()[0]
    }

    /// The ranges, in table order.
    pub fn ranges(&self) -> impl Iterator<Item = MemRange> + '_ {
        self.ranges
            .as_chunks::<RMR_RANGE_LEN>()
            .0
            .iter()
            .filter_map(|d| {
                Some(MemRange {
                    base: u64_at(d, 0)?,
                    len: u64_at(d, 8)?,
                })
            })
    }
}

impl<'a> Node<'a> {
    /// Its offset in the table: what mappings name it by.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Its type ([`kind`]).
    #[must_use]
    pub fn kind(&self) -> u8 {
        self.body[0]
    }

    /// Its revision.
    #[must_use]
    pub fn revision(&self) -> u8 {
        self.body[3]
    }

    /// Its identifier (revision-dependent; unique among the table's nodes
    /// from DEN 0049 E on).
    #[must_use]
    pub fn identifier(&self) -> u32 {
        u32_at(self.body, 4).unwrap_or(0)
    }

    fn data(&self, at: usize) -> Option<u32> {
        u32_at(self.body, NODE_HEADER + at)
    }

    /// Its ID mappings, in table order (those that fit the node).
    pub fn mappings(&self) -> impl Iterator<Item = IdMapping> + use<'a> {
        let body = self.body;
        let count = u32_at(body, 8).unwrap_or(0) as usize;
        let at = u32_at(body, 12).unwrap_or(0) as usize;
        (0..count).map_while(move |i| {
            let start = at.checked_add(i.checked_mul(MAPPING_LEN)?)?;
            let m = body.get(start..start.checked_add(MAPPING_LEN)?)?;
            Some(IdMapping {
                input_base: u32_at(m, 0)?,
                count: u32_at(m, 4)?,
                output_base: u32_at(m, 8)?,
                output: u32_at(m, 12)? as usize,
                single: u32_at(m, 16)? & SINGLE_MAPPING != 0,
            })
        })
    }

    /// The SMMUv3 data, for an SMMUv3 node whose data fits.
    #[must_use]
    pub fn smmu_v3(&self) -> Option<SmmuV3> {
        if self.kind() != kind::SMMU_V3 {
            return None;
        }
        let b = self.body;
        // The ID mapping index field came with revision 1.
        let index = if self.revision() >= 1 {
            Some(self.data(48)?)
        } else {
            None
        };
        Some(SmmuV3 {
            base: u64_at(b, NODE_HEADER)?,
            flags: self.data(8)?,
            model: self.data(24)?,
            event_gsiv: self.data(28)?,
            pri_gsiv: self.data(32)?,
            gerr_gsiv: self.data(36)?,
            sync_gsiv: self.data(40)?,
            id_mapping_index: index,
        })
    }

    /// The root complex data, for a root complex node whose data fits.
    #[must_use]
    pub fn root_complex(&self) -> Option<RootComplex> {
        if self.kind() != kind::ROOT_COMPLEX {
            return None;
        }
        let b = self.body;
        Some(RootComplex {
            cca: self.data(0)?,
            allocation_hints: *b.get(NODE_HEADER + 4)?,
            access_flags: *b.get(NODE_HEADER + 7)?,
            ats: self.data(8)?,
            segment: self.data(12)?,
            address_limit: if self.revision() >= 1 {
                *b.get(NODE_HEADER + 16)?
            } else {
                0
            },
        })
    }

    /// The named component data, for a named component node whose data
    /// and NUL-terminated ASCII path fit.
    #[must_use]
    pub fn named_component(&self) -> Option<NamedComponent<'a>> {
        if self.kind() != kind::NAMED_COMPONENT {
            return None;
        }
        let b = self.body;
        let name_at = NODE_HEADER + 13;
        let rest = b.get(name_at..)?;
        let end = rest.iter().position(|&c| c == 0)?;
        let name = core::str::from_utf8(&rest[..end]).ok()?;
        Some(NamedComponent {
            flags: self.data(0)?,
            cca: self.data(4)?,
            address_limit: *b.get(NODE_HEADER + 12)?,
            name,
        })
    }

    /// The reserved memory range data, for an RMR node whose descriptors
    /// fit.
    #[must_use]
    pub fn rmr(&self) -> Option<Rmr<'a>> {
        if self.kind() != kind::RMR {
            return None;
        }
        let n = self.data(4)? as usize;
        let at = self.data(8)? as usize;
        let len = n.checked_mul(RMR_RANGE_LEN)?;
        Some(Rmr {
            flags: self.data(0)?,
            ranges: self.body.get(at..at.checked_add(len)?)?,
        })
    }

    /// The index of the mapping lookups pass over: an SMMUv3's mapping for
    /// its own MSIs, when it has one in use (FreeBSD's
    /// `iort_entry_get_id_mapping_index`).
    fn skipped_mapping(&self) -> Option<usize> {
        let s = self.smmu_v3()?;
        let index = s.id_mapping_index? as usize;
        let count = u32_at(self.body, 8)? as usize;
        (!s.wired() && index < count).then_some(index)
    }

    /// Maps `id` through this node's mappings: the output ID and the node
    /// it goes to (FreeBSD's `iort_entry_lookup`).
    #[must_use]
    pub fn map(&self, id: u32) -> Option<(u32, usize)> {
        let skip = self.skipped_mapping();
        self.mappings()
            .enumerate()
            .filter(|&(i, _)| Some(i) != skip)
            .find_map(|(_, m)| Some((m.map(id)?, m.output)))
    }
}

/// Where a requester ID goes through an SMMU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stream {
    /// The SMMU's node, by its offset.
    pub smmu: usize,
    /// The stream ID it sees.
    pub stream: u32,
}

/// Where a requester ID ends up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItsTarget {
    /// The ITS (its MADT GIC ITS `id`).
    pub its: u32,
    /// The device ID the ITS sees.
    pub device_id: u32,
}

impl<'a> Iort<'a> {
    /// The node at `offset`, if its header and length fit the table.
    #[must_use]
    pub fn node(&self, offset: usize) -> Option<Node<'a>> {
        let len = usize::from(u16_at(self.bytes, offset.checked_add(1)?)?);
        if len < NODE_HEADER {
            return None;
        }
        Some(Node {
            offset,
            body: self.bytes.get(offset..offset.checked_add(len)?)?,
        })
    }

    /// Every node, in table order (stopping at the first malformed node).
    pub fn nodes(&self) -> impl Iterator<Item = Node<'a>> + '_ {
        let mut at = self.first;
        (0..self.nodes).map_while(move |_| {
            let n = self.node(at)?;
            at = at.checked_add(n.body.len())?;
            Some(n)
        })
    }

    /// The SMMUv3 nodes, in table order, with their offsets.
    pub fn smmu_v3s(&self) -> impl Iterator<Item = (usize, SmmuV3)> + '_ {
        self.nodes()
            .filter_map(|n| n.smmu_v3().map(|s| (n.offset(), s)))
    }

    /// The root complexes, in table order, with their offsets.
    pub fn root_complexes(&self) -> impl Iterator<Item = (usize, RootComplex)> + '_ {
        self.nodes()
            .filter_map(|n| n.root_complex().map(|r| (n.offset(), r)))
    }

    /// The named components, in table order, with their offsets.
    pub fn named_components(&self) -> impl Iterator<Item = (usize, NamedComponent<'a>)> + '_ {
        self.nodes()
            .filter_map(|n| n.named_component().map(|c| (n.offset(), c)))
    }

    /// The reserved memory ranges, in table order, with their offsets.
    pub fn rmrs(&self) -> impl Iterator<Item = (usize, Rmr<'a>)> + '_ {
        self.nodes()
            .filter_map(|n| n.rmr().map(|r| (n.offset(), r)))
    }

    /// The first hop of requester ID `rid` of PCI segment `segment`: the
    /// first root complex of that segment that maps it (FreeBSD's
    /// `iort_pci_rc_map`).
    fn pci_first_hop(&self, segment: u32, rid: u16) -> Option<(u32, usize)> {
        self.nodes()
            .filter(|n| n.root_complex().is_some_and(|r| r.segment == segment))
            .find_map(|n| n.map(u32::from(rid)))
    }

    /// The SMMU and stream that requester ID `rid` of PCI segment
    /// `segment` reaches, if its root complex maps it to an SMMU (v3, or
    /// v1/v2). `None` if it goes straight to an ITS, or nowhere.
    #[must_use]
    pub fn stream_for_pci(&self, segment: u32, rid: u16) -> Option<Stream> {
        let (stream, at) = self.pci_first_hop(segment, rid)?;
        let k = self.node(at)?.kind();
        (k == kind::SMMU_V3 || k == kind::SMMU).then_some(Stream { smmu: at, stream })
    }

    /// The ITS and device ID that requester ID `rid` of PCI segment
    /// `segment` reaches, if the table maps it to an ITS, through an SMMU
    /// or not.
    #[must_use]
    pub fn its_for_pci(&self, segment: u32, rid: u16) -> Option<ItsTarget> {
        let (mut id, mut at) = self.pci_first_hop(segment, rid)?;
        for _ in 0..MAX_HOPS {
            let n = self.node(at)?;
            if n.kind() == kind::ITS_GROUP {
                // The group's first ITS; several in one group share the
                // device ID space.
                let its = n.data(4)?;
                return Some(ItsTarget { its, device_id: id });
            }
            (id, at) = n.map(id)?;
        }
        None
    }

    /// The streams the node at `source` (a named component or an RMR)
    /// names on the SMMU at `smmu`, mapping by mapping: each a range of
    /// stream IDs (FreeBSD's `acpi_iort_named_smmuv3`,
    /// `acpi_iort_rmr_foreach`).
    pub fn node_streams(
        &self,
        source: usize,
        smmu: usize,
    ) -> impl Iterator<Item = RangeInclusive<u32>> + '_ {
        self.node(source)
            .into_iter()
            .flat_map(|n| n.mappings())
            .filter(move |m| m.output == smmu)
            .map(|m| m.outputs())
    }

    /// The number of ITS groups (diagnostics).
    #[must_use]
    pub fn its_groups(&self) -> usize {
        self.nodes().filter(|n| n.kind() == kind::ITS_GROUP).count()
    }
}
