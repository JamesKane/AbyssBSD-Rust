// SPDX-License-Identifier: BSD-2-Clause

use crate::slit::*;
use crate::testutil::{B, one, torture};
use crate::{Error, Sdt};

#[test]
fn decodes_matrix() {
    let b = B::new().u64(3).bytes(&[10, 20, 30, 21, 10, 31, 32, 33, 10]);
    let m = one(*b"SLIT", 1, &b.0);
    let s = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(s.locality_count(), 3);
    assert_eq!(s.distance(0, 0), Some(10));
    assert_eq!(s.distance(0, 2), Some(30));
    assert_eq!(s.distance(1, 0), Some(21));
    assert_eq!(s.distance(2, 1), Some(33));
    assert_eq!(s.distance(3, 0), None);
    assert_eq!(s.distance(0, 3), None);
    assert_eq!(s.distance(usize::MAX, usize::MAX), None);
}

#[test]
fn trailing_bytes_are_ignored_and_empty_is_fine() {
    let b = B::new().u64(1).bytes(&[10, 0xff, 0xff]);
    let m = one(*b"SLIT", 1, &b.0);
    let s = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(
        (s.locality_count(), s.distance(0, 0), s.distance(0, 1)),
        (1, Some(10), None)
    );
    let m = one(*b"SLIT", 1, &B::new().u64(0).0);
    let s = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!((s.locality_count(), s.distance(0, 0)), (0, None));
}

#[test]
fn rejects_matrices_that_do_not_fit() {
    for n in [3u64, 1 << 32, u64::MAX] {
        let b = B::new().u64(n).bytes(&[10; 8]);
        let m = one(*b"SLIT", 1, &b.0);
        assert_eq!(
            parse(&Sdt::read(&&m, 0).unwrap()),
            Err(Error::BadLength(*b"SLIT"))
        );
    }
    let m = one(*b"SLIT", 1, &[1, 0, 0]);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap()),
        Err(Error::BadLength(*b"SLIT"))
    );
    let m = one(*b"SRAT", 1, &B::new().u64(0).0);
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
}

#[test]
fn survives_corruption() {
    torture(*b"SLIT", 1, &B::new().u64(2).bytes(&[10, 20, 20, 10]).0);
}
