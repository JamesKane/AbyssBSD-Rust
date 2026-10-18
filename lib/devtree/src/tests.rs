// SPDX-License-Identifier: BSD-2-Clause
//! Matching across drivers, with realistic tables.

use alloc::string::String;
use alloc::vec;

use crate::ids::{AcpiId, AcpiIdent, PciId, PciIdent};
use crate::matching::{Ambiguity, Candidate, Rank, Selection, find_ambiguities, select};

const NVME: &[PciId] = &[
    PciId::class(0x01_08_02),
    PciId::device(0x144d, 0xa80a).quirks(1),
];
const AHCI: &[PciId] = &[PciId::class(0x01_06_01), PciId::device(0x8086, 0x2922)];
/// A vendor driver claiming one Samsung controller more specifically.
const SAMSUNG: &[PciId] = &[PciId::device(0x144d, 0xa80a).rank(Rank::VENDOR)];
/// A driver that takes any NVMe-class device nobody else wants.
const CATCHALL: &[PciId] = &[PciId::subclass(0x01, 0x08).rank(Rank::HOOVER)];

fn pci(vendor: u16, device: u16, class: u32) -> PciIdent {
    PciIdent {
        vendor,
        device,
        subvendor: vendor,
        subdevice: 0,
        class,
        revision: 0,
    }
}

fn drivers() -> [Candidate<'static, PciId>; 4] {
    [
        Candidate {
            name: "nvme",
            ids: NVME,
            yields: &[],
        },
        Candidate {
            name: "ahci",
            ids: AHCI,
            yields: &[],
        },
        Candidate {
            name: "samsung",
            ids: SAMSUNG,
            yields: &[],
        },
        Candidate {
            name: "catchall",
            ids: CATCHALL,
            yields: &[],
        },
    ]
}

#[test]
fn best_rank_wins() {
    let d = drivers();
    // QEMU's NVMe controller: only the class matches.
    assert_eq!(
        select(&d, &pci(0x1b36, 0x0010, 0x01_08_02), |_| false),
        Selection::Driver {
            index: 0,
            rank: Rank::GENERIC
        }
    );
    // The Samsung 980: nvme's device entry (DEFAULT) beats its class
    // entry, and the vendor driver (VENDOR) beats both.
    assert_eq!(
        select(&d, &pci(0x144d, 0xa80a, 0x01_08_02), |_| false),
        Selection::Driver {
            index: 2,
            rank: Rank::VENDOR
        }
    );
    // If the vendor driver declines, the next best takes it.
    assert_eq!(
        select(&d, &pci(0x144d, 0xa80a, 0x01_08_02), |i| i == 2),
        Selection::Driver {
            index: 0,
            rank: Rank::DEFAULT
        }
    );
    // An ICH9 AHCI controller by device ID.
    assert!(matches!(
        select(&d, &pci(0x8086, 0x2922, 0x01_06_01), |_| false),
        Selection::Driver { index: 1, .. }
    ));
    assert_eq!(
        select(&d, &pci(0x1234, 0x1111, 0x03_00_00), |_| false),
        Selection::NoMatch
    );
}

#[test]
fn ties_need_yields() {
    const OTHER_NVME: &[PciId] = &[PciId::class(0x01_08_02)];
    let mut d = vec![
        Candidate {
            name: "nvme",
            ids: NVME,
            yields: &[],
        },
        Candidate {
            name: "nvme2",
            ids: OTHER_NVME,
            yields: &[],
        },
    ];
    let corpus = [
        pci(0x1b36, 0x0010, 0x01_08_02),
        pci(0x144d, 0xa80a, 0x01_08_02),
    ];
    assert_eq!(
        find_ambiguities(&d, &corpus),
        vec![Ambiguity {
            ident: 0,
            drivers: ("nvme", "nvme2"),
            rank: Rank::GENERIC
        }]
    );
    // Declaring the yield settles it, in either table position.
    d[1].yields = &["nvme"];
    assert!(find_ambiguities(&d, &corpus).is_empty());
    d.swap(0, 1);
    assert!(matches!(
        select(&d, &corpus[0], |_| false),
        Selection::Driver { index: 1, .. }
    ));
}

#[test]
fn acpi_ids_match_hid_or_cid() {
    const UART: &[AcpiId] = &[AcpiId::new("PNP0501"), AcpiId::new("ARMH0011")];
    const PCIB: &[AcpiId] = &[AcpiId::new("PNP0A03").rank(Rank::GENERIC)];
    let d = [
        Candidate {
            name: "uart",
            ids: UART,
            yields: &[],
        },
        Candidate {
            name: "pcib",
            ids: PCIB,
            yields: &[],
        },
    ];
    let host_bridge = AcpiIdent {
        hid: Some(String::from("PNP0A08")),
        cids: vec![String::from("PNP0A03")],
        uid: Some(String::from("0")),
    };
    assert_eq!(
        select(&d, &host_bridge, |_| false),
        Selection::Driver {
            index: 1,
            rank: Rank::GENERIC
        }
    );
    let com1 = AcpiIdent {
        hid: Some(String::from("PNP0501")),
        ..AcpiIdent::default()
    };
    assert!(matches!(
        select(&d, &com1, |_| false),
        Selection::Driver { index: 0, .. }
    ));
}
