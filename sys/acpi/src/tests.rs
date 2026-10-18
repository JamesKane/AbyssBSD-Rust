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
