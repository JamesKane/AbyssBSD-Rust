// SPDX-License-Identifier: BSD-2-Clause
//! The graph builder against the firmware corpus (`tests/corpus/acpi`):
//! each machine's edges, unresolved references, and cycles, from standard
//! ACPI alone. What a platform adds is tested in its crate, and the
//! whole-machine checks with every platform in `kernel/table`
//! (ADR-0020).

extern crate std;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use acpi_corpus::Machine;
use acpi_platform::FirmwareIdentity;
use aml::Interpreter;
use aml::host::RegionSpace;
use devtree::graph::EdgeKind;

use crate::deps::{FirmwareDeps, FirmwareEdge, Provider, Via, build};

/// A machine's DMA controllers, as the kernel hands them to the builder.
fn dma_controllers(name: &str) -> Vec<crate::DmaController> {
    acpi_corpus::dma_controllers(name)
        .into_iter()
        .map(|(mmio_base, requests)| crate::DmaController {
            mmio_base,
            requests,
        })
        .collect()
}

/// The dependencies of the devices `acpi0` adds, as the bus asks for
/// them, with no platform registered: standard ACPI only (ADR-0020).
fn deps_of(name: &str, i: &mut Interpreter, h: &mut Machine) -> FirmwareDeps {
    let devices = crate::deps::devices(i, h);
    build(
        i,
        h,
        &devices,
        &dma_controllers(name),
        &[],
        &FirmwareIdentity::default(),
    )
}

fn machines() -> Vec<(&'static str, Vec<&'static [u8]>)> {
    acpi_corpus::machines()
}

/// A machine's namespace, its firmware state, and its dependencies.
fn machine(name: &str) -> (Interpreter, Machine, FirmwareDeps) {
    let (mut i, mut h) = acpi_corpus::load(name);
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
        let (_, _, deps) = machine(name);
        let mut left: Vec<&str> = deps
            .unclassified
            .iter()
            .map(|u| u.property.as_str())
            .collect();
        left.sort_unstable();
        left.dedup();
        // The Sky1's vendor bindings are its platform's to classify.
        let want: &[&str] = match name {
            "opi6p" => &[
                "actuator-src",
                "arm,remote-ctrl",
                "audss_cru",
                "cdns,cru-ctrl",
                "cdns,pcie-phy",
                "cdnsp,usb3-phy",
                "cix,dsp-ctrl",
                "cix,hw",
                "cix,usb_syscon",
                "cix,usbphy_syscon",
                "cru-ctrl",
                "dp_phy",
                "edp-panel",
                "isp-src",
                "physical-memory-group-manager",
                "protected-memory-allocator",
                "sky1,fch_cru",
                "tzgt",
                "uartclk",
            ],
            _ => &[],
        };
        assert_eq!(left, want, "{name}");
    }
}

/// Firmware's required and power edges never close a cycle: breaking the
/// cycles drops only optional and order edges (a media pipeline's links
/// both ways, a MAC's `phy-handle` to its own child).
#[test]
fn only_soft_edges_close_cycles() {
    for (name, _) in machines() {
        let (i, _, deps) = machine(name);
        let mut g = deps.graph(&i);
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
            deps.edges.is_empty() && deps.unresolved.is_empty() && deps.platform.is_empty(),
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
            ("Optional \"backlight\" device", 1),
            ("Optional Dma device", 7),
            ("Order \"remote-endpoint\" device", 22),
            ("Power \"power-domains\" sub", 3),
            ("Power \"power-supply\" device", 1),
            ("Power \"vcc-pcie-supply\" device", 2),
            ("Power _PR0 bus", 16),
            ("Power _PR3 bus", 16),
            ("Required \"mboxes\" device", 6),
            ("Required \"pwms\" device", 1),
            ("Required \"shmem\" device", 2),
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
        assert_eq!((e.kind, &e.via), (EdgeKind::Optional, &Via::Dma));
    }
}

/// The Q8B depends by `_DEP`; with its camera and sensor hubs off, six of
/// them name a device nothing will serve.
/// The Q8B's AML reaches its PMIC and RTC over serial-bus regions and a
/// WLAN enable over a GPIO region; those are its only operation regions a
/// driver serves.
#[test]
fn q8b_graph() {
    let (i, _, deps) = machine("radxa-q8b");
    expect(
        &deps,
        &[
            ("Optional Region(GeneralPurposeIo) device", 1),
            ("Optional Region(GenericSerialBus) device", 3),
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
    // `ABD`, Qualcomm's ACPI bus device, serves `\_SB.ABD.ROP1`, which
    // `PEP0` and `PMGK` define fields in; `IRTC`'s methods read the RTC
    // registers `IC13` defines (`\_SB.IC13.TMBK`); `QPPX` drives a pin
    // of `GIO0`. `ABD` itself waits for `PEP0` by `_DEP`, so only the
    // region edge's being optional keeps the graph acyclic.
    for (consumer, supplier, space) in [
        ("\\_SB_.PEP0", "\\_SB_.ABD_", RegionSpace::GenericSerialBus),
        ("\\_SB_.PMGK", "\\_SB_.ABD_", RegionSpace::GenericSerialBus),
        ("\\_SB_.IRTC", "\\_SB_.IC13", RegionSpace::GenericSerialBus),
        ("\\_SB_.QPPX", "\\_SB_.GIO0", RegionSpace::GeneralPurposeIo),
    ] {
        let e = deps.edges.iter().find(|e| {
            e.via == Via::Region(space)
                && i.path(e.consumer).to_text() == consumer
                && i.path(e.supplier).to_text() == supplier
        });
        assert_eq!(
            e.map(|e| e.kind),
            Some(EdgeKind::Optional),
            "{supplier} -> {consumer}"
        );
    }
    assert_eq!(
        edge(&i, &deps, "\\_SB_.ABD_", "\\_SB_.PEP0").kind,
        EdgeKind::Required
    );
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
    assert!(deps.platform.is_empty());
}

/// The dependencies of devices named only by `_ADR`, which `acpi0` hands
/// to whatever node stands for them (a PCI function's ACPI companion,
/// DRIVER-MODEL.md §5.2). The Q8B's PCIe root ports wait for the GPIO
/// controller driving their PERST# and are ordered after their USB4
/// ports; the X870E's and the Sky1's companions depend only on power
/// resources, which `acpi0` serves, or on objects above them.
#[test]
fn companion_deps() {
    use devtree::graph::Origin;
    let walk_of = |name: &str| {
        let (mut i, mut h) = acpi_corpus::load(name);
        let id = FirmwareIdentity::default();
        crate::bus::walk(&mut i, &mut h, &dma_controllers(name), &[], &id).companion_deps
    };
    assert!(walk_of("opi6p").is_empty());
    assert!(walk_of("x870e").is_empty());
    let got: Vec<(String, String, EdgeKind, Origin)> = walk_of("radxa-q8b")
        .into_iter()
        .map(|(c, (s, k, o))| (c, s, k, o))
        .collect();
    let d = |c: &str, s: &str, k, o| (String::from(c), String::from(s), k, o);
    let gpio = |n: u32| {
        d(
            &alloc::format!("\\_SB_.PCI{n}.RP1_"),
            "\\_SB_.GIO0",
            EdgeKind::Required,
            Origin::ResourceSource,
        )
    };
    let mut want = alloc::vec![
        d(
            "\\_SB_.ADSP.ADCM",
            "\\_SB_.MMU0",
            EdgeKind::Required,
            Origin::Dep
        ),
        d(
            "\\_SB_.ADSP.ADCM",
            "\\_SB_.IMM0",
            EdgeKind::Required,
            Origin::Dep
        ),
        d(
            "\\_SB_.ADSP.ADCM.AUCD",
            "\\_SB_.GIO0",
            EdgeKind::Required,
            Origin::ResourceSource
        ),
        d(
            "\\_SB_.GPU0.AVS0",
            "\\_SB_.MMU0",
            EdgeKind::Required,
            Origin::Dep
        ),
        d(
            "\\_SB_.GPU0.AVS0",
            "\\_SB_.IMM0",
            EdgeKind::Required,
            Origin::Dep
        ),
        d(
            "\\_SB_.GPU0.AVS0",
            "\\_SB_.VFE0",
            EdgeKind::Required,
            Origin::Dep
        ),
        d(
            "\\_SB_.PCI0.RP1_",
            "\\_SB_.UBF0.PRT0",
            EdgeKind::Order,
            Origin::Dsd
        ),
        d(
            "\\_SB_.PCI1.RP1_",
            "\\_SB_.UBF0.PRT1",
            EdgeKind::Order,
            Origin::Dsd
        ),
    ];
    want.extend((2..=6).map(gpio));
    want.push(d(
        "\\_SB_.URS0.USB0.RHUB.PRT1",
        "\\_SB_.UBF0.PRT0",
        EdgeKind::Order,
        Origin::Dsd,
    ));
    want.push(d(
        "\\_SB_.URS1.USB1.RHUB.PRT1",
        "\\_SB_.UBF0.PRT1",
        EdgeKind::Order,
        Origin::Dsd,
    ));
    assert_eq!(got, want);
}
