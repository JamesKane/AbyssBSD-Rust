// SPDX-License-Identifier: BSD-2-Clause

extern crate std;
use std::vec::Vec;

use crate::gtdt::*;
use crate::testutil::{B, one, torture};
use crate::{Error, Sdt};

fn frame(n: u8, base: u64) -> B {
    B::new()
        .u8(n)
        .zeros(3)
        .u64(base)
        .u64(u64::MAX)
        .u32(60 + u32::from(n))
        .u32(1)
        .u32(0)
        .u32(2)
        .u32(2)
}

fn gt_block(frames: u32, off: u32, len_extra: usize) -> B {
    let mut b = B::new()
        .u8(0)
        .u16(0)
        .u8(0)
        .u64(0x2a81_0000)
        .u32(frames)
        .u32(off)
        .bytes(&frame(0, 0x2a82_0000).0)
        .bytes(&frame(1, 0x2a83_0000).0)
        .zeros(len_extra);
    let len = u16::try_from(b.len()).unwrap();
    b.0[1..3].copy_from_slice(&len.to_le_bytes());
    b
}

fn watchdog() -> B {
    B::new()
        .u8(1)
        .u16(28)
        .u8(0)
        .u64(0x2a44_0000)
        .u64(0x2a45_0000)
        .u32(93)
        .u32(1 | 4)
}

/// The fixed fields of a revision 3 table, with `count` platform timers at
/// `offset`.
fn fixed(count: u32, offset: u32) -> B {
    B::new()
        .u64(u64::MAX) // CntControlBase not provided
        .u32(0)
        .u32(29)
        .u32(0) // secure EL1: level, high
        .u32(30)
        .u32(4) // non-secure EL1: always on
        .u32(27)
        .u32(3) // virtual EL1: edge, low
        .u32(26)
        .u32(0) // EL2
        .u64(0x2a80_0000) // CntReadBase
        .u32(count)
        .u32(offset)
        .u32(28)
        .u32(0) // virtual EL2
}

fn body() -> Vec<u8> {
    fixed(3, 104)
        .bytes(&gt_block(2, 20, 0).0)
        .bytes(&watchdog().0)
        .bytes(&[7, 3, 0])
        .0
}

fn gtdt(m: &crate::testutil::Mem) -> Gtdt<'_> {
    parse(&Sdt::read(&m, 0).unwrap()).unwrap()
}

#[test]
fn decodes_timers() {
    let m = one(*b"GTDT", 3, &body());
    let g = gtdt(&m);
    assert_eq!(g.cnt_control_base(), None);
    assert_eq!(g.cnt_read_base(), Some(0x2a80_0000));
    let s = g.secure_el1().unwrap();
    assert_eq!(s.gsiv, 29);
    assert!(!s.flags.edge_triggered() && !s.flags.active_low() && !s.flags.always_on());
    let ns = g.nonsecure_el1().unwrap();
    assert_eq!(ns.gsiv, 30);
    assert!(ns.flags.always_on());
    let v = g.virtual_el1().unwrap();
    assert!(v.flags.edge_triggered() && v.flags.active_low());
    assert_eq!(g.nonsecure_el2().map(|t| t.gsiv), Some(26));
    assert_eq!(g.virtual_el2().map(|t| t.gsiv), Some(28));

    let p: Vec<_> = g.platform_timers().map(Result::unwrap).collect();
    assert_eq!(p.len(), 3);
    let PlatformTimer::GtBlock(b) = p[0] else {
        panic!("{:?}", p[0])
    };
    assert_eq!(b.cnt_ctl_base, 0x2a81_0000);
    let f: Vec<_> = b.frames().collect();
    assert_eq!(f.len(), 2);
    assert_eq!(
        f[1],
        GtFrame {
            frame_number: 1,
            cnt_base: 0x2a83_0000,
            cnt_el0_base: u64::MAX,
            physical_gsiv: 61,
            physical_flags: GtFrameFlags(1),
            virtual_gsiv: 0,
            virtual_flags: GtFrameFlags(2),
            secure: false,
            always_on: true,
        }
    );
    assert!(f[1].physical_flags.edge_triggered() && f[1].virtual_flags.active_low());
    assert_eq!(
        p[1],
        PlatformTimer::Watchdog(Watchdog {
            refresh_frame: 0x2a44_0000,
            control_frame: 0x2a45_0000,
            gsiv: 93,
            edge_triggered: true,
            active_low: false,
            secure: true,
        })
    );
    assert_eq!(
        p[2],
        PlatformTimer::Other {
            kind: 7,
            bytes: &[7, 3, 0]
        }
    );
}

#[test]
fn older_revisions() {
    // Revision 2: no virtual EL2 timer, platform timers right after.
    let mut b = fixed(1, 96).0;
    b.truncate(96 - 36);
    b.extend_from_slice(&watchdog().0);
    let m = one(*b"GTDT", 2, &b);
    let g = gtdt(&m);
    assert_eq!(g.virtual_el2(), None);
    assert_eq!(g.platform_timers().count(), 1);
    // ACPI 5.0: through the EL2 timer only.
    let mut b = fixed(0, 0).0;
    b.truncate(80 - 36);
    let m = one(*b"GTDT", 1, &b);
    let g = gtdt(&m);
    assert_eq!(g.nonsecure_el1().map(|t| t.gsiv), Some(30));
    assert_eq!(g.cnt_read_base(), None);
    assert_eq!(g.platform_timers().count(), 0);
    // Shorter than that is rejected.
    b.pop();
    let m = one(*b"GTDT", 1, &b);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap()),
        Err(Error::BadLength(*b"GTDT"))
    );
}

#[test]
fn absent_timers_and_bases() {
    let mut b = body();
    b[48 - 36..52 - 36].copy_from_slice(&0u32.to_le_bytes());
    b[96 - 36..100 - 36].copy_from_slice(&0u32.to_le_bytes());
    b[0..8].copy_from_slice(&0u64.to_le_bytes());
    let m = one(*b"GTDT", 3, &b);
    let g = gtdt(&m);
    assert_eq!(g.secure_el1(), None);
    assert_eq!(g.virtual_el2(), None);
    assert_eq!(g.cnt_control_base(), None);
}

#[test]
fn bad_platform_timer_references() {
    let err = Error::Malformed("GTDT platform timer length");
    // Offset past the end, offset into the fixed fields, too many timers.
    for (count, off) in [(1, 4000), (1, 40), (4, 104)] {
        let b = fixed(count, off).bytes(&watchdog().0).0;
        let m = one(*b"GTDT", 3, &b);
        let p: Vec<_> = gtdt(&m).platform_timers().collect();
        assert_eq!(p.last(), Some(&Err(err)), "{count} {off}");
    }
    // Zero length cannot loop.
    let b = fixed(1000, 104).bytes(&[1, 0, 0, 0]).0;
    let m = one(*b"GTDT", 3, &b);
    assert_eq!(gtdt(&m).platform_timers().collect::<Vec<_>>(), [Err(err)]);
    // GT block frames outside the block.
    for (frames, off) in [(3, 20), (2, 21), (u32::MAX, 20), (1, u32::MAX)] {
        let b = fixed(1, 104).bytes(&gt_block(frames, off, 0).0).0;
        let m = one(*b"GTDT", 3, &b);
        let p: Vec<_> = gtdt(&m).platform_timers().collect();
        assert_eq!(p, [Err(Error::Malformed("GTDT platform timer too short"))]);
    }
    // A block with slack after its frames is fine.
    let b = fixed(1, 104).bytes(&gt_block(1, 60, 0).0).0;
    let m = one(*b"GTDT", 3, &b);
    let Some(Ok(PlatformTimer::GtBlock(blk))) = gtdt(&m).platform_timers().next() else {
        panic!()
    };
    assert_eq!(
        blk.frames().map(|f| f.frame_number).collect::<Vec<_>>(),
        [1]
    );
}

#[test]
fn rejects_wrong_signature() {
    let m = one(*b"GTDX", 3, &body());
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
}

#[test]
fn survives_corruption() {
    torture(*b"GTDT", 3, &body());
}
