// SPDX-License-Identifier: BSD-2-Clause
//! Synthetic firmware memory for tests.

extern crate std;
use std::vec::Vec;

use crate::PhysMem;

/// A flat byte vector standing in for physical memory from address 0.
#[derive(Debug)]
pub(crate) struct Mem(pub(crate) Vec<u8>);

impl<'a> PhysMem<'a> for &'a Mem {
    fn bytes(&self, pa: u64, len: usize) -> Option<&'a [u8]> {
        let start = usize::try_from(pa).ok()?;
        self.0.get(start..start.checked_add(len)?)
    }
}

impl Mem {
    pub(crate) fn new() -> Self {
        Self(std::vec![0; 0x2000])
    }

    pub(crate) fn put(&mut self, at: usize, b: &[u8]) {
        self.0[at..at + b.len()].copy_from_slice(b);
    }

    /// Writes a table with a correct header and checksum.
    pub(crate) fn table(&mut self, at: usize, sig: &[u8; 4], rev: u8, body: &[u8]) {
        let len = 36 + body.len();
        let mut t = std::vec![0u8; len];
        t[..4].copy_from_slice(sig);
        t[4..8].copy_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
        t[8] = rev;
        t[10..16].copy_from_slice(b"ABYSS ");
        t[36..].copy_from_slice(body);
        fix_checksum(&mut t, 9);
        self.put(at, &t);
    }

    /// Writes a revision 2 RSDP at `at` pointing to an XSDT at `xsdt`.
    pub(crate) fn rsdp(&mut self, at: usize, xsdt: u64) {
        let mut r = [0u8; 36];
        r[..8].copy_from_slice(b"RSD PTR ");
        r[9..15].copy_from_slice(b"ABYSS ");
        r[15] = 2;
        r[20..24].copy_from_slice(&36u32.to_le_bytes());
        r[24..32].copy_from_slice(&xsdt.to_le_bytes());
        let mut v1 = [0u8; 20];
        v1.copy_from_slice(&r[..20]);
        fix_checksum(&mut v1, 8);
        r[..20].copy_from_slice(&v1);
        fix_checksum(&mut r, 32);
        self.put(at, &r);
    }
}

fn fix_checksum(b: &mut [u8], at: usize) {
    b[at] = 0;
    let sum = b.iter().fold(0u8, |a, &x| a.wrapping_add(x));
    b[at] = 0u8.wrapping_sub(sum);
}

/// A 12-byte GAS.
pub(crate) fn gas(space: u8, bits: u8, access: u8, addr: u64) -> [u8; 12] {
    let mut g = [0u8; 12];
    g[0] = space;
    g[1] = bits;
    g[3] = access;
    g[4..].copy_from_slice(&addr.to_le_bytes());
    g
}
