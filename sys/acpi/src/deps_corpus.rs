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
        crate::bus::walk(&mut i, &mut h, &dma_controllers(name), &[], &id, None).companion_deps
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

/// The GPIO and I2C lines `acpi0` gives a machine's children, by the
/// child's path: what their drivers ask for by index.
fn connection_lines(name: &str) -> BTreeMap<String, Vec<crate::bus::ChildLine>> {
    let (mut i, mut h) = acpi_corpus::load(name);
    let w = crate::bus::walk(
        &mut i,
        &mut h,
        &dma_controllers(name),
        &[],
        &FirmwareIdentity::default(),
        None,
    );
    w.found
        .into_iter()
        .map(|f| {
            let lines: Vec<_> = f
                .lines
                .into_iter()
                .filter(|l| matches!(l.0, bus::LineKind::Gpio | bus::LineKind::I2c))
                .collect();
            (f.device.path, lines)
        })
        .filter(|(_, l)| !l.is_empty())
        .collect()
}

#[test]
fn consumers_get_their_gpio_and_i2c_lines() {
    use bus::LineDetail;
    use bus::gpio::{GpioMode, GpioUse, Pull, Restriction};
    use bus::i2c::I2cTarget;
    let sky1 = connection_lines("opi6p");
    // The RTC: its bus and address, and its alarm interrupt on S5 GPIO.
    let rtc = &sky1["\\_SB_.I2C3.RTC0"];
    assert_eq!(
        rtc[0],
        (
            bus::LineKind::I2c,
            None,
            String::from("\\_SB_.I2C3"),
            0x32,
            LineDetail::I2c(I2cTarget {
                address: 0x32,
                ten_bit: false,
                speed: 400_000
            })
        )
    );
    assert_eq!(rtc[1].2, "\\_SB_.GPI4");
    assert_eq!(
        rtc[1].4,
        LineDetail::Gpio(GpioUse {
            pins: alloc::vec![10],
            mode: GpioMode::Interrupt {
                trigger: irq::Trigger::Level,
                polarity: irq::Polarity::Low,
                wake: false
            },
            shared: false,
            pull: Pull::Up,
            debounce_us: 0,
        })
    );
    // The codec's jack detect: both edges, shared, a wake source.
    let LineDetail::Gpio(jd) = &sky1["\\_SB_.I2C2.RTL5"][0].4 else {
        panic!("RTL5's first line is its GpioInt");
    };
    assert_eq!(
        jd.mode,
        GpioMode::Interrupt {
            trigger: irq::Trigger::Edge,
            polarity: irq::Polarity::Both,
            wake: true
        }
    );
    // Both PD controllers share S5 GPIO 0 line 7 (one interrupt).
    for pd in ["\\_SB_.I2C7.PD00", "\\_SB_.I2C7.PD01"] {
        assert_eq!(sky1[pd][1].2, "\\_SB_.GPI4");
        assert_eq!(sky1[pd][1].3, 7);
    }
    // An I2C controller's own pins (bus recovery), output only.
    let LineDetail::Gpio(rec) = &sky1["\\_SB_.I2C2"][0].4 else {
        panic!("I2C2 has its GpioIo");
    };
    assert_eq!(rec.pins, [12, 13]);
    assert_eq!(rec.mode, GpioMode::Io(Restriction::Output));
    // Counted over the machine: GPIO lines, then I2C ones.
    let count = |m: &BTreeMap<String, Vec<crate::bus::ChildLine>>| {
        let all = m.values().flatten();
        let gpio = all.clone().filter(|l| l.0 == bus::LineKind::Gpio).count();
        (gpio, all.count() - gpio)
    };
    assert_eq!(count(&sky1), (46, 11));
    let x870e = connection_lines("x870e");
    assert_eq!(x870e["\\_SB_.I2CB.ITE2"][0].3, 0x42);
    assert_eq!(count(&x870e), (2, 3));
}

#[test]
fn aei_events_find_their_methods() {
    use irq::{Polarity, Trigger};
    let cases: [(&str, &str); 3] = [
        ("opi6p", "\\_SB.GPI4"),
        ("x870e", "\\_SB.GPIO"),
        ("radxa-q8b", "\\_SB.GIO0"),
    ];
    let mut got = Vec::new();
    for (m, path) in cases {
        let (mut i, mut h) = acpi_corpus::load(m);
        let node = i
            .lookup(&aml::AmlPath::parse(path).unwrap())
            .expect("controller exists");
        let (events, notes) = crate::aei::events(&mut i, &mut h, node);
        assert_eq!(notes, Vec::<String>::new(), "{m}");
        for e in events {
            let bus::gpio::GpioMode::Interrupt {
                trigger, polarity, ..
            } = e.pin.mode
            else {
                panic!("an _AEI event is an interrupt");
            };
            got.push((m, e.pin.pins[0], e.method, trigger, polarity));
        }
    }
    let s = String::from;
    assert_eq!(
        got,
        [
            // The Sky1's EC line, level low: its own method.
            ("opi6p", 6, s("_L06"), Trigger::Level, Polarity::Low),
            // No _E03 on the X870E: _EVT takes the pin.
            ("x870e", 3, s("_EVT"), Trigger::Edge, Polarity::Low),
            ("radxa-q8b", 2, s("_E02"), Trigger::Edge, Polarity::High),
            // Above 255, only _EVT can take it.
            ("radxa-q8b", 704, s("_EVT"), Trigger::Edge, Polarity::Both),
        ]
    );
}

#[test]
fn sky1_i2c_buses_say_their_speed() {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let mut got = Vec::new();
    for n in 0..8 {
        let path = alloc::format!("\\_SB.I2C{n}");
        let node = i.lookup(&aml::AmlPath::parse(&path).unwrap()).unwrap();
        let v = crate::properties::property(&mut i, &mut h, node, "clock-frequency");
        got.push(match v {
            Some(aml::Value::Integer(f)) => f,
            other => panic!("{path}: {other:?}"),
        });
    }
    // The PD controllers' buses (I2C1 on the Orion O6's layout, I2C7 on
    // this board) run at 100 kHz; the rest at 400 kHz.
    assert_eq!(
        got,
        [
            400_000, 100_000, 400_000, 400_000, 400_000, 400_000, 400_000, 100_000
        ]
    );
}

/// The Sky1's mailbox clients get their channels from `_DSD` `mboxes`, in
/// order: the SCMI agent's (sending first), the security engine's, and the
/// audio DSP's.
#[test]
fn the_scmi_agent_gets_its_mailbox_channels() {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let w = crate::bus::walk(
        &mut i,
        &mut h,
        &dma_controllers("opi6p"),
        &[],
        &FirmwareIdentity::default(),
        None,
    );
    let mailboxes: Vec<(String, String, u32)> = w
        .found
        .into_iter()
        .flat_map(|f| {
            let path = f.device.path.clone();
            f.lines
                .into_iter()
                .filter(|l| l.0 == bus::LineKind::Mailbox)
                .map(move |l| (path.clone(), l.2, l.3))
        })
        .collect();
    assert_eq!(
        mailboxes,
        [
            ("\\_SB_.DSMC".into(), "\\_SB_.MBX0".into(), 10),
            ("\\_SB_.DSMC".into(), "\\_SB_.MBX1".into(), 9),
            ("\\_SB_.SCMI".into(), "\\_SB_.MBX6".into(), 8),
            ("\\_SB_.SCMI".into(), "\\_SB_.MBX7".into(), 8),
            ("\\_SB_.DSP_".into(), "\\_SB_.MBX5".into(), 9),
            ("\\_SB_.DSP_".into(), "\\_SB_.MBX4".into(), 9),
        ]
    );
}

/// What `_CCA` says of each machine's devices: the Sky1's USB hosts do not
/// snoop the caches, its PCIe hosts do; QEMU's virt machine says coherent
/// for its PCIe host; the PCs say nothing.
#[test]
fn devices_say_whether_their_dma_is_coherent() {
    let coherency = |name: &str| -> BTreeMap<String, Option<bool>> {
        let (mut i, mut h) = acpi_corpus::load(name);
        let w = crate::bus::walk(
            &mut i,
            &mut h,
            &dma_controllers(name),
            &[],
            &FirmwareIdentity::default(),
            None,
        );
        w.found
            .into_iter()
            .map(|f| (f.device.path, f.coherent))
            .collect()
    };
    let sky1 = coherency("opi6p");
    for usb in ["XHC0", "XHC5", "USB0", "USB3"] {
        assert_eq!(sky1[&alloc::format!("\\_SB_.{usb}")], Some(false), "{usb}");
    }
    let listed: Vec<_> = sky1
        .iter()
        .filter(|(p, c)| p.starts_with("\\_SB_.PCI") && c.is_some())
        .map(|(p, c)| (p.clone(), *c))
        .collect();
    assert_eq!(
        listed,
        ["PCI0", "PCI1", "PCI2", "PCI3"].map(|p| (alloc::format!("\\_SB_.{p}"), Some(true)))
    );
    // Every USB host on the Sky1 says non-coherent: all ten.
    let usb = sky1
        .iter()
        .filter(|(p, c)| (p.contains("XHC") || p.contains(".USB")) && **c == Some(false))
        .count();
    assert_eq!(usb, 10);
    let virt = coherency("qemu-virt");
    assert!(virt.values().all(|c| *c != Some(false)));
    assert_eq!(virt["\\_SB_.PCI0"], Some(true));
    let x870e = coherency("x870e");
    assert!(x870e.values().all(Option::is_none));
}

/// A machine's walk with its IORT, as `acpi0` makes it.
fn walk_with_iort(
    name: &str,
    tables: &[&'static [u8]],
    iort: &'static [u8],
) -> (Interpreter, crate::bus::Walk) {
    let (mut i, mut h) = acpi_corpus::load_tables(name, tables);
    let t = acpi_tables::Sdt {
        phys: 0,
        bytes: iort,
    };
    let w = crate::bus::walk(
        &mut i,
        &mut h,
        &dma_controllers(name),
        &[],
        &FirmwareIdentity::default(),
        Some(t),
    );
    (i, w)
}

fn opi6p_with_iort() -> (Interpreter, crate::bus::Walk) {
    let (_, tables) = machines().into_iter().find(|m| m.0 == "opi6p").unwrap();
    walk_with_iort(
        "opi6p",
        &tables,
        include_bytes!("../../../tests/corpus/acpi/opi6p/iort.dat"),
    )
}

/// The Sky1's IORT (IOMMU.md §3.1): `smmu0` translates the four PCIe host
/// bridges, `smmu1` the present named components; the absent ones are
/// noted.
#[test]
fn opi6p_iort() {
    use crate::iort::Translated;
    let (_, w) = opi6p_with_iort();
    let p = &w.iort;
    let bases: Vec<u64> = p.smmus.iter().map(|s| s.smmu.base).collect();
    assert_eq!(bases, [0xb01_0000, 0xb1b_0000]);
    assert_eq!(
        p.smmus[0].gsivs(),
        [107, 116, 108, 112],
        "event, PRI, sync, gerror"
    );
    assert_eq!(p.smmus[1].gsivs(), [141, 150, 142, 146]);
    let by = |smmu: usize| -> Vec<&str> {
        p.consumers
            .iter()
            .filter(|c| c.smmu == smmu)
            .map(|c| c.path.as_str())
            .collect()
    };
    assert_eq!(
        by(0),
        ["\\_SB_.PCI0", "\\_SB_.PCI1", "\\_SB_.PCI2", "\\_SB_.PCI3"]
    );
    assert_eq!(
        by(1),
        [
            "\\_SB_.DPU0",
            "\\_SB_.DPU1",
            "\\_SB_.DPU2",
            "\\_SB_.DPU3",
            "\\_SB_.DPU4",
            "\\_SB_.NPU0",
            "\\_SB_.ISPM",
            "\\_SB_.CBD0"
        ]
    );
    for c in &p.consumers {
        match &c.how {
            Translated::HostBridge { segment, map } => {
                assert_eq!(*segment, 0);
                assert_eq!(map.len(), 1);
                assert_eq!(
                    (map[0].map(0xc100), map[0].map(0xffff)),
                    (Some(0xc100), Some(0xffff))
                );
            }
            Translated::Named { streams } if c.path == "\\_SB_.DPU2" => {
                let s: Vec<u32> = streams.iter().cloned().flatten().collect();
                assert_eq!(s, [0xc, 0xd, 0xf, 0x10, 0x11]);
            }
            Translated::Named { streams } => assert!(!streams.is_empty()),
        }
    }
    let absent: Vec<&str> = p.notes.iter().map(String::as_str).collect();
    assert_eq!(
        absent,
        ["AEU0", "AEU1", "AEU2", "AEU3", "AEU4", "CBD2"]
            .map(|d| alloc::format!("acpi0: IORT names \\_SB_.{d}, which is not present"))
    );
}

/// QEMU's `virt,iommu=smmuv3`: one SMMU, translating `PCI0` for buses 0
/// and 1 only.
#[test]
fn qemu_virt_smmuv3_iort() {
    use crate::iort::Translated;
    let (_, w) = walk_with_iort(
        "qemu-virt-smmuv3",
        &[include_bytes!(
            "../../../tests/corpus/acpi/qemu-virt-smmuv3/dsdt.dat"
        )],
        include_bytes!("../../../tests/corpus/acpi/qemu-virt-smmuv3/iort.dat"),
    );
    let p = &w.iort;
    assert_eq!(p.smmus.len(), 1);
    assert_eq!(p.smmus[0].smmu.base, 0x905_0000);
    assert_eq!(p.smmus[0].gsivs(), [106, 107, 108, 109]);
    assert_eq!(p.consumers.len(), 1);
    let c = &p.consumers[0];
    assert_eq!((c.smmu, c.path.as_str()), (0, "\\_SB_.PCI0"));
    let Translated::HostBridge { segment: 0, map } = &c.how else {
        panic!("{c:?}");
    };
    assert_eq!(map.len(), 1, "the bypassed range is not this SMMU's");
    assert_eq!(map[0].map(0x100), Some(0x100));
    assert_eq!(map[0].map(0x200), None);
    assert!(p.notes.is_empty());
}

/// The board's graph with the IORT's edges (IOMMU.md §4.3): an `smmu`
/// child of `acpi0` for each SMMU, a settle edge to each device it
/// translates. No cycle runs through them, and each SMMU comes before
/// what it translates.
#[test]
fn opi6p_graph_with_iort_edges() {
    use devtree::graph::{Edge, Graph, Origin};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Key {
        Ns(aml::NodeId),
        Smmu(usize),
    }
    let (i, w) = opi6p_with_iort();
    let (mut i2, mut h2) = acpi_corpus::load("opi6p");
    let deps = deps_of("opi6p", &mut i2, &mut h2);
    let mut g: Graph<Key> = Graph::new();
    for e in deps.graph(&i2).edges() {
        g.add_edge(Edge {
            supplier: Key::Ns(e.supplier),
            consumer: Key::Ns(e.consumer),
            kind: e.kind,
            origin: e.origin,
        });
    }
    let root = i.root();
    let node = |path: &str| {
        let p = aml::AmlPath::parse(path).unwrap();
        i.lookup(&p).unwrap()
    };
    for s in 0..w.iort.smmus.len() {
        g.add_edge(Edge {
            supplier: Key::Ns(root),
            consumer: Key::Smmu(s),
            kind: EdgeKind::Required,
            origin: Origin::Tree,
        });
    }
    for c in &w.iort.consumers {
        let n = node(&c.path);
        // The consumer's place in the tree, as the namespace gives it.
        let mut at = n;
        while let Some(p) = i.parent(at) {
            g.add_edge(Edge {
                supplier: Key::Ns(p),
                consumer: Key::Ns(at),
                kind: EdgeKind::Required,
                origin: Origin::Tree,
            });
            at = p;
        }
        g.add_edge(Edge {
            supplier: Key::Smmu(c.smmu),
            consumer: Key::Ns(n),
            kind: EdgeKind::Settle,
            origin: Origin::Iort,
        });
    }
    let dropped = g.break_cycles().unwrap();
    assert!(
        dropped.iter().all(|e| e.origin != Origin::Iort),
        "{dropped:?}"
    );
    let order = g.topo_order().unwrap();
    let pos = |k: Key| order.iter().position(|&x| x == k).unwrap();
    for c in &w.iort.consumers {
        assert!(
            pos(Key::Smmu(c.smmu)) < pos(Key::Ns(node(&c.path))),
            "{}",
            c.path
        );
    }
    // Shutdown takes every PCIe host bridge down before smmu0.
    let down = g.shutdown_order().unwrap();
    let at = |k: Key| down.iter().position(|&x| x == k).unwrap();
    for b in ["PCI0", "PCI1", "PCI2", "PCI3"] {
        assert!(at(Key::Ns(node(&alloc::format!("\\_SB_.{b}")))) < at(Key::Smmu(0)));
    }
}
