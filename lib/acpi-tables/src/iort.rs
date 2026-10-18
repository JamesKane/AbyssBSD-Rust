// SPDX-License-Identifier: BSD-2-Clause
//! I/O Remapping Table (Arm DEN 0049): how a device's requester ID
//! becomes the device ID an interrupt translation service (ITS) sees,
//! through any SMMUs on the way.
//!
//! Only the mapping walk is parsed: ITS groups, PCI root complexes, named
//! components' and SMMUs' ID mappings. An SMMU in the path passes IDs on
//! through its own mappings; the walk is bounded, so a table whose
//! references loop cannot hang it.
//!
//! Behavior reference: `sys/arm64/acpica/acpi_iort.c`.

use crate::{Error, HEADER_LEN, Sdt, u16_at, u32_at};

/// Node types (DEN 0049 table 4).
const ITS_GROUP: u8 = 0;
const ROOT_COMPLEX: u8 = 2;

/// The node header: type, length, revision, identifier, mapping count,
/// mapping offset.
const NODE_HEADER: usize = 16;
/// One ID mapping: input base, count minus one, output base, output
/// reference, flags.
const MAPPING_LEN: usize = 20;
/// Mapping flag: one ID for the whole node (not a range).
const SINGLE_MAPPING: u32 = 1;

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

/// A node, by its offset in the table.
#[derive(Clone, Copy, Debug)]
struct Node<'a> {
    kind: u8,
    body: &'a [u8],
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
    fn node(&self, offset: usize) -> Option<Node<'a>> {
        let kind = *self.bytes.get(offset)?;
        let len = usize::from(u16_at(self.bytes, offset + 1)?);
        if len < NODE_HEADER {
            return None;
        }
        Some(Node {
            kind,
            body: self.bytes.get(offset..offset.checked_add(len)?)?,
        })
    }

    /// Node offsets, in table order (stopping at the first malformed node).
    fn offsets(&self) -> impl Iterator<Item = usize> + '_ {
        let mut at = self.first;
        (0..self.nodes).map_while(move |_| {
            let n = self.node(at)?;
            let this = at;
            at = at.checked_add(n.body.len())?;
            Some(this)
        })
    }

    /// Maps `id` through `node`'s ID mappings: the output ID and the node
    /// it goes to.
    fn map(node: Node<'_>, id: u32) -> Option<(u32, usize)> {
        let count = u32_at(node.body, 8)? as usize;
        let at = u32_at(node.body, 12)? as usize;
        (0..count.min(node.body.len() / MAPPING_LEN)).find_map(|i| {
            let m = node
                .body
                .get(at + i * MAPPING_LEN..at + (i + 1) * MAPPING_LEN)?;
            let input = u32_at(m, 0)?;
            let last = input.checked_add(u32_at(m, 4)?)?;
            let output = u32_at(m, 8)?;
            let target = u32_at(m, 12)? as usize;
            let flags = u32_at(m, 16)?;
            if flags & SINGLE_MAPPING != 0 {
                return None;
            }
            (input..=last)
                .contains(&id)
                .then(|| (output.wrapping_add(id - input), target))
        })
    }

    /// The ITS and device ID that requester ID `rid` of PCI segment
    /// `segment` reaches, if the table maps it to an ITS.
    #[must_use]
    pub fn its_for_pci(&self, segment: u32, rid: u16) -> Option<ItsTarget> {
        let rc = self.offsets().find(|&o| {
            self.node(o).is_some_and(|n| {
                n.kind == ROOT_COMPLEX && u32_at(n.body, NODE_HEADER + 12) == Some(segment)
            })
        })?;
        let (mut id, mut at) = Self::map(self.node(rc)?, u32::from(rid))?;
        for _ in 0..MAX_HOPS {
            let n = self.node(at)?;
            if n.kind == ITS_GROUP {
                // The group's first ITS; several in one group share the
                // device ID space.
                let its = u32_at(n.body, NODE_HEADER + 4)?;
                return Some(ItsTarget { its, device_id: id });
            }
            (id, at) = Self::map(n, id)?;
        }
        None
    }

    /// The number of ITS groups (diagnostics).
    #[must_use]
    pub fn its_groups(&self) -> usize {
        self.offsets()
            .filter(|&o| self.node(o).is_some_and(|n| n.kind == ITS_GROUP))
            .count()
    }
}
