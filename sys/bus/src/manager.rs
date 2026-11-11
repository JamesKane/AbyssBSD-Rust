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

use devtree::graph::{Edge, EdgeKind, Graph, Origin, Supply};
use devtree::matching::Rank;
use devtree::units::Units;
use sched::lock::{LockClass, SxLock};
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};

use crate::driver::{AttachCx, Bound, DriverDecl, Faults, Ledger};
use crate::gpio::GpioSource;
use crate::i2c::I2cSource;
use crate::mailbox::MailboxSource;
use crate::platform::Platform;
use crate::provider::{ClockSource, DomainSource, ResetSource};
use crate::space::{Range, ResourceSpace, Sharing};
use crate::sysctl::DeviceStrings;
use crate::types::{
    Announce, AttachError, DetachReason, DeviceId, Ident, Line, LineKind, Need, NodeState,
    Resource, ResourceError, WaitReason,
};

mod devctl;

pub use devctl::DEFAULT_QUEUE as DEVCTL_DEFAULT_QUEUE;

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
    /// Where an `Attaching` node is between attach and start.
    start: StartPhase,
    /// Disabled (FreeBSD's `device_disable`): named, never attached.
    disabled: Option<&'static str>,
    /// The one driver it may have (`devctl set driver`: FreeBSD's
    /// `DF_FIXEDCLASS` devclass).
    fixed: Option<usize>,
    /// Its nomatch event went out (FreeBSD's `DF_DONENOMATCH`).
    nomatch_sent: bool,
    /// The clock, reset, and power-domain lines its bus read from
    /// firmware.
    lines: Vec<Line>,
    /// The providers its driver published.
    provides: Provides,
    /// The DMA limits its bus puts on it and its descendants (ACPI
    /// `_CCA`), if any.
    dma: Option<vm::dma::DmaConstraints>,
    /// The IOMMU translation its bus records for it and its descendants
    /// (IORT), if any.
    translation: Option<crate::iommu::Translation>,
    /// Its requester ID, for a PCI function (`%iommu`).
    requester: Option<u16>,
    /// Its bus's hook for a translated context of that requester.
    requester_hook: Option<Arc<dyn crate::iommu::RequesterHook>>,
}

/// The providers a node's driver published (DRIVER-MODEL.md §9.3).
#[derive(Default)]
struct Provides {
    clocks: Option<Arc<ClockSource>>,
    resets: Option<Arc<ResetSource>>,
    domains: Option<Arc<DomainSource>>,
    gpio: Option<Arc<GpioSource>>,
    i2c: Option<Arc<I2cSource>>,
    mailbox: Option<Arc<MailboxSource>>,
    iommu: Option<Arc<crate::iommu::IommuSource>>,
}

impl Provides {
    fn revoke(&self) {
        if let Some(g) = &self.gpio {
            g.revoke();
        }
        if let Some(i) = &self.i2c {
            i.revoke();
        }
        if let Some(m) = &self.mailbox {
            m.revoke();
        }
        if let Some(c) = &self.clocks {
            c.revoke();
        }
        if let Some(r) = &self.resets {
            r.revoke();
        }
        if let Some(d) = &self.domains {
            d.revoke();
        }
        if let Some(u) = &self.iommu {
            u.revoke();
        }
    }
}

/// A published provider, as [`Manager::provide`] takes it.
pub(crate) enum Provided {
    Clocks(Arc<ClockSource>),
    Resets(Arc<ResetSource>),
    Domains(Arc<DomainSource>),
    Gpio(Arc<GpioSource>),
    I2c(Arc<I2cSource>),
    Mailbox(Arc<MailboxSource>),
    Iommu(Arc<crate::iommu::IommuSource>),
}

/// Starts run in tree order (DRIVER-MODEL.md §4.3): a bus adds children
/// in `start`, and they are matched (their units chosen) as they are
/// added, so starting in depth-first order gives every device the unit
/// FreeBSD's serial attach would. Attaches still run in parallel.
#[derive(Default)]
enum StartPhase {
    /// Attach queued or running.
    #[default]
    Attach,
    /// Waiting for its turn to take its unit (`Manager::release_starts`);
    /// its attach is not queued yet.
    Turn,
    /// Attached, waiting for every earlier node to start.
    Parked(Box<dyn Bound>, Option<sysctl::NodeId>),
    /// Start queued or running.
    Starting,
}

/// A worker's job.
#[derive(Clone, Copy, Debug)]
enum Job {
    Attach(DeviceId),
    Start(DeviceId),
}

impl Node {
    /// The devclass: the driver's name, or an adopted device's class.
    fn class(&self, drivers: &[DriverDecl]) -> Option<&'static str> {
        self.driver
            .map(|d| drivers[d].name)
            .or(self.adopted)
            .or(self.disabled)
    }
}

struct Topology {
    nodes: Vec<Option<Node>>,
    free: Vec<(u32, u32)>,
    units: Units,
    graph: Graph<u32>,
    /// Firmware dependencies of namespace objects no node claims yet,
    /// by the consumer's path: a PCI function's ACPI companion
    /// (DRIVER-MODEL.md §5.2), applied when the PCI bus adds the function.
    companion_deps: Vec<CompanionDep>,
}

/// A dependency of a namespace object that another bus's node stands
/// for.
struct CompanionDep {
    consumer: String,
    supplier: DeviceId,
    kind: EdgeKind,
    origin: Origin,
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

    /// Where node `index` is, for the edges to its consumers: attached;
    /// settled without attaching (its attach failed, no driver matched
    /// it, it is disabled, or it is gone); or pending.
    fn supply(&self, index: u32) -> Supply {
        let Some(n) = self.nodes.get(index as usize).and_then(Option::as_ref) else {
            return Supply::Settled;
        };
        match n.state {
            NodeState::Attached => Supply::Attached,
            NodeState::Failed(_) | NodeState::Gone => Supply::Settled,
            NodeState::NotPresent if n.driver.is_none() => Supply::Settled,
            _ => Supply::Pending,
        }
    }
}

/// The device manager. There is one, set up by [`init`].
pub struct Manager {
    platform: &'static dyn Platform,
    /// [`Options::disabled_hint`].
    disabled_hint: fn(&str, u32) -> bool,
    drivers: &'static [DriverDecl],
    topo: SxLock<Topology>,
    /// Nodes waiting for a worker.
    jobs: SpinLock<VecDeque<Job>>,
    jobs_q: WaitQueue,
    /// Attaches queued or running, for [`Manager::wait_idle`].
    busy: AtomicUsize,
    /// Bumped as each job ends, for [`Manager::wait_settled`].
    jobs_done: AtomicU32,
    idle_q: WaitQueue,
    /// Granted ranges, and the ranges firmware or another device shares.
    space: SpinLock<ResourceSpace>,
    /// devctl event lines, for /dev/devctl, bounded.
    events: SpinLock<devctl::Events>,
    /// Called after a line is queued: /dev/devctl's wakeup.
    on_event: Once<fn()>,
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

/// The manager, if [`init`] has run (gates work without it, in host tests).
pub(crate) fn try_manager() -> Option<&'static Manager> {
    MANAGER.get()
}

/// Sleeps `ms` through the manager's platform if there is one; returns
/// the milliseconds slept (0 without a manager, so callers keep counting
/// iterations instead).
pub(crate) fn try_sleep_ms(s: &Sleepable, ms: u64) -> u64 {
    match MANAGER.get() {
        Some(m) => {
            m.sleep_ms(s, ms);
            ms
        }
        None => 0,
    }
}

/// Sleeps `ns` through the manager's platform if there is one (without
/// one, on the host, there is no hardware to wait for).
pub(crate) fn try_sleep_ns(s: &Sleepable, ns: u64) {
    if let Some(m) = MANAGER.get() {
        m.platform().sleep_ns(s, ns);
    }
}

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
    /// devctl events kept before the oldest are dropped, 0 for none
    /// (the `hw.bus.devctl_queue` tunable; FreeBSD's default is
    /// [`DEVCTL_DEFAULT_QUEUE`]).
    pub devctl_queue: usize,
    /// Nomatch events are queued (the `hw.bus.devctl_nomatch_enabled`
    /// tunable; on by default).
    pub devctl_nomatch: bool,
    /// Whether the hints disable driver `name`'s unit `unit`
    /// (`hint.<name>.<unit>.disabled`, FreeBSD's `resource_disabled`).
    pub disabled_hint: fn(name: &str, unit: u32) -> bool,
}

/// No device is disabled by hints.
#[must_use]
pub fn no_hints(_: &str, _: u32) -> bool {
    false
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
        companion_deps: Vec::new(),
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
        start: StartPhase::Attach,
        disabled: None,
        fixed: None,
        nomatch_sent: false,
        lines: Vec::new(),
        provides: Provides::default(),
        dma: None,
        translation: None,
        requester: None,
        requester_hook: None,
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
            jobs_done: AtomicU32::new(0),
            idle_q: WaitQueue::new(),
            space: SpinLock::new(ResourceSpace::new()),
            events: SpinLock::new(devctl::Events::new(o.devctl_queue, o.devctl_nomatch)),
            on_event: Once::new(),
            generation: AtomicU32::new(1),
            faults: SpinLock::new(faults),
            disabled_hint: o.disabled_hint,
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
    /// Sleeps for `ms` milliseconds: a driver's bounded wait for hardware
    /// (a controller reset) in attach or stop.
    pub fn sleep_ms(&self, s: &Sleepable, ms: u64) {
        self.platform.sleep_ns(s, ms.saturating_mul(1_000_000));
    }

    /// Sleeps on `q` until `cond` holds or `ns` nanoseconds pass: a
    /// driver waiting for its interrupt with a timeout (FreeBSD's
    /// `msleep(..., timo)`). Whether `cond` held.
    pub fn wait_timeout(
        &self,
        s: &Sleepable,
        q: &sched::WaitQueue,
        cond: &mut dyn FnMut() -> bool,
        ns: u64,
    ) -> bool {
        self.platform.wait_timeout(s, q, cond, ns)
    }

    /// Busy-waits for `us` microseconds (FreeBSD's `DELAY`): a register
    /// protocol's short polled wait, in any context.
    pub fn delay_us(&self, us: u64) {
        self.platform.delay_us(us);
    }

    /// Writes a console line (attach announcements, driver messages).
    pub fn log(&self, args: core::fmt::Arguments<'_>) {
        self.platform.log(args);
    }

    /// Runs `f` on the sysctl tree, under its lock: a class's or
    /// driver's own nodes (`kern.evdev`).
    pub fn sysctl(&self, s: &Sleepable, f: &mut dyn FnMut(&mut sysctl::Tree)) {
        self.platform.sysctl(s, f);
    }

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

    /// Grants `r` (DRIVER-MODEL.md §8.1): refused if another grant holds
    /// part of it outside a declared-shared range, or if AML touches it
    /// and the driver did not ask knowingly (`firmware_ok`).
    pub(crate) fn claim(&self, r: Range, firmware_ok: bool) -> Result<Sharing, ResourceError> {
        self.space.lock().claim(r, firmware_ok)
    }

    pub(crate) fn release_claim(&self, r: Range) {
        self.space.lock().release(r);
    }

    /// Records a range AML reads or writes (an operation region), before
    /// any child that might be granted it attaches.
    pub(crate) fn share_with_firmware(&self, by: DeviceId, r: Range) {
        self.space.lock().share_with_firmware(by, r);
    }

    /// Records a range the platform description lets two devices' grants
    /// overlap in.
    pub(crate) fn declare_shared(&self, by: DeviceId, r: Range) {
        self.space.lock().declare_shared(by, r);
    }

    /// The firmware-shared ranges and the declared-shared ones.
    #[must_use]
    pub fn shared_ranges(&self) -> (Vec<Range>, Vec<Range>) {
        self.space.lock().shared()
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
                start: StartPhase::Attach,
                disabled: None,
                fixed: None,
                nomatch_sent: false,
                lines: Vec::new(),
                provides: Provides::default(),
                dma: None,
                translation: None,
                requester: None,
                requester_hook: None,
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
            start: StartPhase::Attach,
            disabled: None,
            fixed: None,
            nomatch_sent: false,
            lines: Vec::new(),
            provides: Provides::default(),
            dma: None,
            translation: None,
            requester: None,
            requester_hook: None,
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
            // Its namespace object's own dependencies apply to it.
            let mine: Vec<(DeviceId, EdgeKind, Origin)> = t
                .companion_deps
                .iter()
                .filter(|d| d.consumer == path)
                .map(|d| (d.supplier, d.kind, d.origin))
                .collect();
            for (sup, kind, origin) in mine {
                if t.node(sup).is_some() && sup != id {
                    t.graph.add_edge(Edge {
                        supplier: sup.index,
                        consumer: index,
                        kind,
                        origin,
                    });
                }
            }
            self.break_cycles(&mut t);
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

    /// Moves the node standing for the namespace object at `path`, a child
    /// of a node of class `from_class` that has not attached, under
    /// `parent` with identity `ident(old)`: FreeBSD's `acpi_iicbus`
    /// deleting `acpi0`'s child for a device on its bus and adding its
    /// own. The node is deleted and a new one added, as there, so its id
    /// changes and it is matched again; its bus data, resources, lines,
    /// DMA limits, and firmware dependencies both ways go with it.
    /// `None` if there is no such node, or it is attached or attaching
    /// (FreeBSD leaves an attached child where it is), or `ident`
    /// declines it.
    pub(crate) fn take_child(
        &self,
        s: &Sleepable,
        path: &str,
        from_class: &str,
        parent: DeviceId,
        ident: &dyn Fn(&Ident) -> Option<Ident>,
    ) -> Option<DeviceId> {
        let mut t = self.topo.write(s);
        t.node(parent)?;
        let n = u32::try_from(t.nodes.len()).unwrap_or(u32::MAX);
        let old = (0..n)
            .filter_map(|i| t.id(i))
            .find(|&id| t.node(id).and_then(|n| n.ident.acpi_path()) == Some(path))?;
        let o = t.node(old)?;
        if !matches!(
            o.state,
            NodeState::NotPresent | NodeState::Waiting(_) | NodeState::Failed(_)
        ) || o.adopted.is_some()
            || o.disabled.is_some()
            || !o.children.is_empty()
        {
            return None;
        }
        let from = o.parent.and_then(|p| t.nodes.get(p as usize)?.as_ref())?;
        if from.class(self.drivers) != Some(from_class) {
            return None;
        }
        let new_ident = ident(&o.ident)?;
        // Its firmware edges, both ways, but for the tree's.
        let edges: Vec<Edge<u32>> = t
            .graph
            .edges()
            .filter(|e| {
                e.origin != Origin::Tree && (e.consumer == old.index || e.supplier == old.index)
            })
            .copied()
            .collect();
        // Take it out, as `delete` does.
        let o = t.nodes[old.index as usize].take()?;
        if let (Some(c), Some(u)) = (o.class(self.drivers), o.unit) {
            t.units.release(c, u);
        }
        if let Some(pn) = o.parent.and_then(|p| t.nodes.get_mut(p as usize)?.as_mut()) {
            pn.children.retain(|&c| c != old.index);
        }
        t.graph.remove_node(old.index);
        t.free.push((old.index, old.generation.wrapping_add(1)));
        // And in again under `parent`.
        let (index, generation) = match t.free.pop() {
            Some((i, g)) => (i, g),
            None => (u32::try_from(t.nodes.len()).expect("node count"), 0),
        };
        let node = Node {
            generation,
            parent: Some(parent.index),
            children: Vec::new(),
            ident: new_ident,
            bus_data: o.bus_data,
            resources: o.resources,
            state: NodeState::NotPresent,
            driver: None,
            adopted: None,
            unit: None,
            desc: String::new(),
            declined: Vec::new(),
            binding: None,
            sysctl: None,
            start: StartPhase::Attach,
            disabled: None,
            fixed: None,
            nomatch_sent: false,
            lines: o.lines,
            provides: Provides::default(),
            dma: o.dma,
            translation: o.translation,
            requester: o.requester,
            requester_hook: o.requester_hook,
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
        let moved = |i: u32| if i == old.index { index } else { i };
        for e in edges {
            t.graph.add_edge(Edge {
                supplier: moved(e.supplier),
                consumer: moved(e.consumer),
                ..e
            });
        }
        let id = DeviceId { index, generation };
        // What named the old node by id names the new one.
        for d in &mut t.companion_deps {
            if d.supplier == old {
                d.supplier = id;
            }
        }
        for n in t.nodes.iter_mut().flatten() {
            for l in &mut n.lines {
                if l.supplier == old {
                    l.supplier = id;
                }
            }
        }
        self.break_cycles(&mut t);
        self.match_node(&mut t, id);
        drop(t);
        self.bump();
        Some(id)
    }

    /// `id`'s parent.
    #[must_use]
    pub fn parent(&self, id: DeviceId, s: &Sleepable) -> Option<DeviceId> {
        let t = self.topo.read(s);
        let p = t.node(id)?.parent?;
        t.id(p)
    }

    /// Records that `consumer` also depends on `supplier`: a firmware
    /// dependency found once both exist (a forward `_DEP`). A consumer
    /// already attaching is not held back. An edge that closes a cycle is
    /// broken again, as firmware cycles are, and reported if the consumer
    /// waited on it (optional and order edges close cycles by design: a
    /// media pipeline's links run both ways).
    pub(crate) fn add_dependency(
        &self,
        s: &Sleepable,
        consumer: DeviceId,
        supplier: DeviceId,
        kind: EdgeKind,
        origin: Origin,
    ) {
        let mut t = self.topo.write(s);
        if consumer == supplier || t.node(consumer).is_none() || t.node(supplier).is_none() {
            return;
        }
        t.graph.add_edge(Edge {
            supplier: supplier.index,
            consumer: consumer.index,
            kind,
            origin,
        });
        self.break_cycles(&mut t);
        let missing: Vec<DeviceId> = t
            .graph
            .missing(consumer.index, |i| t.supply(i))
            .iter()
            .filter_map(|&i| t.id(i))
            .collect();
        if let Some(n) = t.node_mut(consumer)
            && let NodeState::Waiting(WaitReason::Suppliers(list)) = &mut n.state
        {
            *list = missing;
        }
        drop(t);
        self.bump();
    }

    /// Breaks the graph's cycles, as firmware cycles are, reporting each
    /// edge a consumer waited on.
    fn break_cycles(&self, t: &mut Topology) {
        if let Ok(broken) = t.graph.break_cycles() {
            for e in broken.into_iter().filter(|e| e.kind.waits()) {
                let (a, b) = (
                    t.nameunit(self.drivers, e.supplier),
                    t.nameunit(self.drivers, e.consumer),
                );
                self.platform.log(format_args!(
                    "bus: dependency cycle: {b} no longer waits for {a}"
                ));
            }
        }
    }

    /// Records that whatever node stands for the namespace object at
    /// `consumer` (a PCI function whose ACPI companion it is) depends on
    /// `supplier`: applied now if such a node exists, else when one is
    /// added.
    pub(crate) fn add_companion_dependency(
        &self,
        s: &Sleepable,
        consumer: String,
        supplier: DeviceId,
        kind: EdgeKind,
        origin: Origin,
    ) {
        let existing = {
            let mut t = self.topo.write(s);
            let n = u32::try_from(t.nodes.len()).unwrap_or(u32::MAX);
            let found = (0..n)
                .filter_map(|i| t.id(i))
                .find(|&id| t.node(id).and_then(|n| n.ident.acpi_path()) == Some(&consumer));
            t.companion_deps.push(CompanionDep {
                consumer,
                supplier,
                kind,
                origin,
            });
            found
        };
        if let Some(c) = existing {
            self.add_dependency(s, c, supplier, kind, origin);
        }
    }

    /// Sets the clock, reset, and power-domain lines `id` consumes.
    pub(crate) fn set_lines(&self, s: &Sleepable, id: DeviceId, lines: Vec<Line>) {
        if let Some(n) = self.topo.write(s).node_mut(id) {
            n.lines = lines;
        }
    }

    /// Records the DMA limits `id`'s bus puts on it.
    pub(crate) fn set_dma(&self, s: &Sleepable, id: DeviceId, c: vm::dma::DmaConstraints) {
        if let Some(n) = self.topo.write(s).node_mut(id) {
            n.dma = Some(c);
        }
    }

    /// Records the IOMMU translation `id`'s bus gives it and its
    /// descendants.
    pub(crate) fn set_translation(
        &self,
        s: &Sleepable,
        id: DeviceId,
        tr: crate::iommu::Translation,
    ) {
        if let Some(n) = self.topo.write(s).node_mut(id) {
            n.translation = Some(tr);
        }
    }

    /// Records `id`'s requester ID (a PCI function's, by FreeBSD's
    /// requester walk), which its `%iommu` shows.
    pub(crate) fn set_requester(&self, s: &Sleepable, id: DeviceId, rid: u16) {
        if let Some(n) = self.topo.write(s).node_mut(id) {
            n.requester = Some(rid);
        }
    }

    /// `id`'s requester ID, if its bus found one.
    #[must_use]
    pub fn requester_id(&self, s: &Sleepable, id: DeviceId) -> Option<u16> {
        self.topo.read(s).node(id)?.requester
    }

    /// Records the hook `id`'s bus wants run when its requester first gets
    /// a translated context.
    pub(crate) fn set_requester_hook(
        &self,
        s: &Sleepable,
        id: DeviceId,
        hook: Arc<dyn crate::iommu::RequesterHook>,
    ) {
        if let Some(n) = self.topo.write(s).node_mut(id) {
            n.requester_hook = Some(hook);
        }
    }

    /// Gives `id`'s requester its context on the IOMMU unit that
    /// translates it, if one does and has published its provider
    /// (docs/IOMMU.md §4.4), and returns it. `None` for a node without a
    /// stream (no translation, no requester ID, or an ID the root complex
    /// does not map to the unit) or whose unit is not running. When the
    /// context translates, the requester's bus hook runs, and its report
    /// is printed under the requester's name (the board port's `relaxed
    /// ordering disabled (IOMMU translation)`).
    ///
    /// # Errors
    ///
    /// The unit's, when it could not make the context.
    pub fn iommu_context(
        &self,
        s: &Sleepable,
        id: DeviceId,
        want: crate::iommu::Want,
    ) -> Result<Option<crate::iommu::Context>, crate::iommu::IommuError> {
        let Some(tr) = self.dma_translation(s, id) else {
            return Ok(None);
        };
        // A device below a PCI function (a virtio device on its transport,
        // a USB device) does its DMA as that function: the nearest
        // requester ID up the tree is its (FreeBSD's `iommu_get_requester`
        // starts from the PCI device too).
        let found = {
            let t = self.topo.read(s);
            let mut at = t.node(id).map(|n| (id, n));
            let mut found = None;
            while let Some((nid, n)) = at {
                if let Some(r) = n.requester {
                    found = Some((r, nid, n.requester_hook.clone()));
                    break;
                }
                at = n.parent.and_then(|p| {
                    let pn = t.nodes.get(p as usize)?.as_ref()?;
                    Some((
                        DeviceId {
                            index: p,
                            generation: pn.generation,
                        },
                        pn,
                    ))
                });
            }
            found
        };
        let Some((rid, holder, hook)) = found else {
            return Ok(None);
        };
        let Some(stream) = tr.stream(rid) else {
            return Ok(None);
        };
        let unit = self
            .topo
            .read(s)
            .node(tr.unit())
            .and_then(|n| n.provides.iommu.clone());
        let Some(unit) = unit else {
            return Ok(None);
        };
        let segment = tr.segment().unwrap_or(0);
        let name = self.nameunit(holder, s).unwrap_or_else(|| {
            alloc::format!(
                "pci{segment}:{}:{}:{}",
                rid >> 8,
                (rid >> 3) & 0x1f,
                rid & 7
            )
        });
        let req = crate::iommu::ContextRequest {
            want,
            name: &name,
            rid,
            segment,
            coherent: self.dma_limits(s, id).coherency == vm::dma::Coherency::Coherent,
            reserved: tr.windows(),
        };
        let ctx = unit.context(stream, &req)?;
        if matches!(ctx, crate::iommu::Context::Translated(_))
            && let Some(h) = hook
            && h.translated()
        {
            self.log(format_args!(
                "{name}: relaxed ordering disabled (IOMMU translation)"
            ));
        }
        Ok(Some(ctx))
    }

    /// The IOMMU translation that applies to `id`: its own record or its
    /// nearest ancestor's (docs/IOMMU.md §4.4).
    #[must_use]
    pub fn dma_translation(
        &self,
        s: &Sleepable,
        id: DeviceId,
    ) -> Option<crate::iommu::Translation> {
        let t = self.topo.read(s);
        let mut at = t.node(id);
        while let Some(n) = at {
            if let Some(tr) = &n.translation {
                return Some(tr.clone());
            }
            at = n
                .parent
                .and_then(|p| t.nodes.get(p as usize))
                .and_then(Option::as_ref);
        }
        None
    }

    /// The DMA limits on `id`: those of every bus between it and the root,
    /// combined (FreeBSD's `bus_get_dma_tag` up the tree).
    pub(crate) fn dma_limits(&self, s: &Sleepable, id: DeviceId) -> vm::dma::DmaConstraints {
        let t = self.topo.read(s);
        let mut c = vm::dma::DmaConstraints::UNLIMITED;
        let mut at = t.node(id);
        while let Some(n) = at {
            if let Some(d) = &n.dma {
                c = c.intersect(d);
            }
            at = n
                .parent
                .and_then(|p| t.nodes.get(p as usize))
                .and_then(Option::as_ref);
        }
        c
    }

    /// The lines `id` consumes.
    pub(crate) fn lines(&self, s: &Sleepable, id: DeviceId) -> Vec<Line> {
        self.topo
            .read(s)
            .node(id)
            .map(|n| n.lines.clone())
            .unwrap_or_default()
    }

    /// Publishes a provider for `id`, replacing one of the same kind.
    pub(crate) fn provide(&self, s: &Sleepable, id: DeviceId, p: Provided) {
        let mut t = self.topo.write(s);
        let Some(n) = t.node_mut(id) else {
            return;
        };
        let old = match p {
            Provided::Clocks(c) => n.provides.clocks.replace(c).map(Provided::Clocks),
            Provided::Resets(r) => n.provides.resets.replace(r).map(Provided::Resets),
            Provided::Domains(d) => n.provides.domains.replace(d).map(Provided::Domains),
            Provided::Gpio(g) => n.provides.gpio.replace(g).map(Provided::Gpio),
            Provided::I2c(i) => n.provides.i2c.replace(i).map(Provided::I2c),
            Provided::Mailbox(m) => n.provides.mailbox.replace(m).map(Provided::Mailbox),
            Provided::Iommu(u) => n.provides.iommu.replace(u).map(Provided::Iommu),
        };
        match old {
            Some(Provided::Clocks(c)) => c.revoke(),
            Some(Provided::Resets(r)) => r.revoke(),
            Some(Provided::Domains(d)) => d.revoke(),
            Some(Provided::Gpio(g)) => g.revoke(),
            Some(Provided::I2c(i)) => i.revoke(),
            Some(Provided::Mailbox(m)) => m.revoke(),
            Some(Provided::Iommu(u)) => u.revoke(),
            None => {}
        }
    }

    /// The provider of `kind` that `supplier` published.
    pub(crate) fn provider(
        &self,
        s: &Sleepable,
        supplier: DeviceId,
        kind: LineKind,
    ) -> Result<Provided, ResourceError> {
        let t = self.topo.read(s);
        let p = t.node(supplier).map(|n| &n.provides);
        let found = match kind {
            LineKind::Clock => p.and_then(|p| p.clocks.clone()).map(Provided::Clocks),
            LineKind::Reset => p.and_then(|p| p.resets.clone()).map(Provided::Resets),
            LineKind::PowerDomain => p.and_then(|p| p.domains.clone()).map(Provided::Domains),
            LineKind::Gpio => p.and_then(|p| p.gpio.clone()).map(Provided::Gpio),
            LineKind::I2c => p.and_then(|p| p.i2c.clone()).map(Provided::I2c),
            LineKind::Mailbox => p.and_then(|p| p.mailbox.clone()).map(Provided::Mailbox),
        };
        found.ok_or(ResourceError::NoProvider)
    }

    /// Adds a disabled channel under `parent`: it takes a unit of its
    /// class, as FreeBSD's named children do, but is never attached
    /// (`devinfo` shows it "(disabled)").
    pub(crate) fn add_disabled_child(
        &self,
        s: &Sleepable,
        parent: DeviceId,
        ident: Ident,
    ) -> DeviceId {
        let Ident::Channel { class, .. } = ident else {
            return self.add_child(s, parent, ident, None, Vec::new(), &[]);
        };
        let mut t = self.topo.write(s);
        let unit = t.units.alloc(class, None).unwrap_or(0);
        let node = Node {
            generation: 0,
            parent: Some(parent.index),
            children: Vec::new(),
            ident,
            bus_data: None,
            resources: Vec::new(),
            state: NodeState::NotPresent,
            driver: None,
            adopted: None,
            unit: Some(unit),
            desc: String::new(),
            declined: Vec::new(),
            binding: None,
            sysctl: None,
            start: StartPhase::Attach,
            disabled: Some(class),
            fixed: None,
            nomatch_sent: false,
            lines: Vec::new(),
            provides: Provides::default(),
            dma: None,
            translation: None,
            requester: None,
            requester_hook: None,
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
        drop(t);
        self.bump();
        DeviceId { index, generation }
    }

    /// Chooses a driver for `id`, allocates its unit, and queues it if its
    /// suppliers are attached. Topology lock held.
    fn match_node(&self, t: &mut Topology, id: DeviceId) {
        let Some(n) = t.node(id) else {
            return;
        };
        let mut best: Option<(usize, Rank)> = None;
        for (i, d) in self.drivers.iter().enumerate() {
            if n.declined.contains(&i) || n.fixed.is_some_and(|f| f != i) {
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
            // No match: FreeBSD's nomatch event, once per node. It has
            // settled, for consumers waiting on that.
            self.nomatch(t, id);
            self.release_consumers(t, id.index);
            return;
        };
        let decl = &self.drivers[d];
        // A bridge, or a device on a bus below one, takes its unit at its
        // turn.
        let unit = if self.takes_turn(t, id, d) {
            None
        } else {
            match self.take_unit(t, id, d) {
                Some(u) => Some(u),
                None => return,
            }
        };
        let desc = match t.node(id) {
            Some(n) => (decl.describe)(&n.ident, n.bus_data.as_deref()),
            None => (decl.describe)(&Ident::Root, None),
        };
        let missing = t.graph.missing(id.index, |i| t.supply(i));
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
            n.unit = unit;
            n.desc = desc;
            n.state = state;
        }
        if queue {
            self.queue(t, id);
        }
    }

    /// Allocates `id`'s unit for driver `d`: the one the driver chooses or
    /// a hint wires, else the lowest free. `None` if the hints disable
    /// the device, which is then named but never attached. Topology lock
    /// held.
    fn take_unit(&self, t: &mut Topology, id: DeviceId, d: usize) -> Option<u32> {
        let decl = &self.drivers[d];
        let wired = t.node(id).and_then(|n| {
            decl.hints
                .iter()
                .find(|h| {
                    n.resources
                        .iter()
                        .any(|r| matches!(r, Resource::IoPort { base, .. } if *base == h.port))
                })
                .map(|h| h.unit)
        });
        let chosen = t
            .node(id)
            .and_then(|n| (decl.unit)(&n.ident, n.bus_data.as_deref()));
        let hinted: Vec<u32> = decl.hints.iter().map(|h| h.unit).collect();
        let unit = chosen
            .or(wired)
            .and_then(|u| t.units.alloc(decl.name, Some(u)).ok())
            .unwrap_or_else(|| t.units.alloc_skipping(decl.name, &hinted));
        // Disabled by the hints: named, never attached (FreeBSD's
        // `device_attach` and `resource_disabled`, "disabled via hints").
        if (self.disabled_hint)(decl.name, unit) {
            if let Some(n) = t.node_mut(id) {
                n.disabled = Some(decl.name);
                n.driver = None;
                n.desc = String::new();
                n.unit = Some(unit);
                n.state = NodeState::NotPresent;
                n.start = StartPhase::Attach;
            }
            self.release_consumers(t, id.index);
            return None;
        }
        Some(unit)
    }

    /// Queues the attach of `id`, which is `Attaching`; a device whose
    /// unit waits for its turn waits for it first. Topology lock held.
    fn queue(&self, t: &mut Topology, id: DeviceId) {
        let turn = t
            .node(id)
            .is_some_and(|n| n.unit.is_none() && n.driver.is_some());
        if turn {
            if let Some(n) = t.node_mut(id) {
                n.start = StartPhase::Turn;
            }
            self.release_starts(t);
        } else {
            self.push(Job::Attach(id));
        }
    }

    fn push(&self, job: Job) {
        self.busy.fetch_add(1, Ordering::AcqRel);
        self.jobs.lock().push_back(job);
        self.jobs_q.wake_one();
    }

    fn worker(&'static self, s: &Sleepable) {
        loop {
            let mut job = None;
            self.jobs_q.wait_until(s, || {
                job = self.jobs.lock().pop_front();
                job.is_some()
            });
            match job {
                Some(Job::Attach(id)) => self.attach(id, s),
                Some(Job::Start(id)) => self.start(id, s),
                None => continue,
            }
            self.done_one();
        }
    }

    fn done_one(&self) {
        self.busy.fetch_sub(1, Ordering::AcqRel);
        self.jobs_done.fetch_add(1, Ordering::AcqRel);
        self.idle_q.wake_all();
    }

    /// A node's position in depth-first creation order: its child index
    /// at each level from the root.
    fn tree_key(t: &Topology, mut i: u32) -> Vec<u32> {
        let mut key = Vec::new();
        while let Some(p) = t
            .nodes
            .get(i as usize)
            .and_then(Option::as_ref)
            .and_then(|n| n.parent)
        {
            let pos = t.nodes[p as usize]
                .as_ref()
                .and_then(|pn| pn.children.iter().position(|&c| c == i))
                .unwrap_or(usize::MAX);
            key.push(u32::try_from(pos).unwrap_or(u32::MAX));
            i = p;
        }
        key.reverse();
        key
    }

    /// Whether node `index` has a driver of [`UnitOrder::Serial`].
    ///
    /// [`UnitOrder::Serial`]: crate::types::UnitOrder::Serial
    fn serial(&self, t: &Topology, index: u32) -> bool {
        t.nodes
            .get(index as usize)
            .and_then(Option::as_ref)
            .and_then(|n| n.driver)
            .is_some_and(|d| self.drivers[d].unit_order == crate::types::UnitOrder::Serial)
    }

    /// Whether `id`, matched to driver `d`, takes its unit at its turn
    /// rather than now: a bridge ([`crate::types::UnitOrder::Serial`]),
    /// or a device on the bus a bridge provides, whose number FreeBSD's
    /// serial attach gives after everything in the bridges before it.
    fn takes_turn(&self, t: &Topology, id: DeviceId, d: usize) -> bool {
        if self.drivers[d].unit_order == crate::types::UnitOrder::Serial {
            return true;
        }
        let grandparent = t
            .node(id)
            .and_then(|n| n.parent)
            .and_then(|p| t.nodes.get(p as usize)?.as_ref()?.parent);
        grandparent.is_some_and(|g| self.serial(t, g))
    }

    /// Gives the nodes waiting for their turn their units, in tree order:
    /// a node's turn comes when no bridge before it in the tree (one that
    /// is not its ancestor), nor the bus a bridge provides, has yet to
    /// start, so every device FreeBSD would have numbered first has its
    /// unit. Then starts the earliest attaching node if it is parked.
    /// Topology lock held.
    fn release_starts(&self, t: &mut Topology) {
        let count = u32::try_from(t.nodes.len()).unwrap_or(0);
        let mut pending: Vec<(Vec<u32>, u32)> = (0..count)
            .filter(|&i| {
                t.nodes[i as usize]
                    .as_ref()
                    .is_some_and(|n| n.state == NodeState::Attaching)
            })
            .map(|i| (Self::tree_key(t, i), i))
            .collect();
        pending.sort_unstable();
        // Bridges and their buses not started yet.
        let unstarted: Vec<&Vec<u32>> = pending
            .iter()
            .filter(|(_, i)| {
                self.serial(t, *i)
                    || t.nodes[*i as usize]
                        .as_ref()
                        .and_then(|n| n.parent)
                        .is_some_and(|p| self.serial(t, p))
            })
            .map(|(k, _)| k)
            .collect();
        let mut turns = Vec::new();
        for (key, i) in &pending {
            let waiting = t.nodes[*i as usize]
                .as_ref()
                .is_some_and(|n| matches!(n.start, StartPhase::Turn));
            if !waiting {
                continue;
            }
            let blocked = unstarted
                .iter()
                .any(|b| *b < key && !key.starts_with(b.as_slice()));
            if blocked {
                // Whatever blocks it blocks every later one too.
                break;
            }
            turns.push(*i);
        }
        for i in turns {
            let Some(id) = t.id(i) else {
                continue;
            };
            // Still waiting (a nested call, through a disabled node's
            // consumers, may have given it its unit).
            let Some(d) = t
                .node(id)
                .filter(|n| matches!(n.start, StartPhase::Turn))
                .and_then(|n| n.driver)
            else {
                continue;
            };
            // Disabled by the hints: no longer attaching.
            let Some(unit) = self.take_unit(t, id, d) else {
                continue;
            };
            if let Some(n) = t.node_mut(id) {
                n.unit = Some(unit);
                n.start = StartPhase::Attach;
            }
            self.push(Job::Attach(id));
        }
        let earliest = pending.first().and_then(|&(_, i)| t.id(i));
        if let Some(id) = earliest
            && t.node(id).is_some_and(|n| n.state == NodeState::Attaching)
        {
            self.start_parked(t, id);
        }
    }

    /// Starts `id`, the earliest attaching node, if it is parked.
    fn start_parked(&self, t: &mut Topology, id: DeviceId) {
        if let Some(n) = t.node_mut(id)
            && matches!(n.start, StartPhase::Parked(..))
            && let StartPhase::Parked(b, sysctl) =
                core::mem::replace(&mut n.start, StartPhase::Starting)
        {
            n.binding = Some(b);
            n.sysctl = sysctl;
            // The parked count becomes the start job's.
            self.push(Job::Start(id));
            self.done_one();
        }
    }

    /// Runs attach for `id`, which is `Attaching`, and parks it for its
    /// start.
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
            let _ = devtree::strings::iommu_rid(&mut strings.iommu, n.requester);
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
        let binding = match result {
            Ok(b) => b,
            Err(e) => {
                drop_node();
                self.attach_failed(id, decl_index, e, s);
                return;
            }
        };
        // Parked until every earlier node has started; counted as work.
        let mut t = self.topo.write(s);
        let Some(n) = t.node_mut(id) else {
            return;
        };
        n.start = StartPhase::Parked(binding, node);
        self.busy.fetch_add(1, Ordering::AcqRel);
        self.release_starts(&mut t);
    }

    /// Runs `start` for a parked node whose turn has come.
    fn start(&'static self, id: DeviceId, s: &Sleepable) {
        let (mut binding, node, decl_index, unit) = {
            let mut t = self.topo.write(s);
            let Some(n) = t.node_mut(id) else {
                return;
            };
            let (Some(b), Some(d), Some(u)) = (n.binding.take(), n.driver, n.unit) else {
                return;
            };
            (b, n.sysctl.take(), d, u)
        };
        let decl = self.drivers[decl_index];
        let drop_node = || {
            if let Some(n) = node {
                crate::sysctl::remove_device(self, s, n);
            }
        };
        // FreeBSD announces a device before its attach runs, so the lines
        // the driver prints follow; FreeBSD prints no `<>` for a device
        // without a description (`usbus0 on xhci0`). Starts run one at a
        // time, in tree order.
        let announce = {
            let t = self.topo.read(s);
            t.node(id).and_then(|n| {
                let parent = t.nameunit(self.drivers, n.parent.unwrap_or(0));
                // `iicbus_print_child`: a device's address, if it has one.
                let at = match &n.ident {
                    crate::Ident::Iic(i) if i.addr != 0 => alloc::format!(" at addr {:#x}", i.addr),
                    _ => alloc::string::String::new(),
                };
                Some(match decl.announce {
                    Announce::Quiet => return None,
                    Announce::FirstUnit if unit != 0 => return None,
                    Announce::Description | Announce::FirstUnit if !n.desc.is_empty() => {
                        alloc::format!("{}{unit}: <{}>{at} on {parent}", decl.name, n.desc)
                    }
                    _ => alloc::format!("{}{unit} on {parent}", decl.name),
                })
            })
        };
        if let Some(line) = announce {
            self.platform.log(format_args!("{line}"));
        }
        if let Err(e) = binding.start(self, s) {
            let report = binding.detach(DetachReason::StartFailed, s);
            self.check_ledger(decl.name, unit, report.leaked_grants);
            drop_node();
            self.attach_failed(id, decl_index, e, s);
            return;
        }
        let mut t = self.topo.write(s);
        if t.node(id).is_none() {
            return;
        }
        let line = self.device_line(&t, '+', &alloc::format!("{}{unit}", decl.name), id.index);
        if let Some(n) = t.node_mut(id) {
            n.binding = Some(binding);
            n.sysctl = node;
            n.state = NodeState::Attached;
            n.start = StartPhase::Attach;
        }
        // Only this node's consumers can have become ready.
        self.release_consumers(&mut t, id.index);
        self.release_starts(&mut t);
        drop(t);
        self.event(line);
        self.bump();
    }

    /// Queues the consumers of node `index` that wait on nothing any more,
    /// now that it has attached or settled. Topology lock held.
    fn release_consumers(&self, t: &mut Topology, index: u32) {
        let consumers: Vec<u32> = t.graph.consumers(index).map(|e| e.consumer).collect();
        for c in consumers {
            let Some(cid) = t.id(c) else {
                continue;
            };
            let ready = t.graph.missing(c, |i| t.supply(i)).is_empty();
            if let Some(n) = t.node_mut(cid)
                && matches!(n.state, NodeState::Waiting(_))
                && ready
            {
                n.state = NodeState::Attaching;
                self.queue(t, cid);
            }
        }
    }

    fn attach_failed(&self, id: DeviceId, decl_index: usize, e: AttachError, s: &Sleepable) {
        let mut t = self.topo.write(s);
        let name = self.drivers[decl_index].name;
        let Some(n) = t.node_mut(id) else {
            return;
        };
        // Providers published by a start that failed never served anyone,
        // and the ranges it recorded have no children to share them.
        core::mem::take(&mut n.provides).revoke();
        self.space.lock().forget(id);
        let unit = n.unit.take();
        n.driver = None;
        n.start = StartPhase::Attach;
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
                // It has settled: what waited for it to (settle edges)
                // goes on without it.
                self.release_consumers(&mut t, id.index);
            }
        }
        // It no longer holds back the nodes after it.
        self.release_starts(&mut t);
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
        if t.graph.missing(consumer.index, |i| t.supply(i)).is_empty()
            && let Some(n) = t.node_mut(consumer)
        {
            n.state = NodeState::Attaching;
            self.queue(t, consumer);
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
        self.space.lock().claims()
    }

    /// Waits until no attach is queued or running.
    pub fn wait_idle(&self, s: &Sleepable) {
        self.idle_q
            .wait_until(s, || self.busy.load(Ordering::Acquire) == 0);
    }

    /// Waits until no node at or below `id` is attaching or starting, as
    /// FreeBSD's USB explore attaches each new device's drivers (and
    /// theirs) before it looks at the next port. Nodes waiting for a
    /// supplier count as settled.
    pub fn wait_settled(&self, id: DeviceId, s: &Sleepable) {
        loop {
            // Read before looking, so a job that ends after the look
            // still wakes this.
            let done = self.jobs_done.load(Ordering::Acquire);
            if !self.attaching_below(id, s) {
                return;
            }
            self.idle_q
                .wait_until(s, || self.jobs_done.load(Ordering::Acquire) != done);
        }
    }

    fn attaching_below(&self, id: DeviceId, s: &Sleepable) -> bool {
        let t = self.topo.read(s);
        let mut stack = alloc::vec![id.index];
        while let Some(i) = stack.pop() {
            let Some(n) = t.nodes.get(i as usize).and_then(Option::as_ref) else {
                continue;
            };
            if n.state == NodeState::Attaching {
                return true;
            }
            stack.extend(n.children.iter().copied());
        }
        false
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
        let (binding, children, name, unit, provides) = {
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
            // No consumer finds its providers from now on.
            let provides = core::mem::take(&mut n.provides);
            // Graph consumers that wait on it (required, power, and settle
            // edges) detach before it too.
            for e in t.graph.consumers(id.index) {
                if e.kind.orders_detach()
                    && let Some(c) = t.id(e.consumer)
                    && !children.contains(&c)
                {
                    children.push(c);
                }
            }
            (binding, children, name, unit, provides)
        };
        // 2. Children and consumers first, newest first; then its
        // providers stop, before its instance goes.
        for c in children.into_iter().rev() {
            let _ = self.detach(c, DetachReason::Parent, s);
        }
        provides.revoke();
        // The sources hold the driver's implementations, and with them
        // its grants (a controller's registers): gone before the ledger
        // is checked.
        drop(provides);
        self.space.lock().forget(id);
        // 3-7. The binding's own teardown. Children a bus driver created
        // go with it, before its instance: their bus data may hold the
        // bus's grants (a transport's registers).
        if let Some(b) = binding {
            for c in b.children() {
                self.delete(c, s);
            }
            let report = b.detach(why, s);
            self.check_ledger(name, unit, report.leaked_grants);
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

    /// Matches and attaches a node that has no driver, again offering it
    /// drivers that declined it before (`devctl attach`, FreeBSD's
    /// `device_probe_and_attach`).
    pub fn probe_and_attach(&self, id: DeviceId, s: &Sleepable) -> Result<(), AttachRequestError> {
        let mut t = self.topo.write(s);
        let Some(n) = t.node_mut(id) else {
            return Err(AttachRequestError::NoSuchDevice);
        };
        if !matches!(n.state, NodeState::NotPresent | NodeState::Failed(_)) {
            return Err(AttachRequestError::HasDriver);
        }
        n.declined.clear();
        n.state = NodeState::NotPresent;
        self.match_node(&mut t, id);
        drop(t);
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
        self.shutdown_under(self.root(), s);
    }

    /// [`Manager::shutdown`] for `top`'s subtree only.
    pub fn shutdown_under(&self, top: DeviceId, s: &Sleepable) {
        let order: Vec<DeviceId> = {
            let t = self.topo.read(s);
            let mut under = alloc::vec![false; t.nodes.len()];
            let mut stack = alloc::vec![top.index];
            while let Some(i) = stack.pop() {
                if let Some(n) = t.nodes.get(i as usize).and_then(Option::as_ref) {
                    under[i as usize] = true;
                    stack.extend(&n.children);
                }
            }
            let mut g = t.graph.clone();
            let _ = g.break_cycles();
            g.shutdown_order()
                .unwrap_or_default()
                .into_iter()
                .filter(|&i| i != 0 && t.attached(i) && under.get(i as usize) == Some(&true))
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

    /// Appends the `index`th device's `hw.bus.devices` record to `out`,
    /// in node-table order (any order serves: `devinfo` links records by
    /// handle); `None` past the last, or if its fields do not fit. It is
    /// written in place: a record is over 3 KiB.
    pub(crate) fn udevice(&self, s: &Sleepable, index: usize, out: &mut Vec<u8>) -> Option<()> {
        use abi::bus::{df, ds};
        use devtree::udevice::{Fields, encode_into};
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
        let mut flags = if n.disabled.is_some() { 0 } else { df::ENABLED };
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
        encode_into(
            handle(i),
            n.parent.map_or(0, handle),
            0,
            flags,
            state,
            &f,
            out,
        )
        .ok()
    }

    /// Whether `id` is disabled (FreeBSD's `device_disable`).
    #[must_use]
    pub fn is_disabled(&self, id: DeviceId, s: &Sleepable) -> bool {
        self.topo
            .read(s)
            .node(id)
            .is_some_and(|n| n.disabled.is_some())
    }

    /// `id`'s children, in the order they were added.
    #[must_use]
    pub fn children(&self, id: DeviceId, s: &Sleepable) -> Vec<DeviceId> {
        let t = self.topo.read(s);
        t.node(id)
            .map(|n| n.children.iter().filter_map(|&c| t.id(c)).collect())
            .unwrap_or_default()
    }

    /// `id`'s identity.
    #[must_use]
    pub fn ident(&self, id: DeviceId, s: &Sleepable) -> Option<Ident> {
        Some(self.topo.read(s).node(id)?.ident.clone())
    }

    /// `id`'s description.
    #[must_use]
    pub fn description(&self, id: DeviceId, s: &Sleepable) -> Option<String> {
        Some(self.topo.read(s).node(id)?.desc.clone())
    }

    /// The state of `id`.
    #[must_use]
    pub fn state(&self, id: DeviceId, s: &Sleepable) -> Option<NodeState> {
        Some(self.topo.read(s).node(id)?.state.clone())
    }

    /// `id`'s unit, once it has one.
    #[must_use]
    pub fn unit(&self, id: DeviceId, s: &Sleepable) -> Option<u32> {
        self.topo.read(s).node(id)?.unit
    }

    /// `id`'s devclass and unit (`device_get_name`, `device_get_unit`),
    /// and whether it is attached, if it has a class and a unit.
    #[must_use]
    pub fn class_unit(&self, id: DeviceId, s: &Sleepable) -> Option<(&'static str, u32, bool)> {
        let t = self.topo.read(s);
        let n = t.node(id)?;
        Some((
            n.class(self.drivers)?,
            n.unit?,
            n.state == NodeState::Attached,
        ))
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

/// Why an attach request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachRequestError {
    /// No such node.
    NoSuchDevice,
    /// It has a driver already (attached, attaching, or waiting).
    HasDriver,
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
