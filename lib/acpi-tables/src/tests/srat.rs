// SPDX-License-Identifier: BSD-2-Clause

extern crate std;
use std::vec::Vec;

use crate::srat::*;
use crate::testutil::{B, one, torture};
use crate::{Error, Sdt};

fn body() -> Vec<u8> {
    let handle: Vec<u8> = (0u8..16).collect();
    B::new()
        .u32(1)
        .u64(0)
        // Domain 0x0302_0105 split as low byte 0x05 and high bytes 01 02 03.
        .entry(
            0,
            &B::new()
                .u8(0x05)
                .u8(7)
                .u32(1)
                .u8(0)
                .bytes(&[1, 2, 3])
                .u32(9),
        )
        .entry(
            1,
            &B::new()
                .u32(1)
                .u16(0)
                .u32(0)
                .u32(1)
                .u32(0x8000_0000)
                .u32(0)
                .u32(0)
                .u32(1 | 2 | 4)
                .u64(0),
        )
        .entry(2, &B::new().u16(0).u32(2).u32(0x100).u32(0).u32(4).u32(0))
        .entry(3, &B::new().u32(3).u32(17).u32(1).u32(0))
        .entry(4, &B::new().u32(4).u16(0).u32(6))
        .entry(5, &B::new().u8(0).u8(1).u32(5).bytes(&handle).u32(3).u32(0))
        .entry(6, &B::new().zeros(30))
        .0
}

#[test]
fn decodes_entries() {
    let m = one(*b"SRAT", 3, &body());
    let s = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    let e: Vec<_> = s.entries().map(Result::unwrap).collect();
    assert_eq!(e.len(), 7);
    assert_eq!(
        e[0],
        SratEntry::ProcessorLocalApic(ProcessorLocalApic {
            domain: 0x0302_0105,
            apic_id: 7,
            enabled: true,
            sapic_eid: 0,
            clock_domain: 9,
        })
    );
    let SratEntry::MemoryAffinity(mem) = e[1] else {
        panic!("{:?}", e[1])
    };
    assert_eq!(
        (mem.domain, mem.base, mem.length),
        (1, 1 << 32, 0x8000_0000)
    );
    assert!(mem.flags.enabled() && mem.flags.hot_pluggable() && mem.flags.non_volatile());
    assert_eq!(
        e[2],
        SratEntry::ProcessorLocalX2Apic(ProcessorLocalX2Apic {
            domain: 2,
            x2apic_id: 0x100,
            enabled: false,
            clock_domain: 4,
        })
    );
    assert_eq!(
        e[3],
        SratEntry::GiccAffinity(GiccAffinity {
            domain: 3,
            processor_uid: 17,
            enabled: true,
            clock_domain: 0,
        })
    );
    assert_eq!(
        e[4],
        SratEntry::GicItsAffinity(GicItsAffinity {
            domain: 4,
            its_id: 6
        })
    );
    let SratEntry::GenericInitiator(gi) = e[5] else {
        panic!()
    };
    assert_eq!((gi.handle_type, gi.domain), (1, 5));
    assert_eq!(gi.handle, (0u8..16).collect::<Vec<_>>().as_slice());
    assert!(gi.enabled && gi.architectural_transactions);
    assert!(matches!(e[6], SratEntry::Other { kind: 6, bytes } if bytes.len() == 32));
}

#[test]
fn malformed_entries() {
    // Zero length ends the walk.
    let b = B::new()
        .zeros(12)
        .entry(4, &B::new().u32(4).u16(0).u32(6))
        .bytes(&[1, 0])
        .0;
    let m = one(*b"SRAT", 3, &b);
    let s = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    let e: Vec<_> = s.entries().collect();
    assert_eq!(e.len(), 2);
    assert_eq!(e[1], Err(Error::Malformed("SRAT entry length")));
    // A memory entry missing its flags is an error; the walk goes on.
    let b = B::new()
        .zeros(12)
        .entry(1, &B::new().zeros(24))
        .entry(4, &B::new().u32(4).u16(0).u32(6))
        .0;
    let m = one(*b"SRAT", 3, &b);
    let s = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    let e: Vec<_> = s.entries().collect();
    assert_eq!(e[0], Err(Error::Malformed("SRAT entry too short")));
    assert!(e[1].is_ok());
    // Overrun.
    let b = B::new().zeros(12).bytes(&[1, 40, 0, 0]).0;
    let m = one(*b"SRAT", 3, &b);
    let s = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(
        s.entries().collect::<Vec<_>>(),
        [Err(Error::Malformed("SRAT entry length"))]
    );
}

#[test]
fn rejects_short_and_wrong() {
    let m = one(*b"SRAT", 3, &[0; 11]);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap()),
        Err(Error::BadLength(*b"SRAT"))
    );
    let m = one(*b"SRAT", 3, &[0; 12]);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap())
            .unwrap()
            .entries()
            .count(),
        0
    );
    let m = one(*b"SLIT", 3, &body());
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
}

#[test]
fn survives_corruption() {
    torture(*b"SRAT", 3, &body());
}
