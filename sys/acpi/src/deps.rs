// SPDX-License-Identifier: BSD-2-Clause
//! The graph builder: what the namespace says each device depends on
//! (DRIVER-MODEL.md §6.1), before any driver runs.
//!
//! For every device it reads
//! - `_DEP`;
//! - the resource sources in `_CRS`: the GPIO controller of a `GpioInt` or
//!   `GpioIo`, the controller of a serial bus, the combiner an `Interrupt`
//!   names;
//! - the power resources of `_PR0` and `_PR3`;
//! - `_DSD`'s reference properties (`power-domains`, `mboxes`, `phys`,
//!   graph endpoints, ...), following the Hierarchical Data Extension into
//!   data nodes, and classifying each by [`devtree::dsd::reference_kind`].
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

use aml::resource::Resource;
use aml::{DepRef, Interpreter, NodeId, ObjectType, Value};
use devtree::graph::{EdgeKind, Origin};

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
    /// A power resource in `_PR0` (D0) or `_PR3` (D3hot).
    PowerResource {
        /// From `_PR3` rather than `_PR0`.
        d3hot: bool,
    },
    /// A `_DSD` property, by name.
    Property(String),
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
            Self::PowerResource { d3hot: false } => String::from("_PR0"),
            Self::PowerResource { d3hot: true } => String::from("_PR3"),
            Self::Property(p) => alloc::format!("_DSD {p}"),
        }
    }

    /// The graph origin of edges named this way.
    #[must_use]
    pub const fn origin(&self) -> Origin {
        match self {
            Self::Dep => Origin::Dep,
            Self::Interrupt | Self::Gpio | Self::SerialBus => Origin::ResourceSource,
            Self::PowerResource { .. } => Origin::PowerResource,
            Self::Property(_) => Origin::Dsd,
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

/// Reads the dependencies of `devices` from the namespace.
pub fn build(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    devices: &[NodeId],
) -> FirmwareDeps {
    let mut out = FirmwareDeps::default();
    for &d in devices {
        dep(interp, host, d, &mut out);
        resource_sources(interp, host, d, &mut out);
        power_resources(interp, host, d, &mut out);
        if let Ok(Some(dsd)) = interp.evaluate_child(d, "_DSD", Vec::new(), host) {
            dsd_package(interp, host, d, d, &dsd, 0, &mut out);
        }
    }
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
    if interp.object_type(object) == Some(ObjectType::PowerResource) {
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
        let (via, name) = match r {
            Resource::ExtendedIrq(e) => match &e.source {
                Some(s) => (Via::Interrupt, &s.name),
                None => continue,
            },
            Resource::Gpio(g) => (Via::Gpio, &g.source.name),
            Resource::SerialBus(b) => (Via::SerialBus, &b.source.name),
            _ => continue,
        };
        match interp.resolve(d, name) {
            Some(s) => out.edge(s, d, EdgeKind::Required, via),
            None => out.unresolved(d, via, name.clone()),
        }
    }
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
fn dsd_package(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
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
                    dsd_package(interp, host, device, node, &data, depth + 1, out);
                }
            }
        } else if uuid.as_slice() == PROPERTIES_UUID || is_property_list(entries) {
            for entry in entries {
                if let Value::Package(kv) = entry
                    && let [Value::String(name), value] = kv.as_slice()
                {
                    property(interp, device, name, value, out);
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
    device: NodeId,
    name: &str,
    value: &Value,
    out: &mut FirmwareDeps,
) {
    match value {
        Value::Package(items) => {
            for v in items {
                property(interp, device, name, v, out);
            }
        }
        Value::Name(..) | Value::Reference(_) => {
            let via = Via::Property(String::from(name));
            match (
                interp.resolve_value_name(value),
                devtree::dsd::reference_kind(name),
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
