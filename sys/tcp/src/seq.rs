// SPDX-License-Identifier: BSD-2-Clause
//! Sequence-number arithmetic, modulo 2^32 (`tcp_seq.h`).
//!
//! Behavior reference: sys/netinet/tcp_seq.h.

/// A sequence number (`tcp_seq`).
pub type Seq = u32;

/// `SEQ_LT`.
#[inline]
#[must_use]
pub const fn lt(a: Seq, b: Seq) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// `SEQ_LEQ`.
#[inline]
#[must_use]
pub const fn leq(a: Seq, b: Seq) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

/// `SEQ_GT`.
#[inline]
#[must_use]
pub const fn gt(a: Seq, b: Seq) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// `SEQ_GEQ`.
#[inline]
#[must_use]
pub const fn geq(a: Seq, b: Seq) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

/// `SEQ_MIN`.
#[inline]
#[must_use]
pub const fn min(a: Seq, b: Seq) -> Seq {
    if lt(a, b) { a } else { b }
}

/// `SEQ_MAX`.
#[inline]
#[must_use]
pub const fn max(a: Seq, b: Seq) -> Seq {
    if gt(a, b) { a } else { b }
}

/// `SEQ_SUB`: `a - b` as a signed distance.
#[inline]
#[must_use]
pub const fn sub(a: Seq, b: Seq) -> i32 {
    a.wrapping_sub(b) as i32
}

/// `TSTMP_LT`, `TSTMP_GT`, `TSTMP_GEQ`: timestamps compare the same way.
#[inline]
#[must_use]
pub const fn ts_lt(a: u32, b: u32) -> bool {
    lt(a, b)
}

#[inline]
#[must_use]
pub const fn ts_gt(a: u32, b: u32) -> bool {
    gt(a, b)
}

#[inline]
#[must_use]
pub const fn ts_geq(a: u32, b: u32) -> bool {
    geq(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons_wrap() {
        assert!(lt(0xffff_fff0, 0x10));
        assert!(gt(0x10, 0xffff_fff0));
        assert!(leq(5, 5) && geq(5, 5));
        assert_eq!(max(0xffff_fff0, 0x10), 0x10);
        assert_eq!(min(0xffff_fff0, 0x10), 0xffff_fff0);
        assert_eq!(sub(0x10, 0xffff_fff0), 0x20);
    }
}
