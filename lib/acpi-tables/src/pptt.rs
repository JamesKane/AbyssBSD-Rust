// SPDX-License-Identifier: BSD-2-Clause
//! Processor Properties Topology Table (ACPI 6.5 §5.2.30): the tree of
//! packages, clusters, cores, and threads, and the caches attached to them.
//!
//! Nodes refer to each other by byte offset from the start of the table, so
//! a node's offset doubles as a stable identifier for grouping processors.
//! References are untrusted: a lookup only succeeds for an offset where a
//! structure actually starts, and ancestor walks are depth-capped so a cycle
//! cannot loop.
//!
//! FreeBSD does not parse PPTT outside ACPICA; this follows the specification.

use crate::raw::{Entries, Fields, LenField, Raw};
use crate::{Error, HEADER_LEN, Sdt};

const SHORT: Error = Error::Malformed("PPTT node too short");
const WALK: Error = Error::Malformed("PPTT node length");
const BAD_PARENT: Error = Error::Malformed("PPTT parent reference");
const TOO_DEEP: Error = Error::Malformed("PPTT hierarchy too deep");

/// The deepest hierarchy an ancestor walk follows. Real systems use about
/// five levels (system, package, cluster, core, thread).
pub const MAX_DEPTH: u32 = 32;

const PROCESSOR_LEN: usize = 20;
const CACHE_ID_LEN: usize = 28;

/// A parsed PPTT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pptt<'a> {
    bytes: &'a [u8],
    revision: u8,
}

/// Parses a PPTT. Structures are validated as they are walked.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Pptt<'a>, Error> {
    if t.signature() != *b"PPTT" {
        return Err(Error::BadSignature);
    }
    Ok(Pptt {
        bytes: t.bytes,
        revision: t.revision(),
    })
}

/// A PPTT structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PpttNode<'a> {
    /// Processor Hierarchy Node (type 0).
    ProcessorHierarchy(ProcessorNode<'a>),
    /// Cache Type Structure (type 1).
    Cache(CacheNode),
    /// A structure this parser does not decode.
    Other {
        /// Offset from the start of the table.
        offset: u32,
        /// The structure type.
        kind: u8,
        /// The whole structure, header included.
        bytes: &'a [u8],
    },
}

impl<'a> PpttNode<'a> {
    /// Offset of the structure from the start of the table.
    #[must_use]
    pub fn offset(&self) -> u32 {
        match self {
            Self::ProcessorHierarchy(p) => p.offset,
            Self::Cache(c) => c.offset,
            Self::Other { offset, .. } => *offset,
        }
    }

    /// The processor node, if this is one.
    #[must_use]
    pub fn processor(self) -> Option<ProcessorNode<'a>> {
        match self {
            Self::ProcessorHierarchy(p) => Some(p),
            _ => None,
        }
    }
}

/// Processor hierarchy node flags (ACPI 6.5 Table 5.160).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorFlags(pub u32);

impl ProcessorFlags {
    /// The node is a physical package.
    #[must_use]
    pub const fn physical_package(self) -> bool {
        self.0 & 1 != 0
    }

    /// The ACPI processor ID field is valid.
    #[must_use]
    pub const fn acpi_id_valid(self) -> bool {
        self.0 & 2 != 0
    }

    /// The processing element is a thread of an SMT core.
    #[must_use]
    pub const fn is_thread(self) -> bool {
        self.0 & 4 != 0
    }

    /// The node is a leaf (revision 2).
    #[must_use]
    pub const fn is_leaf(self) -> bool {
        self.0 & 8 != 0
    }

    /// Every child implements the same architecture.
    #[must_use]
    pub const fn identical(self) -> bool {
        self.0 & 0x10 != 0
    }
}

/// A processor hierarchy node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorNode<'a> {
    /// Offset from the start of the table.
    pub offset: u32,
    /// Flags.
    pub flags: ProcessorFlags,
    /// Offset of the parent node, 0 for a root.
    pub parent: u32,
    /// The ACPI processor UID (for a leaf) or processor container `_UID`,
    /// when `flags.acpi_id_valid()`.
    pub acpi_processor_id: u32,
    resources: &'a [u8],
}

impl<'a> ProcessorNode<'a> {
    /// Offsets of the private resources (usually caches), validated to lie
    /// within the node.
    pub fn private_resources(&self) -> impl Iterator<Item = u32> + 'a {
        self.resources
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
    }

    /// The parent's offset, or `None` for a root.
    #[must_use]
    pub fn parent(&self) -> Option<u32> {
        (self.parent != 0).then_some(self.parent)
    }
}

/// Cache type structure flags (ACPI 6.5 Table 5.162): which fields are
/// valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheFlags(pub u32);

impl CacheFlags {
    /// `size` is valid.
    #[must_use]
    pub const fn size_valid(self) -> bool {
        self.0 & 1 != 0
    }

    /// `sets` is valid.
    #[must_use]
    pub const fn sets_valid(self) -> bool {
        self.0 & 2 != 0
    }

    /// `associativity` is valid.
    #[must_use]
    pub const fn associativity_valid(self) -> bool {
        self.0 & 4 != 0
    }

    /// The allocation type attribute is valid.
    #[must_use]
    pub const fn allocation_type_valid(self) -> bool {
        self.0 & 8 != 0
    }

    /// The cache type attribute is valid.
    #[must_use]
    pub const fn cache_type_valid(self) -> bool {
        self.0 & 0x10 != 0
    }

    /// The write policy attribute is valid.
    #[must_use]
    pub const fn write_policy_valid(self) -> bool {
        self.0 & 0x20 != 0
    }

    /// `line_size` is valid.
    #[must_use]
    pub const fn line_size_valid(self) -> bool {
        self.0 & 0x40 != 0
    }

    /// `cache_id` is valid (revision 3).
    #[must_use]
    pub const fn cache_id_valid(self) -> bool {
        self.0 & 0x80 != 0
    }
}

/// What a cache holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheType {
    /// Data only.
    Data,
    /// Instructions only.
    Instruction,
    /// Both.
    Unified,
}

/// Which accesses allocate cache lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allocation {
    /// Reads.
    Read,
    /// Writes.
    Write,
    /// Reads and writes.
    ReadWrite,
}

/// Cache attributes (ACPI 6.5 Table 5.161, Attributes field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheAttributes(pub u8);

impl CacheAttributes {
    /// Allocation type, bits 1:0.
    #[must_use]
    pub const fn allocation(self) -> Allocation {
        match self.0 & 3 {
            0 => Allocation::Read,
            1 => Allocation::Write,
            _ => Allocation::ReadWrite,
        }
    }

    /// Cache type, bits 3:2.
    #[must_use]
    pub const fn cache_type(self) -> CacheType {
        match (self.0 >> 2) & 3 {
            0 => CacheType::Data,
            1 => CacheType::Instruction,
            _ => CacheType::Unified,
        }
    }

    /// Write-through (else write-back), bit 4.
    #[must_use]
    pub const fn write_through(self) -> bool {
        self.0 & 0x10 != 0
    }
}

/// A cache type structure. Fields are raw; check `flags` for validity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheNode {
    /// Offset from the start of the table.
    pub offset: u32,
    /// Which fields are valid.
    pub flags: CacheFlags,
    /// Offset of the next level of cache, 0 if none.
    pub next_level: u32,
    /// Size in bytes.
    pub size: u32,
    /// Number of sets.
    pub sets: u32,
    /// Ways of associativity.
    pub associativity: u8,
    /// Allocation type, cache type, and write policy.
    pub attributes: CacheAttributes,
    /// Line size in bytes.
    pub line_size: u16,
    /// Cache ID (revision 3 tables with a 28-byte structure).
    pub cache_id: Option<u32>,
}

/// Where a processor sits in the hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTopology<'a> {
    /// The leaf node for the processor.
    pub leaf: ProcessorNode<'a>,
    /// The core: the leaf, or the leaf's parent if the leaf is a thread.
    pub core: ProcessorNode<'a>,
    /// The core's parent, if it is not the package.
    pub cluster: Option<ProcessorNode<'a>>,
    /// The nearest node at or above the leaf marked as a physical package.
    pub package: Option<ProcessorNode<'a>>,
}

impl CpuTopology<'_> {
    /// The processor is an SMT thread.
    #[must_use]
    pub fn is_thread(&self) -> bool {
        self.leaf.flags.is_thread()
    }
}

impl<'a> Pptt<'a> {
    /// Every structure in the table. A malformed structure length yields
    /// one error and ends the walk; a known structure too short for its
    /// fields yields an error and the walk continues.
    #[must_use]
    pub fn nodes(&self) -> PpttNodes<'a> {
        PpttNodes {
            walk: Entries::new(self.bytes, HEADER_LEN, None, LenField::U8, WALK),
            revision: self.revision,
        }
    }

    /// The structure starting at `offset`, if one does.
    #[must_use]
    pub fn node_at(&self, offset: u32) -> Option<PpttNode<'a>> {
        self.nodes()
            .map_while(Result::ok)
            .find(|n| n.offset() == offset)
    }

    /// The processor node starting at `offset`, if one does.
    #[must_use]
    pub fn processor_at(&self, offset: u32) -> Option<ProcessorNode<'a>> {
        self.node_at(offset)?.processor()
    }

    /// The cache structure starting at `offset`, if one does.
    #[must_use]
    pub fn cache_at(&self, offset: u32) -> Option<CacheNode> {
        match self.node_at(offset)? {
            PpttNode::Cache(c) => Some(c),
            _ => None,
        }
    }

    fn processors(&self) -> impl Iterator<Item = ProcessorNode<'a>> + 'a {
        self.nodes()
            .map_while(Result::ok)
            .filter_map(PpttNode::processor)
    }

    fn has_children(&self, offset: u32) -> bool {
        self.processors().any(|p| p.parent == offset)
    }

    /// The leaf node for the processor with ACPI processor UID `uid`. Leaves
    /// are recognized by the leaf flag, or for revision 1 tables (which lack
    /// it) by having no children.
    #[must_use]
    pub fn processor(&self, uid: u32) -> Option<ProcessorNode<'a>> {
        self.processors().find(|p| {
            p.flags.acpi_id_valid()
                && p.acpi_processor_id == uid
                && (p.flags.is_leaf() || !self.has_children(p.offset))
        })
    }

    /// The ancestors of `node`, nearest first, excluding the node itself.
    #[must_use]
    pub fn ancestors(&self, node: &ProcessorNode<'a>) -> Ancestors<'a> {
        Ancestors {
            pptt: *self,
            next: node.parent(),
            depth: 0,
        }
    }

    /// The topology of the processor with ACPI processor UID `uid`, or
    /// `None` if it is not described or its ancestry is malformed.
    #[must_use]
    pub fn topology(&self, uid: u32) -> Option<CpuTopology<'a>> {
        let leaf = self.processor(uid)?;
        let core = if leaf.flags.is_thread() {
            self.ancestors(&leaf).next()?.ok()?
        } else {
            leaf
        };
        let cluster = match self.ancestors(&core).next() {
            Some(Ok(n)) => Some(n).filter(|n| !n.flags.physical_package()),
            Some(Err(_)) => return None,
            None => None,
        };
        let mut package = leaf.flags.physical_package().then_some(leaf);
        if package.is_none() {
            for n in self.ancestors(&leaf) {
                let n = n.ok()?;
                if n.flags.physical_package() {
                    package = Some(n);
                    break;
                }
            }
        }
        Some(CpuTopology {
            leaf,
            core,
            cluster,
            package,
        })
    }
}

/// Iterator returned by [`Pptt::nodes`].
#[derive(Clone, Debug)]
pub struct PpttNodes<'a> {
    walk: Entries<'a>,
    revision: u8,
}

impl<'a> Iterator for PpttNodes<'a> {
    type Item = Result<PpttNode<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let revision = self.revision;
        Some(self.walk.next()?.and_then(|raw| decode(raw, revision)))
    }
}

/// Iterator returned by [`Pptt::ancestors`]. A parent reference that is not
/// a processor node, or a chain deeper than [`MAX_DEPTH`], yields one error
/// and ends the walk.
#[derive(Clone, Debug)]
pub struct Ancestors<'a> {
    pptt: Pptt<'a>,
    next: Option<u32>,
    depth: u32,
}

impl<'a> Iterator for Ancestors<'a> {
    type Item = Result<ProcessorNode<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let off = self.next.take()?;
        if self.depth >= MAX_DEPTH {
            return Some(Err(TOO_DEEP));
        }
        self.depth += 1;
        let Some(node) = self.pptt.processor_at(off) else {
            return Some(Err(BAD_PARENT));
        };
        self.next = node.parent();
        Some(Ok(node))
    }
}

fn decode(raw: Raw<'_>, revision: u8) -> Result<PpttNode<'_>, Error> {
    let offset = u32::try_from(raw.off).map_err(|_| WALK)?;
    let f = Fields::new(raw.bytes, SHORT);
    Ok(match raw.kind {
        0 => {
            let count = usize::try_from(f.u32(16)?).map_err(|_| SHORT)?;
            let resources = count
                .checked_mul(4)
                .and_then(|n| raw.bytes.get(PROCESSOR_LEN..)?.get(..n))
                .ok_or(SHORT)?;
            PpttNode::ProcessorHierarchy(ProcessorNode {
                offset,
                flags: ProcessorFlags(f.u32(4)?),
                parent: f.u32(8)?,
                acpi_processor_id: f.u32(12)?,
                resources,
            })
        }
        1 => PpttNode::Cache(CacheNode {
            offset,
            flags: CacheFlags(f.u32(4)?),
            next_level: f.u32(8)?,
            size: f.u32(12)?,
            sets: f.u32(16)?,
            associativity: f.u8(20)?,
            attributes: CacheAttributes(f.u8(21)?),
            line_size: f.u16(22)?,
            cache_id: if revision >= 3 && raw.bytes.len() >= CACHE_ID_LEN {
                f.u32(24).ok()
            } else {
                None
            },
        }),
        kind => PpttNode::Other {
            offset,
            kind,
            bytes: raw.bytes,
        },
    })
}
