// SPDX-License-Identifier: BSD-2-Clause
//! The graph builder against the firmware corpus (`tests/corpus/acpi`):
//! each machine's edges, unresolved references, and cycles.

extern crate std;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use aml::host::{Host, HostError, RegionAccess, RegionSpace};
use aml::{Config, Interpreter, Limits, NodeId};
use devtree::graph::{Edge, EdgeKind, Graph, Origin};

use crate::deps::{FirmwareDeps, FirmwareEdge, Provider, Via, build};

/// Registers as sparse memory (unwritten bytes read as zero), with
/// index/data port pairs, seeded with the firmware state each machine's
/// AML reads (as `lib/aml`'s corpus tests establish it from Linux's view
/// of the machine).
#[derive(Default)]
struct Machine {
    mem: BTreeMap<(RegionSpace, u64), u8>,
    /// `(space, index port, data port)`.
    pairs: Vec<(RegionSpace, u64, u64)>,
}

impl Machine {
    fn poke(&mut self, space: RegionSpace, addr: u64, bytes: &[u8]) {
        for (k, &b) in (addr..).zip(bytes) {
            self.mem.insert((space, k), b);
        }
    }

    /// Where byte `i` of an access lives: behind a data port, the register
    /// its index port selects.
    fn slot(&self, a: &RegionAccess, i: u64) -> (RegionSpace, u64) {
        let addr = a.address();
        for &(space, index, data) in &self.pairs {
            if space == a.space && data == addr {
                let reg = self.mem.get(&(space, index)).copied().unwrap_or(0);
                return (space, 1 << 63 | data << 8 | u64::from(reg));
            }
        }
        (a.space, addr.wrapping_add(i))
    }

    fn for_machine(name: &str) -> Self {
        let mut m = Self::default();
        match name {
            // GETV's switches at 0xFFFE0000: on, but for I2C6, PCIe port
            // 4, and three xHCI controllers.
            "opi6p" => {
                let mut g = [1u8; 0x2c];
                for k in [0x0b, 0x11, 0x1c, 0x1d, 0x1e] {
                    g[k] = 0;
                }
                m.poke(RegionSpace::SystemMemory, 0xfffe_0000, &g);
            }
            // AMI's setup variables, HPET's enable, the FCH controller
            // block (behind extended CMOS 0x08-0x0B), the interrupt
            // router.
            "x870e" => {
                const FRTB: u32 = 0x7000_0000;
                m.poke(RegionSpace::SystemMemory, 0x8fe0_d000, &[1, 0, 2, 1]);
                m.poke(RegionSpace::SystemMemory, 0xfed8_0300, &[0x40]);
                m.pairs.push((RegionSpace::SystemIo, 0x72, 0x73));
                for (k, b) in (8u64..).zip(FRTB.to_le_bytes()) {
                    m.mem
                        .insert((RegionSpace::SystemIo, 1 << 63 | 0x73 << 8 | k), b);
                }
                m.poke(RegionSpace::SystemMemory, u64::from(FRTB) + 4, &[0x60]);
                m.poke(RegionSpace::SystemMemory, u64::from(FRTB) + 0x1c, &[0x0e]);
                m.pairs.push((RegionSpace::SystemIo, 0xc00, 0xc01));
                for k in 0..8u64 {
                    m.mem
                        .insert((RegionSpace::SystemIo, 1 << 63 | 0xc01 << 8 | k), 0x0b);
                }
            }
            _ => {}
        }
        m
    }
}

impl Host for Machine {
    fn read(&mut self, a: &RegionAccess) -> Result<u64, HostError> {
        let mut v = 0;
        for i in 0..a.width.bytes() {
            let b = self.mem.get(&self.slot(a, i)).copied().unwrap_or(0);
            v |= u64::from(b) << (8 * i);
        }
        Ok(v)
    }

    fn write(&mut self, a: &RegionAccess, value: u64) -> Result<(), HostError> {
        for (i, &b) in (0..a.width.bytes()).zip(&value.to_le_bytes()) {
            let slot = self.slot(a, i);
            self.mem.insert(slot, b);
        }
        Ok(())
    }
}

macro_rules! corpus {
    ($path:literal) => {
        include_bytes!(concat!("../../../tests/corpus/acpi/", $path)).as_slice()
    };
}

/// Each machine's AML tables, in XSDT order.
fn machines() -> Vec<(&'static str, Vec<&'static [u8]>)> {
    alloc::vec![
        ("qemu-q35", alloc::vec![corpus!("qemu-q35/dsdt.dat")]),
        ("qemu-virt", alloc::vec![corpus!("qemu-virt/dsdt.dat")]),
        ("bhyve", alloc::vec![corpus!("bhyve/dsdt.dat")]),
        ("radxa-q8b", alloc::vec![corpus!("radxa-q8b/dsdt.dat")]),
        (
            "opi6p",
            alloc::vec![
                corpus!("opi6p/dsdt.dat"),
                corpus!("opi6p/ssdt1.dat"),
                corpus!("opi6p/ssdt2.dat"),
            ]
        ),
        (
            "x870e",
            alloc::vec![
                corpus!("x870e/dsdt.dat"),
                corpus!("x870e/ssdt1.dat"),
                corpus!("x870e/ssdt2.dat"),
                corpus!("x870e/ssdt3.dat"),
                corpus!("x870e/ssdt4.dat"),
                corpus!("x870e/ssdt5.dat"),
                corpus!("x870e/ssdt6.dat"),
                corpus!("x870e/ssdt7.dat"),
                corpus!("x870e/ssdt8.dat"),
                corpus!("x870e/ssdt9.dat"),
                corpus!("x870e/ssdt10.dat"),
                corpus!("x870e/ssdt11.dat"),
                corpus!("x870e/ssdt12.dat"),
                corpus!("x870e/ssdt13.dat"),
                corpus!("x870e/ssdt14.dat"),
                corpus!("x870e/ssdt15.dat"),
            ]
        ),
    ]
}

/// A machine's namespace, loaded with its firmware state in place, and
/// `_INI` run.
fn load(name: &str, tables: &[&[u8]]) -> (Interpreter, Machine) {
    let mut i = Interpreter::new(Config {
        limits: Limits::for_stack(64 * 1024),
        ..Config::default()
    });
    let mut h = Machine::for_machine(name);
    for t in tables {
        let r = i.load_table(t, &mut h).unwrap();
        assert!(r.warnings.is_empty(), "{name}: {:?}", r.warnings);
    }
    let _ = i.initialize(&mut h);
    (i, h)
}

/// The dependencies of the devices `acpi0` adds, as the bus asks for them.
fn deps_of(name: &str, i: &mut Interpreter, h: &mut Machine) -> FirmwareDeps {
    let devices: Vec<NodeId> = i
        .devices()
        .into_iter()
        .filter(|&d| crate::bus::enumerated(i, h, d).is_some())
        .collect();
    build(i, h, &devices, &dma_controllers(name))
}

/// A table's bytes as the memory it is read from, at address 0.
struct Table(&'static [u8]);

impl acpi_tables::PhysMem<'static> for &Table {
    fn bytes(&self, pa: u64, len: usize) -> Option<&'static [u8]> {
        let start = usize::try_from(pa).ok()?;
        self.0.get(start..start.checked_add(len)?)
    }
}

/// The DMA controllers a machine's CSRT describes, as the kernel reads
/// them.
fn dma_controllers(name: &str) -> Vec<crate::DmaController> {
    let csrt = match name {
        "opi6p" => corpus!("opi6p/csrt.dat"),
        _ => return Vec::new(),
    };
    let t = acpi_tables::Sdt::read(&&Table(csrt), 0).unwrap();
    acpi_tables::csrt::parse(&t)
        .unwrap()
        .groups()
        .filter_map(|g| g.ok()?.dma_controller())
        .map(|c| crate::DmaController {
            mmio_base: c.mmio_base,
            requests: c.base_request_line..c.base_request_line.saturating_add(c.request_lines),
        })
        .collect()
}

/// The device that serves an edge's supplier: the namespace root stands
/// for `acpi0`.
fn serving(i: &Interpreter, p: Provider) -> NodeId {
    match p {
        Provider::Device(n) | Provider::Sub { owner: n, .. } | Provider::Absent(n) => n,
        Provider::Bus(_) => i.root(),
    }
}

/// Whether `a` is `b` or one of its namespace ancestors.
fn ancestor_or_self(i: &Interpreter, a: NodeId, b: NodeId) -> bool {
    let mut cur = Some(b);
    while let Some(n) = cur {
        if n == a {
            return true;
        }
        cur = i.parent(n);
    }
    false
}

/// The namespace tree (parent → child, over every node an edge touches)
/// and the firmware edges between serving devices, as the manager's graph
/// would hold them. An edge from a consumer's own ancestor adds nothing to
/// the tree's order and is left out.
fn graph(i: &Interpreter, deps: &FirmwareDeps) -> Graph<NodeId> {
    let mut g = Graph::new();
    let tree = |g: &mut Graph<NodeId>, mut n: NodeId| {
        while let Some(p) = i.parent(n) {
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
    for e in &deps.edges {
        let s = serving(i, e.provider);
        if ancestor_or_self(i, s, e.consumer) {
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

/// A machine's namespace, its firmware state, and its dependencies.
fn machine(name: &str) -> (Interpreter, Machine, FirmwareDeps) {
    let (_, tables) = machines().into_iter().find(|(n, _)| *n == name).unwrap();
    let (mut i, mut h) = load(name, &tables);
    let deps = deps_of(name, &mut i, &mut h);
    (i, h, deps)
}

/// How many edges of each kind each source names, by provider class:
/// `"Required Gpio device"`.
fn summary(deps: &FirmwareDeps) -> BTreeMap<String, usize> {
    let mut by = BTreeMap::new();
    for e in &deps.edges {
        let p = match e.provider {
            Provider::Device(_) => "device",
            Provider::Sub { .. } => "sub",
            Provider::Bus(_) => "bus",
            Provider::Absent(_) => "absent",
        };
        let via = match &e.via {
            Via::Property(name) => alloc::format!("{name:?}"),
            Via::PowerResource { d3hot: false } => String::from("_PR0"),
            Via::PowerResource { d3hot: true } => String::from("_PR3"),
            v => alloc::format!("{v:?}"),
        };
        *by.entry(alloc::format!("{:?} {via} {p}", e.kind))
            .or_default() += 1;
    }
    by
}

fn expect(deps: &FirmwareDeps, want: &[(&str, usize)]) {
    let want: BTreeMap<String, usize> = want.iter().map(|&(k, n)| (String::from(k), n)).collect();
    assert_eq!(summary(deps), want);
}

/// `(consumer, supplier)` paths of the edges whose supplier nothing will
/// serve.
fn absent(i: &Interpreter, deps: &FirmwareDeps) -> Vec<(String, String)> {
    deps.edges
        .iter()
        .filter(|e| matches!(e.provider, Provider::Absent(_)))
        .map(|e| (i.path(e.consumer).to_text(), i.path(e.supplier).to_text()))
        .collect()
}

fn unresolved(i: &Interpreter, deps: &FirmwareDeps) -> Vec<(String, Via, String)> {
    deps.unresolved
        .iter()
        .map(|u| (i.path(u.consumer).to_text(), u.via.clone(), u.name.clone()))
        .collect()
}

/// The edge from `supplier` to `consumer`, by path.
fn edge<'a>(
    i: &Interpreter,
    deps: &'a FirmwareDeps,
    consumer: &str,
    supplier: &str,
) -> &'a FirmwareEdge {
    deps.edges
        .iter()
        .find(|e| {
            i.path(e.consumer).to_text() == consumer && i.path(e.supplier).to_text() == supplier
        })
        .unwrap_or_else(|| panic!("no edge {supplier} -> {consumer}"))
}

/// Every reference a `_DSD` makes is under a name the table knows.
#[test]
fn every_reference_is_classified() {
    for (name, _) in machines() {
        let (i, _, deps) = machine(name);
        let left: Vec<_> = deps
            .unclassified
            .iter()
            .map(|u| (i.path(u.consumer).to_text(), u.property.clone()))
            .collect();
        assert!(left.is_empty(), "{name}: {left:?}");
    }
}

/// Firmware's required and power edges never close a cycle: breaking the
/// cycles drops only optional and order edges (a media pipeline's links
/// both ways, a MAC's `phy-handle` to its own child).
#[test]
fn only_soft_edges_close_cycles() {
    for (name, _) in machines() {
        let (i, _, deps) = machine(name);
        let mut g = graph(&i, &deps);
        let dropped = g.break_cycles().unwrap_or_else(|c| panic!("{name}: {c:?}"));
        for e in dropped {
            assert!(
                matches!(e.kind, EdgeKind::Optional | EdgeKind::Order),
                "{name}: {} -> {} {:?}",
                i.path(e.supplier),
                i.path(e.consumer),
                e.kind
            );
        }
        g.topo_order().unwrap();
    }
}

#[test]
fn virtual_machines_name_no_suppliers() {
    for name in ["qemu-q35", "qemu-virt", "bhyve"] {
        let (_, _, deps) = machine(name);
        assert!(
            deps.edges.is_empty() && deps.unresolved.is_empty(),
            "{name}"
        );
    }
}

/// The Sky1 has no `_DEP`: its dependencies are `_DSD` references,
/// resource sources, and power resources.
#[test]
fn opi6p_graph() {
    let (i, _, deps) = machine("opi6p");
    expect(
        &deps,
        &[
            ("Optional \"actuator-src\" device", 1),
            ("Optional \"backlight\" device", 1),
            ("Optional \"edp-panel\" device", 1),
            ("Optional \"isp-src\" device", 1),
            ("Order \"remote-endpoint\" device", 22),
            ("Power \"power-domains\" sub", 3),
            ("Power \"power-supply\" device", 1),
            ("Power \"vcc-pcie-supply\" device", 2),
            ("Power _PR0 bus", 16),
            ("Power _PR3 bus", 16),
            ("Required \"arm,remote-ctrl\" device", 1),
            ("Required \"audss_cru\" device", 2),
            ("Required \"cdns,cru-ctrl\" device", 4),
            ("Required \"cdns,pcie-phy\" sub", 4),
            ("Required \"cdnsp,usb3-phy\" sub", 6),
            ("Required \"cix,dsp-ctrl\" device", 1),
            ("Required \"cix,hw\" device", 2),
            ("Required \"cix,usb_syscon\" device", 10),
            ("Required \"cix,usbphy_syscon\" device", 5),
            ("Required \"cru-ctrl\" device", 1),
            ("Required \"dp_phy\" sub", 4),
            ("Required \"mboxes\" device", 6),
            ("Required \"physical-memory-group-manager\" device", 1),
            ("Required \"protected-memory-allocator\" device", 1),
            ("Required \"pwms\" device", 1),
            ("Required \"shmem\" device", 2),
            ("Required \"sky1,fch_cru\" device", 3),
            // Static data: a fixed clock rate, a TrustZone description.
            ("Required \"tzgt\" bus", 1),
            ("Required \"uartclk\" bus", 4),
            ("Required Dma device", 7),
            ("Required Gpio device", 22),
            ("Required Interrupt device", 10),
            ("Required Pin device", 42),
            ("Required SerialBus device", 16),
        ],
    );
    // Four I2S ports ask the pin controller for "..._dbg" groups it never
    // declares (SSDT1's MUX0 has pinctrl_substrate_i2s5, i2s6, and i2s8,
    // no debug variants, and nothing for i2s9). And HARDWARE.md §4: the
    // graph endpoints of the DP ports name USB-C connectors this firmware
    // never defines; on this board only DPR1 is present.
    let mut want: Vec<(String, Via, String)> = [5, 6, 8, 9]
        .iter()
        .map(|n| {
            (
                alloc::format!("\\_SB_.I2S{n}"),
                Via::Pin,
                alloc::format!("\\_SB.MUX0 group \"pinctrl_substrate_i2s{n}_dbg\""),
            )
        })
        .collect();
    want.push((
        String::from("\\_SB_.DPR1"),
        Via::Property(String::from("remote-endpoint")),
        String::from("^I2C0.UXC0"),
    ));
    assert_eq!(unresolved(&i, &deps), want);
    assert!(absent(&i, &deps).is_empty());
    // SCMI runs over the CIX mailbox (DRIVER-MODEL.md §17 question 7),
    // and serves the GPU's, VPU's, and NPU's power domain as a sub-handle.
    for (supplier, via) in [
        ("\\_SB_.MBX6", "mboxes"),
        ("\\_SB_.MBX7", "mboxes"),
        ("\\_SB_.SHM0", "shmem"),
        ("\\_SB_.SHM1", "shmem"),
    ] {
        let e = edge(&i, &deps, "\\_SB_.SCMI", supplier);
        assert_eq!(
            (e.kind, &e.via),
            (EdgeKind::Required, &Via::Property(String::from(via)))
        );
        assert!(matches!(e.provider, Provider::Device(_)));
    }
    let scmi = i
        .lookup(&aml::AmlPath::parse("\\_SB.SCMI").unwrap())
        .unwrap();
    for consumer in ["\\_SB_.GPU_", "\\_SB_.VPU0", "\\_SB_.NPU0"] {
        let e = edge(&i, &deps, consumer, "\\_SB_.SCMI.DVFS");
        assert_eq!(e.kind, EdgeKind::Power);
        assert!(matches!(e.provider, Provider::Sub { owner, .. } if owner == scmi));
    }
    // FixedDMA, by the CSRT: the UARTs' request lines (0-7) are DMA0's,
    // the I2S ports' (32-50) DMA1's.
    for (consumer, supplier) in [
        ("\\_SB_.COM0", "\\_SB_.DMA0"),
        ("\\_SB_.COM3", "\\_SB_.DMA0"),
        ("\\_SB_.I2S5", "\\_SB_.DMA1"),
        ("\\_SB_.I2S9", "\\_SB_.DMA1"),
    ] {
        let e = edge(&i, &deps, consumer, supplier);
        assert_eq!((e.kind, &e.via), (EdgeKind::Required, &Via::Dma));
    }
}

/// The Q8B depends by `_DEP`; with its camera and sensor hubs off, six of
/// them name a device nothing will serve.
#[test]
fn q8b_graph() {
    let (i, _, deps) = machine("radxa-q8b");
    expect(
        &deps,
        &[
            ("Power _PR0 bus", 2),
            ("Power _PR3 bus", 2),
            ("Required Dep absent", 6),
            ("Required Dep device", 94),
            ("Required Gpio device", 9),
            ("Required SerialBus device", 1),
        ],
    );
    let want: Vec<(String, String)> = [
        ("\\_SB_.FLSH", "\\_SB_.CAMP"),
        ("\\_SB_.MPCS", "\\_SB_.CAMP"),
        ("\\_SB_.JPGE", "\\_SB_.CAMP"),
        ("\\_SB_.VFE0", "\\_SB_.CAMP"),
        ("\\_SB_.SEN2", "\\_SB_.SCSS"),
        ("\\_SB_.SEN3", "\\_SB_.SCSS"),
    ]
    .iter()
    .map(|&(c, s)| (String::from(c), String::from(s)))
    .collect();
    assert_eq!(absent(&i, &deps), want);
    assert!(deps.unresolved.is_empty());
}

/// The X870E names few suppliers: its I2C devices' controllers and GPIO
/// interrupts. (The power resources of its PCIe bridges belong to devices
/// AMD's CPM library hides from the test host; see `lib/aml`'s corpus.)
#[test]
fn x870e_graph() {
    let (i, _, deps) = machine("x870e");
    expect(
        &deps,
        &[
            ("Required Gpio device", 2),
            ("Required SerialBus device", 3),
        ],
    );
    assert!(deps.unresolved.is_empty());
    assert!(absent(&i, &deps).is_empty());
}
