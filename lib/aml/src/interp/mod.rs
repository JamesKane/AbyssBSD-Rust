// SPDX-License-Identifier: BSD-2-Clause
//! The interpreter: loading definition blocks and evaluating control
//! methods (ACPI 6.5 §19, §20).
//!
//! AML is executed straight from the table bytes; there is no separate
//! syntax tree. Loading a table runs its top-level term list with the root
//! as the scope: named-object opcodes create nodes, method bodies are
//! recorded as byte ranges and parsed only when invoked (their operands
//! depend on the namespace at run time), and module-level code such as
//! `If` blocks runs in order. A failure inside a scope body during loading
//! is recorded as a [`LoadWarning`] and loading resumes after that body.
//!
//! Every recursion (nested terms, method calls, field accesses) passes a
//! guard that checks both the nesting depth and the stack actually used
//! since the outermost entry ([`Limits::stack_budget`]), so the bound holds
//! whatever a build's frame sizes are. `While` loops are bounded by an
//! iteration count and an optional time limit, allocations by
//! [`Limits::max_object_size`], and value nesting (packages and references,
//! which drop and clone recursively) by a fixed depth, so hostile AML ends
//! in an [`Error`].
//!
//! Behavior reference: `sys/dev/acpica/acpi.c` (interpreter slack mode,
//! `_OSI` configuration), `sys/contrib/dev/acpica/components/utilities/utosi.c`
//! (the interface strings FreeBSD answers true to),
//! `sys/contrib/dev/acpica/components/utilities/utglobal.c` (`\_OS`, `\_REV`).

mod exec;
mod expr;
mod field;
mod ops;
mod store;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::Error;
use crate::host::Host;
use crate::name::{AmlPath, NameSeg, NameString};
use crate::namespace::{MutexObj, Namespace, Native, NodeId, Object};
use crate::stream::Cursor;
use crate::value::{IntWidth, ObjectType, Value};

/// The `_OSI` strings FreeBSD's ACPICA answers true to by default
/// (`utosi.c`): every Windows version string and the one mandatory feature
/// group string. `"Linux"` and `"Darwin"` are not on the list.
pub const FREEBSD_OSI: &[&str] = &[
    "Windows 2000",
    "Windows 2001",
    "Windows 2001 SP1",
    "Windows 2001.1",
    "Windows 2001 SP2",
    "Windows 2001.1 SP1",
    "Windows 2006",
    "Windows 2006.1",
    "Windows 2006 SP1",
    "Windows 2006 SP2",
    "Windows 2009",
    "Windows 2012",
    "Windows 2013",
    "Windows 2015",
    "Windows 2016",
    "Windows 2017",
    "Windows 2017.2",
    "Windows 2018",
    "Windows 2018.2",
    "Windows 2019",
    "Windows 2020",
    "Windows 2021",
    "Windows 2022",
    "Extended Address Space Descriptor",
];

/// Resource bounds for evaluating untrusted AML.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Iterations one `While` loop may run before it fails with
    /// [`Error::LoopLimit`].
    pub max_loop_iterations: u64,
    /// Time one `While` loop may run, in 100 ns units of [`Host::timer`]
    /// (ACPICA's default is 30 s). Ignored while the timer reads 0.
    pub loop_timeout: u64,
    /// Nested method calls.
    pub max_call_depth: usize,
    /// Nested terms (expressions, scopes, and calls together).
    pub max_depth: u32,
    /// Bytes of stack the interpreter may use below its outermost entry
    /// (measured, not estimated: every nesting level checks the distance
    /// from the entry point). Past it, evaluation fails with
    /// [`Error::DepthLimit`] instead of overflowing. The default fits a
    /// 16 KiB kernel thread stack with 6 KiB left for the caller, host
    /// callbacks, and interrupts; see [`Limits::for_stack`].
    pub stack_budget: usize,
    /// Largest buffer, string, or package (in bytes or elements) AML may
    /// create.
    pub max_object_size: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_loop_iterations: 1 << 20,
            loop_timeout: 30 * 10_000_000,
            max_call_depth: 32,
            max_depth: 256,
            stack_budget: 10 * 1024,
            max_object_size: 1 << 20,
        }
    }
}

impl Limits {
    /// Stack left for the caller, host callbacks, and interrupts when the
    /// budget is derived from a stack size.
    pub const STACK_HEADROOM: usize = 6 * 1024;

    /// Default limits for an interpreter running on a stack of
    /// `stack_bytes`. Real firmware nests about 30 levels deep (the Radxa
    /// Q8B DSDT), and a level costs roughly 400 bytes in an optimized
    /// build, so a kernel should evaluate AML on a dedicated thread with a
    /// stack of 32 KiB or more rather than on a 16 KiB thread stack.
    #[must_use]
    pub fn for_stack(stack_bytes: usize) -> Self {
        Self {
            stack_budget: stack_bytes.saturating_sub(Self::STACK_HEADROOM),
            ..Self::default()
        }
    }
}

/// Interpreter configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Interface strings `\_OSI` answers true to.
    pub osi: Vec<String>,
    /// The value of `\_OS`.
    pub os_name: String,
    /// The value of `\_REV` (FreeBSD: 2).
    pub rev: u64,
    /// The value of the `Revision` opcode (ACPICA reports its version).
    pub revision: u64,
    /// ACPICA's "interpreter slack" (FreeBSD enables it): a method without
    /// `Return` returns the last value it computed, an uninitialized local
    /// or argument reads as 0, and a field may reach past its region up to
    /// the end of the access unit containing the region's last byte.
    pub slack: bool,
    /// Resource bounds.
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            osi: FREEBSD_OSI.iter().map(|s| (*s).to_string()).collect(),
            os_name: String::from("Microsoft Windows NT"),
            rev: 2,
            revision: 0x2026_0408,
            slack: true,
            limits: Limits::default(),
        }
    }
}

/// A problem found while loading a table that did not stop the load: the
/// rest of the enclosing scope body was skipped.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadWarning {
    /// Offset in the table of the scope body that was cut short.
    pub offset: usize,
    /// What went wrong.
    pub error: Error,
}

/// The outcome of loading one table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadReport {
    /// Problems that cut scope bodies short.
    pub warnings: Vec<LoadWarning>,
}

/// Control flow out of a term list.
#[derive(Debug)]
pub(crate) enum Flow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

/// A method activation (or the module-level code of a table being loaded).
#[derive(Debug)]
pub(crate) struct Frame {
    pub(crate) id: u32,
    pub(crate) args: [Value; 7],
    pub(crate) locals: [Value; 8],
    /// Objects created by the method, deleted when it returns.
    pub(crate) temps: Vec<NodeId>,
    /// The last value computed, returned under slack mode.
    pub(crate) implicit: Value,
    /// True for a method; false for a table's module-level code.
    pub(crate) method: bool,
    /// Mutexes held when the frame started.
    pub(crate) held_base: usize,
    /// The sync level of a serialized method.
    pub(crate) sync_level: Option<u8>,
}

impl Frame {
    fn new(id: u32, method: bool, held_base: usize) -> Self {
        Self {
            id,
            args: Default::default(),
            locals: Default::default(),
            temps: Vec::new(),
            implicit: Value::Uninitialized,
            method,
            held_base,
            sync_level: None,
        }
    }
}

/// A control method's declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodInfo {
    /// Number of arguments.
    pub args: u8,
    /// Serialized (`Serialized` in ASL).
    pub serialized: bool,
    /// Sync level of a serialized method.
    pub sync_level: u8,
}

/// A (deprecated) `Processor` object's operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorInfo {
    /// Processor ID (matches the MADT's ACPI processor UID).
    pub id: u8,
    /// Processor block address.
    pub pblk: u32,
    /// Processor block length.
    pub pblk_len: u8,
}

/// A `PowerResource` object's operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerResourceInfo {
    /// Deepest system sleep state in which the resource must stay on.
    pub system_level: u8,
    /// Order relative to other power resources.
    pub resource_order: u16,
}

/// An AML interpreter and the namespace it maintains.
#[derive(Debug)]
pub struct Interpreter {
    pub(crate) ns: Namespace,
    pub(crate) config: Config,
    pub(crate) width: IntWidth,
    pub(crate) frames: Vec<Frame>,
    next_frame: u32,
    /// Mutexes currently held, in acquisition order.
    pub(crate) held: Vec<NodeId>,
    pub(crate) depth: u32,
    /// The deepest nesting reached so far (diagnostics, stack sizing).
    pub(crate) peak_depth: u32,
    /// Stack address at the outermost `enter`.
    pub(crate) stack_base: usize,
    /// The most stack used below `stack_base` so far.
    pub(crate) peak_stack: usize,
    /// Test builds record the stack address at each depth, to measure the
    /// stack a nesting level costs.
    #[cfg(test)]
    pub(crate) stack_probe: Vec<usize>,
    /// True while a table's module-level code (not a method it calls) runs.
    pub(crate) loading: bool,
    pub(crate) warnings: Vec<LoadWarning>,
    dsdt_loaded: bool,
}

impl Interpreter {
    /// An interpreter with the predefined namespace (ACPI 6.5 §5.3.1):
    /// `\_GPE`, `\_PR`, `\_SB`, `\_SI`, `\_TZ`, `\_GL`, `\_OS`, `\_OSI`,
    /// `\_REV`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let mut me = Self {
            ns: Namespace::new(),
            config,
            width: IntWidth::W64,
            frames: Vec::new(),
            next_frame: 0,
            held: Vec::new(),
            depth: 0,
            peak_depth: 0,
            stack_base: 0,
            peak_stack: 0,
            #[cfg(test)]
            stack_probe: Vec::new(),
            loading: false,
            warnings: Vec::new(),
            dsdt_loaded: false,
        };
        let root = Namespace::root();
        let os = Value::String(me.config.os_name.clone());
        let rev = Value::Integer(me.config.rev);
        let predefined = [
            ("_GPE", Object::Scope),
            ("_PR", Object::Scope),
            ("_SB", Object::Device),
            ("_SI", Object::Scope),
            ("_TZ", Object::Scope),
            (
                "_GL",
                Object::Mutex(MutexObj {
                    sync_level: 0,
                    depth: 0,
                    global: true,
                }),
            ),
            ("_OS", Object::Name(os)),
            ("_OSI", Object::Native(Native::Osi, 1)),
            ("_REV", Object::Name(rev)),
        ];
        for (name, obj) in predefined {
            if let Some(seg) = NameSeg::from_str_padded(name) {
                // A fresh namespace has none of these names, so adding
                // cannot fail.
                let _ = me.ns.add(root, seg, obj);
            }
        }
        me
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The integer width in effect (set by the DSDT's revision).
    #[must_use]
    pub fn int_width(&self) -> IntWidth {
        self.width
    }

    /// The deepest term nesting any evaluation has reached, in the units
    /// of [`Limits::max_depth`].
    #[must_use]
    pub fn peak_depth(&self) -> u32 {
        self.peak_depth
    }

    /// The most stack any evaluation has used below its entry point, in
    /// bytes (as measured at nesting-level boundaries).
    #[must_use]
    pub fn peak_stack(&self) -> usize {
        self.peak_stack
    }

    /// Number of nodes in the namespace, the root included.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.ns.len()
    }

    /// Loads a definition block (DSDT or SSDT, header included) into the
    /// namespace. Load the DSDT first: its revision sets the integer width.
    ///
    /// # Errors
    ///
    /// [`Error::BadTable`] for a bad header, length, or checksum.
    /// Problems inside the AML do not fail the load; they are reported in
    /// the [`LoadReport`].
    pub fn load_table(&mut self, table: &[u8], host: &mut dyn Host) -> Result<LoadReport, Error> {
        const HEADER: usize = 36;
        if table.len() < HEADER {
            return Err(Error::BadTable("shorter than the header"));
        }
        let len = u32::from_le_bytes([table[4], table[5], table[6], table[7]]);
        let len = usize::try_from(len).map_err(|_| Error::BadTable("length"))?;
        if len < HEADER || len > table.len() {
            return Err(Error::BadTable("length"));
        }
        let table = &table[..len];
        if table.iter().fold(0u8, |a, &b| a.wrapping_add(b)) != 0 {
            return Err(Error::BadTable("checksum"));
        }
        // Any signature is accepted (SSDT, PSDT, OEM tables); only the
        // DSDT's revision matters.
        let sig = &table[..4];
        if sig == b"DSDT" && !self.dsdt_loaded {
            self.width = if table[8] < 2 {
                IntWidth::W32
            } else {
                IntWidth::W64
            };
            self.dsdt_loaded = true;
        }
        let data: Arc<[u8]> = Arc::from(table);
        self.run_module_code(Cursor::new(data, HEADER, len), host)
    }

    fn run_module_code(
        &mut self,
        mut cur: Cursor,
        host: &mut dyn Host,
    ) -> Result<LoadReport, Error> {
        let id = self.alloc_frame_id();
        self.frames.push(Frame::new(id, false, self.held.len()));
        let saved_loading = core::mem::replace(&mut self.loading, true);
        let saved_warnings = core::mem::take(&mut self.warnings);
        let saved_depth = self.depth;
        let r = self.exec_list(host, &mut cur, Namespace::root());
        self.depth = saved_depth;
        self.loading = saved_loading;
        let warnings = core::mem::replace(&mut self.warnings, saved_warnings);
        if let Some(frame) = self.frames.pop() {
            self.release_held_from(host, frame.held_base);
        }
        match r {
            Ok(_) => Ok(LoadReport { warnings }),
            Err(e) => {
                let mut warnings = warnings;
                warnings.push(LoadWarning {
                    offset: cur.pos,
                    error: e,
                });
                Ok(LoadReport { warnings })
            }
        }
    }

    pub(crate) fn alloc_frame_id(&mut self) -> u32 {
        self.next_frame = self.next_frame.wrapping_add(1);
        self.next_frame
    }

    /// Finds a node by absolute path (`\_SB.PCI0`).
    #[must_use]
    pub fn lookup(&self, path: &AmlPath) -> Option<NodeId> {
        self.ns.lookup(path)
    }

    /// Finds a node by name text, absolute or relative to `scope` (with the
    /// upward search rule for a single segment).
    #[must_use]
    pub fn resolve(&self, scope: NodeId, name: &str) -> Option<NodeId> {
        let n = NameString::parse(name).ok()?;
        self.ns.resolve(scope, &n)
    }

    /// The root node.
    #[must_use]
    pub fn root(&self) -> NodeId {
        Namespace::root()
    }

    /// The absolute path of a node.
    #[must_use]
    pub fn path(&self, node: NodeId) -> AmlPath {
        self.ns.path(node)
    }

    /// The node's own name segment, or `None` for the root or a stale id.
    #[must_use]
    pub fn name(&self, node: NodeId) -> Option<NameSeg> {
        let n = self.ns.get(node)?;
        n.parent.map(|_| n.name)
    }

    /// The node's parent.
    #[must_use]
    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.ns.parent(node)
    }

    /// The node's children, in definition order.
    #[must_use]
    pub fn children(&self, node: NodeId) -> &[NodeId] {
        self.ns.children(node)
    }

    /// A child by name segment text (`"_HID"`).
    #[must_use]
    pub fn child(&self, node: NodeId, name: &str) -> Option<NodeId> {
        let seg = NameSeg::from_str_padded(name)?;
        self.ns.child(node, seg).and_then(|c| self.ns.follow(c))
    }

    /// The object type of a node; `None` for a stale id, an alias that
    /// leads nowhere, or an `External` declaration not yet defined.
    #[must_use]
    pub fn object_type(&self, node: NodeId) -> Option<ObjectType> {
        let node = self.ns.follow(node)?;
        match self.ns.object(node)? {
            Object::External(..) => None,
            o => Some(o.object_type()),
        }
    }

    /// A method's declaration, or `None` if the node is not a method.
    #[must_use]
    pub fn method_info(&self, node: NodeId) -> Option<MethodInfo> {
        match self.ns.object(self.ns.follow(node)?)? {
            Object::Method(m) => Some(MethodInfo {
                args: m.args,
                serialized: m.serialized,
                sync_level: m.sync_level,
            }),
            Object::Native(_, args) => Some(MethodInfo {
                args: *args,
                serialized: false,
                sync_level: 0,
            }),
            _ => None,
        }
    }

    /// A `Processor` object's operands.
    #[must_use]
    pub fn processor_info(&self, node: NodeId) -> Option<ProcessorInfo> {
        match self.ns.object(self.ns.follow(node)?)? {
            Object::Processor { id, pblk, pblk_len } => Some(ProcessorInfo {
                id: *id,
                pblk: *pblk,
                pblk_len: *pblk_len,
            }),
            _ => None,
        }
    }

    /// A `PowerResource` object's operands.
    #[must_use]
    pub fn power_resource_info(&self, node: NodeId) -> Option<PowerResourceInfo> {
        match self.ns.object(self.ns.follow(node)?)? {
            Object::PowerResource {
                system_level,
                resource_order,
            } => Some(PowerResourceInfo {
                system_level: *system_level,
                resource_order: *resource_order,
            }),
            _ => None,
        }
    }

    /// True for a plain scope (the root and the predefined scopes).
    #[must_use]
    pub fn is_scope(&self, node: NodeId) -> bool {
        matches!(self.ns.object(node), Some(Object::Scope))
    }

    /// Every node in depth-first definition order, the root first.
    #[must_use]
    pub fn nodes(&self) -> Vec<NodeId> {
        self.ns.walk(Namespace::root())
    }

    /// Evaluates the object at `path`: invokes a method with `args`, or
    /// reads a named value or field.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if nothing is at `path`; otherwise whatever
    /// error evaluation produced.
    pub fn evaluate(
        &mut self,
        path: &AmlPath,
        args: Vec<Value>,
        host: &mut dyn Host,
    ) -> Result<Value, Error> {
        let node = self.ns.lookup(path).ok_or(Error::NotFound)?;
        self.evaluate_node(node, args, host)
    }

    /// Evaluates a child of `node` by segment name (`"_STA"`), returning
    /// `Ok(None)` if there is no such child.
    ///
    /// # Errors
    ///
    /// Whatever error evaluation produced.
    pub fn evaluate_child(
        &mut self,
        node: NodeId,
        name: &str,
        args: Vec<Value>,
        host: &mut dyn Host,
    ) -> Result<Option<Value>, Error> {
        let Some(child) = self.child(node, name) else {
            return Ok(None);
        };
        self.evaluate_node(child, args, host).map(Some)
    }

    /// Evaluates a node: invokes a method with `args`, or reads a named
    /// value or field. Package elements that are names stay unresolved
    /// ([`Value::Name`]); see [`Interpreter::resolve_value_name`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a stale id; otherwise whatever error
    /// evaluation produced.
    pub fn evaluate_node(
        &mut self,
        node: NodeId,
        args: Vec<Value>,
        host: &mut dyn Host,
    ) -> Result<Value, Error> {
        let node = self.ns.follow(node).ok_or(Error::NotFound)?;
        let obj = self.ns.object(node).ok_or(Error::NotFound)?;
        let saved_depth = self.depth;
        let r = if obj.method_args().is_some() {
            if args.len() > 7 {
                return Err(Error::TooManyArgs);
            }
            self.invoke(host, node, args)
        } else {
            self.read_node(host, node)
        };
        self.depth = saved_depth;
        r
    }

    /// Resolves a [`Value::Name`] (a name inside a package) to the node it
    /// names, from the scope the package was created in.
    #[must_use]
    pub fn resolve_value_name(&self, v: &Value) -> Option<NodeId> {
        match v {
            Value::Name(scope, name) => self.ns.resolve(*scope, name),
            Value::Reference(r) => r.node(),
            _ => None,
        }
    }

    /// Runs `_REG(space, 1)` under every device that has an operation
    /// region in `space` (ACPI 6.5 §6.5.4), as the OS does once it can
    /// serve that space. Returns how many `_REG` methods ran; failures are
    /// returned with the device's node.
    pub fn run_reg(
        &mut self,
        space: crate::host::RegionSpace,
        host: &mut dyn Host,
    ) -> (usize, Vec<(NodeId, Error)>) {
        let mut parents = Vec::new();
        for n in self.ns.walk(Namespace::root()) {
            if let Some(Object::Region(r)) = self.ns.object(n)
                && r.space == space
                && let Some(p) = self.ns.parent(n)
                && !parents.contains(&p)
            {
                parents.push(p);
            }
        }
        let mut ran = 0;
        let mut errors = Vec::new();
        for p in parents {
            let args = alloc::vec![Value::Integer(u64::from(space.to_u8())), Value::Integer(1)];
            match self.evaluate_child(p, "_REG", args, host) {
                Ok(Some(_)) => ran += 1,
                Ok(None) => {}
                Err(e) => errors.push((p, e)),
            }
        }
        (ran, errors)
    }

    /// Runs `\_SB._INI`, then `_INI` of every device whose `_STA` reports
    /// it present, descending into devices that are present or functional
    /// (ACPI 6.5 §6.5.1). Returns the failures with their nodes.
    pub fn initialize(&mut self, host: &mut dyn Host) -> Vec<(NodeId, Error)> {
        let mut errors = Vec::new();
        if let Some(sb) = self.child(Namespace::root(), "_SB")
            && let Err(e) = self.evaluate_child(sb, "_INI", Vec::new(), host)
        {
            errors.push((sb, e));
        }
        let mut stack: Vec<NodeId> = self.ns.children(Namespace::root()).to_vec();
        stack.reverse();
        while let Some(n) = stack.pop() {
            let is_dev = matches!(
                self.ns.object(n),
                Some(
                    Object::Device
                        | Object::Processor { .. }
                        | Object::ThermalZone
                        | Object::PowerResource { .. }
                )
            );
            let mut descend = matches!(self.ns.object(n), Some(Object::Scope | Object::Device));
            if is_dev {
                let sta = match self.evaluate_child(n, "_STA", Vec::new(), host) {
                    Ok(Some(v)) => v.to_integer(self.width).unwrap_or(0x0f),
                    Ok(None) => 0x0f,
                    Err(e) => {
                        errors.push((n, e));
                        0
                    }
                };
                if sta & 1 != 0 {
                    if !(self.path(n).0.len() == 1 && self.name(n).is_some_and(|s| s.0 == *b"_SB_"))
                        && let Err(e) = self.evaluate_child(n, "_INI", Vec::new(), host)
                    {
                        errors.push((n, e));
                    }
                    descend = true;
                } else {
                    descend = sta & 8 != 0;
                }
            }
            if descend {
                let kids: Vec<NodeId> = self.ns.children(n).iter().rev().copied().collect();
                stack.extend(kids);
            }
        }
        errors
    }
}
