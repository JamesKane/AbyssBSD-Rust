// SPDX-License-Identifier: BSD-2-Clause
//! Shared ranges against the firmware corpus (`tests/corpus/acpi`): which
//! devices' ranges AML also touches, and which overlap each other, with
//! no platform registered. What a platform declares is tested in its
//! crate.

extern crate std;

use alloc::string::String;
use alloc::vec::Vec;

use bus::Range;

use crate::ranges::{Party, consumed, firmware_regions, overlaps, system_resource};

/// Pairs of namespace paths.
type Pairs = Vec<(String, String)>;

/// A machine's overlaps among the devices `acpi0` adds (system-resource
/// devices left out), by path: each device whose ranges AML touches,
/// with the region, and each pair of devices whose ranges overlap.
fn report(name: &str) -> (Pairs, Pairs) {
    let (mut i, mut h) = acpi_corpus::load(name);
    let devices = crate::deps::devices(&mut i, &mut h);
    let mut ranges: Vec<(aml::NodeId, Vec<Range>)> = Vec::new();
    for d in devices {
        let ident = crate::bus::enumerated(&mut i, &mut h, d).unwrap();
        if !system_resource(&ident) {
            let crs = i.crs(d, &mut h).ok().flatten().unwrap_or_default();
            ranges.push((d, consumed(&crs)));
        }
    }
    let path = |n| i.path(n).to_text();
    let (mut firmware, mut devices) = (Vec::new(), Vec::new());
    for o in overlaps(&ranges, &firmware_regions(&i), &[]) {
        let (list, other) = match o.with {
            Party::Firmware(n) => (&mut firmware, n),
            Party::Device(n) | Party::Declared(n) => (&mut devices, n),
        };
        let pair = (path(o.device), path(other));
        if !list.contains(&pair) {
            list.push(pair);
        }
    }
    (firmware, devices)
}

fn pairs(list: &[(&str, &str)]) -> Pairs {
    list.iter()
        .map(|&(a, b)| (String::from(a), String::from(b)))
        .collect()
}

/// The devices whose registers AML also reads or writes. A driver for one
/// must ask for its range knowingly.
#[test]
fn firmware_shared() {
    let want: &[(&str, &[(&str, &str)])] = &[
        // The PCI hotplug containers' ports, which QEMU's AML drives.
        (
            "qemu-q35",
            &[
                ("\\_SB_.PCI0.PRES", "\\_SB_.PCI0.PRES.PRST"),
                ("\\_SB_.PCI0.PHPR", "\\_SB_.PCI0.PCST"),
                ("\\_SB_.PCI0.PHPR", "\\_SB_.PCI0.SEJ_"),
                ("\\_SB_.PCI0.PHPR", "\\_SB_.PCI0.BNMR"),
            ],
        ),
        ("qemu-virt", &[]),
        ("bhyve", &[]),
        // The GPU's power and clock controls, and the UCSI mailbox.
        (
            "radxa-q8b",
            &[
                ("\\_SB_.GPU0", "\\_SB_.CP35"),
                ("\\_SB_.GPU0", "\\_SB_.CP36"),
                ("\\_SB_.UCS0", "\\_SB_.UCS0.USBC"),
            ],
        ),
        // Power resources that write a control register inside the
        // device's own window: the audio clocks, PCIe RC0, the VPU, the
        // GPU.
        (
            "opi6p",
            &[
                ("\\_SB_.DSP_", "\\_SB_.ADSS.ACLK.PPRS.OPR0"),
                ("\\_SB_.PRC0", "\\_SB_.PRC0.OPR0"),
                ("\\_SB_.VPU0", "\\_SB_.VPU0.PPRS.OPR0"),
                ("\\_SB_.GPU_", "\\_SB_.GPUP.PPRS.OPR0"),
            ],
        ),
        // POST codes at port 0x80 (dword fields reaching 0x83) over the
        // legacy DMA controller's page registers. `SSMI` and `P1E0` sit
        // at port 0 only because their bases come from firmware state
        // the corpus host leaves zero. The FCH's GPIO and I2C
        // controllers lie inside the 8 MiB `SMIC` region but outside
        // every field declared on it, so they are not shared.
        (
            "x870e",
            &[
                ("\\_SB_.PCI0.SBRG.DMAD", "\\_SB_.SSMI"),
                ("\\_SB_.PCI0.SBRG.DMAD", "\\P1E0"),
                ("\\_SB_.PCI0.SBRG.DMAD", "\\_SB_.PCI0.GP17.ACP_.VARM"),
                ("\\_SB_.PCI0.SBRG.DMAD", "\\_SB_.A001"),
                ("\\_SB_.PCI0.SBRG.DMAD", "\\_SB_.A003"),
                ("\\_SB_.PCI0.SBRG.DMAD", "\\DEB2"),
            ],
        ),
    ];
    let names: Vec<_> = acpi_corpus::machines().into_iter().map(|m| m.0).collect();
    assert_eq!(names, want.iter().map(|w| w.0).collect::<Vec<_>>());
    for &(name, list) in want {
        assert_eq!(report(name).0, pairs(list), "{name}");
    }
}

/// Devices whose ranges overlap another's, before any platform declares
/// them. Only the first driver to map an undeclared overlap gets it.
#[test]
fn device_overlaps() {
    // The PS/2 controller's ports, which both of its devices list.
    assert_eq!(
        report("bhyve").1,
        pairs(&[("\\_SB_.PC00.ISA_.KBD_", "\\_SB_.PC00.ISA_.MOU_")])
    );
    for name in ["qemu-q35", "qemu-virt", "radxa-q8b", "x870e"] {
        assert_eq!(report(name).1, [], "{name}");
    }
    // The Sky1's register blocks shared by design: reset and clock units
    // in one page, mailboxes, CRUs, HDA, DMA and I2S inside the audio
    // subsystem's window, PCIe controllers' shared control block, xHCI
    // inside its Cadence USB wrapper, and the SCMI shared memory inside
    // its mailbox (which `platform/cix-sky1` declares).
    let sky1 = report("opi6p").1;
    assert_eq!(sky1.len(), 33, "{sky1:?}");
    for pair in [
        ("\\_SB_.MBX6", "\\_SB_.SHM0"),
        ("\\_SB_.MBX7", "\\_SB_.SHM1"),
        ("\\_SB_.XHC0", "\\_SB_.SUB0.CUB0"),
        ("\\_SB_.DSP_", "\\_SB_.I2S5"),
    ] {
        assert!(sky1.contains(&(pair.0.into(), pair.1.into())), "{pair:?}");
    }
}

/// Field extents, not whole regions: a region much larger than its
/// fields shares only the fields' bytes.
#[test]
fn only_field_extents_are_shared() {
    let (i, _) = acpi_corpus::load("x870e");
    let regions = firmware_regions(&i);
    let smic: Vec<Range> = regions
        .iter()
        .filter(|r| i.path(r.region).to_text() == "\\_SB_.SMIC")
        .map(|r| r.range)
        .collect();
    assert!(!smic.is_empty());
    let total: u64 = smic.iter().map(|r| r.len).sum();
    assert!(total < 0x1000, "{smic:?}");
    // The Sky1's SCMI mailbox for AML (0xa0 bytes): its header and
    // message, and the doorbell at 0x80, but not the words between that
    // no field names.
    let (i, _) = acpi_corpus::load("opi6p");
    let mbxo: Vec<(u64, u64)> = firmware_regions(&i)
        .iter()
        .filter(|r| i.path(r.region).to_text() == "\\_SB_.PMMX.MBXO")
        .map(|r| (r.range.base, r.range.len))
        .collect();
    assert_eq!(
        mbxo,
        [(0x065d_0000, 8), (0x065d_000c, 0x70), (0x065d_0080, 4)]
    );
}
