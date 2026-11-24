// SPDX-License-Identifier: MIT
// Copyright Advanced Micro Devices, Inc.
// Copyright (c) 2026 The AbyssBSD Project
//! Tests for the discovery table: the two real Raphael captures (the
//! specification's section 9.1 vectors) and hostile variants.

use alloc::vec::Vec;

use super::*;

const FIRST: &[u8] = include_bytes!("../../../tests/corpus/x870e/linux/amdgpu/discovery.bin");
const SECOND: &[u8] =
    include_bytes!("../../../tests/corpus/x870e/linux/amdgpu-igpu-primary/discovery.bin");

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

/// Fixes the binary's checksum after a test changed bytes inside it.
fn refix(b: &mut [u8]) {
    let size = usize::from(u16::from_le_bytes([b[0x0a], b[0x0b]]));
    let sum = sum16(&b[0x0a..size]);
    put16(b, 8, sum);
}

#[test]
fn the_raphael_table_as_the_specification_gives_it() {
    let d = parse(FIRST).unwrap();
    assert_eq!(d.version, (1, 1));
    assert_eq!(d.dies.len(), 1);
    assert_eq!(d.dies[0].ips.len(), 30);
    assert!(d.harvest.is_empty());
    let v = |hw, inst| d.ip(hw, inst).unwrap().version();
    assert_eq!(v(hw::GC, 0), (10, 3, 6));
    assert_eq!(v(hw::SDMA0, 0), (5, 2, 6));
    assert_eq!(v(hw::MMHUB, 0), (2, 4, 1));
    assert_eq!(v(hw::ATHUB, 0), (2, 4, 1));
    assert_eq!(v(hw::OSSSYS, 0), (5, 2, 1));
    assert_eq!(v(hw::HDP, 0), (5, 2, 0));
    assert_eq!(v(hw::NBIF, 0), (7, 3, 0));
    assert_eq!(v(hw::MP0, 0), (13, 0, 5));
    assert_eq!(v(hw::MP1, 0), (13, 0, 5));
    assert_eq!(v(hw::DMU, 0), (3, 1, 5));
    assert_eq!(v(hw::VCN, 0), (3, 1, 2));
    assert_eq!(v(hw::UMC, 0), (9, 5, 0));
    assert_eq!(v(hw::UMC, 1), (9, 5, 0));
}

#[test]
fn the_segment_bases_of_section_9_1() {
    let d = parse(FIRST).unwrap();
    let bases = |hw, inst| d.ip(hw, inst).unwrap().bases.clone();
    assert_eq!(bases(hw::GC, 0), [0x1260, 0xa000, 0x240_2c00]);
    assert_eq!(bases(hw::SDMA0, 0), [0x1260, 0xa000, 0x240_2c00]);
    assert_eq!(bases(hw::MMHUB, 0), [0x13200, 0x1a000, 0x240_8800]);
    assert_eq!(bases(hw::ATHUB, 0), [0xc00, 0x13300, 0x240_8c00]);
    assert_eq!(bases(hw::OSSSYS, 0), [0x10a0, 0x240_a000]);
    assert_eq!(bases(hw::HDP, 0), [0xf20, 0x240_a400]);
    assert_eq!(
        bases(hw::NBIF, 0),
        [0x0, 0x14, 0xd20, 0x10400, 0x241_b000, 0x404_0000]
    );
    for id in [hw::MP0, hw::MP1, hw::MP2] {
        assert_eq!(
            bases(id, 0),
            [0x16000, 0xdc_0000, 0xe0_0000, 0xe4_0000, 0x243_fc00]
        );
    }
    assert_eq!(bases(hw::DMU, 0), [0x12, 0xc0, 0x34c0, 0x9000, 0x240_3c00]);
    assert_eq!(bases(hw::VCN, 0), [0x7800, 0x7e00, 0x240_3000]);
    assert_eq!(
        bases(hw::SMUIO, 0),
        [0x16800, 0x16a00, 0x1cc00, 0x44_0000, 0x240_1000, 0x242_d400]
    );
    assert_eq!(bases(hw::THM, 0), [0x16600, 0x240_0c00]);
    assert_eq!(bases(hw::UMC, 0), [0x14000, 0x242_5800]);
    assert_eq!(bases(hw::UMC, 1), [0x54000, 0x242_5c00]);
    assert_eq!(d.base(hw::DMU, 0, 4), Some(0x240_3c00));
    assert_eq!(d.base(hw::DMU, 0, 5), None);
}

#[test]
fn the_gc_info_table() {
    let gc = parse(FIRST).unwrap().gc.unwrap();
    assert_eq!(gc.version, (1, 1));
    assert_eq!(gc.shader_engines, 1);
    assert_eq!((gc.wgp0_per_sa, gc.wgp1_per_sa), (1, 0));
    assert_eq!(gc.cus_per_sa(), 2, "Linux reports 2 active CUs");
    assert_eq!((gc.rbs_per_se, gc.l2_channels, gc.gprs), (1, 2, 1024));
    assert_eq!(
        (gc.wave_size, gc.max_waves_per_simd, gc.lds_kib),
        (32, 16, 64)
    );
    assert_eq!(gc.shader_arrays_per_se, 1);
    // The three words v1.1 adds (TCPs per SA, SDP interfaces, TCP count)
    // follow the 17 named above.
    assert_eq!(gc.rest.len(), 5);
}

#[test]
fn both_boots_parse_to_the_same_table() {
    // They differ past `binary_size`, which the parser ignores.
    assert_ne!(FIRST, SECOND);
    assert_eq!(parse(FIRST).unwrap(), parse(SECOND).unwrap());
    // The header alone says how much to read.
    let head: [u8; 12] = FIRST[..12].try_into().unwrap();
    assert_eq!(binary_size(&head), Ok(1032));
    assert_eq!(FIRST[..1032], SECOND[..1032]);
    // Reading only what the header says is enough.
    assert!(parse(&FIRST[..1032]).is_ok());
}

#[test]
fn the_seventh_slot_of_a_version_1_header_is_not_a_table() {
    // Slot 6 sits at 0x3c, where the IP table starts: its "offset" is the
    // table's signature bytes, 0x5049.
    assert_eq!(u16::from_le_bytes([FIRST[0x3c], FIRST[0x3d]]), 0x5049);
    let b = &FIRST[..1032];
    let s = slots(b, 1032, 1).unwrap();
    assert_eq!(s.len(), 7);
    assert!(s[0].is_some() && s[1].is_some() && s[2].is_some());
    assert!(s[3..].iter().all(Option::is_none), "{s:?}");
    // Absent slots 3 to 5 have offset 0; slot 6 overlaps.
    assert_eq!(s[0].unwrap().offset, 0x3c);
    assert_eq!(s[1].unwrap().offset, 0x31c);
    assert_eq!(s[2].unwrap().offset, 0x380);
}

#[test]
fn headers_that_are_not_tables() {
    assert_eq!(parse(&[]).unwrap_err(), Error::Truncated);
    assert_eq!(parse(&[0; 11]).unwrap_err(), Error::Truncated);
    assert_eq!(parse(&[0; 12]).unwrap_err(), Error::BadSignature);
    let mut b = FIRST.to_vec();
    b[0] ^= 1;
    assert_eq!(parse(&b).unwrap_err(), Error::BadSignature);
    // binary_size of 11, 0, 10 KiB + 1, and more than was given.
    for size in [0u16, 11, 10 * 1024 + 1, 0xffff] {
        let mut b = FIRST.to_vec();
        put16(&mut b, 0x0a, size);
        assert_eq!(parse(&b).unwrap_err(), Error::BadSize, "{size}");
    }
    assert_eq!(parse(&FIRST[..1000]).unwrap_err(), Error::BadSize);
}

#[test]
fn every_checksum_is_checked() {
    // The binary's.
    let mut b = FIRST.to_vec();
    b[0x300] ^= 1;
    assert_eq!(parse(&b).unwrap_err(), Error::BadChecksum("binary"));
    // The IP table's: change a byte and keep the binary's own sum right.
    let mut b = FIRST.to_vec();
    b[0x100] ^= 1;
    refix(&mut b);
    assert_eq!(parse(&b).unwrap_err(), Error::BadChecksum("IP table"));
    // The harvest table's.
    let mut b = FIRST.to_vec();
    b[0x380 + 20] ^= 1;
    refix(&mut b);
    assert_eq!(parse(&b).unwrap_err(), Error::BadChecksum("harvest table"));
    // The GC info table's.
    let mut b = FIRST.to_vec();
    b[0x31c + 20] ^= 1;
    refix(&mut b);
    assert_eq!(parse(&b).unwrap_err(), Error::BadChecksum("GC info table"));
}

#[test]
fn hostile_table_lists_and_tables() {
    let fix = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = FIRST[..1032].to_vec();
        f(&mut b);
        refix(&mut b);
        parse(&b).unwrap_err()
    };
    // The IP table's offset or size runs outside the binary.
    assert!(matches!(
        fix(&|b| put16(b, 0x0c + 4, 0xffff)),
        Error::OutOfRange(_)
    ));
    assert!(matches!(
        fix(&|b| put16(b, 0x0c, 0x1000)),
        Error::OutOfRange(_) | Error::BadTable(_)
    ));
    // No IP table at all.
    assert_eq!(fix(&|b| put16(b, 0x0c, 0)), Error::BadTable("no IP table"));
    // A wrong IP table signature (the checksum is fixed up too).
    let e = fix(&|b| {
        b[0x3c] ^= 1;
        let sum = sum16(&b[0x3c..0x3c + 736]);
        put16(b, 0x0c + 2, sum);
    });
    assert_eq!(e, Error::BadTable("IP table signature"));
}

/// The IP table with `f` applied, its two checksums put right.
fn with_ip_table(f: &dyn Fn(&mut Vec<u8>)) -> Error {
    let mut b = FIRST[..1032].to_vec();
    f(&mut b);
    let sum = sum16(&b[0x3c..0x3c + 736]);
    put16(&mut b, 0x0c + 2, sum);
    refix(&mut b);
    parse(&b).unwrap_err()
}

#[test]
fn hostile_ip_tables() {
    // More dies than the table can describe (and than there are slots).
    assert_eq!(
        with_ip_table(&|b| put16(b, 0x3c + 0x0c, 17)),
        Error::OutOfRange("die count")
    );
    // A die that is not where it says, or whose ID is not its index.
    assert!(matches!(
        with_ip_table(&|b| put16(b, 0x3c + 0x0e + 2, 0xffff)),
        Error::OutOfRange(_)
    ));
    assert!(matches!(
        with_ip_table(&|b| put16(b, 0x3c + 0x0e + 2, 0x10)),
        Error::OutOfRange(_)
    ));
    assert_eq!(
        with_ip_table(&|b| put16(b, 0x3c + 0x0e, 3)),
        Error::BadTable("die ID")
    );
    // Die 0 is at 0x8c: more IPs than the table holds.
    assert!(matches!(
        with_ip_table(&|b| put16(b, 0x8c + 2, 5000)),
        Error::OutOfRange(_)
    ));
    // An IP that claims 255 base addresses, 32 bits or 64.
    assert!(matches!(
        with_ip_table(&|b| b[0x90 + 3] = 255),
        Error::OutOfRange(_)
    ));
    assert!(matches!(
        with_ip_table(&|b| {
            b[0x3c + 0x4e] = 1;
            b[0x90 + 3] = 255;
        }),
        Error::OutOfRange(_)
    ));
}

#[test]
fn a_version_4_table_with_64_bit_bases_keeps_29_bits_of_the_low_word() {
    // The first IP has `n` bases; make them 8 bytes each in a copy of the
    // table built by hand.
    let mut t = Vec::new();
    t.extend(IP_SIGNATURE.to_le_bytes());
    t.extend(4u16.to_le_bytes());
    t.extend(0u16.to_le_bytes()); // size, set below
    t.extend(0u32.to_le_bytes());
    t.extend(1u16.to_le_bytes()); // one die
    // Die 0 at offset 0x3c + 0x50 (the table starts at 0x3c).
    t.extend(0u16.to_le_bytes());
    t.extend(u16::try_from(0x3c + 0x50).unwrap().to_le_bytes());
    t.resize(0x4e, 0);
    t.extend([1, 0]); // 64-bit bases
    t.extend(0u16.to_le_bytes()); // die id
    t.extend(1u16.to_le_bytes()); // one IP
    t.extend(11u16.to_le_bytes()); // GC
    t.extend([0, 1, 10, 3, 6, 0]); // instance 0, 1 base, 10.3.6
    t.extend(0xf000_1260u32.to_le_bytes()); // low word, with junk in 31:30
    t.extend(0x1234u32.to_le_bytes()); // high word
    let size = u16::try_from(t.len()).unwrap();
    t[6..8].copy_from_slice(&size.to_le_bytes());
    let mut b = alloc::vec![0u8; 0x3c];
    b[..4].copy_from_slice(&SIGNATURE.to_le_bytes());
    put16(&mut b, 4, 1);
    put16(&mut b, 6, 1);
    put16(&mut b, 0x0c, 0x3c);
    put16(&mut b, 0x0c + 2, sum16(&t));
    put16(&mut b, 0x0c + 4, size);
    b.extend(&t);
    let len = u16::try_from(b.len()).unwrap();
    put16(&mut b, 0x0a, len);
    refix(&mut b);
    let d = parse(&b).unwrap();
    assert_eq!(d.ip(hw::GC, 0).unwrap().bases, [0x3000_1260 & 0x3fff_ffff]);
    assert_eq!(d.ip(hw::GC, 0).unwrap().version(), (10, 3, 6));
}
