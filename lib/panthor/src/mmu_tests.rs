// SPDX-License-Identifier: BSD-2-Clause
//! The page tables and attribute words against specification 04's encodings and worked values.

use std::collections::BTreeMap;
use std::vec::Vec;

use crate::Error;
use crate::mmu::{self, Map, Tables};

/// Tables in a map from fake physical address to 512 descriptors.
struct Mem {
    tables: BTreeMap<u64, Vec<u64>>,
    next: u64,
    limit: usize,
}

impl Mem {
    fn new() -> Self {
        let mut m = Self {
            tables: BTreeMap::new(),
            next: 0x8000_0000,
            limit: 64,
        };
        let _ = m.alloc();
        m
    }
}

impl Tables for Mem {
    fn alloc(&mut self) -> Option<u64> {
        if self.tables.len() >= self.limit {
            return None;
        }
        let pa = self.next;
        self.next += 0x1000;
        self.tables.insert(pa, std::vec![0; 512]);
        Some(pa)
    }
    fn read(&self, t: u64, i: usize) -> u64 {
        self.tables[&t][i]
    }
    fn write(&mut self, t: u64, i: usize, d: u64) {
        self.tables.get_mut(&t).unwrap()[i] = d;
    }
}

const RW: Map = Map {
    read_only: false,
    no_exec: false,
    cacheable: true,
};

#[test]
fn page_descriptors_carry_the_specified_bits() {
    // Read/write, executable, cacheable: AttrIndx 1, SH 3, AP[1], AF, nG.
    let d = mmu::page(0x1234_5000, RW);
    assert_eq!(
        d,
        0x1234_5000 | 0b11 | 1 << 2 | 1 << 6 | 3 << 8 | 1 << 10 | 1 << 11
    );
    let ro_nx_uc = mmu::page(
        0x1000,
        Map {
            read_only: true,
            no_exec: true,
            cacheable: false,
        },
    );
    assert_eq!(ro_nx_uc & 0b111 << 2, 0, "AttrIndx 0");
    assert_eq!(ro_nx_uc >> 8 & 3, 2, "outer shareable");
    assert_ne!(ro_nx_uc & 1 << 7, 0, "AP[2]");
    assert_ne!(ro_nx_uc & (1 << 53 | 1 << 54), 0, "PXN and UXN");
}

#[test]
fn forty_eight_bit_addresses_take_four_levels() {
    assert_eq!(mmu::levels(48), 4);
    assert_eq!(mmu::levels(39), 3);
}

#[test]
fn a_mapping_is_walked_back_and_shares_tables() {
    let mut m = Mem::new();
    let root = 0x8000_0000;
    mmu::map(&mut m, root, 48, 0x0400_0000, 0x9_0000_0000, 0x3000, RW).unwrap();
    for k in 0..3 {
        let d = mmu::walk(&m, root, 48, 0x0400_0000 + k * 0x1000);
        assert_eq!(d & 0x0000_FFFF_FFFF_F000, 0x9_0000_0000 + k * 0x1000);
    }
    assert_eq!(mmu::walk(&m, root, 48, 0x0400_3000), 0);
    // Root plus one table per lower level.
    assert_eq!(m.tables.len(), 4);
    // A neighbour in the same 2 MiB uses the same last-level table.
    mmu::map(&mut m, root, 48, 0x0400_5000, 0x9_1000_0000, 0x1000, RW).unwrap();
    assert_eq!(m.tables.len(), 4);
}

#[test]
fn bad_mappings_are_refused() {
    let mut m = Mem::new();
    let root = 0x8000_0000;
    assert_eq!(
        mmu::map(&mut m, root, 48, 0x10, 0, 0x1000, RW),
        Err(Error::Bad("unaligned mapping"))
    );
    assert_eq!(
        mmu::map(&mut m, root, 32, 0xFFFF_F000, 0, 0x2000, RW),
        Err(Error::Bad("mapping beyond the address space"))
    );
    mmu::map(&mut m, root, 48, 0, 0, 0x1000, RW).unwrap();
    assert_eq!(
        mmu::map(&mut m, root, 48, 0, 0x5000, 0x1000, RW),
        Err(Error::Bad("already mapped"))
    );
    // More than four levels' worth of address bits: refused, not walked.
    assert_eq!(
        mmu::map(&mut m, root, 52, 0, 0, 0x1000, RW),
        Err(Error::Bad("mapping beyond the address space"))
    );
    assert_eq!(mmu::walk(&m, root, 52, 0), 0);
    m.limit = m.tables.len();
    assert_eq!(
        mmu::map(&mut m, root, 48, 0x40_0000_0000, 0, 0x1000, RW),
        Err(Error::Bad("out of table memory"))
    );
}

#[test]
fn unmapping_clears_pages_and_skips_holes() {
    let mut m = Mem::new();
    let root = 0x8000_0000;
    mmu::map(&mut m, root, 48, 0x10_0000, 0x5000_0000, 0x3000, RW).unwrap();
    // Across mapped pages and an unmapped tail, and a range with no tables at all.
    mmu::unmap(&mut m, root, 48, 0x10_1000, 0x4000).unwrap();
    mmu::unmap(&mut m, root, 48, 0x40_0000_0000, 0x1000).unwrap();
    assert_ne!(mmu::walk(&m, root, 48, 0x10_0000), 0);
    assert_eq!(mmu::walk(&m, root, 48, 0x10_1000), 0);
    assert_eq!(mmu::walk(&m, root, 48, 0x10_2000), 0);
    // And mapped again.
    mmu::map(&mut m, root, 48, 0x10_1000, 0x6000_0000, 0x1000, RW).unwrap();
    assert_eq!(
        mmu::walk(&m, root, 48, 0x10_1000) & 0x0000_FFFF_FFFF_F000,
        0x6000_0000
    );
}

#[test]
fn attribute_words_are_the_specified_values() {
    assert_eq!(mmu::memattr(false), 0x4C4C_4C4C_8C4C_8F4C);
    assert_eq!(mmu::memattr(true), 0x4C4C_4C4C_9C4C_9F4C);
    assert_eq!(mmu::transcfg(48, false), 0x4200_01C6);
    assert_eq!(mmu::transcfg(48, true), 0x6200_01C6);
}

#[test]
fn lock_regions_follow_the_worked_example() {
    // 04 §4.4.2: 8 KiB at 0x1_0000_3000 locks 32 KiB at 0x1_0000_0000.
    assert_eq!(mmu::lockaddr(0x1_0000_3000, 0x1_0000_5000), 0x1_0000_000E);
    // A larger range: 1 MiB at 1 MiB, width 20, so w = 19.
    assert_eq!(mmu::lockaddr(0x10_0000, 0x20_0000), 0x10_0000 | 19);
    // A single byte still locks 32 KiB, aligned to 32 KiB.
    assert_eq!(mmu::lockaddr(0x4000, 0x4001), 14);
}
