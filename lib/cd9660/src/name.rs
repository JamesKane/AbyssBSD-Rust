// SPDX-License-Identifier: BSD-2-Clause
//! ISO 9660 and Joliet names as FreeBSD reads them without `kiconv`
//! (`cd9660_util.c`): a Joliet character is two bytes, `?` unless its
//! high byte is zero.

use alloc::vec::Vec;

/// One character of a recorded name from `iso` (`isochar`): the
/// character and how many bytes it took. Joliet characters are two
/// bytes, but for a last odd byte.
#[must_use]
pub fn isochar(iso: &[u8], joliet_level: u8) -> (u16, usize) {
    let Some(&first) = iso.first() else {
        return (0, 1);
    };
    if joliet_level == 0 || iso.len() == 1 {
        return (u16::from(first), 1);
    }
    let c = if first == 0 {
        u16::from(iso[1])
    } else {
        u16::from(b'?')
    };
    (c, 2)
}

/// Compares a name looked up (`name`) with a recorded one (`iso`)
/// (`isofncmp`): 0 when they match. Recorded upper case matches either
/// case; the version (`;1`) may be left out, and a `.` before it.
#[must_use]
pub fn isofncmp(name: &[u8], iso: &[u8], joliet_level: u8) -> i32 {
    let mut i = 0;
    let mut j = 0;
    while i < name.len() {
        let d = i32::from(name[i]);
        i += 1;
        if j >= iso.len() {
            return d;
        }
        let (c, n) = isochar(&iso[j..], joliet_level);
        j += n;
        let c = i32::from(c);
        if c == i32::from(b';') {
            if d != i32::from(b';') {
                return d;
            }
            // The versions, as numbers.
            let mut want: i32 = 0;
            while i < name.len() {
                let ch = name[i];
                if !ch.is_ascii_digit() {
                    return -1;
                }
                want = want.wrapping_mul(10).wrapping_add(i32::from(ch - b'0'));
                i += 1;
            }
            let mut have: i32 = 0;
            while j < iso.len() {
                let (c, n) = isochar(&iso[j..], joliet_level);
                j += n;
                have = have
                    .wrapping_mul(10)
                    .wrapping_add(i32::from(c).wrapping_sub(i32::from(b'0')));
            }
            return want.wrapping_sub(have);
        }
        if c != d {
            if (i32::from(b'A')..=i32::from(b'Z')).contains(&c) {
                if c + 32 != d {
                    return if (i32::from(b'a')..=i32::from(b'z')).contains(&d) {
                        d - 32 - c
                    } else {
                        d - c
                    };
                }
            } else {
                return d - c;
            }
        }
    }
    if j < iso.len() {
        let (c, n) = isochar(&iso[j..], joliet_level);
        j += n;
        return match c {
            0x2e => {
                // `.` then `;`: matches.
                if j < iso.len() && isochar(&iso[j..], joliet_level).0 == u16::from(b';') {
                    0
                } else {
                    -1
                }
            }
            0x3b => 0,
            other => -i32::from(other),
        };
    }
    0
}

/// A recorded name as `readdir` shows it (`isofntrans`): unless
/// `original`, plain ISO names in lower case and the version (with a `.`
/// before it) dropped; an associated file's prefixed `=`.
#[must_use]
pub fn isofntrans(iso: &[u8], original: bool, assoc: bool, joliet_level: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(iso.len() + 1);
    if assoc {
        out.push(b'=');
    }
    let mut d = 0u16;
    let mut at = 0;
    while at < iso.len() {
        let (mut c, n) = isochar(&iso[at..], joliet_level);
        at += n;
        if !original && joliet_level == 0 && (u16::from(b'A')..=u16::from(b'Z')).contains(&c) {
            c += 32;
        } else if !original && c == u16::from(b';') {
            if d == u16::from(b'.') {
                out.pop();
            }
            break;
        }
        d = c;
        out.push(c.to_le_bytes()[0]);
    }
    out
}
