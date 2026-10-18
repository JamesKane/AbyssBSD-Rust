// SPDX-License-Identifier: BSD-2-Clause
//! The device manager: the node table, matching, the dependency graph,
//! attach workers, and the detach protocol (DRIVER-MODEL.md §4–§6, §10).
//!
//! - A node is matched as soon as its bus adds it, and its unit is chosen
//!   then, so names never depend on attach timing.
//! - A matched node whose required suppliers have all attached goes to the
//!   attach workers; otherwise it waits, naming what it waits for. When a
//!   node attaches, only its consumers are looked at again.
//! - The topology lock protects the tree's structure and is never held
//!   across a driver callback: a node in `Attaching` or `Detaching` is
//!   owned by the thread running that transition.
//! - Detach runs the same steps whatever the reason: children first, then
//!   the binding's teardown (`driver::Bound::detach`), then the node goes
//!   back to `NotPresent` with its unit released.
//!
//! Behavior reference: sys/kern/subr_bus.c.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use devtree::graph::{Edge, EdgeKind, Graph, Origin};
use devtree::matching::Rank;
use devtree::units::Units;
use sched::lock::{LockClass, SxLock};
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};

use crate::driver::{AttachCx, Bound, DriverDecl, Faults, Ledger};
use crate::platform::Platform;
use crate::sysctl::DeviceStrings;
use crate::types::{
    AttachError, DetachReason, DeviceId, Ident, Need, NodeState, Resource, ResourceError,
    WaitReason,
};

static TOPOLOGY: LockClass = LockClass::new("bus topology", 300);

struct Node {
    generation: u32,
    parent: Option<u32>,
    children: Vec<u32>,
    ident: Ident,
    bus_data: Option<Arc<dyn Any + Send + Sync>>,
    resources: Vec<Resource>,
    state: NodeState,
    /// The chosen driver (an index into the table).
    driver: Option<usize>,
    /// The class of a device the kernel owns itself ([`Manager::adopt`]).
    adopted: Option<&'static str>,
    unit: Option<u32>,
    desc: String,
    /// Drivers that declined this node.
    declined: Vec<usize>,
    binding: Option<Box<dyn Bound>>,
    /// `dev.<driver>.<unit>`, while attached.
    sysctl: Option<sysctl::NodeId>,
}

impl Node {
    /// The devclass: the driver's name, or an adopted device's class.
    fn class(&self, drivers: &[DriverDecl]) -> Option<&'static str> {
        self.driver.map(|d| drivers[d].name).or(self.adopted)
    }
}

struct Topology {
    nodes: Vec<Option<Node>>,
    free: Vec<(u32, u32)>,
    units: Units,
    graph: Graph<u32>,
}

impl Topology {
    fn node(&self, id: DeviceId) -> Option<&Node> {
        self.nodes
            .get(id.index as usize)?
            .as_ref()
            .filter(|n| n.generation == id.generation)
    }

    fn node_mut(&mut self, id: DeviceId) -> Option<&mut Node> {
        self.nodes
            .get_mut(id.index as usize)?
            .as_mut()
            .filter(|n| n.generation == id.generation)
    }

    fn id(&self, index: u32) -> Option<DeviceId> {
        let n = self.nodes.get(index as usize)?.as_ref()?;
        Some(DeviceId {
            index,
            generation: n.generation,
        })
    }

    fn nameunit(&self, drivers: &[DriverDecl], index: u32) -> String {
        let Some(n) = self.nodes.get(index as usize).and_then(Option::as_ref) else {
            return String::new();
        };
        match (n.class(drivers), n.unit) {
            (Some(c), Some(u)) => alloc::format!("{c}{u}"),
            _ if n.parent.is_none() => String::from("root0"),
            _ => String::new(),
        }
    }

    fn attached(&self, index: u32) -> bool {
        self.nodes
            .get(index as usize)
            .and_then(Option::as_ref)
            .is_some_and(|n| n.state == NodeState::Attached)
    }
}

/// The device manager. There is one, set up by [`init`].
pub struct Manager {
    platform: &'static dyn Platform,
    drivers: &'static [DriverDecl],
    topo: SxLock<Topology>,
    /// Nodes waiting for a worker.
    jobs: SpinLock<VecDeque<DeviceId>>,
    jobs_q: WaitQueue,
    /// Attaches queued or running, for [`Manager::wait_idle`].
    busy: AtomicUsize,
    idle_q: WaitQueue,
    /// Granted memory ranges (base, length).
    claims: SpinLock<Vec<(u64, u64)>>,
    /// devctl event lines, for /dev/devctl (P4), bounded.
    events: SpinLock<VecDeque<String>>,
    /// `hw.bus.info`'s generation: bumped on every visible change.
    generation: AtomicU32,
    /// Verifier mode: fault injection and ledger enforcement.
    faults: SpinLock<Option<Faults>>,
}

impl core::fmt::Debug for Manager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Manager")
            .field("drivers", &self.drivers.len())
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

static MANAGER: Once<Manager> = Once::new();

/// Events kept for /dev/devctl before the oldest are dropped
/// (`hw.bus.devctl_queue`'s default).
const EVENT_QUEUE: usize = 1000;

/// Options for [`init`].
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Attach worker threads (`abyss.bus.serial` forces 1).
    pub workers: usize,
    /// Verifier mode: fail one grant in this many (0: never), with this
    /// seed.
    pub fault_one_in: u64,
    /// The fault generator's seed.
    pub fault_seed: u64,
}

/// Sets up the manager with the build's driver table and starts its
/// workers. Returns the root node.
///
/// # Panics
///
/// If called twice, or if the workers cannot start.
pub fn init(
    s: &Sleepable,
    platform: &'static dyn Platform,
    drivers: &'static [DriverDecl],
    o: Options,
) -> DeviceId {
    let mut topo = Topology {
        nodes: Vec::new(),
        free: Vec::new(),
        units: Units::new(),
        graph: Graph::new(),
    };
    topo.nodes.push(Some(Node {
        generation: 0,
        parent: None,
        children: Vec::new(),
        ident: Ident::Root,
        bus_data: None,
        resources: Vec::new(),
        state: NodeState::Attached,
        driver: None,
        adopted: None,
        unit: None,
        desc: String::from("System root bus"),
        declined: Vec::new(),
        binding: None,
        sysctl: None,
    }));
    topo.graph.add_node(0);
    let faults = (o.fault_one_in > 0).then_some(Faults {
        state: o.fault_seed | 1,
        one_in: o.fault_one_in,
    });
    let ok = MANAGER
        .set(Manager {
            platform,
            drivers,
            topo: SxLock::new(&TOPOLOGY, topo),
            jobs: SpinLock::new(VecDeque::new()),
            jobs_q: WaitQueue::new(),
            busy: AtomicUsize::new(0),
            idle_q: WaitQueue::new(),
            claims: SpinLock::new(Vec::new()),
            events: SpinLock::new(VecDeque::new()),
            generation: AtomicU32::new(1),
            faults: SpinLock::new(faults),
        })
        .is_ok();
    assert!(ok, "bus::init called twice");
    for i in 0..o.workers.max(1) {
        sched::Builder::new("bus-attach")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| {
                let _ = i;
                manager().worker(s);
            })
            .expect("bus attach worker");
    }
    crate::sysctl::register(manager(), s, o.workers.max(1));
    manager().root()
}

/// The manager.
///
/// # Panics
///
/// Before [`init`].
pub fn manager() -> &'static Manager {
    MANAGER.get().expect("bus::init not called")
}

impl Manager {
    pub(crate) fn platform(&self) -> &'static dyn Platform {
        self.platform
    }

    /// The root node.
    #[must_use]
    pub fn root(&self) -> DeviceId {
        DeviceId {
            index: 0,
            generation: 0,
        }
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// `hw.bus.info`'s generation.
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }

    pub(crate) fn claim(&self, base: u64, len: u64) -> Result<(), ResourceError> {
        let mut c = self.claims.lock();
        if c.iter().any(|&(b, l)| base < b + l && b < base + len) {
            return Err(ResourceError::Claimed);
        }
        c.push((base, len));
        Ok(())
    }

    pub(crate) fn release_claim(&self, base: u64) {
        self.claims.lock().retain(|&(b, _)| b != base);
    }

    /// A devctl device event line for node `index` (attached as `what`,
    /// or a nomatch when `what` is `None`).
    fn device_line(&self, t: &Topology, sigil: char, what: &str, index: u32) -> String {
        use devtree::devctl::{DeviceEvent, device_event};
        let Some(n) = t.nodes.get(index as usize).and_then(Option::as_ref) else {
            return String::new();
        };
        let (mut loc, mut pnp) = (String::new(), String::new());
        n.ident.location(&mut loc);
        n.ident.pnpinfo(&mut pnp);
        let parent = n.parent.map(|p| t.nameunit(self.drivers, p));
        let ev = match sigil {
            '+' => DeviceEvent::Attach(what),
            '-' => DeviceEvent::Detach(what),
            _ => DeviceEvent::NoMatch,
        };
        let mut line = String::new();
        let _ = device_event(&mut line, ev, &loc, &pnp, parent.as_deref());
        line
    }

    fn event(&self, line: String) {
        let mut e = self.events.lock();
        if e.len() == EVENT_QUEUE {
            e.pop_front();
        }
        e.push_back(line);
    }

    /// The devctl events so far (oldest first), consuming them.
    pub fn take_events(&self) -> Vec<String> {
        self.events.lock().drain(..).collect()
    }

    /// Adds a device the kernel drives itself (the platform root, the
    /// interrupt controllers it set up at boot) as an attached node of
    /// class `class`, so the tree shows it as FreeBSD's does. It has no
    /// binding and refuses a requested detach.
    pub fn adopt(
        &self,
        s: &Sleepable,
        parent: DeviceId,
        class: &'static str,
        desc: &str,
        ident: Ident,
    ) -> DeviceId {
        let (id, unit, strings) = {
            let mut t = self.topo.write(s);
            let unit = t.units.alloc(class, None).unwrap_or(0);
            let (index, generation) = t
                .free
                .pop()
                .unwrap_or((u32::try_from(t.nodes.len()).expect("node count"), 0));
            let node = Node {
                generation,
                parent: Some(parent.index),
                children: Vec::new(),
                ident,
                bus_data: None,
                resources: Vec::new(),
                state: NodeState::Attached,
                driver: None,
                adopted: Some(class),
                unit: Some(unit),
                desc: String::from(desc),
                declined: Vec::new(),
                binding: None,
                sysctl: None,
            };
            if index as usize == t.nodes.len() {
                t.nodes.push(Some(node));
            } else {
                t.nodes[index as usize] = Some(node);
            }
            if let Some(p) = t.node_mut(parent) {
                p.children.push(index);
            }
            t.graph.add_edge(Edge {
                supplier: parent.index,
                consumer: index,
                kind: EdgeKind::Required,
                origin: Origin::Tree,
            });
            let strings = DeviceStrings {
                desc: String::from(desc),
                driver: class,
                parent: t.nameunit(self.drivers, parent.index),
                ..DeviceStrings::default()
            };
            (DeviceId { index, generation }, unit, strings)
        };
        let node = crate::sysctl::add_device(self, s, unit, strings);
        let mut t = self.topo.write(s);
        if let Some(n) = t.node_mut(id) {
            n.sysctl = node;
        }
        let line = self.device_line(&t, '+', &alloc::format!("{class}{unit}"), id.index);
        drop(t);
        self.event(line);
        self.bump();
        id
    }

    /// Adds a child of `parent` and matches it. `deps` are firmware
    /// suppliers beyond the parent. The caller holds no manager lock.
    pub fn add_child(
        &self,
        s: &Sleepable,
        parent: DeviceId,
        ident: Ident,
        bus_data: Option<Arc<dyn Any + Send + Sync>>,
        resources: Vec<Resource>,
        deps: &[(DeviceId, EdgeKind)],
    ) -> DeviceId {
        let mut t = self.topo.write(s);
        let node = Node {
            generation: 0,
            parent: Some(parent.index),
            children: Vec::new(),
            ident,
            bus_data,
            resources,
            state: NodeState::NotPresent,
            driver: None,
            adopted: None,
            unit: None,
            desc: String::new(),
            declined: Vec::new(),
            binding: None,
            sysctl: None,
        };
        let (index, generation) = match t.free.pop() {
            Some((i, g)) => (i, g),
            None => (u32::try_from(t.nodes.len()).expect("node count"), 0),
        };
        let node = Node { generation, ..node };
        if index as usize == t.nodes.len() {
            t.nodes.push(Some(node));
        } else {
            t.nodes[index as usize] = Some(node);
        }
        if let Some(p) = t.node_mut(parent) {
            p.children.push(index);
        }
        t.graph.add_edge(Edge {
            supplier: parent.index,
            consumer: index,
            kind: EdgeKind::Required,
            origin: Origin::Tree,
        });
        for &(sup, kind) in deps {
            t.graph.add_edge(Edge {
                supplier: sup.index,
                consumer: index,
                kind,
                origin: Origin::Dep,
            });
        }
        let id = DeviceId { index, generation };
        // Nodes that asked for this one by firmware path can now wait on it.
        if let Some(path) = t
            .node(id)
            .and_then(|n| n.ident.acpi_path())
            .map(String::from)
        {
            let waiting: Vec<DeviceId> = (0..u32::try_from(t.nodes.len()).unwrap_or(u32::MAX))
                .filter_map(|i| t.id(i))
                .filter(|&w| {
                    t.node(w).is_some_and(|n| {
                        matches!(&n.state, NodeState::Waiting(WaitReason::Need(Need::Firmware(p))) if *p == path)
                    })
                })
                .collect();
            for w in waiting {
                self.add_driver_edge(&mut t, id, w);
            }
        }
        self.match_node(&mut t, id);
        drop(t);
        self.bump();
        id
    }

    /// Chooses a driver for `id`, allocates its unit, and queues it if its
    /// suppliers are attached. Topology lock held.
    fn match_node(&self, t: &mut Topology, id: DeviceId) {
        let Some(n) = t.node(id) else {
            return;
        };
        let mut best: Option<(usize, Rank)> = None;
        for (i, d) in self.drivers.iter().enumerate() {
            if n.declined.contains(&i) {
                continue;
            }
            let Some(r) = d.ids.rank_for(&n.ident) else {
                continue;
            };
            let better = match best {
                None => true,
                Some((b, br)) => r > br || (r == br && self.drivers[b].yields.contains(&d.name)),
            };
            if better {
                best = Some((i, r));
            }
        }
        let Some((d, _)) = best else {
            // No match: FreeBSD's nomatch event.
            let line = self.device_line(t, '?', "", id.index);
            self.event(line);
            return;
        };
        let decl = &self.drivers[d];
        let unit = t.units.alloc(decl.name, None).unwrap_or(0);
        let desc = (decl.describe)(&t.node(id).map(|n| n.ident.clone()).unwrap_or(Ident::Root));
        let missing = t.graph.missing(id.index, |i| t.attached(i));
        let state = if missing.is_empty() {
            NodeState::Attaching
        } else {
            NodeState::Waiting(WaitReason::Suppliers(
                missing.iter().filter_map(|&i| t.id(i)).collect(),
            ))
        };
        let queue = state == NodeState::Attaching;
        if let Some(n) = t.node_mut(id) {
            n.driver = Some(d);
            n.unit = Some(unit);
            n.desc = desc;
            n.state = state;
        }
        if queue {
            self.queue(id);
        }
    }

    fn queue(&self, id: DeviceId) {
        self.busy.fetch_add(1, Ordering::AcqRel);
        self.jobs.lock().push_back(id);
        self.jobs_q.wake_one();
    }

    fn worker(&'static self, s: &Sleepable) {
        loop {
            let mut job = None;
            self.jobs_q.wait_until(s, || {
                job = self.jobs.lock().pop_front();
                job.is_some()
            });
            if let Some(id) = job {
                self.attach(id, s);
                if self.busy.fetch_sub(1, Ordering::AcqRel) == 1 {
                    self.idle_q.wake_all();
                }
            }
        }
    }

    /// Runs attach and start for `id`, which is `Attaching`.
    fn attach(&'static self, id: DeviceId, s: &Sleepable) {
        // Gather what attach needs without holding the lock across it.
        let (decl_index, ident, resources, bus_data, unit, strings) = {
            let t = self.topo.read(s);
            let Some(n) = t.node(id) else {
                return;
            };
            let (Some(d), Some(u)) = (n.driver, n.unit) else {
                return;
            };
            let mut strings = DeviceStrings {
                desc: n.desc.clone(),
                driver: self.drivers[d].name,
                parent: t.nameunit(self.drivers, n.parent.unwrap_or(0)),
                ..DeviceStrings::default()
            };
            n.ident.location(&mut strings.location);
            n.ident.pnpinfo(&mut strings.pnpinfo);
            (
                d,
                n.ident.clone(),
                n.resources.clone(),
                n.bus_data.clone(),
                u,
                strings,
            )
        };
        let decl = self.drivers[decl_index];
        // As in FreeBSD, the device's sysctl node exists during attach
        // (drivers will add their own nodes under it) and goes if attach
        // fails.
        let node = crate::sysctl::add_device(self, s, unit, strings);
        let ledger = Arc::new(Ledger::default());
        let mut faults = self.faults.lock().take();
        let result = {
            let mut cx = AttachCx {
                mgr: self,
                node: id,
                ident: &ident,
                resources: &resources,
                bus_data,
                unit,
                s,
                ledger,
                faults: faults.as_mut(),
            };
            (decl.attach)(&mut cx)
        };
        if let Some(f) = faults {
            *self.faults.lock() = Some(f);
        }
        let drop_node = || {
            if let Some(n) = node {
                crate::sysctl::remove_device(self, s, n);
            }
        };
        let mut binding = match result {
            Ok(b) => b,
            Err(e) => {
                drop_node();
                self.attach_failed(id, decl_index, e, s);
                return;
            }
        };
        if let Err(e) = binding.start(self, s) {
            let report = binding.detach(DetachReason::StartFailed, s);
            self.check_ledger(decl.name, unit, report.leaked_grants);
            drop_node();
            self.attach_failed(id, decl_index, e, s);
            return;
        }
        let mut t = self.topo.write(s);
        let Some(n) = t.node(id) else {
            return;
        };
        let parent = t.nameunit(self.drivers, n.parent.unwrap_or(0));
        let line = self.device_line(&t, '+', &alloc::format!("{}{unit}", decl.name), id.index);
        if let Some(n) = t.node_mut(id) {
            n.binding = Some(binding);
            n.sysctl = node;
            n.state = NodeState::Attached;
            self.platform.log(format_args!(
                "{}{unit}: <{}> on {parent}",
                decl.name, n.desc
            ));
        }
        // Only this node's consumers can have become ready.
        let consumers: Vec<u32> = t.graph.consumers(id.index).map(|e| e.consumer).collect();
        for c in consumers {
            let Some(cid) = t.id(c) else {
                continue;
            };
            let ready = t.graph.missing(c, |i| t.attached(i)).is_empty();
            if let Some(n) = t.node_mut(cid)
                && matches!(n.state, NodeState::Waiting(_))
                && ready
            {
                n.state = NodeState::Attaching;
                self.queue(cid);
            }
        }
        drop(t);
        self.event(line);
        self.bump();
    }

    fn attach_failed(&self, id: DeviceId, decl_index: usize, e: AttachError, s: &Sleepable) {
        let mut t = self.topo.write(s);
        let name = self.drivers[decl_index].name;
        let Some(n) = t.node_mut(id) else {
            return;
        };
        let unit = n.unit.take();
        n.driver = None;
        match e {
            AttachError::Decline => {
                n.declined.push(decl_index);
                n.state = NodeState::NotPresent;
                if let Some(u) = unit {
                    t.units.release(name, u);
                }
                self.match_node(&mut t, id);
            }
            AttachError::Wait(need) => {
                // Keep the unit: the same driver attaches when the need is
                // met.
                n.driver = Some(decl_index);
                n.unit = unit;
                n.state = NodeState::Waiting(WaitReason::Need(need.clone()));
                // A need that names a node becomes a graph edge, so the
                // node is rechecked when its supplier attaches; a firmware
                // path not in the tree yet is resolved in `add_child`.
                if let Some(sup) = Self::resolve(&t, &need) {
                    self.add_driver_edge(&mut t, sup, id);
                }
            }
            other => {
                self.platform.log(format_args!(
                    "{name}{}: attach failed: {other:?}",
                    unit.unwrap_or(0)
                ));
                n.state = NodeState::Failed(other);
                // FreeBSD frees the unit of a failed attach.
                if let Some(u) = unit {
                    t.units.release(name, u);
                }
            }
        }
        drop(t);
        self.bump();
    }

    /// The node a need names, if it is in the tree.
    fn resolve(t: &Topology, need: &Need) -> Option<DeviceId> {
        match need {
            Need::Device(d) => t.node(*d).map(|_| *d),
            Need::Firmware(path) => t
                .nodes
                .iter()
                .enumerate()
                .find(|(_, n)| {
                    n.as_ref()
                        .is_some_and(|n| n.ident.acpi_path() == Some(path))
                })
                .and_then(|(i, _)| t.id(u32::try_from(i).ok()?)),
        }
    }

    /// Records that `consumer` needs `supplier`, queueing it at once if the
    /// supplier is already attached.
    fn add_driver_edge(&self, t: &mut Topology, supplier: DeviceId, consumer: DeviceId) {
        t.graph.add_edge(Edge {
            supplier: supplier.index,
            consumer: consumer.index,
            kind: EdgeKind::Required,
            origin: Origin::Driver,
        });
        if t.graph
            .missing(consumer.index, |i| t.attached(i))
            .is_empty()
            && let Some(n) = t.node_mut(consumer)
        {
            n.state = NodeState::Attaching;
            self.queue(consumer);
        }
    }

    fn check_ledger(&self, name: &str, unit: u32, leaked: u32) {
        if leaked != 0 {
            self.platform.log(format_args!(
                "{name}{unit}: detach left {leaked} grant(s) outstanding (a driver bug)"
            ));
            debug_assert_eq!(leaked, 0, "{name}{unit} leaked grants");
        }
    }

    /// Sets verifier-mode fault injection: one grant in `one_in` fails
    /// (0 turns it off), from a generator seeded with `seed`.
    pub fn inject_faults(&self, one_in: u64, seed: u64) {
        *self.faults.lock() = (one_in > 0).then_some(Faults {
            state: seed | 1,
            one_in,
        });
    }

    /// Memory ranges currently granted to drivers.
    #[must_use]
    pub fn claims(&self) -> usize {
        self.claims.lock().len()
    }

    /// Waits until no attach is queued or running.
    pub fn wait_idle(&self, s: &Sleepable) {
        self.idle_q
            .wait_until(s, || self.busy.load(Ordering::Acquire) == 0);
    }

    /// Detaches `id` (and its children first), returning it to
    /// `NotPresent`. A voluntary detach (`Requested`) asks the driver
    /// first.
    pub fn detach(
        &self,
        id: DeviceId,
        why: DetachReason,
        s: &Sleepable,
    ) -> Result<(), DetachError> {
        // Take the binding out under the lock; the node becomes ours.
        let (binding, children, name, unit) = {
            let mut t = self.topo.write(s);
            let Some(n) = t.node(id) else {
                return Err(DetachError::NoSuchDevice);
            };
            if n.state != NodeState::Attached {
                return Err(DetachError::NotAttached);
            }
            if why == DetachReason::Requested
                && (n.adopted.is_some()
                    || n.binding.as_ref().is_some_and(|b| b.may_detach().is_err()))
            {
                return Err(DetachError::Busy);
            }
            let unit = n.unit.unwrap_or(0);
            let name = n.class(self.drivers).unwrap_or("");
            let mut children: Vec<DeviceId> = n.children.iter().filter_map(|&c| t.id(c)).collect();
            let n = t.node_mut(id).expect("checked above");
            n.state = NodeState::Detaching(why);
            let binding = n.binding.take();
            // Graph consumers detach before their suppliers too.
            for e in t.graph.consumers(id.index) {
                if e.kind == EdgeKind::Required
                    && let Some(c) = t.id(e.consumer)
                    && !children.contains(&c)
                {
                    children.push(c);
                }
            }
            (binding, children, name, unit)
        };
        // 2. Children and consumers first, newest first.
        for c in children.into_iter().rev() {
            let _ = self.detach(c, DetachReason::Parent, s);
        }
        // 3-7. The binding's own teardown.
        let mut created = Vec::new();
        if let Some(b) = binding {
            created = b.children();
            let report = b.detach(why, s);
            self.check_ledger(name, unit, report.leaked_grants);
        }
        // Children a bus driver created go with it: their bus is gone.
        for c in created {
            self.delete(c, s);
        }
        // 8. Release.
        let mut t = self.topo.write(s);
        let line = t
            .node(id)
            .map(|_| self.device_line(&t, '-', &alloc::format!("{name}{unit}"), id.index));
        let mut node = None;
        if let Some(n) = t.node_mut(id) {
            n.state = NodeState::NotPresent;
            n.driver = None;
            n.unit = None;
            node = n.sysctl.take();
        }
        t.units.release(name, unit);
        drop(t);
        if let Some(n) = node {
            crate::sysctl::remove_device(self, s, n);
        }
        if let Some(l) = line {
            self.event(l);
        }
        self.bump();
        Ok(())
    }

    /// Removes a node that is not attached (and its subtree) from the tree.
    fn delete(&self, id: DeviceId, s: &Sleepable) {
        let mut t = self.topo.write(s);
        let mut stack = alloc::vec![id];
        while let Some(id) = stack.pop() {
            let Some(n) = t.node(id) else {
                continue;
            };
            if matches!(
                n.state,
                NodeState::Attached | NodeState::Attaching | NodeState::Detaching(_)
            ) {
                continue;
            }
            let children: Vec<DeviceId> = n.children.iter().filter_map(|&c| t.id(c)).collect();
            stack.extend(children);
            let parent = n.parent;
            if let (Some(c), Some(u)) = (n.class(self.drivers), n.unit) {
                t.units.release(c, u);
            }
            if let Some(p) = parent.and_then(|p| t.id(p))
                && let Some(pn) = t.node_mut(p)
            {
                pn.children.retain(|&c| c != id.index);
            }
            t.graph.remove_node(id.index);
            t.nodes[id.index as usize] = None;
            t.free.push((id.index, id.generation.wrapping_add(1)));
        }
        drop(t);
        self.bump();
    }

    /// Detaches every attached node, consumers before suppliers, for
    /// shutdown, so devices stop DMA before firmware takes over.
    pub fn shutdown(&self, s: &Sleepable) {
        let order: Vec<DeviceId> = {
            let t = self.topo.read(s);
            let mut g = t.graph.clone();
            let _ = g.break_cycles();
            g.shutdown_order()
                .unwrap_or_default()
                .into_iter()
                .filter(|&i| i != 0 && t.attached(i))
                // Adopted devices are the kernel's: they outlive the drivers.
                .filter(|&i| {
                    t.nodes[i as usize]
                        .as_ref()
                        .is_some_and(|n| n.adopted.is_none())
                })
                .filter_map(|i| t.id(i))
                .collect()
        };
        for id in order {
            let _ = self.detach(id, DetachReason::Shutdown, s);
        }
    }

    /// The `index`th device's `hw.bus.devices` record, in node-table
    /// order (any order serves: `devinfo` links records by handle).
    pub(crate) fn udevice(&self, s: &Sleepable, index: usize) -> Option<abi::bus::UDevice> {
        use abi::bus::{df, ds};
        use devtree::udevice::{Fields, encode};
        let t = self.topo.read(s);
        let (i, n) = t
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| Some((i, n.as_ref()?)))
            .nth(index)?;
        let i = u32::try_from(i).ok()?;
        let handle = |i: u32| {
            t.nodes[i as usize].as_ref().map_or(0, |n| {
                (usize::try_from(n.generation).unwrap_or(0) << 32) | (i as usize + 1)
            })
        };
        let root = n.parent.is_none();
        let nameunit = t.nameunit(self.drivers, i);
        let driver = match n.class(self.drivers) {
            Some(c) => c,
            None if root => "root",
            None => "",
        };
        let (mut loc, mut pnp) = (String::new(), String::new());
        n.ident.location(&mut loc);
        n.ident.pnpinfo(&mut pnp);
        let state = if root {
            ds::ATTACHED
        } else {
            n.state.freebsd()
        };
        let mut flags = df::ENABLED;
        if state == ds::ATTACHED {
            flags |= df::ATTACHED_ONCE;
        }
        let f = Fields {
            nameunit: &nameunit,
            desc: &n.desc,
            driver,
            pnpinfo: &pnp,
            location: &loc,
        };
        encode(handle(i), n.parent.map_or(0, handle), 0, flags, state, &f).ok()
    }

    /// The state of `id`.
    #[must_use]
    pub fn state(&self, id: DeviceId, s: &Sleepable) -> Option<NodeState> {
        Some(self.topo.read(s).node(id)?.state.clone())
    }

    /// `id`'s name with unit (`nvme0`), if it has a driver.
    #[must_use]
    pub fn nameunit(&self, id: DeviceId, s: &Sleepable) -> Option<String> {
        let t = self.topo.read(s);
        t.node(id)?;
        let n = t.nameunit(self.drivers, id.index);
        (!n.is_empty()).then_some(n)
    }

    /// Every node in tree order: (id, depth, name, description, state).
    #[must_use]
    pub fn snapshot(&self, s: &Sleepable) -> Vec<(DeviceId, usize, String, String, NodeState)> {
        let t = self.topo.read(s);
        let mut out = Vec::new();
        let mut stack = alloc::vec![(0u32, 0usize)];
        while let Some((i, depth)) = stack.pop() {
            let Some(n) = t.nodes.get(i as usize).and_then(Option::as_ref) else {
                continue;
            };
            if let Some(id) = t.id(i) {
                out.push((
                    id,
                    depth,
                    t.nameunit(self.drivers, i),
                    n.desc.clone(),
                    n.state.clone(),
                ));
            }
            for &c in n.children.iter().rev() {
                stack.push((c, depth + 1));
            }
        }
        out
    }
}

/// Why a detach was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachError {
    /// No such node.
    NoSuchDevice,
    /// It is not attached.
    NotAttached,
    /// The driver refused a voluntary detach (`EBUSY`).
    Busy,
}
