// SPDX-License-Identifier: BSD-2-Clause
//! The Internet checksum (RFC 1071) over bytes and over packets whose
//! segments may split a 16-bit word, and the pseudo-headers UDP and TCP
//! add to theirs.
//!
//! Behavior reference: sys/netinet/in_cksum.c (`in_cksum_skip`,
//! `in_pseudo`), sys/netinet/ip_output.c (`in_delayed_cksum`).

use mbuf::Chain;

/// The one's-complement sum of `b`'s big-endian words, unfolded; an odd
/// last byte is the high half of a word.
#[must_use]
pub fn sum(b: &[u8]) -> u64 {
    let mut s = 0u64;
    let (words, rest) = b.as_chunks::<2>();
    for c in words {
        s += u64::from(u16::from_be_bytes(*c));
    }
    if let [x] = rest {
        s += u64::from(*x) << 8;
    }
    s
}

/// `s` folded to 16 bits (not complemented).
#[must_use]
pub fn fold(mut s: u64) -> u16 {
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    #[allow(clippy::cast_possible_truncation, reason = "folded to 16 bits")]
    {
        s as u16
    }
}

/// The checksum of `b` (`in_cksum` of contiguous bytes).
#[must_use]
pub fn of(b: &[u8]) -> u16 {
    !fold(sum(b))
}

/// The unfolded sum of `len` bytes of `c` from `off`; a segment that
/// starts on an odd byte has its sum's bytes swapped, as the words it
/// holds straddle the segments.
#[must_use]
pub fn sum_chain(c: &Chain, mut off: usize, mut len: usize) -> u64 {
    let mut s = 0u64;
    let mut odd = false;
    for seg in c.segments() {
        if len == 0 {
            break;
        }
        if off >= seg.len() {
            off -= seg.len();
            continue;
        }
        let n = (seg.len() - off).min(len);
        let part = &seg[off..off + n];
        let p = fold(sum(part));
        s += u64::from(if odd { p.swap_bytes() } else { p });
        odd ^= n % 2 == 1;
        len -= n;
        off = 0;
    }
    s
}

/// The checksum of `len` bytes of `c` from `off` (`in_cksum_skip`).
#[must_use]
pub fn of_chain(c: &Chain, off: usize, len: usize) -> u16 {
    !fold(sum_chain(c, off, len))
}

/// The IPv4 pseudo-header's sum for a transport segment of `len` bytes of
/// protocol `proto` (`in_pseudo` of the addresses with `htons(len +
/// proto)`), unfolded.
#[must_use]
pub fn pseudo4(src: [u8; 4], dst: [u8; 4], proto: u8, len: usize) -> u64 {
    sum(&src) + sum(&dst) + u64::from(proto) + len as u64
}
