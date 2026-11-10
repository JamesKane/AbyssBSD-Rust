// SPDX-License-Identifier: BSD-2-Clause
//! The graph builder: what the namespace says each device depends on
//! (DRIVER-MODEL.md §6.1), before any driver runs.
//!
//! For every device it reads
//! - `_DEP`;
//! - the resource sources in `_CRS`: the GPIO controller of a `GpioInt` or
//!   `GpioIo`, the controller of a serial bus, the combiner an `Interrupt`
//!   names, the pin controller of a pin function, configuration, or group
//!   (whose `PinGroup` label must exist);
//! - the DMA controller serving each `FixedDMA` request line, which the
//!   CSRT names ([`crate::DmaController`]);
//! - the power resources of `_PR0` and `_PR3`;
//! - `_DSD`'s reference properties (`power-domains`, `mboxes`, `phys`,
//!   graph endpoints, ...), following the Hierarchical Data Extension into
//!   data nodes, and classifying each by [`devtree::dsd::reference_kind`]
//!   and then by the applying platforms' vendor names;
//! - the operation regions whose accesses an OS driver serves (GPIO and
//!   serial-bus regions through their field's `Connection`, embedded
//!   controller, SMBus and IPMI regions through the device that declares
//!   them, PCC regions through `acpi0`): the device defining a field in
//!   one, and each device whose methods name the field, use that driver
//!   ([`region_edges`]);
//! - the clock and reset lines and device links the applying platforms
//!   extract from their vendor tables (ADR-0020, [`acpi_platform`]), whose
//!   entries are kept as the platform description
//!   ([`FirmwareDeps::platform`]).
//!
//! Suppliers are namespace objects, not yet devices: a power resource, a
//! `_STA = 0` stub, or a data node becomes a provider's sub-handle later.
//! A name that resolves to nothing is reported in
//! [`FirmwareDeps::unresolved`], never an error (the Sky1's graph
//! endpoints name `\_SB.I2C0.UXC0`–`UXC3`, which its firmware never
//! defines), and a reference property the table does not know in
//! [`FirmwareDeps::unclassified`].

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use alloc::collections::BTreeSet;

use aml::resource::Resource;
use aml::{DepRef, Interpreter, NodeId, ObjectType, RegionSpace, Value};
use devtree::graph::{Edge, EdgeKind, Graph, Origin};
use devtree::platform::Description;

use crate::DmaController;
use acpi_platform::{FirmwareIdentity, Malformed, PlatformSource};

/// `_DSD` device properties (daffd814-6eba-4d8c-8a91-bc9bbf4aa301).
const PROPERTIES_UUID: [u8; 16] = [
    0x14, 0xd8, 0xff, 0xda, 0xba, 0x6e, 0x8c, 0x4d, 0x8a, 0x91, 0xbc, 0x9b, 0xbf, 0x4a, 0xa3, 0x01,
];

/// `_DSD` Hierarchical Data Extension (dbb8e3e6-5886-4ba6-8795-1319f52a966b):
/// named child data nodes.
const DATA_NODES_UUID: [u8; 16] = [
    0xe6, 0xe3, 0xb8, 0xdb, 0x86, 0x58, 0xa6, 0x4b, 0x87, 0x95, 0x13, 0x19, 0xf5, 0x2a, 0x96, 0x6b,
];

/// How deep data nodes may nest (firmware loops are not followed).
const MAX_DATA_DEPTH: usize = 8;

/// What named a supplier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Via {
    /// `_DEP`.
    Dep,
    /// The source of an `Interrupt` descriptor.
    Interrupt,
    /// The controller of a `GpioInt` or `GpioIo`.
    Gpio,
    /// The controller of a serial bus connection.
    SerialBus,
    /// The pin controller of a `PinFunction`, `PinConfig`, or pin group
    /// descriptor.
    Pin,
    /// The DMA controller serving a `FixedDMA` request line (by the CSRT).
    Dma,
    /// A power resource in `_PR0` (D0) or `_PR3` (D3hot).
    PowerResource {
        /// From `_PR3` rather than `_PR0`.
        d3hot: bool,
    },
    /// A `_DSD` property, by name.
    Property(String),
    /// A field in an operation region of this space, which the supplier's
    /// driver serves.
    Region(RegionSpace),
    /// A platform's description (ADR-0020): from `source`, its `table`
    /// (the description's `"clocks"`, `"resets"`, or `"links"` for an
    /// edge; the vendor table's name for an unresolved reference).
    Platform {
        /// The platform (`"cix-sky1"`).
        source: &'static str,
        /// The section or table.
        table: &'static str,
    },
}

impl Via {
    /// For a log line: `"_DEP"`, `"GpioInt/GpioIo"`, `"_DSD remote-endpoint"`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Dep => String::from("_DEP"),
            Self::Interrupt => String::from("Interrupt"),
            Self::Gpio => String::from("GpioInt/GpioIo"),
            Self::SerialBus => String::from("SerialBus"),
            Self::Pin => String::from("pin resource"),
            Self::Dma => String::from("FixedDMA"),
            Self::PowerResource { d3hot: false } => String::from("_PR0"),
            Self::PowerResource { d3hot: true } => String::from("_PR3"),
            Self::Property(p) => alloc::format!("_DSD {p}"),
            Self::Region(space) => alloc::format!("{space:?} region"),
            Self::Platform { source, table } => alloc::format!("{source} {table}"),
        }
    }

    /// The graph origin of edges named this way.
    #[must_use]
    pub const fn origin(&self) -> Origin {
        match self {
            Self::Dep => Origin::Dep,
            Self::Interrupt | Self::Gpio | Self::SerialBus => Origin::ResourceSource,
            Self::Pin => Origin::Pin,
            Self::Dma => Origin::Dma,
            Self::PowerResource { .. } => Origin::PowerResource,
            Self::Property(_) => Origin::Dsd,
            Self::Region(RegionSpace::Pcc) => Origin::Pcc,
            Self::Region(_) => Origin::Region,
            Self::Platform { .. } => Origin::Platform,
        }
    }
}

/// Who serves a supplier (DRIVER-MODEL.md §6.1's `ProviderRef`): the
/// device whose driver the consumer waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provider {
    /// A device `acpi0` adds a child for.
    Device(NodeId),
    /// An object such a device owns: a `_STA = 0` stub (the Sky1's SCMI
    /// protocol nodes), a data-only child, a data node, a buffer it
    /// shares. The nearest enumerated ancestor serves it.
    Sub {
        /// The serving device.
        owner: NodeId,
        /// The object named.
        object: NodeId,
    },
    /// A power resource, or an object (not a device) under no enumerated
    /// device: `acpi0` serves it.
    Bus(NodeId),
    /// A device `acpi0` leaves out (absent by `_STA`, or without an ID),
    /// under no enumerated device: nothing will serve it (the Sky1's
    /// `I2C6`, which firmware keeps for its embedded controller).
    Absent(NodeId),
}

/// A supplier → consumer edge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FirmwareEdge {
    /// The object depended on.
    pub supplier: NodeId,
    /// The dependent device.
    pub consumer: NodeId,
    /// What the edge holds the consumer to.
    pub kind: EdgeKind,
    /// What named the supplier.
    pub via: Via,
    /// Who serves the supplier.
    pub provider: Provider,
}

/// A reference to an object the namespace does not have.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unresolved {
    /// The device that made it.
    pub consumer: NodeId,
    /// Where.
    pub via: Via,
    /// The name, as written.
    pub name: String,
}

/// A reference in a `_DSD` property the table does not know.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unclassified {
    /// The device that made it.
    pub consumer: NodeId,
    /// The property.
    pub property: String,
    /// The object it names.
    pub target: NodeId,
}

/// Everything the namespace says about dependencies.
#[derive(Clone, Debug, Default)]
pub struct FirmwareDeps {
    /// The edges, each once, in device order.
    pub edges: Vec<FirmwareEdge>,
    /// References that name nothing.
    pub unresolved: Vec<Unresolved>,
    /// References under property names the table does not know.
    pub unclassified: Vec<Unclassified>,
    /// What the platform's own tables describe (DRIVER-MODEL.md §5.5).
    pub platform: Description<NodeId>,
    /// Entries of those tables that could not be read, and lines two
    /// entries give.
    pub malformed: Vec<Malformed>,
    /// The platforms that applied, by name.
    pub platforms: Vec<&'static str>,
}

impl FirmwareDeps {
    fn edge(&mut self, supplier: NodeId, consumer: NodeId, kind: EdgeKind, via: Via) {
        let e = FirmwareEdge {
            supplier,
            consumer,
            kind,
            via,
            // Filled in once every edge is known.
            provider: Provider::Bus(supplier),
        };
        // A device's `_DSD` GPIO bindings name the device itself (the pin
        // is its own `_CRS` entry, whose controller is the supplier).
        if supplier != consumer && !self.edges.contains(&e) {
            self.edges.push(e);
        }
    }

    fn unresolved(&mut self, consumer: NodeId, via: Via, name: String) {
        let u = Unresolved {
            consumer,
            via,
            name,
        };
        if !self.unresolved.contains(&u) {
            self.unresolved.push(u);
        }
    }
}

/// The devices `acpi0` adds a child for, in namespace order: the ones
/// [`build`] is asked about.
pub fn devices(interp: &mut Interpreter, host: &mut dyn aml::Host) -> Vec<NodeId> {
    let mut out = Vec::new();
    for d in interp.devices() {
        if crate::bus::enumerated(interp, host, d).is_some() {
            out.push(d);
        }
    }
    out
}

impl FirmwareDeps {
    /// The namespace tree (parent → child, over every node an edge
    /// touches) and the edges between serving devices, as the manager's
    /// graph holds them. An edge from a consumer's own ancestor adds
    /// nothing to the tree's order and is left out.
    #[must_use]
    pub fn graph(&self, interp: &Interpreter) -> Graph<NodeId> {
        let mut g = Graph::new();
        let tree = |g: &mut Graph<NodeId>, mut n: NodeId| {
            while let Some(p) = interp.parent(n) {
                let added = g.add_edge(Edge {
                    supplier: p,
                    consumer: n,
                    kind: EdgeKind::Required,
                    origin: Origin::Tree,
                });
                if !added {
                    break;
                }
                n = p;
            }
        };
        for e in &self.edges {
            let s = match e.provider {
                Provider::Device(n) | Provider::Sub { owner: n, .. } | Provider::Absent(n) => n,
                Provider::Bus(_) => interp.root(),
            };
            if nearest(interp, e.consumer, |_, a| a == s).is_some() {
                continue;
            }
            tree(&mut g, s);
            tree(&mut g, e.consumer);
            g.add_edge(Edge {
                supplier: s,
                consumer: e.consumer,
                kind: e.kind,
                origin: e.via.origin(),
            });
        }
        g
    }
}

/// Reads the dependencies of `devices` from the namespace, with what
/// each of `platforms` that applies to the firmware `id` adds.
pub fn build(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    devices: &[NodeId],
    dma: &[DmaController],
    platforms: &[&dyn PlatformSource],
    id: &FirmwareIdentity,
) -> FirmwareDeps {
    let mut out = FirmwareDeps::default();
    let applying: Vec<&dyn PlatformSource> = platforms
        .iter()
        .copied()
        .filter(|p| p.applies(id, interp, host, devices))
        .collect();
    out.platforms = applying.iter().map(|p| p.name()).collect();
    let mut dma_devices: Option<Vec<(u64, NodeId)>> = None;
    for &d in devices {
        dep(interp, host, d, &mut out);
        resource_sources(interp, host, d, &mut out);
        fixed_dma(interp, host, d, dma, &mut dma_devices, &mut out);
        power_resources(interp, host, d, &mut out);
        if let Ok(Some(dsd)) = interp.evaluate_child(d, "_DSD", Vec::new(), host) {
            dsd_package(interp, host, &applying, d, d, &dsd, 0, &mut out);
        }
    }
    region_edges(interp, devices, &mut out);
    for p in &applying {
        let x = p.extract(interp, host, devices);
        platform_edges(p.name(), x, devices, &mut out);
    }
    duplicate_lines(&mut out);
    let mut enumerated: BTreeMap<NodeId, bool> = BTreeMap::new();
    for i in 0..out.edges.len() {
        out.edges[i].provider = provider(interp, host, out.edges[i].supplier, &mut enumerated);
    }
    out
}

/// Who serves `object` (see [`Provider`]); `enumerated` caches which
/// devices `acpi0` adds.
fn provider(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    object: NodeId,
    enumerated: &mut BTreeMap<NodeId, bool>,
) -> Provider {
    // PCC channels belong to the platform (the PCCT), not to the device
    // whose scope declares the region.
    if interp.object_type(object) == Some(ObjectType::PowerResource)
        || interp
            .region_info(object)
            .is_some_and(|r| r.space == RegionSpace::Pcc)
    {
        return Provider::Bus(object);
    }
    let mut cur = Some(object);
    while let Some(n) = cur {
        if interp.object_type(n) == Some(ObjectType::Device) {
            let added = match enumerated.get(&n) {
                Some(&a) => a,
                None => {
                    let a = crate::bus::enumerated(interp, host, n).is_some();
                    enumerated.insert(n, a);
                    a
                }
            };
            if added {
                return if n == object {
                    Provider::Device(n)
                } else {
                    Provider::Sub { owner: n, object }
                };
            }
        }
        cur = interp.parent(n);
    }
    if interp.object_type(object) == Some(ObjectType::Device) {
        Provider::Absent(object)
    } else {
        Provider::Bus(object)
    }
}

fn dep(interp: &mut Interpreter, host: &mut dyn aml::Host, d: NodeId, out: &mut FirmwareDeps) {
    for r in interp.dep(d, host).unwrap_or_default() {
        match r {
            DepRef::Resolved(s) => out.edge(s, d, EdgeKind::Required, Via::Dep),
            DepRef::Unresolved(name) => out.unresolved(d, Via::Dep, name),
        }
    }
}

fn resource_sources(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    d: NodeId,
    out: &mut FirmwareDeps,
) {
    let crs = interp.crs(d, host).ok().flatten().unwrap_or_default();
    for r in &crs {
        let (via, name, group) = match r {
            Resource::ExtendedIrq(e) => match &e.source {
                Some(s) => (Via::Interrupt, &s.name, None),
                None => continue,
            },
            Resource::Gpio(g) => (Via::Gpio, &g.source.name, None),
            Resource::SerialBus(b) => (Via::SerialBus, &b.source.name, None),
            Resource::PinFunction(p) => (Via::Pin, &p.source.name, None),
            Resource::PinConfig(p) => (Via::Pin, &p.source.name, None),
            Resource::PinGroupFunction(p) => (Via::Pin, &p.source.name, Some(&p.group)),
            Resource::PinGroupConfig(p) => (Via::Pin, &p.source.name, Some(&p.group)),
            _ => continue,
        };
        let Some(s) = interp.resolve(d, name) else {
            out.unresolved(d, via, name.clone());
            continue;
        };
        out.edge(s, d, EdgeKind::Required, via.clone());
        // A pin group is named by the label of one of the controller's
        // `PinGroup` descriptors.
        if let Some(label) = group
            && !declares_group(interp, host, s, label)
        {
            out.unresolved(d, via, alloc::format!("{name} group \"{label}\""));
        }
    }
}

/// A field whose region an OS driver serves, and what serves it.
struct ServedField {
    field: NodeId,
    space: RegionSpace,
    /// The supplier, or the name that resolved to nothing.
    supplier: Result<NodeId, String>,
}

/// Operation-region edges. A region in a space only a driver can serve
/// is unusable until that driver installs its handler (ACPICA dispatches
/// an access to the handler installed nearest above the region, and
/// FreeBSD's `acpi_ec`, `acpi_gpiobus`, and `acpi_iicbus` install theirs
/// on the controller's own handle):
/// - a GPIO or serial-bus field reaches the controller its `Connection`
///   names (the descriptor FreeBSD's handlers decode);
/// - an embedded-controller, SMBus, or IPMI region belongs to the device
///   declaring it;
/// - a PCC region belongs to `acpi0`, which owns the PCCT.
///
/// The consumers are the device that defines the field and each device
/// whose methods name it (a battery's `_BST` reading the EC, a `_PS0`
/// writing a PMIC over I2C). The edges are optional: AML may not touch
/// such a region before `_REG (space, 1)` says its handler is connected
/// (ACPI 6.5 §6.5.4), and ACPICA runs `_REG` when the driver installs the
/// handler, so the consumer can attach first and is told when the region
/// appears. Firmware relies on that: the Q8B's `ABD` waits for `PEP0` by
/// `_DEP` while `PEP0`'s AML talks over `ABD`'s serial bus. Memory, I/O, PCI configuration, and CMOS
/// regions need no driver and make no edge: the Sky1's clock and sensor
/// methods talk SCMI to firmware through a `SystemMemory` mailbox
/// (`\_SB.PMMX`, at 0x065d0000, outside every device's `_CRS`), so they
/// depend on nothing the graph orders.
fn region_edges(interp: &Interpreter, devices: &[NodeId], out: &mut FirmwareDeps) {
    let nodes = interp.nodes();
    let mut served = Vec::new();
    for &n in &nodes {
        let Some(f) = interp.field_info(n) else {
            continue;
        };
        let Some(region) = f.region else {
            continue;
        };
        let Some(r) = interp.region_info(region) else {
            continue;
        };
        let supplier = match r.space {
            RegionSpace::GeneralPurposeIo | RegionSpace::GenericSerialBus => {
                let scope = interp.parent(n).unwrap_or_else(|| interp.root());
                match f.connection.as_deref().map(aml::resource::connection) {
                    Some(Ok(Resource::Gpio(g))) => {
                        interp.resolve(scope, &g.source.name).ok_or(g.source.name)
                    }
                    Some(Ok(Resource::SerialBus(b))) => {
                        interp.resolve(scope, &b.source.name).ok_or(b.source.name)
                    }
                    Some(_) => Err(alloc::format!("{} connection", interp.path(n))),
                    None => Err(alloc::format!("{} without Connection", interp.path(n))),
                }
            }
            RegionSpace::EmbeddedControl | RegionSpace::SmBus | RegionSpace::Ipmi => {
                nearest(interp, region, |i, a| {
                    i.object_type(a) == Some(ObjectType::Device)
                })
                .ok_or_else(|| alloc::format!("{} outside a device", interp.path(region)))
            }
            RegionSpace::Pcc => Ok(region),
            _ => continue,
        };
        served.push(ServedField {
            field: n,
            space: r.space,
            supplier,
        });
    }
    if served.is_empty() {
        return;
    }
    let listed: BTreeSet<NodeId> = devices.iter().copied().collect();
    let consumer_of = |i: &Interpreter, n: NodeId| nearest(i, n, |_, a| listed.contains(&a));
    let mut uses: Vec<(NodeId, NodeId)> = Vec::new();
    for s in &served {
        if let Some(c) = consumer_of(interp, s.field) {
            uses.push((s.field, c));
        }
    }
    let targets: Vec<NodeId> = served.iter().map(|s| s.field).collect();
    for &m in &nodes {
        if interp.method_info(m).is_none() {
            continue;
        }
        let named = interp.method_names(m, &targets);
        if named.is_empty() {
            continue;
        }
        if let Some(c) = consumer_of(interp, m) {
            uses.extend(named.into_iter().map(|f| (f, c)));
        }
    }
    // In device order, as the other edges are.
    uses.sort_by_key(|&(_, c)| devices.iter().position(|&d| d == c));
    for (field, consumer) in uses {
        let Some(s) = served.iter().find(|s| s.field == field) else {
            continue;
        };
        let via = Via::Region(s.space);
        match &s.supplier {
            Ok(supplier) => out.edge(*supplier, consumer, EdgeKind::Optional, via),
            Err(name) => out.unresolved(consumer, via, name.clone()),
        }
    }
}

/// The edges a platform's description makes, each from the device
/// serving the line to its consumer, for the consumers in `devices` (a
/// table may describe a device firmware left out, as the Sky1's `MAC1`):
/// a clock line from its clock provider, a reset line from its reset
/// controller, a device link from its supplier. All are required: a
/// driver needs its clocks and resets in attach, and a link is an attach
/// order by definition. The entries join [`FirmwareDeps::platform`].
fn platform_edges(
    source: &'static str,
    x: acpi_platform::Extracted,
    devices: &[NodeId],
    out: &mut FirmwareDeps,
) {
    let p = &x.description;
    let via = |table| Via::Platform { source, table };
    for &d in devices {
        for c in p.clocks.iter().filter(|c| c.consumer == d) {
            out.edge(c.provider, d, EdgeKind::Required, via("clocks"));
        }
        for r in p.resets.iter().filter(|r| r.consumer == d) {
            out.edge(r.controller, d, EdgeKind::Required, via("resets"));
        }
        for l in p.links.iter().filter(|l| l.consumer == d) {
            out.edge(l.supplier, d, EdgeKind::Required, via("links"));
        }
    }
    for u in x.unresolved {
        out.unresolved(u.carrier, via(u.table), u.name);
    }
    let mut d = x.description;
    let all = &mut out.platform;
    all.clocks.append(&mut d.clocks);
    all.resets.append(&mut d.resets);
    all.resource_names.append(&mut d.resource_names);
    all.links.append(&mut d.links);
    all.reserved.append(&mut d.reserved);
    all.shared.append(&mut d.shared);
    out.malformed.extend(x.malformed);
}

/// Two clock or reset lines of one consumer under one name, from one
/// platform or two: a diagnostic, never a silent choice (ADR-0020).
fn duplicate_lines(out: &mut FirmwareDeps) {
    let mut dups = Vec::new();
    let p = &out.platform;
    let mut check = |table: &'static str, keys: Vec<(NodeId, Option<&String>)>| {
        for (k, &(consumer, name)) in keys.iter().enumerate() {
            if keys[..k].contains(&(consumer, name)) {
                dups.push(Malformed {
                    source: "platform",
                    table,
                    carrier: consumer,
                    entry: None,
                    why: alloc::format!("two lines named {name:?}"),
                });
            }
        }
    };
    check(
        "clocks",
        p.clocks
            .iter()
            .map(|c| (c.consumer, c.name.as_ref()))
            .collect(),
    );
    check(
        "resets",
        p.resets
            .iter()
            .map(|r| (r.consumer, r.name.as_ref()))
            .collect(),
    );
    out.malformed.extend(dups);
}

/// The nearest of `n` and its ancestors that `pred` accepts.
fn nearest(
    interp: &Interpreter,
    n: NodeId,
    pred: impl Fn(&Interpreter, NodeId) -> bool,
) -> Option<NodeId> {
    let mut cur = Some(n);
    while let Some(a) = cur {
        if pred(interp, a) {
            return Some(a);
        }
        cur = interp.parent(a);
    }
    None
}

/// The DMA controller serving each `FixedDMA` request line: the CSRT says
/// which controller serves the line, and the controller is the device
/// whose `_CRS` begins with its MMIO base.
fn fixed_dma(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    d: NodeId,
    dma: &[DmaController],
    devices: &mut Option<Vec<(u64, NodeId)>>,
    out: &mut FirmwareDeps,
) {
    let crs = interp.crs(d, host).ok().flatten().unwrap_or_default();
    for r in &crs {
        let Resource::FixedDma { request, .. } = *r else {
            continue;
        };
        let Some(c) = dma.iter().find(|c| c.requests.contains(&request)) else {
            out.unresolved(d, Via::Dma, alloc::format!("request line {request}"));
            continue;
        };
        // Every device's first memory range, found once.
        let by_base = devices.get_or_insert_with(|| {
            let mut v = Vec::new();
            for n in interp.devices() {
                let crs = interp.crs(n, host).ok().flatten().unwrap_or_default();
                if let Some(base) = crs.iter().find_map(memory_base) {
                    v.push((base, n));
                }
            }
            v
        });
        match by_base.iter().find(|&&(b, _)| b == c.mmio_base) {
            Some(&(_, s)) => out.edge(s, d, EdgeKind::Required, Via::Dma),
            None => out.unresolved(
                d,
                Via::Dma,
                alloc::format!("DMA controller at {:#x}", c.mmio_base),
            ),
        }
    }
}

/// The start of a memory range a device consumes.
fn memory_base(r: &Resource) -> Option<u64> {
    match r {
        Resource::FixedMemory32(m) | Resource::Memory32(m) | Resource::Memory24(m) => {
            Some(u64::from(m.min))
        }
        Resource::Address(a) if matches!(a.kind, aml::resource::AddressKind::Memory { .. }) => {
            Some(a.min)
        }
        _ => None,
    }
}

/// Whether `controller`'s `_CRS` declares a `PinGroup` labelled `label`.
fn declares_group(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    controller: NodeId,
    label: &str,
) -> bool {
    let crs = interp
        .crs(controller, host)
        .ok()
        .flatten()
        .unwrap_or_default();
    crs.iter()
        .any(|r| matches!(r, Resource::PinGroup(g) if g.label == label))
}

fn power_resources(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    d: NodeId,
    out: &mut FirmwareDeps,
) {
    for (method, d3hot) in [("_PR0", false), ("_PR3", true)] {
        let Ok(Some(Value::Package(list))) = interp.evaluate_child(d, method, Vec::new(), host)
        else {
            continue;
        };
        let via = Via::PowerResource { d3hot };
        for v in &list {
            match interp.resolve_value_name(v) {
                Some(s) => out.edge(s, d, EdgeKind::Power, via.clone()),
                None => {
                    if let Some(name) = name_text(v) {
                        out.unresolved(d, via.clone(), name);
                    }
                }
            }
        }
    }
}

/// A `_DSD` package (or a data node's): pairs of a UUID and its data.
#[allow(clippy::too_many_arguments)]
fn dsd_package(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    platforms: &[&dyn PlatformSource],
    device: NodeId,
    scope: NodeId,
    v: &Value,
    depth: usize,
    out: &mut FirmwareDeps,
) {
    let Value::Package(items) = v else {
        return;
    };
    for pair in items.chunks(2) {
        let [Value::Buffer(uuid), Value::Package(entries)] = pair else {
            continue;
        };
        if uuid.as_slice() == DATA_NODES_UUID {
            for entry in entries {
                if depth < MAX_DATA_DEPTH
                    && let Some(node) = data_node(interp, scope, entry)
                    && let Ok(data) = interp.evaluate_node(node, Vec::new(), host)
                {
                    dsd_package(interp, host, platforms, device, node, &data, depth + 1, out);
                }
            }
        } else if uuid.as_slice() == PROPERTIES_UUID || is_property_list(entries) {
            for entry in entries {
                if let Value::Package(kv) = entry
                    && let [Value::String(name), value] = kv.as_slice()
                {
                    property(interp, platforms, device, name, value, out);
                }
            }
        }
    }
}

/// Whether `entries` looks like a property list (`{"name", value}` pairs),
/// for the UUIDs other than device properties that use the same format
/// (USB4's, hotplug's).
fn is_property_list(entries: &[Value]) -> bool {
    entries.iter().all(
        |e| matches!(e, Value::Package(kv) if kv.len() == 2 && matches!(kv[0], Value::String(_))),
    )
}

/// The data node an entry of the Hierarchical Data Extension names: a
/// `{"name", target}` pair whose target is a string or a name, resolved
/// from the scope the package came from.
fn data_node(interp: &Interpreter, scope: NodeId, entry: &Value) -> Option<NodeId> {
    let Value::Package(kv) = entry else {
        return None;
    };
    match kv.as_slice() {
        [Value::String(_), Value::String(target)] => interp.resolve(scope, target),
        [Value::String(_), target] => interp.resolve_value_name(target),
        _ => None,
    }
}

/// Every reference in a property's value, at any depth (a reference may
/// come with arguments: `{GPI4, 3, 0}`, `{MPC3, "port@0", "endpoint@1"}`).
fn property(
    interp: &Interpreter,
    platforms: &[&dyn PlatformSource],
    device: NodeId,
    name: &str,
    value: &Value,
    out: &mut FirmwareDeps,
) {
    match value {
        Value::Package(items) => {
            for v in items {
                property(interp, platforms, device, name, v, out);
            }
        }
        Value::Name(..) | Value::Reference(_) => {
            let via = Via::Property(String::from(name));
            match (
                interp.resolve_value_name(value),
                acpi_platform::reference_kind(platforms, name),
            ) {
                (Some(s), Some(kind)) => out.edge(s, device, kind, via),
                (Some(s), None) => {
                    let u = Unclassified {
                        consumer: device,
                        property: String::from(name),
                        target: s,
                    };
                    if s != device && !out.unclassified.contains(&u) {
                        out.unclassified.push(u);
                    }
                }
                (None, _) => {
                    if let Some(text) = name_text(value) {
                        out.unresolved(device, via, text);
                    }
                }
            }
        }
        _ => {}
    }
}

/// The text of an unresolved name in a package.
fn name_text(v: &Value) -> Option<String> {
    match v {
        Value::Name(_, name) => Some(alloc::format!("{name}")),
        _ => None,
    }
}
