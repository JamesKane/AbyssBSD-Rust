// SPDX-License-Identifier: BSD-2-Clause

extern crate std;

use aml::host::{Host, HostError, RegionAccess};
use aml::{AmlPath, Config, Interpreter};

use crate::pcib::{Osc, osc, osc_control, osc_support};

/// Regions read as zero; writes are dropped.
struct Quiet;

impl Host for Quiet {
    fn read(&mut self, _: &RegionAccess) -> Result<u64, HostError> {
        Ok(0)
    }

    fn write(&mut self, _: &RegionAccess, _: u64) -> Result<(), HostError> {
        Ok(())
    }
}

fn load(dsdt: &[u8]) -> Interpreter {
    let mut c = Config::default();
    c.limits.stack_budget = 1 << 20;
    let mut i = Interpreter::new(c);
    i.load_table(dsdt, &mut Quiet).unwrap();
    i
}

const Q35: &[u8] = include_bytes!("../../../tests/corpus/acpi/qemu-q35/dsdt.dat");
const BHYVE: &[u8] = include_bytes!("../../../tests/corpus/acpi/bhyve/dsdt.dat");
const VIRT: &[u8] = include_bytes!("../../../tests/corpus/acpi/qemu-virt/dsdt.dat");
const Q8B: &[u8] = include_bytes!("../../../tests/corpus/acpi/radxa-q8b/dsdt.dat");

#[test]
fn osc_as_freebsd_asks_at_attach() {
    let all = osc_support::EXT_CONFIG
        | osc_support::ASPM
        | osc_support::CLOCK_PM
        | osc_support::SEGMENTS
        | osc_support::MSI;
    // q35 grants nothing when asked for nothing.
    let mut i = load(Q35);
    let pci0 = i.lookup(&AmlPath::parse("\\_SB.PCI0").unwrap()).unwrap();
    assert_eq!(
        osc(&mut i, pci0, &mut Quiet, all, 0),
        Some(Osc {
            status: 0,
            control: 0
        })
    );
    // Asked for native hotplug and AER, it grants AER but keeps hotplug,
    // which QEMU does through ACPI itself.
    let ask = osc_control::PCIE_HOTPLUG | osc_control::PCIE_AER;
    let got = osc(&mut i, pci0, &mut Quiet, all, ask).unwrap();
    assert!(!got.failed(), "{got:?}");
    assert_eq!(got.control & ask, osc_control::PCIE_AER);
    // arm64 virt and bhyve answer too, without errors.
    for (name, dsdt, path) in [("virt", VIRT, "\\_SB.PCI0"), ("bhyve", BHYVE, "\\_SB.PC00")] {
        let mut i = load(dsdt);
        let node = i.lookup(&AmlPath::parse(path).unwrap()).unwrap();
        match osc(&mut i, node, &mut Quiet, all & !osc_support::ASPM, 0) {
            Some(o) => assert!(!o.failed(), "{name}: {o:?}"),
            None => std::println!("{name}: no _OSC"),
        }
    }
}

#[test]
fn osc_absent_is_none() {
    let mut i = load(Q35);
    let root = i.root();
    assert_eq!(osc(&mut i, root, &mut Quiet, 0, 0), None);
}

#[test]
fn q8b_dependencies() {
    let mut i = load(Q8B);
    let found = crate::bus::walk(
        &mut i,
        &mut Quiet,
        &[],
        &[],
        &acpi_platform::FirmwareIdentity::default(),
    )
    .found;
    let paths: std::vec::Vec<&str> = found.iter().map(|f| f.device.path.as_str()).collect();
    let mut forward = 0;
    let mut edges = 0;
    for (n, f) in found.iter().enumerate() {
        for (d, _, _) in &f.deps {
            edges += 1;
            assert_ne!(d, &f.device.path, "a device depends on itself");
            // Every dependency names a device the walk found, or one that
            // is not a bus child (a power resource, an absent device).
            if let Some(at) = paths.iter().position(|p| p == d) {
                forward += usize::from(at > n);
            }
        }
    }
    std::println!(
        "q8b: {} devices, {edges} dependency edges, {forward} forward",
        found.len()
    );
    assert!(edges >= 40, "{edges} dependency edges");
    assert!(forward > 0, "the Q8B has _DEPs on later devices");
}

mod hand {
    use alloc::vec::Vec;

    use aml::asm::{
        cat, device, int, method, name, name_op, package, power_resource, processor, ret, store,
        table,
    };
    use aml::{AmlPath, Value};

    use super::{Quiet, load};

    fn dsdt(body: &[u8]) -> Vec<u8> {
        table(b"DSDT", 2, body)
    }

    fn node(i: &aml::Interpreter, p: &str) -> aml::NodeId {
        i.lookup(&AmlPath::parse(p).unwrap()).unwrap()
    }

    fn eval_int(i: &mut aml::Interpreter, p: &str) -> u64 {
        match i.evaluate_path(p, Vec::new(), &mut Quiet) {
            Ok(Value::Integer(n)) => n,
            v => panic!("{p}: {v:?}"),
        }
    }

    /// `Add (n, One, n)`.
    fn count(n: &str) -> Vec<u8> {
        cat(&[&[0x72], &name(n), &[0x01], &name(n)])
    }

    #[test]
    fn power_resources_switch_on_once() {
        let pr = |n: &str, on: u64| {
            power_resource(
                n,
                0,
                0,
                &cat(&[
                    &name_op("STAT", &int(on)),
                    &method("_STA", 0, &ret(&name("STAT"))),
                    &method(
                        "_ON",
                        0,
                        &cat(&[&store(&[0x01], &name("STAT")), &count("\\ONS")]),
                    ),
                    &method("_OFF", 0, &store(&[0x00], &name("STAT"))),
                ]),
            )
        };
        let body = cat(&[
            &name_op("ONS", &[0x00]),
            &name_op("PS1", &[0x00]),
            &pr("PRA", 0),
            &pr("PRB", 1),
            &device(
                "DEV1",
                &cat(&[
                    &name_op("_PR0", &package(&[name("\\PRA"), name("\\PRB")])),
                    &method("_PS0", 0, &store(&[0x01], &name("\\PS1"))),
                ]),
            ),
            &device("DEV2", &name_op("_PR0", &package(&[name("\\PRA")]))),
        ]);
        let mut i = load(&dsdt(&body));
        let (d1, d2) = (node(&i, "\\DEV1"), node(&i, "\\DEV2"));
        let got = crate::power::to_d0(&mut i, d1, &mut Quiet);
        // PRA was off and is switched on; PRB already reported on.
        assert_eq!(
            got,
            crate::power::PoweredUp {
                switched_on: 1,
                ps0: true
            }
        );
        assert_eq!(
            (eval_int(&mut i, "\\ONS"), eval_int(&mut i, "\\PS1")),
            (1, 1)
        );
        // A second consumer only counts.
        let got = crate::power::to_d0(&mut i, d2, &mut Quiet);
        assert_eq!(
            got,
            crate::power::PoweredUp {
                switched_on: 0,
                ps0: false
            }
        );
        assert_eq!(eval_int(&mut i, "\\ONS"), 1);
        assert_eq!(crate::power::consumers(node(&i, "\\PRA")), 2);
        assert_eq!(crate::power::consumers(node(&i, "\\PRB")), 1);
    }

    /// Kernel services for tests: one 32-bit memory register.
    struct TestOps;

    const REG: u64 = 0xfee0_1000;

    impl crate::Ops for TestOps {
        fn mem_read(&self, pa: u64, bytes: u8) -> Option<u64> {
            (pa == REG && bytes == 4).then_some(600)
        }
        fn mem_write(&self, _: u64, _: u8, _: u64) -> Option<()> {
            None
        }
        fn io_read(&self, _: u16, _: u8) -> Option<u32> {
            None
        }
        fn io_write(&self, _: u16, _: u8, _: u32) -> Option<()> {
            None
        }
        fn pci_config(&self, _: pci::PciAddress) -> Option<&'static (dyn pci::ConfigSpace + Sync)> {
            None
        }
        fn sleep_ms(&self, _: &sched::Sleepable, _: u64) {}
        fn stall_us(&self, _: u64) {}
        fn timer_100ns(&self) -> u64 {
            0
        }
        fn gsi(&self, _: u32) -> Option<(&'static dyn irq::IrqDomain, u32)> {
            None
        }
        fn isa_irq(&self, irq: u8) -> (u32, irq::Trigger, irq::Polarity) {
            (u32::from(irq), irq::Trigger::Edge, irq::Polarity::High)
        }
        fn definition_blocks(&self) -> Vec<&'static [u8]> {
            Vec::new()
        }
        fn oem_description(&self) -> alloc::string::String {
            alloc::string::String::new()
        }
        fn fixed_hardware(&self) -> Option<crate::FixedHardware> {
            None
        }
        fn power_button(&self) {}
        fn log(&self, _: core::fmt::Arguments<'_>) {}
    }

    /// A `Register (SystemMemory, 32, 0, address)` resource template.
    fn register(address: u64) -> Vec<u8> {
        let mut b = alloc::vec![0x82, 0x0c, 0x00, 0x00, 32, 0, 3];
        b.extend_from_slice(&address.to_le_bytes());
        b.extend_from_slice(&[0x79, 0x00]);
        aml::asm::buffer(&b)
    }

    #[test]
    fn cpc_levels() {
        // _CPC: NumEntries, Revision, Highest, Nominal, LowestNonlinear,
        // Lowest.
        let cpc = |highest: Vec<u8>| {
            name_op(
                "_CPC",
                &package(&[int(23), int(3), highest, int(200), int(50), int(10)]),
            )
        };
        let cpu = |n: &str, uid: u64, highest: u64| {
            device(
                n,
                &cat(&[
                    &name_op("_HID", &aml::asm::string("ACPI0007")),
                    &name_op("_UID", &int(uid)),
                    &cpc(int(highest)),
                ]),
            )
        };
        let body = cat(&[
            &aml::asm::scope(
                "\\_SB",
                &cat(&[
                    &cpu("CPU0", 0, 300),
                    &cpu("CPU1", 1, 150),
                    &device("CPU9", &[]),
                ]),
            ),
            // A legacy Processor object whose highest level is a register.
            &processor("CPU2", 2, &cpc(register(REG))),
        ]);
        let mut i = load(&dsdt(&body));
        let mut got = crate::cpc::levels(&mut i, &mut Quiet, &TestOps);
        got.sort_by_key(|c| c.uid);
        let highest: Vec<(u32, u64)> = got.iter().map(|c| (c.uid, c.highest)).collect();
        assert_eq!(highest, [(0, 300), (1, 150), (2, 600)]);
        assert_eq!((got[0].nominal, got[0].lowest), (Some(200), Some(10)));
    }

    /// `OperationRegion (n, space, base, len)` and a byte-access field of
    /// one-byte units over it.
    fn region_with_fields(n: &str, space: u8, base: u64, units: &[&str]) -> Vec<u8> {
        let mut list = cat(&[&name(n), &[0x01]]);
        for u in units {
            list.extend_from_slice(&name(u));
            list.push(8);
        }
        cat(&[
            &[0x5b, 0x80],
            &name(n),
            &[space],
            &int(base),
            &int(0x100),
            &aml::asm::pkg(&[0x5b, 0x81], &list),
        ])
    }

    /// The region-edge rules no corpus machine exercises: an embedded
    /// controller's region is served by the device declaring it, a PCC
    /// region by `acpi0`, and a serial-bus field without a `Connection`
    /// is reported rather than guessed.
    #[test]
    fn region_edges_by_space() {
        use devtree::graph::{EdgeKind, Origin};

        use crate::deps::{Provider, Via, build};
        use aml::RegionSpace;

        let hid = |h: &str| name_op("_HID", &aml::asm::string(h));
        let body = aml::asm::scope(
            "\\_SB",
            &cat(&[
                &device(
                    "EC0",
                    &cat(&[
                        &hid("PNP0C09"),
                        &region_with_fields("ECOR", 3, 0, &["BST0"]),
                    ]),
                ),
                &device(
                    "BAT0",
                    &cat(&[
                        &hid("PNP0C0A"),
                        &method("_BST", 0, &ret(&name("\\_SB.EC0.BST0"))),
                    ]),
                ),
                &device(
                    "CPC0",
                    &cat(&[
                        &hid("XCPC0001"),
                        &region_with_fields("PCCR", 0x0a, 3, &["CMD"]),
                    ]),
                ),
                &device(
                    "I2C1",
                    &cat(&[
                        &hid("XI2C0001"),
                        &region_with_fields("GSB", 9, 0, &["REG0"]),
                    ]),
                ),
                &device(
                    "PMIC",
                    &cat(&[
                        &hid("XPMC0001"),
                        &method("_PS0", 0, &store(&int(1), &name("^^I2C1.REG0"))),
                    ]),
                ),
            ]),
        );
        let mut i = load(&dsdt(&body));
        let devs: Vec<aml::NodeId> = ["EC0", "BAT0", "CPC0", "I2C1", "PMIC"]
            .iter()
            .map(|d| node(&i, &alloc::format!("\\_SB.{d}")))
            .collect();
        let deps = build(
            &mut i,
            &mut Quiet,
            &devs,
            &[],
            &[],
            &acpi_platform::FirmwareIdentity::default(),
        );
        let got: Vec<(aml::NodeId, aml::NodeId, EdgeKind, Via, Provider)> = deps
            .edges
            .iter()
            .map(|e| (e.supplier, e.consumer, e.kind, e.via.clone(), e.provider))
            .collect();
        let pccr = node(&i, "\\_SB.CPC0.PCCR");
        assert_eq!(
            got,
            [
                (
                    devs[0],
                    devs[1],
                    EdgeKind::Optional,
                    Via::Region(RegionSpace::EmbeddedControl),
                    Provider::Device(devs[0])
                ),
                (
                    pccr,
                    devs[2],
                    EdgeKind::Optional,
                    Via::Region(RegionSpace::Pcc),
                    Provider::Bus(pccr)
                ),
            ]
        );
        assert_eq!(Via::Region(RegionSpace::Pcc).origin(), Origin::Pcc);
        assert_eq!(
            Via::Region(RegionSpace::EmbeddedControl).origin(),
            Origin::Region
        );
        let unresolved: Vec<(aml::NodeId, alloc::string::String)> = deps
            .unresolved
            .iter()
            .map(|u| (u.consumer, u.name.clone()))
            .collect();
        assert_eq!(
            unresolved,
            [
                (
                    devs[3],
                    alloc::string::String::from("\\_SB_.I2C1.REG0 without Connection")
                ),
                (
                    devs[4],
                    alloc::string::String::from("\\_SB_.I2C1.REG0 without Connection")
                ),
            ]
        );
    }

    /// A platform for the tests below: it applies when `\_SB.TAGD` is
    /// present, gives `UAR0` a clock and a reset line from `PROV`, and
    /// knows the vendor property `x,ctl`.
    struct Fake;

    impl acpi_platform::PlatformSource for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn applies(
            &self,
            _: &acpi_platform::FirmwareIdentity,
            interp: &mut aml::Interpreter,
            _: &mut dyn aml::Host,
            devices: &[aml::NodeId],
        ) -> bool {
            devices.contains(&node(interp, "\\_SB.TAGD"))
        }

        fn extract(
            &self,
            interp: &mut aml::Interpreter,
            _: &mut dyn aml::Host,
            _: &[aml::NodeId],
        ) -> acpi_platform::Extracted {
            use devtree::platform::{ClockLine, ResetLine};
            let (uar0, prov) = (node(interp, "\\_SB.UAR0"), node(interp, "\\_SB.PROV"));
            let mut x = acpi_platform::Extracted::default();
            x.description.clocks.push(ClockLine {
                consumer: uar0,
                name: Some("core".into()),
                provider: prov,
                id: 7,
            });
            x.description.resets.push(ResetLine {
                consumer: uar0,
                name: None,
                controller: prov,
                index: 3,
            });
            x
        }

        fn reference_kinds(&self) -> &'static [(&'static str, devtree::graph::EdgeKind)] {
            &[("x,ctl", devtree::graph::EdgeKind::Required)]
        }
    }

    static FAKE: Fake = Fake;

    /// What `acpi0` does with a platform's output, whatever the platform:
    /// its lines become required edges and the child's lines (by the
    /// serving device's path), its vendor `_DSD` names classify
    /// references, and nothing of it happens when it does not apply.
    #[test]
    fn a_platform_feeds_the_walk() {
        use alloc::string::String;
        use bus::LineKind;
        use devtree::graph::{EdgeKind, Origin};

        let hid = |h: &str| name_op("_HID", &aml::asm::string(h));
        let s = |t: &str| aml::asm::string(t);
        let walk_with = |tagged: bool| {
            let dsd = name_op(
                "_DSD",
                &package(&[
                    aml::asm::buffer(&[
                        0x14, 0xd8, 0xff, 0xda, 0xba, 0x6e, 0x8c, 0x4d, 0x8a, 0x91, 0xbc, 0x9b,
                        0xbf, 0x4a, 0xa3, 0x01,
                    ]),
                    package(&[package(&[s("x,ctl"), name("\\_SB.CTL0")])]),
                ]),
            );
            let body = aml::asm::scope(
                "\\_SB",
                &cat(&[
                    &device("TAGD", &hid(if tagged { "XTAG0001" } else { "XNOT0001" })),
                    &device("PROV", &hid("XPRV0001")),
                    &device("CTL0", &hid("XCTL0001")),
                    &device("UAR0", &cat(&[&hid("XUAR0001"), &dsd])),
                ]),
            );
            let mut i = load(&dsdt(&body));
            let platforms: &[&dyn acpi_platform::PlatformSource] =
                if tagged { &[&FAKE] } else { &[] };
            crate::bus::walk(
                &mut i,
                &mut Quiet,
                &[],
                platforms,
                &acpi_platform::FirmwareIdentity::default(),
            )
        };
        let uar0 = |w: &crate::bus::Walk| {
            let f = w
                .found
                .iter()
                .find(|f| f.device.path == "\\_SB_.UAR0")
                .unwrap();
            (f.deps.clone(), f.lines.clone())
        };

        // Without the platform: an unclassified reference, no lines.
        let w = walk_with(false);
        assert_eq!(uar0(&w), (Vec::new(), Vec::new()));

        let w = walk_with(true);
        assert!(
            w.notes.iter().any(|n| n == "acpi0: platform fake"),
            "{:?}",
            w.notes
        );
        let (deps, lines) = uar0(&w);
        let prov = String::from("\\_SB_.PROV");
        assert_eq!(
            deps,
            [
                (String::from("\\_SB_.CTL0"), EdgeKind::Required, Origin::Dsd),
                (prov.clone(), EdgeKind::Required, Origin::Platform),
            ]
        );
        assert_eq!(
            lines,
            [
                (LineKind::Clock, Some(String::from("core")), prov.clone(), 7),
                (LineKind::Reset, None, prov, 3),
            ]
        );
    }
}
