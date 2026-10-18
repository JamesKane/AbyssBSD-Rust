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
    pub(crate) fn table(&mut self, at: usize, sig: [u8; 4], rev: u8, body: &[u8]) {
        let len = 36 + body.len();
        let mut t = std::vec![0u8; len];
        t[..4].copy_from_slice(&sig);
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

/// Memory holding exactly one table at address 0.
pub(crate) fn one(sig: [u8; 4], rev: u8, body: &[u8]) -> Mem {
    let mut m = Mem(std::vec![0; 36 + body.len()]);
    m.table(0, sig, rev, body);
    m
}

/// A little-endian byte builder for table bodies.
#[derive(Clone, Debug, Default)]
pub(crate) struct B(pub(crate) Vec<u8>);

impl B {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub(crate) fn u16(mut self, v: u16) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub(crate) fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub(crate) fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub(crate) fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    pub(crate) fn zeros(mut self, n: usize) -> Self {
        self.0.resize(self.0.len() + n, 0);
        self
    }
    /// Appends a type/length-prefixed structure, with `len` as its u8 length.
    pub(crate) fn entry(self, kind: u8, rest: &B) -> Self {
        let len = u8::try_from(rest.0.len() + 2).unwrap();
        self.u8(kind).u8(len).bytes(&rest.0)
    }
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// Runs every parser and drains every iterator and helper over `t`; the
/// point is that none of it panics or loops.
pub(crate) fn exercise(t: &crate::Sdt<'_>) {
    use crate::{fadt, gtdt, hpet, iort, madt, mcfg, pptt, slit, srat};
    if let Ok(m) = madt::parse(t) {
        let _ = m.entries().count();
        let _ = m.enabled_cpu_count();
        let _ = (m.local_apic_address(), m.gic_distributor(), m.pcat_compat());
    }
    if let Ok(f) = fadt::parse(t) {
        let _ = (f.facs(), f.dsdt(), f.pm_timer(), f.reset_register());
        let _ = (f.pm1a_event_block(), f.gpe0_block(), f.gpe1_block());
        let _ = (f.sleep_control_register(), f.hypervisor_vendor());
        let _ = (f.iapc_boot_arch(), f.arm_boot_arch(), f.minor_version());
    }
    if let Ok(m) = mcfg::parse(t) {
        let _ = m.regions().count();
    }
    if let Ok(i) = iort::parse(t) {
        let _ = i.its_groups();
        for rid in [0, 1, 3, 0x100, 0xffff] {
            let _ = i.its_for_pci(0, rid);
        }
    }
    let _ = hpet::parse(t);
    if let Ok(g) = gtdt::parse(t) {
        let _ = (g.virtual_el2(), g.cnt_read_base());
        for p in g.platform_timers().flatten() {
            if let gtdt::PlatformTimer::GtBlock(b) = p {
                let _ = b.frames().count();
            }
        }
    }
    if let Ok(s) = srat::parse(t) {
        let _ = s.entries().count();
    }
    if let Ok(s) = slit::parse(t) {
        let n = s.locality_count();
        let _ = (
            s.distance(0, 0),
            s.distance(n, 0),
            s.distance(n.saturating_sub(1), n.saturating_sub(1)),
        );
    }
    if let Ok(p) = pptt::parse(t) {
        for n in p.nodes().flatten() {
            if let pptt::PpttNode::ProcessorHierarchy(node) = n {
                let _ = p.ancestors(&node).count();
                let _ = node
                    .private_resources()
                    .filter_map(|r| p.node_at(r))
                    .count();
                let _ = p.topology(node.acpi_processor_id);
            }
        }
    }
}

/// Exercises every truncation of `body` and a few thousand deterministic
/// single- and multi-byte corruptions of it, as table `sig`.
pub(crate) fn torture(sig: [u8; 4], rev: u8, body: &[u8]) {
    for n in 0..=body.len() {
        let m = one(sig, rev, &body[..n]);
        exercise(&crate::Sdt::read(&&m, 0).unwrap());
    }
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    if body.is_empty() {
        return;
    }
    for _ in 0..2000 {
        let mut b = body.to_vec();
        for _ in 0..=rnd() % 4 {
            let i = usize::try_from(rnd() % u64::try_from(b.len()).unwrap()).unwrap();
            b[i] = rnd().to_le_bytes()[0];
        }
        let m = one(sig, rev, &b);
        exercise(&crate::Sdt::read(&&m, 0).unwrap());
    }
}
