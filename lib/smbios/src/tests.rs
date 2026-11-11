// SPDX-License-Identifier: BSD-2-Clause
extern crate std;

use std::string::String;
use std::vec::Vec;

use super::*;

/// One structure: `ty`, a formatted area of `len` bytes with `fields`
/// (offset, bytes) set, then `strings`.
fn structure(ty: u8, len: u8, handle: u16, fields: &[(usize, &[u8])], strings: &[&str]) -> Vec<u8> {
    let mut s = std::vec![0u8; usize::from(len)];
    s[0] = ty;
    s[1] = len;
    s[2..4].copy_from_slice(&handle.to_le_bytes());
    for (at, b) in fields {
        s[*at..*at + b.len()].copy_from_slice(b);
    }
    for st in strings {
        s.extend_from_slice(st.as_bytes());
        s.push(0);
    }
    if strings.is_empty() {
        s.push(0);
    }
    s.push(0);
    s
}

fn v3_entry(table: u64, len: u32, major: u8, minor: u8) -> Vec<u8> {
    let mut e = std::vec![0u8; 0x18];
    e[..5].copy_from_slice(b"_SM3_");
    e[6] = 0x18;
    e[7] = major;
    e[8] = minor;
    e[0x0a] = 1;
    e[0x0c..0x10].copy_from_slice(&len.to_le_bytes());
    e[0x10..0x18].copy_from_slice(&table.to_le_bytes());
    let sum = e.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    e[5] = 0u8.wrapping_sub(sum);
    e
}

fn v2_entry(table: u32, len: u16, count: u16, bcd: u8) -> Vec<u8> {
    let mut e = std::vec![0u8; 0x1f];
    e[..4].copy_from_slice(b"_SM_");
    e[5] = 0x1f;
    e[6] = 2;
    e[7] = 8;
    e[0x10..0x15].copy_from_slice(b"_DMI_");
    e[0x16..0x18].copy_from_slice(&len.to_le_bytes());
    e[0x18..0x1c].copy_from_slice(&table.to_le_bytes());
    e[0x1c..0x1e].copy_from_slice(&count.to_le_bytes());
    e[0x1e] = bcd;
    let dmi = e[0x10..0x1f].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    e[0x15] = 0u8.wrapping_sub(dmi);
    let sum = e.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    e[4] = 0u8.wrapping_sub(sum);
    e
}

/// The Orange Pi 6 Plus's structures FreeBSD reads, from its firmware's
/// table as `dmidecode` decodes it (tests/corpus/opi6p/linux/dmidecode.txt).
fn opi6p_table() -> Vec<u8> {
    const CIX: &str = "Cix Technology Group Co., Ltd.";
    let mut t = Vec::new();
    for (h, name) in [(0u16, "L1 Instruction Cache"), (1, "L1 Data Cache")] {
        t.extend(structure(7, 27, h, &[(4, &[1])], &[name]));
    }
    // Type 4: version string 4 at 0x10, status populated and enabled.
    t.extend(structure(
        4,
        50,
        4,
        &[
            (4, &[1]),
            (7, &[2]),
            (0x10, &[3]),
            (0x18, &[0x41]),
            (0x20, &[4]),
        ],
        &["CPU01", CIX, "CIX P1 CD8160", "44D8160A0491C0E0"],
    ));
    for (h, bank) in [
        (5u16, "BANK 0"),
        (6, "BANK 1"),
        (7, "BANK 2"),
        (8, "BANK 3"),
    ] {
        // 8 GiB each, in MiB.
        t.extend(structure(
            17,
            92,
            h,
            &[(0x0c, &0x2000u16.to_le_bytes()), (0x10, &[1]), (0x11, &[2])],
            &["Top - on board", bank],
        ));
    }
    // Type 0: BIOS 1.3, revision 0.5.
    t.extend(structure(
        0,
        26,
        0x0a,
        &[(4, &[1]), (5, &[2]), (8, &[3]), (0x14, &[0]), (0x15, &[5])],
        &[CIX, "1.3", "11/13/2025"],
    ));
    // Type 1: no UUID (all ones), SKU and family.
    t.extend(structure(
        1,
        27,
        0x0b,
        &[
            (4, &[1]),
            (5, &[2]),
            (6, &[3]),
            (7, &[4]),
            (8, &[0xff; 16]),
            (0x18, &[6]),
            (0x19, &[5]),
            (0x1a, &[5]),
        ],
        &[
            CIX,
            "CIX Phecda Board",
            "1.0",
            "Cix System Serial Numbe",
            "Default",
        ],
    ));
    t.extend(structure(
        2,
        17,
        0x1000,
        &[
            (4, &[1]),
            (5, &[2]),
            (6, &[3]),
            (7, &[4]),
            (8, &[5]),
            (9, &[1]),
            (0x0a, &[6]),
        ],
        &[
            CIX,
            "CIX Phecda Board",
            "1.0",
            "Cix Base Board Serial Number",
            "Cix Base Board Asset Tag",
            "Part Component",
        ],
    ));
    // Type 3: chassis type 2 ("Unknown"), no lock.
    t.extend(structure(
        3,
        24,
        0x1001,
        &[(4, &[1]), (5, &[2]), (6, &[2]), (7, &[3]), (8, &[4])],
        &[
            CIX,
            "1.0",
            "Cix Chassis Serial Number",
            "Cix Chassis Asset Tag",
        ],
    ));
    t.extend(structure(127, 4, 0xfeff, &[], &[]));
    t
}

/// `name="value"` lines, sorted as `kenv` prints them.
fn kenv_lines(vars: &Vars) -> Vec<String> {
    let mut m = std::collections::BTreeMap::new();
    for (n, v) in vars {
        m.insert(*n, String::from_utf8(v.clone()).unwrap());
    }
    m.iter().map(|(n, v)| std::format!("{n}=\"{v}\"")).collect()
}

#[test]
fn the_orange_pi_reads_as_its_freebsd_kenv() {
    let t = opi6p_table();
    let len = u32::try_from(t.len() + 0x100).unwrap();
    let ep = EntryPoint::parse(&v3_entry(0x7_fe81_0000, len, 3, 6)).unwrap();
    assert_eq!(
        (ep.kind, ep.table, ep.count, ep.version()),
        (Kind::V3, 0x7_fe81_0000, None, 0x0306)
    );
    let got = kenv_lines(&variables(&ep, &t));
    // FreeBSD's kenv on the board, every smbios.* line.
    let corpus = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/corpus/opi6p/freebsd/kenv.txt"
    ))
    .unwrap();
    let want: Vec<String> = corpus
        .lines()
        .filter(|l| l.starts_with("smbios."))
        .map(String::from)
        .collect();
    assert_eq!(got, want);
    assert_eq!(hint_value(0x7_fe82_0000), b"0x7fe820000");
}

#[test]
fn a_v2_table_counts_its_structures_and_reads_its_bcd_version() {
    let mut t = structure(
        1,
        27,
        1,
        &[
            (4, &[1]),
            (5, &[2]),
            (
                8,
                &[
                    0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 1, 2, 3, 4, 5, 6,
                ],
            ),
        ],
        &["QEMU", "Standard PC"],
    );
    // Past the count: not read.
    t.extend(structure(0, 26, 2, &[(4, &[1])], &["Ignored"]));
    let ep = EntryPoint::parse(&v2_entry(
        0xf_0000,
        u16::try_from(t.len()).unwrap(),
        1,
        0x28,
    ))
    .unwrap();
    assert_eq!(
        (ep.kind, ep.major, ep.minor, ep.count),
        (Kind::V2, 2, 8, Some(1))
    );
    let v = variables(&ep, &t);
    let get = |n: &str| v.iter().find(|(k, _)| *k == n).map(|(_, v)| v.as_slice());
    assert_eq!(get("smbios.system.maker"), Some(&b"QEMU"[..]));
    assert_eq!(get("smbios.bios.vendor"), None);
    // 2.6 and later: little-endian time fields.
    assert_eq!(
        get("smbios.system.uuid"),
        Some(&b"00112233-4455-6677-8899-010203040506"[..])
    );
    assert_eq!(get("smbios.entry_point_type"), Some(&b"v2.1 (32-bit)"[..]));
    assert_eq!(get("smbios.version"), Some(&b"2.8"[..]));
    // A bad BCD revision falls back to the version fields; before 2.6 the
    // UUID is in network order.
    let ep = EntryPoint::parse(&v2_entry(
        0xf_0000,
        u16::try_from(t.len()).unwrap(),
        2,
        0x2a,
    ))
    .unwrap();
    assert_eq!((ep.major, ep.minor), (2, 8));
    let ep = EntryPoint { minor: 5, ..ep };
    let v = variables(&ep, &t);
    let get = |n: &str| v.iter().find(|(k, _)| *k == n).map(|(_, v)| v.as_slice());
    assert_eq!(
        get("smbios.system.uuid"),
        Some(&b"33221100-5544-7766-8899-010203040506"[..])
    );
    assert_eq!(get("smbios.bios.vendor"), Some(&b"Ignored"[..]));
}

#[test]
fn bad_entry_points_and_truncated_tables_are_refused_or_cut_short() {
    let mut e = v3_entry(0x1000, 0x100, 3, 0);
    e[5] ^= 1;
    assert_eq!(EntryPoint::parse(&e), None);
    let mut e = v3_entry(0x1000, 0x100, 3, 0);
    e[0x0a] = 0;
    let sum = e.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    e[5] = e[5].wrapping_sub(sum);
    assert_eq!(EntryPoint::parse(&e), None, "revision 0");
    assert_eq!(EntryPoint::parse(b"_SM3_"), None);
    assert_eq!(EntryPoint::parse(&[]), None);
    // A structure whose strings run off the end: the walk stops, and
    // nothing panics, whatever the cut.
    let t = opi6p_table();
    let ep = EntryPoint::parse(&v3_entry(0, 0xffff, 3, 6)).unwrap();
    for cut in 0..t.len() {
        let _ = variables(&ep, &t[..cut]);
    }
    // A string index past the strings.
    let s = structure(1, 27, 1, &[(4, &[9])], &["only"]);
    assert!(
        variables(&ep, &s)
            .iter()
            .all(|(n, _)| *n != "smbios.system.maker")
    );
}

#[test]
fn chassis_types_are_freebsds_names() {
    assert_eq!(chassis_type(0x03), "Desktop");
    assert_eq!(chassis_type(0x24), "Stick PC");
    assert_eq!(chassis_type(0x25), "Undefined");
    assert_eq!(chassis_type(0x00), "Undefined");
    // The lock bit is not masked.
    assert_eq!(chassis_type(0x83), "Undefined");
}
