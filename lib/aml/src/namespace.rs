// SPDX-License-Identifier: BSD-2-Clause
//! The ACPI namespace (ACPI 6.5 §5.3): a tree of named objects.
//!
//! Nodes live in an arena and are named by [`NodeId`], an index plus a
//! generation, so an id kept after its node was deleted (a method's
//! temporary objects are deleted when it returns) is detected rather than
//! aliasing a new node. Children keep definition order, which is the order
//! enumeration reports them in.
//!
//! Behavior reference: `sys/contrib/dev/acpica/components/namespace/nsaccess.c`,
//! `nssearch.c` (observable lookup rules).

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::Error;
use crate::host::{PciAddress, RegionSpace};
use crate::name::{AmlPath, Anchor, NameSeg, NameString};
use crate::value::{Location, ObjectType, Value};

/// Identifies a namespace node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId {
    index: u32,
    generation: u32,
}

/// A control method's body.
#[derive(Clone, Debug)]
pub(crate) struct Method {
    pub(crate) data: Arc<[u8]>,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) args: u8,
    pub(crate) serialized: bool,
    pub(crate) sync_level: u8,
}

/// Methods the interpreter implements itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Native {
    /// `\_OSI`.
    Osi,
}

/// A mutex (`Mutex` or `\_GL`).
#[derive(Clone, Debug)]
pub(crate) struct MutexObj {
    pub(crate) sync_level: u8,
    /// Acquisitions not yet released (recursive acquisition by the single
    /// thread of execution is allowed).
    pub(crate) depth: u32,
    /// The global lock, which is also acquired from the host.
    pub(crate) global: bool,
}

/// An operation region.
#[derive(Clone, Debug)]
pub(crate) struct Region {
    pub(crate) space: RegionSpace,
    pub(crate) base: u64,
    pub(crate) len: u64,
    /// The PCI address of a `PCI_Config` or `PciBarTarget` region, found on
    /// first access.
    pub(crate) pci: Option<PciAddress>,
}

/// How a field unit's update rule treats bits outside the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpdateRule {
    Preserve,
    WriteAsOnes,
    WriteAsZeros,
}

/// Field access types (ACPI 6.5 §19.6.48).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessType {
    Any,
    Byte,
    Word,
    DWord,
    QWord,
    Buffer,
}

/// What a field unit is a window onto.
#[derive(Clone, Debug)]
pub(crate) enum FieldKind {
    Region(NodeId),
    Index {
        index: NodeId,
        data: NodeId,
    },
    Bank {
        region: NodeId,
        bank: NodeId,
        value: u64,
    },
}

/// A field unit.
#[derive(Clone, Debug)]
pub(crate) struct FieldUnit {
    pub(crate) kind: FieldKind,
    pub(crate) bit_offset: u64,
    pub(crate) bit_len: u64,
    pub(crate) access: AccessType,
    /// Access attribute byte (`AccessAs`), for buffer-protocol spaces.
    pub(crate) attrib: u8,
    /// Access length for `AttribBytes`, `AttribRawBytes`,
    /// `AttribRawProcessBytes`.
    pub(crate) access_len: u8,
    pub(crate) lock: bool,
    pub(crate) update: UpdateRule,
    /// The `Connection` resource descriptor in effect.
    pub(crate) connection: Option<Arc<[u8]>>,
}

/// A buffer field (`CreateXxxField`).
#[derive(Clone, Debug)]
pub(crate) struct BufferField {
    pub(crate) source: Location,
    pub(crate) bit_offset: u64,
    pub(crate) bit_len: u64,
}

/// What a node holds.
#[derive(Clone, Debug)]
pub(crate) enum Object {
    /// A plain scope (the root, `\_GPE`, `\_PR`, `\_TZ`, `\_SI`).
    Scope,
    /// A named data value (`Name`).
    Name(Value),
    Device,
    Processor {
        id: u8,
        pblk: u32,
        pblk_len: u8,
    },
    PowerResource {
        system_level: u8,
        resource_order: u16,
    },
    ThermalZone,
    Method(Method),
    Native(Native, u8),
    Mutex(MutexObj),
    Event(u64),
    Region(Region),
    Field(FieldUnit),
    BufferField(BufferField),
    Alias(NodeId),
    /// Declared by `External` but not (yet) defined.
    External(u8, u8),
}

impl Object {
    pub(crate) fn object_type(&self) -> ObjectType {
        match self {
            Self::Scope | Self::Alias(_) => ObjectType::Uninitialized,
            Self::External(t, _) => ObjectType::from_u8(*t).unwrap_or(ObjectType::Uninitialized),
            Self::Name(v) => v.object_type(),
            Self::Device => ObjectType::Device,
            Self::Processor { .. } => ObjectType::Processor,
            Self::PowerResource { .. } => ObjectType::PowerResource,
            Self::ThermalZone => ObjectType::ThermalZone,
            Self::Method(_) | Self::Native(..) => ObjectType::Method,
            Self::Mutex(_) => ObjectType::Mutex,
            Self::Event(_) => ObjectType::Event,
            Self::Region(_) => ObjectType::Region,
            Self::Field(_) => ObjectType::FieldUnit,
            Self::BufferField(_) => ObjectType::BufferField,
        }
    }

    /// Arguments a method invocation of this object takes, or `None` if it
    /// is not invocable.
    pub(crate) fn method_args(&self) -> Option<u8> {
        match self {
            Self::Method(m) => Some(m.args),
            Self::Native(_, n) => Some(*n),
            Self::External(8, n) => Some(*n),
            _ => None,
        }
    }
}

/// A namespace node.
#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) name: NameSeg,
    pub(crate) parent: Option<NodeId>,
    pub(crate) children: Vec<NodeId>,
    pub(crate) object: Object,
}

#[derive(Debug)]
struct Slot {
    generation: u32,
    node: Option<Node>,
}

/// The namespace tree.
#[derive(Debug)]
pub(crate) struct Namespace {
    slots: Vec<Slot>,
    free: Vec<u32>,
    live: usize,
}

const MAX_ALIAS_CHAIN: usize = 16;

impl Namespace {
    pub(crate) fn new() -> Self {
        let root = Node {
            name: NameSeg::ROOT,
            parent: None,
            children: Vec::new(),
            object: Object::Scope,
        };
        Self {
            slots: alloc::vec![Slot {
                generation: 0,
                node: Some(root),
            }],
            free: Vec::new(),
            live: 1,
        }
    }

    pub(crate) const fn root() -> NodeId {
        NodeId {
            index: 0,
            generation: 0,
        }
    }

    /// Number of live nodes, the root included.
    pub(crate) fn len(&self) -> usize {
        self.live
    }

    pub(crate) fn get(&self, id: NodeId) -> Option<&Node> {
        let s = self.slots.get(usize::try_from(id.index).ok()?)?;
        if s.generation != id.generation {
            return None;
        }
        s.node.as_ref()
    }

    pub(crate) fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let s = self.slots.get_mut(usize::try_from(id.index).ok()?)?;
        if s.generation != id.generation {
            return None;
        }
        s.node.as_mut()
    }

    pub(crate) fn object(&self, id: NodeId) -> Option<&Object> {
        self.get(id).map(|n| &n.object)
    }

    pub(crate) fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.get(id)?.parent
    }

    pub(crate) fn child(&self, parent: NodeId, name: NameSeg) -> Option<NodeId> {
        self.get(parent)?
            .children
            .iter()
            .copied()
            .find(|&c| self.get(c).is_some_and(|n| n.name == name))
    }

    pub(crate) fn children(&self, id: NodeId) -> &[NodeId] {
        self.get(id).map_or(&[], |n| &n.children)
    }

    /// Adds a child. Fails if the name exists, unless the existing node is
    /// an `External` placeholder, which the definition replaces.
    pub(crate) fn add(
        &mut self,
        parent: NodeId,
        name: NameSeg,
        object: Object,
    ) -> Result<NodeId, Error> {
        if let Some(existing) = self.child(parent, name) {
            let node = self.get_mut(existing).ok_or(Error::Internal)?;
            if matches!(node.object, Object::External(..)) {
                node.object = object;
                return Ok(existing);
            }
            return Err(Error::AlreadyExists);
        }
        self.get(parent).ok_or(Error::NotFound)?;
        let node = Node {
            name,
            parent: Some(parent),
            children: Vec::new(),
            object,
        };
        let id = if let Some(index) = self.free.pop() {
            let slot = self
                .slots
                .get_mut(usize::try_from(index).map_err(|_| Error::Internal)?)
                .ok_or(Error::Internal)?;
            slot.node = Some(node);
            NodeId {
                index,
                generation: slot.generation,
            }
        } else {
            let index = u32::try_from(self.slots.len()).map_err(|_| Error::NoMemory)?;
            self.slots.try_reserve(1).map_err(|_| Error::NoMemory)?;
            self.slots.push(Slot {
                generation: 0,
                node: Some(node),
            });
            NodeId {
                index,
                generation: 0,
            }
        };
        let p = self.get_mut(parent).ok_or(Error::Internal)?;
        p.children.try_reserve(1).map_err(|_| Error::NoMemory)?;
        p.children.push(id);
        self.live += 1;
        Ok(id)
    }

    /// Deletes a node and its subtree. The root cannot be deleted.
    pub(crate) fn remove(&mut self, id: NodeId) {
        let Some(node) = self.get(id) else {
            return;
        };
        let Some(parent) = node.parent else {
            return;
        };
        if let Some(p) = self.get_mut(parent) {
            p.children.retain(|&c| c != id);
        }
        let mut stack = alloc::vec![id];
        while let Some(n) = stack.pop() {
            let Ok(index) = usize::try_from(n.index) else {
                continue;
            };
            let Some(slot) = self.slots.get_mut(index) else {
                continue;
            };
            if slot.generation != n.generation {
                continue;
            }
            if let Some(node) = slot.node.take() {
                stack.extend(node.children);
                slot.generation = slot.generation.wrapping_add(1);
                self.free.push(n.index);
                self.live -= 1;
            }
        }
    }

    /// Follows aliases to the object they name.
    pub(crate) fn follow(&self, mut id: NodeId) -> Option<NodeId> {
        for _ in 0..MAX_ALIAS_CHAIN {
            match self.object(id)? {
                Object::Alias(t) => id = *t,
                _ => return Some(id),
            }
        }
        None
    }

    /// The node a name's anchor and leading segments lead to, without the
    /// search rule. Returns the scope that would contain the last segment.
    fn walk_prefix(&self, scope: NodeId, name: &NameString, skip_last: bool) -> Option<NodeId> {
        let mut at = match name.anchor {
            Anchor::Root => Self::root(),
            Anchor::Relative(up) => {
                let mut at = scope;
                for _ in 0..up {
                    at = self.parent(at)?;
                }
                at
            }
        };
        let n = if skip_last {
            name.segs.len().saturating_sub(1)
        } else {
            name.segs.len()
        };
        for seg in name.segs.get(..n)? {
            at = self.child(at, *seg)?;
            at = self.follow(at)?;
        }
        Some(at)
    }

    /// Resolves a name from `scope` (ACPI 6.5 §5.3): absolute and prefixed
    /// names directly, and a single bare segment by searching `scope` and
    /// then each enclosing scope up to the root. Aliases are followed.
    pub(crate) fn resolve(&self, scope: NodeId, name: &NameString) -> Option<NodeId> {
        if name.uses_search_rules() {
            let seg = name.segs[0];
            let mut at = Some(scope);
            while let Some(s) = at {
                if let Some(c) = self.child(s, seg) {
                    return self.follow(c);
                }
                at = self.parent(s);
            }
            return None;
        }
        self.walk_prefix(scope, name, false)
    }

    /// The parent scope and final segment for creating `name` in `scope`.
    /// The parent must exist; no search rule applies.
    pub(crate) fn creation_target(
        &self,
        scope: NodeId,
        name: &NameString,
    ) -> Result<(NodeId, NameSeg), Error> {
        let last = *name.segs.last().ok_or(Error::InvalidName)?;
        let parent = self.walk_prefix(scope, name, true).ok_or(Error::NotFound)?;
        Ok((parent, last))
    }

    /// Resolves an absolute path.
    pub(crate) fn lookup(&self, path: &AmlPath) -> Option<NodeId> {
        let mut at = Self::root();
        for seg in &path.0 {
            at = self.follow(self.child(at, *seg)?)?;
        }
        Some(at)
    }

    /// The absolute path of a node.
    pub(crate) fn path(&self, id: NodeId) -> AmlPath {
        let mut segs = Vec::new();
        let mut at = Some(id);
        while let Some(n) = at {
            let Some(node) = self.get(n) else {
                break;
            };
            if node.parent.is_some() {
                segs.push(node.name);
            }
            at = node.parent;
        }
        segs.reverse();
        AmlPath(segs)
    }

    /// Every live node in depth-first definition order, starting at `from`.
    pub(crate) fn walk(&self, from: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = alloc::vec![from];
        while let Some(n) = stack.pop() {
            if self.get(n).is_none() {
                continue;
            }
            out.push(n);
            stack.extend(self.children(n).iter().rev().copied());
        }
        out
    }
}
