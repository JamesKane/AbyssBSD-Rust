// SPDX-License-Identifier: BSD-2-Clause
//! Names between Unix and FAT, as FreeBSD's msdosfs converts them without
//! `kiconv` (its default): file names are bytes in ISO 8859-1, short names
//! are code page 850, and long names are UCS-2 holding those bytes.
//!
//! Behavior reference: sys/fs/msdosfs/msdosfs_conv.c (`unix2dosfn`,
//! `unix2winfn`, `win2unixfn`, `winChkName`, `winChksum`, `winSlotCnt`,
//! `winLenFixup`, `dos2unixfn`, and the code page tables).

use alloc::vec::Vec;

/// Characters per long-name slot.
pub const WIN_CHARS: usize = 13;
/// The longest long name.
pub const WIN_MAXLEN: usize = 255;
/// `deLowerCase` bits: the short name's base, or extension, is lower case.
pub const LCASE_BASE: u8 = 0x08;
pub const LCASE_EXT: u8 = 0x10;
/// What `dn[0]` holds for a name starting with 0xe5 (`SLOT_E5`).
pub const SLOT_E5: u8 = 0x05;

/// ISO 8859-1 0xa0..=0xff in code page 850, upper case (FreeBSD's
/// `unix2dos` upper half; 0 is disallowed).
const LATIN1_TO_CP850: [u8; 96] = [
    0, 0xad, 0xbd, 0x9c, 0xcf, 0xbe, 0xdd, 0xf5, 0xf9, 0xb8, 0xa6, 0xae, 0xaa, 0xf0, 0xa9,
    0xee, //
    0xf8, 0xf1, 0xfd, 0xfc, 0xef, 0xe6, 0xf4, 0xfa, 0xf7, 0xfb, 0xa7, 0xaf, 0xac, 0xab, 0xf3,
    0xa8, //
    0xb7, 0xb5, 0xb6, 0xc7, 0x8e, 0x8f, 0x92, 0x80, 0xd4, 0x90, 0xd2, 0xd3, 0xde, 0xd6, 0xd7,
    0xd8, //
    0xd1, 0xa5, 0xe3, 0xe0, 0xe2, 0xe5, 0x99, 0x9e, 0x9d, 0xeb, 0xe9, 0xea, 0x9a, 0xed, 0xe8,
    0xe1, //
    0xb7, 0xb5, 0xb6, 0xc7, 0x8e, 0x8f, 0x92, 0x80, 0xd4, 0x90, 0xd2, 0xd3, 0xde, 0xd6, 0xd7,
    0xd8, //
    0xd1, 0xa5, 0xe3, 0xe0, 0xe2, 0xe5, 0x99, 0xf6, 0x9d, 0xeb, 0xe9, 0xea, 0x9a, 0xed, 0xe8,
    0x98, //
];

/// Code page 850 0x80..=0xff in ISO 8859-1 (FreeBSD's `dos2unix` upper
/// half; `?` for what has no equivalent).
const CP850_TO_LATIN1: [u8; 128] = [
    0xc7, 0xfc, 0xe9, 0xe2, 0xe4, 0xe0, 0xe5, 0xe7, 0xea, 0xeb, 0xe8, 0xef, 0xee, 0xec, 0xc4,
    0xc5, //
    0xc9, 0xe6, 0xc6, 0xf4, 0xf6, 0xf2, 0xfb, 0xf9, 0xff, 0xd6, 0xdc, 0xf8, 0xa3, 0xd8, 0xd7,
    0x3f, //
    0xe1, 0xed, 0xf3, 0xfa, 0xf1, 0xd1, 0xaa, 0xba, 0xbf, 0xae, 0xac, 0xbd, 0xbc, 0xa1, 0xab,
    0xbb, //
    0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0xc1, 0xc2, 0xc0, 0xa9, 0x3f, 0x3f, 0x3f, 0x3f, 0xa2, 0xa5,
    0x3f, //
    0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0xe3, 0xc3, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f,
    0xa4, //
    0xf0, 0xd0, 0xca, 0xcb, 0xc8, 0x3f, 0xcd, 0xce, 0xcf, 0x3f, 0x3f, 0x3f, 0x3f, 0xa6, 0xcc,
    0x3f, //
    0xd3, 0xdf, 0xd4, 0xd2, 0xf5, 0xd5, 0xb5, 0xfe, 0xde, 0xda, 0xdb, 0xd9, 0xfd, 0xdd, 0xaf,
    0x3f, //
    0xad, 0xb1, 0x3f, 0xbe, 0xb6, 0xa7, 0xf7, 0xb8, 0xb0, 0xa8, 0xb7, 0xb9, 0xb3, 0xb2, 0x3f,
    0x3f, //
];

/// What a Unix character becomes in a short name (FreeBSD's `unix2dos`):
/// a code page 850 byte, or 0 (not allowed), 1 (replaced by `_`), or 2
/// (left out); the last two force a generation number.
fn unix2dos(c: u8) -> u8 {
    match c {
        0x00..=0x1f | b'"' | b'*' | b'/' | b':' | b'<' | b'>' | b'?' | b'\\' | b'|' | 0x7f => 0,
        b' ' | b'.' => 2,
        b'+' | b',' | b';' | b'=' | b'[' | b']' => 1,
        b'a'..=b'z' => c - 0x20,
        0x80..=0x9f => 0,
        0xa0..=0xff => LATIN1_TO_CP850[usize::from(c - 0xa0)],
        _ => c,
    }
}

/// What a short-name byte shows as (FreeBSD's `dos2unix`).
fn dos2unix(c: u8) -> u8 {
    match c {
        0x00..=0x1f => b'?',
        0x80..=0xff => CP850_TO_LATIN1[usize::from(c - 0x80)],
        _ => c,
    }
}

/// Lower case, for ISO 8859-1 (FreeBSD's `u2l`).
#[must_use]
pub fn to_lower(c: u8) -> u8 {
    match c {
        b'A'..=b'Z' => c + 0x20,
        0xc0..=0xde if c != 0xd7 => c + 0x20,
        _ => c,
    }
}

/// How a Unix name maps to a short name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conv {
    /// Not a valid name (`unix2dosfn`'s 0).
    Invalid,
    /// The name is its own short name: no long name (1).
    Exact,
    /// A long name keeps it, and its short name needs no number (2).
    Long,
    /// A long name keeps it, and its short name carries a generation number
    /// (3).
    LongNumbered,
}

/// The short name for Unix name `un` with generation number `generation` (0
/// for none, as lookups use; creation tries 1, 2, ...), and how the name
/// maps (FreeBSD's `unix2dosfn`).
#[must_use]
pub fn unix2dosfn(un: &[u8], generation: u32) -> ([u8; 11], Conv) {
    let mut dn = [b' '; 11];
    if un == b"." {
        dn[0] = b'.';
        return (
            dn,
            if generation <= 1 {
                Conv::Exact
            } else {
                Conv::Invalid
            },
        );
    }
    if un == b".." {
        dn[0] = b'.';
        dn[1] = b'.';
        return (
            dn,
            if generation <= 1 {
                Conv::Exact
            } else {
                Conv::Invalid
            },
        );
    }
    // Only blanks and dots, or a character never allowed.
    if un.iter().all(|&c| c == b' ' || c == b'.') || un.iter().any(|&c| unix2dos(c) == 0) {
        return (dn, Conv::Invalid);
    }
    let mut conv = 1u8;
    // The extension starts after the last dot followed by something other
    // than blanks and dots (a dot in front starts none).
    let mut ext = None;
    let mut dot = None;
    for (i, &c) in un.iter().enumerate().skip(1) {
        match c {
            b'.' => {
                if dot.is_none() {
                    dot = Some(i + 1);
                }
            }
            b' ' => {}
            _ => {
                if dot.is_some() {
                    ext = dot;
                }
                dot = None;
            }
        }
    }
    // Converts `src` into `dn[from..to]`, updating `conv`; returns
    // whether all of `src` fit.
    let convert =
        |src: &[u8], dn: &mut [u8; 11], from: usize, to: usize, conv: &mut u8| -> (usize, bool) {
            let mut j = from;
            let mut i = 0;
            while i < src.len() && j < to {
                let c = unix2dos(src[i]);
                dn[j] = c;
                if (c & 0x80 != 0 || src[i] != c) && *conv != 3 {
                    *conv = 2;
                }
                match c {
                    1 => {
                        *conv = 3;
                        dn[j] = b'_';
                        j += 1;
                    }
                    2 => {
                        *conv = 3;
                        dn[j] = b' ';
                    }
                    _ => j += 1,
                }
                i += 1;
            }
            (j, i >= src.len())
        };
    let base_end = match ext {
        Some(e) => {
            // Up to (with) the first of any trailing dots, as FreeBSD
            // takes it: the dot is then left out, forcing a number.
            let ext_end = dot.unwrap_or(un.len());
            let (_, all) = convert(&un[e..ext_end], &mut dn, 8, 11, &mut conv);
            if !all {
                conv = 3;
            }
            e - 1
        }
        None => {
            // Trailing blanks and dots are not part of the name.
            let mut end = un.len();
            while end > 0 && (un[end - 1] == b' ' || un[end - 1] == b'.') {
                end -= 1;
            }
            end
        }
    };
    let (j, all) = convert(&un[..base_end], &mut dn, 0, 8, &mut conv);
    if !all {
        conv = 3;
    }
    if j == 0 {
        dn[0] = b'_';
    }
    if conv != 3 {
        if generation > 1 {
            return (dn, Conv::Invalid);
        }
    } else if generation != 0 {
        // Insert `~generation` into the base, after what of it still fits.
        let digits = alloc::format!("{generation}");
        if digits.len() > 6 {
            return (dn, Conv::Invalid);
        }
        let mut i = 8;
        while i > 0 && dn[i - 1] == b' ' {
            i -= 1;
        }
        let need = digits.len() + 1;
        if need > 8 - i {
            i = 8 - need;
        }
        dn[i] = b'~';
        dn[i + 1..i + need].copy_from_slice(digits.as_bytes());
        for d in &mut dn[i + need..8] {
            *d = b' ';
        }
    }
    if dn[0] == 0xe5 {
        dn[0] = SLOT_E5;
    }
    let conv = match conv {
        1 => Conv::Exact,
        2 => Conv::Long,
        _ => Conv::LongNumbered,
    };
    (dn, conv)
}

/// A short name as a Unix name, lower-casing the base or extension as
/// `lower`'s `LCASE_*` bits say (FreeBSD's `dos2unixfn`).
#[must_use]
pub fn dos2unixfn(dn: &[u8; 11], lower: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    let map = |c: u8, low: bool| {
        let c = dos2unix(c);
        let c = if low { to_lower(c) } else { c };
        if c == b'/' { b'?' } else { c }
    };
    for (i, &c) in dn[..8].iter().enumerate() {
        if c == b' ' {
            break;
        }
        let c = if i == 0 && c == SLOT_E5 { 0xe5 } else { c };
        out.push(map(c, lower & LCASE_BASE != 0));
    }
    if dn[8] != b' ' {
        out.push(b'.');
        for &c in &dn[8..] {
            if c == b' ' {
                break;
            }
            out.push(map(c, lower & LCASE_EXT != 0));
        }
    }
    out
}

/// A name without its trailing blanks and dots (FreeBSD's
/// `winLenFixup`): what a long name stores.
#[must_use]
pub fn win_trim(un: &[u8]) -> &[u8] {
    let mut end = un.len();
    while end > 0 && (un[end - 1] == b' ' || un[end - 1] == b'.') {
        end -= 1;
    }
    &un[..end]
}

/// How many long-name slots `un` needs (FreeBSD's `winSlotCnt`); 0 if it
/// is too long.
#[must_use]
pub fn win_slots(un: &[u8]) -> usize {
    let n = win_trim(un).len();
    if n > WIN_MAXLEN {
        0
    } else {
        n.div_ceil(WIN_CHARS)
    }
}

/// The checksum of a short name that its long-name slots carry (FreeBSD's
/// `winChksum`).
#[must_use]
pub fn win_chksum(dn: &[u8; 11]) -> u8 {
    dn.iter()
        .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c))
}

/// Long-name slot number `cnt` (1 is next to the short entry) of `un`,
/// as 32 directory bytes (FreeBSD's `unix2winfn`).
#[must_use]
pub fn unix2winfn(un: &[u8], cnt: usize, chksum: u8) -> [u8; 32] {
    let name = win_trim(un);
    let mut e = [0xffu8; 32];
    let start = (cnt - 1) * WIN_CHARS;
    let piece: &[u8] = name.get(start..).unwrap_or(&[]);
    let mut last = false;
    // The UCS-2 characters of this slot: the bytes, a NUL after the last,
    // then 0xffff padding (left as filled).
    let positions = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    for (k, &at) in positions.iter().enumerate() {
        if k < piece.len() {
            e[at] = piece[k];
            e[at + 1] = 0;
        } else if k == piece.len() {
            e[at] = 0;
            e[at + 1] = 0;
            last = true;
            break;
        }
    }
    if piece.len() <= WIN_CHARS && start + piece.len() >= name.len() {
        last = true;
    }
    e[0] = u8::try_from(cnt).unwrap_or(0) | if last { 0x40 } else { 0 };
    e[11] = 0x0f;
    e[12] = 0;
    e[13] = chksum;
    e[26] = 0;
    e[27] = 0;
    e
}

/// A long-name slot's characters (up to 13), as Unix bytes: what has no
/// ISO 8859-1 equivalent shows as `?`; `None` if the slot holds a `/`.
#[must_use]
pub fn win2unix_piece(e: &[u8]) -> Option<Vec<u8>> {
    let positions = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    let mut out = Vec::with_capacity(WIN_CHARS);
    for &at in &positions {
        let code = u16::from_le_bytes([e[at], e[at + 1]]);
        match code {
            0 => break,
            0x2f => return None,
            0x0100.. => out.push(b'?'),
            c => out.push(u8::try_from(c).unwrap_or(b'?')),
        }
    }
    Some(out)
}

/// Whether long name `stored` (as read back) is `un`, ignoring case as
/// FreeBSD's `winChkName` does.
#[must_use]
pub fn win_matches(stored: &[u8], un: &[u8]) -> bool {
    let un = win_trim(un);
    stored.len() == un.len()
        && stored
            .iter()
            .zip(un)
            .all(|(&a, &b)| to_lower(a) == to_lower(b))
}
