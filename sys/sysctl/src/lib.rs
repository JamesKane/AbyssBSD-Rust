// SPDX-License-Identifier: BSD-2-Clause
//! The sysctl tree: FreeBSD's management information base.
//!
//! Every node has a number, unique among its siblings, and a name; a node's
//! OID is the path of numbers from the root, and its full name the path of
//! names joined by dots (`kern.ostype` is `{1, 1}`). Nodes with a fixed
//! number in FreeBSD (`lib/abi::sysctl`) keep it, because userland asks for
//! them by number; the rest get numbers counted down from `0x7fffffff` as
//! they are registered, so newer nodes list first, and are found by name,
//! as in FreeBSD.
//!
//! Interior nodes group children; leaves have a [`Value`] that is read on
//! demand. The tree answers the queries the `__sysctl` syscall serves:
//! read by OID, name to OID and back, the next OID in listing order, a
//! node's kind and format, and its description. A writable leaf has a
//! writer ([`WriteFn`]) the kernel calls once it has checked the node's
//! access flags against the caller. The tree does no locking; the kernel
//! keeps it behind a lock.
//!
//! Behavior reference: sys/kern/kern_sysctl.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use abi::sysctl::{CTL_AUTO_START, CTL_MAXNAME, ctl, ctlflag, ctltype};

/// Reads a value into a string (for [`Value::Str`]).
pub type StrFn = Box<dyn Fn(&mut String) + Send + Sync>;
/// Reads a value's raw bytes (for [`Value::Opaque`]).
pub type BytesFn = Box<dyn Fn(&mut Vec<u8>) + Send + Sync>;
/// A node handler: reads the value named by the OID components after the
/// node (`hw.bus.devices.<generation>.<index>`). It is shared so the
/// kernel can call it after dropping the tree's lock (see
/// [`Tree::handler`]), as FreeBSD runs handlers.
pub type NodeFn = Arc<dyn Fn(&[i32], &mut Vec<u8>) -> Result<(), ReadError> + Send + Sync>;

/// A node handler's writer: takes what is written to the value named by
/// the OID components after the node (`kern.proc.args.<pid>`).
pub type NodeWriteFn = Arc<dyn Fn(&[i32], &[u8]) -> Result<(), abi::errno::Errno> + Send + Sync>;

/// A node whose value depends on what the caller passes in (FreeBSD's
/// handlers that `SYSCTL_IN` a key before they `SYSCTL_OUT`, as
/// `kern.devname` takes a device number): the bytes written, if any, give
/// the bytes read. It is shared and called after dropping the tree's lock,
/// as [`NodeFn`]s are.
pub type QueryFn =
    Arc<dyn Fn(Option<&[u8]>, &mut Vec<u8>) -> Result<(), abi::errno::Errno> + Send + Sync>;

/// Takes a written value (the bytes the caller passed); the kernel calls
/// it after dropping the tree's lock.
pub type WriteFn = Arc<dyn Fn(&[u8]) -> Result<(), abi::errno::Errno> + Send + Sync>;

/// A writer for an `int` node (`sysctl_handle_int`): `EINVAL` if fewer
/// than four bytes come; `set` decides whether the value is allowed.
pub fn int_writer(
    set: impl Fn(i32) -> Result<(), abi::errno::Errno> + Send + Sync + 'static,
) -> WriteFn {
    Arc::new(move |b: &[u8]| {
        let v = b
            .first_chunk::<4>()
            .map(|c| i32::from_ne_bytes(*c))
            .ok_or(abi::errno::Errno::EINVAL)?;
        set(v)
    })
}

/// A writer for a `uint8_t` node (`sysctl_handle_8`): `EINVAL` if
/// nothing comes. A `SYSCTL_BOOL` stores any nonzero value as 1.
pub fn u8_writer(
    set: impl Fn(u8) -> Result<(), abi::errno::Errno> + Send + Sync + 'static,
) -> WriteFn {
    Arc::new(move |b: &[u8]| set(*b.first().ok_or(abi::errno::Errno::EINVAL)?))
}

/// A writer for a string node of at most `max - 1` bytes
/// (`sysctl_handle_string`): `max` bytes or more written is `EINVAL`, NUL
/// or not; the value stops at its first NUL.
pub fn str_writer(
    max: usize,
    set: impl Fn(&[u8]) -> Result<(), abi::errno::Errno> + Send + Sync + 'static,
) -> WriteFn {
    Arc::new(move |b: &[u8]| {
        if b.len() >= max {
            return Err(abi::errno::Errno::EINVAL);
        }
        let b = b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n]);
        set(b)
    })
}

/// What a leaf holds, read on demand. Integers are in native byte order,
/// strings NUL-terminated, as FreeBSD returns them.
pub enum Value {
    /// `int`.
    Int(Box<dyn Fn() -> i32 + Send + Sync>),
    /// `u_int`.
    Uint(Box<dyn Fn() -> u32 + Send + Sync>),
    /// `uint8_t` (`SYSCTL_U8`, and `SYSCTL_BOOL`'s `bool`).
    U8(Box<dyn Fn() -> u8 + Send + Sync>),
    /// `long` (64 bits on every supported architecture).
    Long(Box<dyn Fn() -> i64 + Send + Sync>),
    /// `u_long`.
    Ulong(Box<dyn Fn() -> u64 + Send + Sync>),
    /// `int64_t`.
    S64(Box<dyn Fn() -> i64 + Send + Sync>),
    /// `uint64_t`.
    U64(Box<dyn Fn() -> u64 + Send + Sync>),
    /// A string.
    Str(StrFn),
    /// A string of bytes, which need not be UTF-8 (a host or domain
    /// name may hold any byte but NUL).
    ByteStr(BytesFn),
    /// A structure, described by `format` (for example `S,timeval`).
    Opaque {
        /// FreeBSD's format string for the structure.
        format: &'static str,
        /// Produces the bytes.
        read: BytesFn,
    },
}

impl Value {
    /// A fixed string.
    #[must_use]
    pub fn text(s: &'static str) -> Self {
        Self::Str(Box::new(move |out| out.push_str(s)))
    }

    /// A fixed `int`.
    #[must_use]
    pub fn int(v: i32) -> Self {
        Self::Int(Box::new(move || v))
    }

    fn ctltype(&self) -> u32 {
        match self {
            Self::Int(_) => ctltype::INT,
            Self::Uint(_) => ctltype::UINT,
            Self::U8(_) => ctltype::U8,
            Self::Long(_) => ctltype::LONG,
            Self::Ulong(_) => ctltype::ULONG,
            Self::S64(_) => ctltype::S64,
            Self::U64(_) => ctltype::U64,
            Self::Str(_) | Self::ByteStr(_) => ctltype::STRING,
            Self::Opaque { .. } => ctltype::OPAQUE,
        }
    }

    /// FreeBSD's format string for the type.
    fn format(&self) -> &'static str {
        match self {
            Self::Int(_) => "I",
            Self::Uint(_) => "IU",
            Self::U8(_) => "CU",
            Self::Long(_) => "L",
            Self::Ulong(_) => "LU",
            Self::S64(_) => "Q",
            Self::U64(_) => "QU",
            Self::Str(_) | Self::ByteStr(_) => "A",
            Self::Opaque { format, .. } => format,
        }
    }

    fn read(&self, out: &mut Vec<u8>) {
        match self {
            Self::Int(f) => out.extend_from_slice(&f().to_ne_bytes()),
            Self::Uint(f) => out.extend_from_slice(&f().to_ne_bytes()),
            Self::U8(f) => out.push(f()),
            Self::Long(f) | Self::S64(f) => out.extend_from_slice(&f().to_ne_bytes()),
            Self::Ulong(f) | Self::U64(f) => out.extend_from_slice(&f().to_ne_bytes()),
            Self::Str(f) => {
                let mut s = String::new();
                f(&mut s);
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            Self::ByteStr(f) => {
                f(out);
                out.push(0);
            }
            Self::Opaque { read, .. } => read(out),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Value({})", self.format())
    }
}

/// A node's number: FreeBSD's fixed one, or the next free one
/// (`OID_AUTO`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Number {
    /// A fixed number (ABI for well-known nodes).
    Fixed(i32),
    /// The next number from the tree's counter, which counts down from
    /// `0x7fffffff` (`OID_AUTO`).
    Auto,
}

/// A node, identified by slot and generation: an id from a removed node
/// never reaches a node that reused its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeId {
    index: u32,
    generation: u32,
}

/// What [`Tree::add`] needs to make a node.
#[derive(Debug)]
pub struct Spec<'a> {
    /// The number.
    pub number: Number,
    /// The name: unique among siblings, no dots.
    pub name: &'a str,
    /// The description (`sysctl -d`).
    pub description: &'a str,
    /// `CTLFLAG_*` bits beyond the access mode, which the value implies.
    pub flags: u32,
    /// The value, or `None` for an interior node.
    pub value: Option<Value>,
}

/// Why a node could not be added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddError {
    /// The parent does not exist (or is a leaf).
    NoParent,
    /// A sibling already has the name.
    NameTaken,
    /// A sibling already has the number.
    NumberTaken,
    /// The name is empty or contains a dot.
    BadName,
    /// The OID would exceed `CTL_MAXNAME` components.
    TooDeep,
}

/// Why a read failed (mapped to errno values by the syscall layer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// No such OID (`ENOENT`).
    NotFound,
    /// An interior node with no value (`EISDIR`).
    IsNode,
    /// A handler rejected the trailing components (`EINVAL`).
    Invalid,
    /// A handler failed for its own reason (`ESRCH` for a process that
    /// is not there, and the like).
    Failed(abi::errno::Errno),
}

struct Node {
    generation: u32,
    number: i32,
    name: String,
    description: String,
    /// `CTLTYPE_*` plus `CTLFLAG_*`.
    kind: u32,
    value: Option<Value>,
    /// For a node read through a handler.
    handler: Option<NodeFn>,
    /// For a handler node that takes writes.
    handler_writer: Option<NodeWriteFn>,
    /// For a writable leaf.
    writer: Option<WriteFn>,
    /// For a node read with what is written ([`QueryFn`]).
    query: Option<QueryFn>,
    /// A format other than its value's (`B` for a boolean `int`).
    format: Option<&'static str>,
    parent: Option<u32>,
    /// Children's slots, in increasing number.
    children: Vec<u32>,
    depth: usize,
}

/// The tree.
pub struct Tree {
    slots: Vec<Option<Node>>,
    /// Generations of freed slots, so reuse gets a new one.
    free: Vec<(u32, u32)>,
    /// The last `OID_AUTO` number handed out, shared by the whole tree as
    /// FreeBSD's `newoid` is.
    last_auto: i32,
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tree")
            .field("nodes", &self.slots.iter().flatten().count())
            .finish()
    }
}

const ROOT: u32 = 0;

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    /// A tree with FreeBSD's fixed top-level nodes (`kern`, `vm`, `vfs`,
    /// `net`, `debug`, `hw`, `machdep`, `user`, `p1003_1b`).
    #[must_use]
    pub fn new() -> Self {
        let mut t = Self {
            slots: Vec::new(),
            free: Vec::new(),
            last_auto: 0,
        };
        t.slots.push(Some(Node {
            generation: 0,
            number: 0,
            name: String::new(),
            description: String::new(),
            kind: ctltype::NODE | ctlflag::RD,
            value: None,
            handler: None,
            handler_writer: None,
            writer: None,
            query: None,
            format: None,
            parent: None,
            children: Vec::new(),
            depth: 0,
        }));
        let top = [
            (ctl::KERN, "kern", "High kernel, proc, limits &c"),
            (ctl::VM, "vm", "Virtual memory"),
            (ctl::VFS, "vfs", "File system"),
            (ctl::NET, "net", "Network, (see socket.h)"),
            (ctl::DEBUG, "debug", "Debugging"),
            (ctl::HW, "hw", "hardware"),
            (ctl::MACHDEP, "machdep", "machine dependent"),
            (ctl::USER, "user", "user-level"),
            (ctl::P1003_1B, "p1003_1b", "p1003_1b, (see p1003_1b.h)"),
        ];
        for (n, name, description) in top {
            let added = t.add(
                t.root(),
                Spec {
                    number: Number::Fixed(n),
                    name,
                    description,
                    flags: 0,
                    value: None,
                },
            );
            debug_assert!(added.is_ok());
        }
        t
    }

    /// The root (its OID is empty).
    #[must_use]
    pub fn root(&self) -> NodeId {
        NodeId {
            index: ROOT,
            generation: 0,
        }
    }

    fn node(&self, id: NodeId) -> Option<&Node> {
        self.slots
            .get(usize::try_from(id.index).ok()?)?
            .as_ref()
            .filter(|n| n.generation == id.generation)
    }

    fn at(&self, index: u32) -> &Node {
        self.slots[index as usize]
            .as_ref()
            .expect("tree links point at live nodes")
    }

    fn at_mut(&mut self, index: u32) -> &mut Node {
        self.slots[index as usize]
            .as_mut()
            .expect("tree links point at live nodes")
    }

    fn id(&self, index: u32) -> NodeId {
        NodeId {
            index,
            generation: self.at(index).generation,
        }
    }

    /// Adds a node under `parent`.
    pub fn add(&mut self, parent: NodeId, spec: Spec<'_>) -> Result<NodeId, AddError> {
        let p = self.node(parent).ok_or(AddError::NoParent)?;
        if p.value.is_some() || p.handler.is_some() {
            return Err(AddError::NoParent);
        }
        if spec.name.is_empty() || spec.name.contains('.') {
            return Err(AddError::BadName);
        }
        if p.depth >= CTL_MAXNAME {
            return Err(AddError::TooDeep);
        }
        if p.children.iter().any(|&c| self.at(c).name == spec.name) {
            return Err(AddError::NameTaken);
        }
        let taken = |n: i32| p.children.iter().any(|&c| self.at(c).number == n);
        let (number, last_auto) = match spec.number {
            Number::Fixed(n) if taken(n) => return Err(AddError::NumberTaken),
            Number::Fixed(n) => (n, self.last_auto),
            Number::Auto => {
                // Count down, so a new node usually goes first among its
                // siblings; on a collision count up from it, and wrap to
                // `CTL_AUTO_START` at the top (sysctl_register_oid).
                let next = match self.last_auto.checked_sub(1) {
                    Some(n) if n >= CTL_AUTO_START => n,
                    _ => i32::MAX,
                };
                let mut n = next;
                let mut wraps = 2;
                while taken(n) {
                    if n == i32::MAX {
                        if wraps == 0 {
                            return Err(AddError::NumberTaken);
                        }
                        wraps -= 1;
                        n = CTL_AUTO_START;
                    } else {
                        n += 1;
                    }
                }
                (n, next)
            }
        };
        let kind = match &spec.value {
            None => ctltype::NODE | ctlflag::RD,
            Some(v) => v.ctltype() | ctlflag::RD,
        } | spec.flags;
        let depth = p.depth + 1;
        let (index, generation) = match self.free.pop() {
            Some((i, g)) => (i, g),
            None => (
                u32::try_from(self.slots.len()).expect("sysctl tree size"),
                0,
            ),
        };
        let node = Node {
            generation,
            number,
            name: String::from(spec.name),
            description: String::from(spec.description),
            kind,
            value: spec.value,
            handler: None,
            handler_writer: None,
            writer: None,
            query: None,
            format: None,
            parent: Some(parent.index),
            children: Vec::new(),
            depth,
        };
        if index as usize == self.slots.len() {
            self.slots.push(Some(node));
        } else {
            self.slots[index as usize] = Some(node);
        }
        // Keep siblings sorted by number.
        let pos = {
            let p = self.at(parent.index);
            p.children
                .iter()
                .position(|&c| self.at(c).number > number)
                .unwrap_or(p.children.len())
        };
        self.at_mut(parent.index).children.insert(pos, index);
        self.last_auto = last_auto;
        Ok(NodeId { index, generation })
    }

    /// Adds an interior node.
    pub fn add_node(
        &mut self,
        parent: NodeId,
        number: Number,
        name: &str,
        description: &str,
    ) -> Result<NodeId, AddError> {
        self.add(
            parent,
            Spec {
                number,
                name,
                description,
                flags: 0,
                value: None,
            },
        )
    }

    /// Adds a read-only leaf.
    pub fn add_value(
        &mut self,
        parent: NodeId,
        number: Number,
        name: &str,
        description: &str,
        value: Value,
    ) -> Result<NodeId, AddError> {
        self.add(
            parent,
            Spec {
                number,
                name,
                description,
                flags: 0,
                value: Some(value),
            },
        )
    }

    /// Adds a leaf whose type and format are given, not its value's
    /// (FreeBSD's jail parameters: an `int` formatted `B` for a boolean,
    /// `E,jailsys` for a jailsys one). Its name may be empty, as a jailsys
    /// parameter's own (`host.`) is.
    #[allow(clippy::too_many_arguments, reason = "SYSCTL_JAIL_PARAM's")]
    pub fn add_formatted(
        &mut self,
        parent: NodeId,
        number: Number,
        name: &str,
        description: &str,
        kind: u32,
        format: &'static str,
        value: Value,
    ) -> Result<NodeId, AddError> {
        // An empty name is allowed here alone: give it a placeholder to
        // pass `add`, then clear it.
        let placeholder = name.is_empty();
        let id = self.add(
            parent,
            Spec {
                number,
                name: if placeholder { "\u{0}" } else { name },
                description,
                flags: 0,
                value: Some(value),
            },
        )?;
        let n = self.at_mut(id.index);
        n.kind = (n.kind & !ctltype::MASK) | (kind & ctltype::MASK);
        n.format = Some(format);
        if placeholder {
            n.name = String::new();
        }
        Ok(id)
    }

    /// Adds a writable leaf: `flags` are its `CTLFLAG_*` access bits
    /// besides `CTLFLAG_RW` (`ANYBODY`, `PRISON`, `SECURE`, `CAPWR`...).
    #[allow(clippy::too_many_arguments)] // `SYSCTL_PROC`'s, and a writer
    pub fn add_writable(
        &mut self,
        parent: NodeId,
        number: Number,
        name: &str,
        description: &str,
        flags: u32,
        value: Value,
        writer: WriteFn,
    ) -> Result<NodeId, AddError> {
        let id = self.add(
            parent,
            Spec {
                number,
                name,
                description,
                flags: flags | ctlflag::WR,
                value: Some(value),
            },
        )?;
        self.at_mut(id.index).writer = Some(writer);
        Ok(id)
    }

    /// The `CTLTYPE_*` and `CTLFLAG_*` bits of the node at `oid` (or of
    /// the handler node it is under), and its writer if it has one.
    #[must_use]
    pub fn access(&self, oid: &[i32]) -> Option<(u32, Option<WriteFn>)> {
        let mut cur = ROOT;
        for &n in oid {
            cur = *self
                .at(cur)
                .children
                .iter()
                .find(|&&c| self.at(c).number == n)?;
            if self.at(cur).handler.is_some() {
                break;
            }
        }
        let n = self.at(cur);
        if let Some(w) = &n.handler_writer {
            let depth = n.depth;
            let rest = oid.get(depth..).unwrap_or(&[]).to_vec();
            let w = Arc::clone(w);
            let writer: WriteFn = Arc::new(move |bytes| w(&rest, bytes));
            return Some((n.kind, Some(writer)));
        }
        Some((n.kind, n.writer.clone()))
    }

    /// Lets the handler node `id` take writes through `writer`, with the
    /// `CTLFLAG_*` bits `flags` (`CTLFLAG_RW | CTLFLAG_ANYBODY` for
    /// `kern.proc.args`).
    pub fn set_handler_writer(&mut self, id: NodeId, flags: u32, writer: NodeWriteFn) {
        if self.node(id).is_some_and(|n| n.handler.is_some()) {
            let n = self.at_mut(id.index);
            n.kind |= flags;
            n.handler_writer = Some(writer);
        }
    }

    /// Sets `CTLFLAG_*` bits on the node `id` (`CTLFLAG_CAPRD` on nodes a
    /// sandboxed program may read).
    pub fn set_flags(&mut self, id: NodeId, flags: u32) {
        if self.node(id).is_some() {
            self.at_mut(id.index).kind |= flags;
        }
    }

    /// Adds a node read through `handler`, which gets the OID components
    /// after the node (FreeBSD's `SYSCTL_NODE` with a handler).
    pub fn add_handler(
        &mut self,
        parent: NodeId,
        number: Number,
        name: &str,
        description: &str,
        handler: NodeFn,
    ) -> Result<NodeId, AddError> {
        let id = self.add_node(parent, number, name, description)?;
        self.at_mut(id.index).handler = Some(handler);
        Ok(id)
    }

    /// Adds a leaf of `kind` (`CTLTYPE_*`, `CTLFLAG_*`) and `format` read
    /// through `query`, with what the caller writes (see [`QueryFn`]).
    #[allow(clippy::too_many_arguments)]
    pub fn add_query(
        &mut self,
        parent: NodeId,
        number: Number,
        name: &str,
        description: &str,
        kind: u32,
        format: &'static str,
        query: QueryFn,
    ) -> Result<NodeId, AddError> {
        let id = self.add_node(parent, number, name, description)?;
        let n = self.at_mut(id.index);
        n.kind = kind;
        n.format = Some(format);
        n.query = Some(query);
        Ok(id)
    }

    /// The query serving `oid`, if it names a node added with
    /// [`Tree::add_query`]: the kernel clones it under the lock and calls
    /// it after releasing the lock.
    #[must_use]
    pub fn query(&self, oid: &[i32]) -> Option<QueryFn> {
        let n = self.node(self.lookup(oid)?)?;
        n.query.clone()
    }

    /// The handler serving `oid` and the components it is passed, if a
    /// handler node is on the path: the kernel clones it under the lock
    /// and calls it after releasing the lock.
    #[must_use]
    pub fn handler(&self, oid: &[i32]) -> Option<(NodeFn, Vec<i32>)> {
        let mut cur = ROOT;
        for (i, &n) in oid.iter().enumerate() {
            cur = *self
                .at(cur)
                .children
                .iter()
                .find(|&&c| self.at(c).number == n)?;
            if let Some(h) = &self.at(cur).handler {
                return Some((Arc::clone(h), oid[i + 1..].to_vec()));
            }
        }
        None
    }

    /// Removes a node and everything under it. Removing a stale id is a
    /// no-op (it returns false).
    pub fn remove(&mut self, id: NodeId) -> bool {
        if id.index == ROOT || self.node(id).is_none() {
            return false;
        }
        let parent = self.at(id.index).parent.unwrap_or(ROOT);
        self.at_mut(parent).children.retain(|&c| c != id.index);
        let mut stack = alloc::vec![id.index];
        while let Some(i) = stack.pop() {
            if let Some(n) = self.slots[i as usize].take() {
                stack.extend(n.children);
                self.free.push((i, n.generation.wrapping_add(1)));
            }
        }
        true
    }

    /// The child of `parent` named `name`.
    #[must_use]
    pub fn child(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        self.node(parent)?
            .children
            .iter()
            .find(|&&c| self.at(c).name == name)
            .map(|&c| self.id(c))
    }

    /// The node at `oid`.
    #[must_use]
    pub fn lookup(&self, oid: &[i32]) -> Option<NodeId> {
        let mut cur = ROOT;
        for &n in oid {
            cur = *self
                .at(cur)
                .children
                .iter()
                .find(|&&c| self.at(c).number == n)?;
        }
        Some(self.id(cur))
    }

    /// A node's OID.
    #[must_use]
    pub fn oid(&self, id: NodeId) -> Option<Vec<i32>> {
        let mut path = Vec::new();
        let mut cur = self.node(id)?;
        while let Some(p) = cur.parent {
            path.push(cur.number);
            cur = self.at(p);
        }
        path.reverse();
        Some(path)
    }

    /// The OID of a full name (`kern.ostype`); FreeBSD's `sysctl.name2oid`.
    #[must_use]
    pub fn name2oid(&self, name: &str) -> Option<Vec<i32>> {
        let mut cur = self.root();
        for part in name.split('.') {
            cur = self.child(cur, part)?;
        }
        self.oid(cur)
    }

    /// The full name of an OID; FreeBSD's `sysctl.name`.
    #[must_use]
    pub fn name(&self, oid: &[i32]) -> Option<String> {
        let mut cur = ROOT;
        let mut name = String::new();
        for &n in oid {
            cur = *self
                .at(cur)
                .children
                .iter()
                .find(|&&c| self.at(c).number == n)?;
            if !name.is_empty() {
                name.push('.');
            }
            name.push_str(&self.at(cur).name);
        }
        Some(name)
    }

    /// The next readable OID after `oid` in listing order (depth first,
    /// siblings by number), as `sysctl -a` walks the tree; FreeBSD's
    /// `sysctl.next` (`honor_skip`) and `sysctl.nextnoskip`. `CTLFLAG_SKIP`
    /// nodes and their subtrees are left out when `honor_skip`.
    #[must_use]
    pub fn next(&self, oid: &[i32], honor_skip: bool) -> Option<Vec<i32>> {
        let mut path = Vec::new();
        self.next_under(ROOT, &mut path, oid, honor_skip)
    }

    fn next_under(
        &self,
        index: u32,
        path: &mut Vec<i32>,
        after: &[i32],
        honor_skip: bool,
    ) -> Option<Vec<i32>> {
        for &c in &self.at(index).children {
            let n = self.at(c);
            if honor_skip && n.kind & ctlflag::SKIP != 0 {
                continue;
            }
            path.push(n.number);
            // Prune subtrees that lie wholly before `after`.
            let before = path.as_slice() < after && !after.starts_with(path);
            if !before {
                // A handler node lists as a leaf, as in FreeBSD.
                if (n.value.is_some() || n.handler.is_some()) && path.as_slice() > after {
                    return Some(path.clone());
                }
                if let Some(found) = self.next_under(c, path, after, honor_skip) {
                    return Some(found);
                }
            }
            path.pop();
        }
        None
    }

    /// An OID's kind (`CTLTYPE_*` with `CTLFLAG_*`) and format string;
    /// FreeBSD's `sysctl.oidfmt`.
    #[must_use]
    pub fn format(&self, oid: &[i32]) -> Option<(u32, &str)> {
        let n = self.node(self.lookup(oid)?)?;
        let fmt = n
            .format
            .unwrap_or_else(|| n.value.as_ref().map_or("N", Value::format));
        Some((n.kind, fmt))
    }

    /// An OID's description; FreeBSD's `sysctl.oiddescr`.
    #[must_use]
    pub fn description(&self, oid: &[i32]) -> Option<&str> {
        Some(&self.node(self.lookup(oid)?)?.description)
    }

    /// Appends the value at `oid` to `out`, calling a handler node's
    /// handler in place (the kernel uses [`Tree::handler`] instead).
    pub fn read(&self, oid: &[i32], out: &mut Vec<u8>) -> Result<(), ReadError> {
        if let Some((h, rest)) = self.handler(oid) {
            return h(&rest, out);
        }
        let id = self.lookup(oid).ok_or(ReadError::NotFound)?;
        let n = self.node(id).ok_or(ReadError::NotFound)?;
        let v = n.value.as_ref().ok_or(ReadError::IsNode)?;
        v.read(out);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
