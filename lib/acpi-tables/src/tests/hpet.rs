// SPDX-License-Identifier: BSD-2-Clause

extern crate std;
use std::vec::Vec;

use crate::hpet::*;
use crate::testutil::{B, gas, one, torture};
use crate::{AddressSpace, Error, Sdt};

fn body() -> Vec<u8> {
    // Intel, 3 comparators (field 2), 64-bit, legacy capable, revision 1.
    B::new()
        .u32(0x8086_a201)
        .bytes(&gas(0, 64, 0, 0xfed0_0000))
        .u8(0)
        .u16(0x80)
        .u8(0x21)
        .0
}

#[test]
fn decodes() {
    let m = one(*b"HPET", 1, &body());
    let h = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(h.block_id.hardware_revision(), 1);
    assert_eq!(h.block_id.comparators(), 3);
    assert!(h.block_id.counter_64bit());
    assert!(h.block_id.legacy_replacement());
    assert_eq!(h.block_id.vendor_id(), 0x8086);
    assert_eq!(h.base.space, AddressSpace::Memory);
    assert_eq!(h.base.address, 0xfed0_0000);
    assert_eq!(h.number, 0);
    assert_eq!(h.min_tick, 0x80);
    assert_eq!(h.page_protection.protected_bytes(), Some(4096));
    assert_eq!(h.page_protection.oem_attributes(), 2);
    assert_eq!(PageProtection(2).protected_bytes(), Some(65_536));
    assert_eq!(PageProtection(0).protected_bytes(), None);
    assert_eq!(PageProtection(3).protected_bytes(), None);
    let narrow = BlockId(0x1166_0000 | (31 << 8));
    assert_eq!(narrow.comparators(), 32);
    assert!(!narrow.counter_64bit() && !narrow.legacy_replacement());
}

#[test]
fn rejects_short_and_wrong() {
    let mut b = body();
    b.pop();
    let m = one(*b"HPET", 1, &b);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap()),
        Err(Error::BadLength(*b"HPET"))
    );
    let m = one(*b"HPEX", 1, &body());
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
}

#[test]
fn survives_corruption() {
    torture(*b"HPET", 1, &body());
}
