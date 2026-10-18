// SPDX-License-Identifier: BSD-2-Clause

extern crate std;
use std::vec::Vec;

use crate::madt::*;
use crate::testutil::{B, one, torture};
use crate::{Error, Sdt};

fn header(lapic: u32, flags: u32) -> B {
    B::new().u32(lapic).u32(flags)
}

fn lapic(uid: u8, id: u8, flags: u32) -> B {
    B::new().u8(uid).u8(id).u32(flags)
}

/// A GICC with every field through TRBE, cut to `len` bytes.
fn gicc(len: u8, uid: u32, flags: u32, mpidr: u64) -> Vec<u8> {
    let mut b = B::new()
        .u8(11)
        .u8(len)
        .u16(0)
        .u32(uid + 100) // CPU interface number
        .u32(uid)
        .u32(flags)
        .u32(1) // parking protocol version
        .u32(23) // performance interrupt
        .u64(0x8000_0000)
        .u64(0x0801_0000)
        .u64(0x0804_0000)
        .u64(0x0803_0000)
        .u32(25)
        .u64(0x080a_0000)
        .u64(mpidr)
        .u8(2) // efficiency class
        .u8(0)
        .u16(21) // SPE
        .u16(22) // TRBE
        .0;
    assert_eq!(b.len(), 82);
    b.truncate(usize::from(len));
    b
}

fn x86_body() -> Vec<u8> {
    header(0xfee0_0000, 1)
        .entry(0, &lapic(0, 0, 1))
        .entry(0, &lapic(1, 1, 2))
        .entry(0, &lapic(2, 2, 0))
        .entry(1, &B::new().u8(4).u8(0).u32(0xfec0_0000).u32(0))
        .entry(2, &B::new().u8(0).u8(0).u32(2).u16(0))
        .entry(2, &B::new().u8(0).u8(9).u32(9).u16(0xd))
        .entry(3, &B::new().u16(0xf).u32(5))
        .entry(4, &B::new().u8(0xff).u16(5).u8(1))
        .entry(9, &B::new().u16(0).u32(0x100).u32(1).u32(3))
        .entry(10, &B::new().u16(0).u32(0xffff_ffff).u8(1).zeros(3))
        .entry(5, &B::new().u16(0).u64(0x1_fee0_0000))
        .0
}

#[test]
fn decodes_x86_entries() {
    let m = one(*b"APIC", 5, &x86_body());
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.local_interrupt_controller_address(), 0xfee0_0000);
    assert!(t.pcat_compat());
    assert_eq!(t.local_apic_address(), 0x1_fee0_0000);
    let e: Vec<_> = t.entries().map(Result::unwrap).collect();
    assert_eq!(e.len(), 11);
    assert_eq!(
        e[0],
        MadtEntry::LocalApic(LocalApic {
            processor_uid: 0,
            apic_id: 0,
            flags: ProcessorFlags(1)
        })
    );
    assert_eq!(
        e[3],
        MadtEntry::IoApic(IoApic {
            id: 4,
            address: 0xfec0_0000,
            gsi_base: 0
        })
    );
    let MadtEntry::InterruptSourceOverride(iso) = e[5] else {
        panic!("{:?}", e[5])
    };
    assert_eq!((iso.source, iso.gsi), (9, 9));
    assert_eq!(iso.flags.polarity(), Polarity::ActiveHigh);
    assert_eq!(iso.flags.trigger(), Trigger::Level);
    let MadtEntry::InterruptSourceOverride(iso) = e[4] else {
        panic!()
    };
    assert_eq!(iso.flags.polarity(), Polarity::Conforming);
    assert_eq!(iso.flags.trigger(), Trigger::Conforming);
    assert_eq!(
        e[6],
        MadtEntry::NmiSource(NmiSource {
            flags: MpsIntiFlags(0xf),
            gsi: 5
        })
    );
    assert_eq!(
        e[7],
        MadtEntry::LocalApicNmi(LocalApicNmi {
            processor_uid: 0xff,
            flags: MpsIntiFlags(5),
            lint: 1
        })
    );
    assert_eq!(
        e[8],
        MadtEntry::LocalX2Apic(LocalX2Apic {
            x2apic_id: 0x100,
            flags: ProcessorFlags(1),
            processor_uid: 3
        })
    );
    assert_eq!(
        e[9],
        MadtEntry::LocalX2ApicNmi(LocalX2ApicNmi {
            flags: MpsIntiFlags(0),
            processor_uid: 0xffff_ffff,
            lint: 1
        })
    );
    // uid 0 enabled, uid 1 online capable, uid 2 disabled, x2APIC enabled.
    assert_eq!(t.enabled_cpu_count(), 3);
    let cpus: Vec<_> = t.cpus().collect();
    assert_eq!(cpus.len(), 4);
    assert_eq!(cpus[3].hw_id, HwId::Apic(0x100));
    assert!(cpus[1].online_capable && !cpus[1].enabled);
    assert_eq!(t.gic_distributor(), None);
}

#[test]
fn decodes_gic_entries_of_every_revision() {
    let body = header(0, 0)
        .bytes(&gicc(82, 0, 1, 0x0000))
        .bytes(&gicc(80, 1, 1 | 8, 0x0001))
        .bytes(&gicc(76, 2, 0, 0x0100))
        .bytes(&gicc(40, 3, 1, 0))
        .entry(
            12,
            &B::new()
                .u16(0)
                .u32(0)
                .u64(0x0800_0000)
                .u32(0)
                .u8(3)
                .zeros(3),
        )
        .entry(
            13,
            &B::new()
                .u16(0)
                .u32(7)
                .u64(0x0802_0000)
                .u32(1)
                .u16(64)
                .u16(80),
        )
        .entry(
            13,
            &B::new()
                .u16(0)
                .u32(8)
                .u64(0x0803_0000)
                .u32(0)
                .u16(64)
                .u16(80),
        )
        .entry(14, &B::new().u16(0).u64(0x080a_0000).u32(0xf6_0000))
        .entry(15, &B::new().u16(0).u32(0).u64(0x0808_0000).u32(0))
        .entry(0x7f, &B::new().u16(0xabcd))
        .0;
    let m = one(*b"APIC", 6, &body);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert!(!t.pcat_compat());
    let e: Vec<_> = t.entries().map(Result::unwrap).collect();
    let g: Vec<_> = e
        .iter()
        .filter_map(|e| match e {
            MadtEntry::GicCpuInterface(g) => Some(*g),
            _ => None,
        })
        .collect();
    assert_eq!(g.len(), 4);
    // ACPI 6.5: everything.
    assert_eq!(g[0].cpu_interface_number, 100);
    assert_eq!(g[0].parking_protocol_version, 1);
    assert_eq!(g[0].performance_interrupt, 23);
    assert_eq!(g[0].parked_address, 0x8000_0000);
    assert_eq!(g[0].physical_base, 0x0801_0000);
    assert_eq!(g[0].gicv, Some(0x0804_0000));
    assert_eq!(g[0].gich, Some(0x0803_0000));
    assert_eq!(g[0].vgic_maintenance_interrupt, Some(25));
    assert_eq!(g[0].gicr_base, Some(0x080a_0000));
    assert_eq!(g[0].mpidr, Some(0));
    assert_eq!(g[0].efficiency_class, Some(2));
    assert_eq!(g[0].spe_overflow_interrupt, Some(21));
    assert_eq!(g[0].trbe_interrupt, Some(22));
    // ACPI 6.3: no TRBE.
    assert_eq!(g[1].spe_overflow_interrupt, Some(21));
    assert_eq!(g[1].trbe_interrupt, None);
    assert!(g[1].flags.enabled() && g[1].flags.online_capable());
    // ACPI 5.1: no efficiency class.
    assert_eq!(g[2].mpidr, Some(0x100));
    assert_eq!(g[2].efficiency_class, None);
    assert_eq!(g[2].spe_overflow_interrupt, None);
    assert!(!g[2].flags.enabled());
    // ACPI 5.0: no MPIDR.
    assert_eq!(g[3].physical_base, 0x0801_0000);
    assert_eq!((g[3].gicv, g[3].mpidr), (None, None));
    assert_eq!(
        e[4],
        MadtEntry::GicDistributor(GicDistributor {
            id: 0,
            physical_base: 0x0800_0000,
            version: 3
        })
    );
    assert_eq!(
        e[5],
        MadtEntry::GicMsiFrame(GicMsiFrame {
            id: 7,
            physical_base: 0x0802_0000,
            spis: Some((64, 80))
        })
    );
    assert!(matches!(
        e[6],
        MadtEntry::GicMsiFrame(GicMsiFrame { spis: None, .. })
    ));
    assert_eq!(
        e[7],
        MadtEntry::GicRedistributor(GicRedistributor {
            base: 0x080a_0000,
            length: 0xf6_0000
        })
    );
    assert_eq!(
        e[8],
        MadtEntry::GicIts(GicIts {
            id: 0,
            physical_base: 0x0808_0000
        })
    );
    assert_eq!(
        e[9],
        MadtEntry::Other {
            kind: 0x7f,
            bytes: &[0x7f, 4, 0xcd, 0xab]
        }
    );
    // The disabled GICC is not counted; online-capable alone is not enough
    // for GICC, but the second entry is also enabled.
    assert_eq!(t.enabled_cpu_count(), 3);
    let cpus: Vec<_> = t.cpus().collect();
    assert_eq!(cpus[2].hw_id, HwId::Mpidr(0x100));
    assert_eq!(cpus[3].hw_id, HwId::GicCpuInterface(103));
    assert_eq!(t.gic_distributor().map(|d| d.version), Some(3));
}

#[test]
fn zero_and_one_length_entries_end_the_walk() {
    for bad_len in [0u8, 1] {
        let mut body = header(0, 0).entry(0, &lapic(0, 0, 1)).0;
        body.extend_from_slice(&[0, bad_len, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(&lapic(1, 1, 1).0);
        let m = one(*b"APIC", 5, &body);
        let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
        let e: Vec<_> = t.entries().collect();
        assert_eq!(e.len(), 2);
        assert!(e[0].is_ok());
        assert_eq!(e[1], Err(Error::Malformed("MADT entry length")));
        assert_eq!(t.enabled_cpu_count(), 1);
    }
}

#[test]
fn overrunning_and_dangling_entries_end_the_walk() {
    let mut body = header(0, 0).entry(0, &lapic(0, 0, 1)).0;
    body.extend_from_slice(&[0, 200, 0, 0]);
    let m = one(*b"APIC", 5, &body);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.entries().count(), 2);
    assert!(t.entries().nth(1).unwrap().is_err());
    // A single trailing byte cannot hold a header.
    let mut body = header(0, 0).entry(0, &lapic(0, 0, 1)).0;
    body.push(0);
    let m = one(*b"APIC", 5, &body);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    let e: Vec<_> = t.entries().collect();
    assert_eq!(e.len(), 2);
    assert!(e[1].is_err());
}

#[test]
fn short_known_entry_is_an_error_but_the_walk_continues() {
    let body = header(0, 0)
        .entry(0, &B::new().u8(0).u8(0).u16(1)) // 6 bytes, flags cut short
        .entry(0, &lapic(1, 1, 1))
        .bytes(&gicc(39, 2, 1, 0))
        .entry(0, &lapic(3, 3, 1))
        .0;
    let m = one(*b"APIC", 5, &body);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    let e: Vec<_> = t.entries().collect();
    assert_eq!(e.len(), 4);
    assert_eq!(e[0], Err(Error::Malformed("MADT entry too short")));
    assert!(e[1].is_ok());
    assert!(e[2].is_err());
    assert!(e[3].is_ok());
    // Helpers stop at the first error.
    assert_eq!(t.enabled_cpu_count(), 0);
}

#[test]
fn rejects_wrong_signature_and_short_header() {
    let m = one(*b"APIX", 5, &x86_body());
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
    let m = one(*b"APIC", 5, &[0; 7]);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap()),
        Err(Error::BadLength(*b"APIC"))
    );
    // A header with no entries is fine.
    let m = one(*b"APIC", 5, &[0; 8]);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.entries().count(), 0);
    assert_eq!(t.local_apic_address(), 0);
}

#[test]
fn survives_corruption() {
    torture(*b"APIC", 5, &x86_body());
    let arm = header(0, 0)
        .bytes(&gicc(82, 0, 1, 0))
        .entry(
            12,
            &B::new()
                .u16(0)
                .u32(0)
                .u64(0x0800_0000)
                .u32(0)
                .u8(3)
                .zeros(3),
        )
        .0;
    torture(*b"APIC", 6, &arm);
}
