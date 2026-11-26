// SPDX-License-Identifier: BSD-2-Clause
//! The ring instructions against specification 07 §7.4.5's literal words.

use crate::ring;

#[test]
fn encodings_are_the_specified_literals() {
    // With 128 work registers: RA = r124, RV = r126.
    let (ra, rv) = (124u8, 126u8);
    assert_eq!(ring::mov32(rv, 7), 0x02 << 56 | u64::from(rv) << 48 | 7);
    assert_eq!(
        ring::flush_cache2(rv),
        0x24 << 56 | u64::from(rv) << 40 | 0x233
    );
    assert_eq!(
        ring::mov48(ra, 0x1234_5678_9ABC),
        0x01 << 56 | u64::from(ra) << 48 | 0x1234_5678_9ABC
    );
    // The immediate is 48 bits: higher bits are dropped, never smeared into the register.
    assert_eq!(ring::mov48(ra, u64::MAX) >> 48 & 0xFF, u64::from(ra));
    assert_eq!(ring::wait(1), 0x03 << 56 | 1 << 16);
    assert_eq!(
        ring::call(ra, rv),
        0x20 << 56 | u64::from(ra) << 40 | u64::from(rv) << 32
    );
    assert_eq!(
        ring::sync_add64(ra, rv),
        0x33 << 56 | u64::from(ra) << 40 | u64::from(rv) << 32 | 1
    );
    assert_eq!(ring::error_barrier(), 0x2F << 56);
}

#[test]
fn a_job_is_the_specified_sequence_in_two_lines() {
    let w = ring::job(128, 8, 0x10_0000, 0x200, 0xFF_0000_1000, 5);
    let (ra, rv) = (124, 126);
    assert_eq!(
        w[..11],
        [
            ring::mov32(rv, 5),
            ring::flush_cache2(rv),
            ring::mov48(ra, 0x10_0000),
            ring::mov32(rv, 0x200),
            ring::wait(1),
            ring::call(ra, rv),
            ring::mov48(ra, 0xFF_0000_1000),
            ring::mov48(rv, 1),
            ring::wait(0xFF),
            ring::sync_add64(ra, rv),
            ring::error_barrier(),
        ]
    );
    // Zero padding to 16 instructions: two 64-byte lines.
    assert_eq!(w.len(), 2 * ring::LINE);
    assert!(w[11..].iter().all(|&x| x == 0));
}

#[test]
fn sixteen_scoreboards_wait_on_all() {
    let w = ring::job(96, 16, 0, 8, 0, 0);
    assert_eq!(w[8], ring::wait(0xFFFF));
    // 96 work registers: RA = r92, RV = r94, as the specification's example.
    assert_eq!(w[2] >> 48 & 0xFF, 92);
    assert_eq!(w[0] >> 48 & 0xFF, 94);
}
