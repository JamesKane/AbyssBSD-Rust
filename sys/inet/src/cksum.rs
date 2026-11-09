// SPDX-License-Identifier: BSD-2-Clause
//! The Internet checksum (RFC 1071) over bytes and over packets whose
//! segments may split a 16-bit word, and the pseudo-headers UDP and TCP
//! add to theirs.
//!
//! Behavior reference: sys/netinet/in_cksum.c (`in_cksum_skip`,
//! `in_pseudo`), sys/netinet/ip_output.c (`in_delayed_cksum`).

use mbuf::Chain;

/// The one's-complement sum of `b`'s big-endian words, folded to 16 bits;
/// an odd last byte is the high half of a word.
///
/// Summed in native order eight bytes at a time with the carries wrapped
/// round, then folded and put in network order once (RFC 1071 §2(A) and
/// (B): the sum does not depend on the word size or the byte order).
#[must_use]
pub fn sum(b: &[u8]) -> u64 {
    let add = |acc: u64, v: u64| {
        let (s, carry) = acc.overflowing_add(v);
        s + u64::from(carry)
    };
    let (words, rest) = b.as_chunks::<8>();
    let mut acc = words
        .iter()
        .fold(0u64, |acc, w| add(acc, u64::from_ne_bytes(*w)));
    let (pairs, last) = rest.as_chunks::<2>();
    for p in pairs {
        acc = add(acc, u64::from(u16::from_ne_bytes(*p)));
    }
    if let [x] = last {
        acc = add(acc, u64::from(u16::from_ne_bytes([*x, 0])));
    }
    // Native 16-bit words, folded; then in network order.
    let mut f = (acc & 0xffff_ffff) + (acc >> 32);
    f = (f & 0xffff) + (f >> 16);
    f = (f & 0xffff) + (f >> 16);
    f = (f & 0xffff) + (f >> 16);
    #[allow(clippy::cast_possible_truncation, reason = "folded to 16 bits")]
    let f = f as u16;
    u64::from(if cfg!(target_endian = "little") {
        f.swap_bytes()
    } else {
        f
    })
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

#[cfg(test)]
mod sum_tests {
    use super::*;

    /// RFC 1071's sum one big-endian word at a time, as it was.
    fn plain(b: &[u8]) -> u16 {
        let mut s = 0u64;
        let (words, rest) = b.as_chunks::<2>();
        for c in words {
            s += u64::from(u16::from_be_bytes(*c));
        }
        if let [x] = rest {
            s += u64::from(*x) << 8;
        }
        fold(s)
    }

    #[test]
    fn wide_native_sums_match_the_plain_one() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut buf = [0u8; 2100];
        for b in &mut buf {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x.to_be_bytes()[4];
        }
        // Every length and alignment up to the first 2 KiB, and all ones.
        for start in 0..9 {
            for len in 0..2048 {
                let b = &buf[start..start + len];
                assert_eq!(fold(sum(b)), plain(b), "start {start} len {len}");
            }
        }
        let ones = [0xffu8; 1501];
        assert_eq!(fold(sum(&ones)), plain(&ones));
        // RFC 1071's example: 0001 f203 f4f5 f6f7 sums to ddf2.
        assert_eq!(
            fold(sum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7])),
            0xddf2
        );
    }
}
