// SPDX-License-Identifier: BSD-2-Clause

extern crate std;
use std::vec::Vec;

use crate::mcfg::*;
use crate::testutil::{B, one, torture};
use crate::{Error, Sdt};

fn alloc(b: B, base: u64, seg: u16, start: u8, end: u8) -> B {
    b.u64(base).u16(seg).u8(start).u8(end).u32(0)
}

fn body() -> Vec<u8> {
    let b = alloc(B::new().zeros(8), 0xb000_0000, 0, 0, 0xff);
    alloc(b, 0x40_1000_0000, 1, 0x10, 0x1f).0
}

#[test]
fn decodes_regions() {
    let m = one(*b"MCFG", 1, &body());
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    let r: Vec<_> = t.regions().map(Result::unwrap).collect();
    assert_eq!(
        r,
        [
            EcamRegion {
                base: 0xb000_0000,
                segment: 0,
                start_bus: 0,
                end_bus: 0xff
            },
            EcamRegion {
                base: 0x40_1000_0000,
                segment: 1,
                start_bus: 0x10,
                end_bus: 0x1f
            },
        ]
    );
    assert_eq!(r[0].bus_count(), 256);
    assert_eq!(r[1].bus_count(), 16);
    assert_eq!(r[0].function_address(0, 0, 0), Some(0xb000_0000));
    assert_eq!(
        r[0].function_address(1, 2, 3),
        Some(0xb000_0000 + (1 << 20) + (2 << 15) + (3 << 12))
    );
    assert_eq!(
        r[1].function_address(0x10, 0, 0),
        Some(0x40_1000_0000 + (0x10 << 20))
    );
    assert_eq!(r[1].function_address(0x0f, 0, 0), None);
    assert_eq!(r[1].function_address(0x20, 0, 0), None);
    assert_eq!(r[0].function_address(0, 32, 0), None);
    assert_eq!(r[0].function_address(0, 0, 8), None);
    let top = EcamRegion {
        base: u64::MAX - 4095,
        segment: 0,
        start_bus: 0,
        end_bus: 1,
    };
    assert_eq!(top.function_address(1, 0, 0), None);
}

#[test]
fn inverted_bus_range_is_an_error_for_that_entry_only() {
    let b = alloc(B::new().zeros(8), 0x1000, 0, 5, 4);
    let b = alloc(b, 0x2000, 0, 0, 0);
    let m = one(*b"MCFG", 1, &b.0);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    let r: Vec<_> = t.regions().collect();
    assert_eq!(r[0], Err(Error::Malformed("MCFG bus range")));
    assert_eq!(r[1].map(|r| r.base), Ok(0x2000));
}

#[test]
fn rejects_ragged_or_short_tables() {
    let mut b = body();
    b.pop();
    let m = one(*b"MCFG", 1, &b);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap()),
        Err(Error::BadLength(*b"MCFG"))
    );
    let m = one(*b"MCFG", 1, &[0; 7]);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap()),
        Err(Error::BadLength(*b"MCFG"))
    );
    let m = one(*b"MCFG", 1, &[0; 8]);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap())
            .unwrap()
            .regions()
            .count(),
        0
    );
    let m = one(*b"MCFX", 1, &body());
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
}

#[test]
fn survives_corruption() {
    torture(*b"MCFG", 1, &body());
}
