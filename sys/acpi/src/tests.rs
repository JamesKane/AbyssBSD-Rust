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
    let found = crate::bus::walk(&mut i, &mut Quiet);
    let paths: std::vec::Vec<&str> = found.iter().map(|f| f.device.path.as_str()).collect();
    let mut forward = 0;
    let mut edges = 0;
    for (n, f) in found.iter().enumerate() {
        for d in &f.deps {
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
}
