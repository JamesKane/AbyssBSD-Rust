// SPDX-License-Identifier: BSD-2-Clause
//! A minimal AML assembler, for tests that need definition blocks no
//! firmware in the corpus has (enabled for other crates by the `asm`
//! feature). Each function encodes one ASL construct; they panic on input
//! AML cannot express (an oversized package), which suits test code.

#![allow(
    clippy::cast_possible_truncation,
    reason = "sizes are checked by the encoders"
)]

use alloc::vec::Vec;

/// Wraps AML in a definition block header with a valid checksum.
pub fn table(sig: &[u8; 4], revision: u8, aml: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(sig);
    t.extend_from_slice(&u32::try_from(36 + aml.len()).unwrap().to_le_bytes());
    t.push(revision);
    t.push(0);
    t.extend_from_slice(b"ABYSS ");
    t.extend_from_slice(b"TESTTABL");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(b"ABYS");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(aml);
    fix_checksum(&mut t);
    t
}

pub fn fix_checksum(t: &mut [u8]) {
    if t.len() < 36 {
        return;
    }
    t[9] = 0;
    let sum = t.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    t[9] = 0u8.wrapping_sub(sum);
}

/// Encodes a package length for a body of `n` bytes (the length includes
/// its own encoding).
pub fn pkg_len(n: usize) -> Vec<u8> {
    if n + 1 < 0x40 {
        return alloc::vec![u8::try_from(n + 1).unwrap()];
    }
    for extra in 1..=3usize {
        let total = n + 1 + extra;
        if total < 1 << (4 + 8 * extra) {
            let mut v = alloc::vec![u8::try_from((extra << 6) | (total & 0xf)).unwrap()];
            for i in 0..extra {
                v.push(u8::try_from((total >> (4 + 8 * i)) & 0xff).unwrap());
            }
            return v;
        }
    }
    panic!("package too large");
}

/// `op PkgLength body`.
pub fn pkg(op: &[u8], body: &[u8]) -> Vec<u8> {
    let mut v = op.to_vec();
    v.extend(pkg_len(body.len()));
    v.extend_from_slice(body);
    v
}

/// Concatenates byte pieces.
pub fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

/// A `NameString` for ASL text such as `\_SB.PCI0` or `FOO`.
pub fn name(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    let mut rest = s;
    if let Some(r) = rest.strip_prefix('\\') {
        v.push(b'\\');
        rest = r;
    }
    while let Some(r) = rest.strip_prefix('^') {
        v.push(b'^');
        rest = r;
    }
    let segs: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('.').collect()
    };
    match segs.len() {
        0 => v.push(0),
        1 => {}
        2 => v.push(0x2e),
        n => {
            v.push(0x2f);
            v.push(u8::try_from(n).unwrap());
        }
    }
    for s in segs {
        let mut seg = [b'_'; 4];
        seg[..s.len()].copy_from_slice(s.as_bytes());
        v.extend_from_slice(&seg);
    }
    v
}

/// An integer constant in its shortest encoding.
pub fn int(v: u64) -> Vec<u8> {
    match v {
        0 => alloc::vec![0x00],
        1 => alloc::vec![0x01],
        2..=0xff => alloc::vec![0x0a, u8::try_from(v).unwrap()],
        0x100..=0xffff => cat(&[&[0x0b], &u16::try_from(v).unwrap().to_le_bytes()]),
        0x1_0000..=0xffff_ffff => cat(&[&[0x0c], &u32::try_from(v).unwrap().to_le_bytes()]),
        _ => cat(&[&[0x0e], &v.to_le_bytes()]),
    }
}

/// A string constant.
pub fn string(s: &str) -> Vec<u8> {
    cat(&[&[0x0d], s.as_bytes(), &[0]])
}

/// `Buffer (n) { bytes }`.
pub fn buffer(bytes: &[u8]) -> Vec<u8> {
    pkg(&[0x11], &cat(&[&int(bytes.len() as u64), bytes]))
}

/// `Package () { elements }`.
pub fn package(elems: &[Vec<u8>]) -> Vec<u8> {
    let mut body = alloc::vec![u8::try_from(elems.len()).unwrap()];
    for e in elems {
        body.extend_from_slice(e);
    }
    pkg(&[0x12], &body)
}

/// `Method (name, args) { body }`.
pub fn method(n: &str, flags: u8, body: &[u8]) -> Vec<u8> {
    pkg(&[0x14], &cat(&[&name(n), &[flags], body]))
}

/// `Name (name, value)`.
pub fn name_op(n: &str, value: &[u8]) -> Vec<u8> {
    cat(&[&[0x08], &name(n), value])
}

/// `Device (name) { body }`.
pub fn device(n: &str, body: &[u8]) -> Vec<u8> {
    pkg(&[0x5b, 0x82], &cat(&[&name(n), body]))
}

/// `Scope (name) { body }`.
pub fn scope(n: &str, body: &[u8]) -> Vec<u8> {
    pkg(&[0x10], &cat(&[&name(n), body]))
}

/// `Return (x)`.
pub fn ret(x: &[u8]) -> Vec<u8> {
    cat(&[&[0xa4], x])
}

/// `Store (src, dst)`.
pub fn store(src: &[u8], dst: &[u8]) -> Vec<u8> {
    cat(&[&[0x70], src, dst])
}

/// `If (pred) { body }`.
pub fn if_(pred: &[u8], body: &[u8]) -> Vec<u8> {
    pkg(&[0xa0], &cat(&[pred, body]))
}

/// `Else { body }`.
pub fn else_(body: &[u8]) -> Vec<u8> {
    pkg(&[0xa1], body)
}

/// `While (pred) { body }`.
pub fn while_(pred: &[u8], body: &[u8]) -> Vec<u8> {
    pkg(&[0xa2], &cat(&[pred, body]))
}

pub const LOCAL0: u8 = 0x60;
pub const ARG0: u8 = 0x68;

/// `PowerResource (name, level, order) { body }`.
pub fn power_resource(n: &str, level: u8, order: u16, body: &[u8]) -> Vec<u8> {
    pkg(
        &[0x5b, 0x84],
        &cat(&[&name(n), &[level], &order.to_le_bytes(), body]),
    )
}

/// `Processor (name, id, pblk, pblk_len) { body }` (the legacy object).
pub fn processor(n: &str, id: u8, body: &[u8]) -> Vec<u8> {
    pkg(
        &[0x5b, 0x83],
        &cat(&[&name(n), &[id], &0u32.to_le_bytes(), &[0], body]),
    )
}
