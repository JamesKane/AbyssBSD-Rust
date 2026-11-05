// SPDX-License-Identifier: BSD-2-Clause
//! Sealing against opening in every suite, and the checks of what user
//! space and the wire give. `ktls_test` is the check against OpenSSL.

extern crate std;

use std::vec;
use std::vec::Vec;

use abi::errno::Errno;
use abi::ktls::*;

use crate::{Direction, Enable, HDR, MAX_FRAME, Session, Tx, check_lengths};

fn random(b: &mut [u8]) {
    b.fill(0xa5);
}

const KEY: [u8; 32] = [7; 32];
const MAC: [u8; 48] = [9; 48];
const IV: [u8; 16] = [3; 16];

fn en(cipher: i32, klen: usize, auth: i32, minor: u8) -> Enable<'static> {
    let iv_len = match (cipher, minor) {
        (CRYPTO_AES_NIST_GCM_16, TLS_MINOR_VER_TWO) => 4,
        (CRYPTO_AES_CBC, TLS_MINOR_VER_ZERO) => 16,
        (CRYPTO_AES_CBC, _) => 0,
        _ => 12,
    };
    let mac_len = match auth {
        CRYPTO_SHA1_HMAC => 20,
        CRYPTO_SHA2_256_HMAC => 32,
        CRYPTO_SHA2_384_HMAC => 48,
        _ => 0,
    };
    Enable {
        cipher_key: &KEY[..klen],
        iv: &IV[..iv_len],
        auth_key: &MAC[..mac_len],
        cipher_algorithm: cipher,
        auth_algorithm: auth,
        flags: 0,
        vmajor: TLS_MAJOR_VER_ONE,
        vminor: minor,
        rec_seq: 41u64.to_be_bytes(),
    }
}

/// The suites that receive, as `ktls_test` lists them.
fn suites() -> Vec<Enable<'static>> {
    let mut v = Vec::new();
    for k in [16, 32] {
        for m in [TLS_MINOR_VER_ONE, TLS_MINOR_VER_TWO] {
            v.push(en(CRYPTO_AES_CBC, k, CRYPTO_SHA1_HMAC, m));
        }
        v.push(en(
            CRYPTO_AES_CBC,
            k,
            CRYPTO_SHA2_256_HMAC,
            TLS_MINOR_VER_TWO,
        ));
        v.push(en(
            CRYPTO_AES_CBC,
            k,
            CRYPTO_SHA2_384_HMAC,
            TLS_MINOR_VER_TWO,
        ));
        v.push(en(CRYPTO_AES_NIST_GCM_16, k, 0, TLS_MINOR_VER_TWO));
        v.push(en(CRYPTO_AES_NIST_GCM_16, k, 0, TLS_MINOR_VER_THREE));
    }
    v.push(en(CRYPTO_CHACHA20_POLY1305, 32, 0, TLS_MINOR_VER_TWO));
    v.push(en(CRYPTO_CHACHA20_POLY1305, 32, 0, TLS_MINOR_VER_THREE));
    v
}

fn seal(tx: &mut Tx, rtype: u8, data: &[u8]) -> Vec<u8> {
    let l = tx.layout();
    let mut rec = vec![0; l.record_len(data.len())];
    rec[l.hlen..l.hlen + data.len()].copy_from_slice(data);
    tx.seal(rtype, &mut rec, data.len(), &mut random);
    rec
}

#[test]
fn records_open_in_every_suite() {
    for e in suites() {
        let mut tx = Tx::new(&e, &mut random).unwrap();
        let rx = Session::new(&e, Direction::Rx).unwrap();
        for (i, n) in [1usize, 15, 16, 17, 64, 1000, MAX_FRAME].iter().enumerate() {
            let data: Vec<u8> = (0..*n)
                .map(|i| u8::try_from(i % 251).unwrap() + 1)
                .collect();
            let mut rec = seal(&mut tx, TLS_RLTYPE_APP, &data);
            assert_eq!(rx.check_header(&rec[..rx.layout().hlen]), Ok(rec.len()));
            let o = rx.open(&mut rec, 41 + i as u64).unwrap();
            assert_eq!(o.rtype, TLS_RLTYPE_APP);
            assert_eq!(&rec[o.start..o.start + o.len], &data[..]);
        }
    }
}

#[test]
fn a_record_keeps_its_type_and_sequence() {
    for e in suites() {
        let mut tx = Tx::new(&e, &mut random).unwrap();
        let rx = Session::new(&e, Direction::Rx).unwrap();
        let mut rec = seal(&mut tx, TLS_RLTYPE_ALERT, b"bye");
        let mut good = rec.clone();
        let o = rx.open(&mut good, 41).unwrap();
        assert_eq!(o.rtype, TLS_RLTYPE_ALERT);
        // The wrong sequence number fails, though GCM 1.2 carries a nonce:
        // it is in the additional data.
        assert_eq!(rx.open(&mut rec, 42), Err(Errno::EBADMSG));
    }
}

#[test]
fn tls13_records_go_as_tls12_application_data() {
    let e = en(CRYPTO_AES_NIST_GCM_16, 16, 0, TLS_MINOR_VER_THREE);
    let mut tx = Tx::new(&e, &mut random).unwrap();
    let rec = seal(&mut tx, TLS_RLTYPE_ALERT, b"x");
    assert_eq!(rec[..3], [TLS_RLTYPE_APP, 3, TLS_MINOR_VER_TWO]);
}

#[test]
fn tampering_is_found() {
    for e in suites() {
        let mut tx = Tx::new(&e, &mut random).unwrap();
        let rx = Session::new(&e, Direction::Rx).unwrap();
        let rec = seal(&mut tx, TLS_RLTYPE_APP, &[0x55; 100]);
        for at in [rx.layout().hlen, rec.len() - 1, rec.len() / 2] {
            let mut bad = rec.clone();
            bad[at] ^= 1;
            assert_eq!(rx.open(&mut bad, 41), Err(Errno::EBADMSG));
        }
    }
}

#[test]
fn an_all_zero_tls13_plaintext_has_no_type() {
    let e = en(CRYPTO_CHACHA20_POLY1305, 32, 0, TLS_MINOR_VER_THREE);
    let mut tx = Tx::new(&e, &mut random).unwrap();
    let rx = Session::new(&e, Direction::Rx).unwrap();
    let mut rec = seal(&mut tx, 0, &[0; 8]);
    assert_eq!(rx.open(&mut rec, 41), Err(Errno::EBADMSG));
}

#[test]
fn short_records_are_the_wrong_size() {
    for e in suites() {
        let rx = Session::new(&e, Direction::Rx).unwrap();
        let mut rec = vec![0; rx.layout().hlen];
        rec[0] = TLS_RLTYPE_APP;
        assert_eq!(rx.open(&mut rec, 0), Err(Errno::EMSGSIZE));
    }
}

#[test]
fn headers_are_checked() {
    let e = en(CRYPTO_AES_NIST_GCM_16, 16, 0, TLS_MINOR_VER_THREE);
    let rx = Session::new(&e, Direction::Rx).unwrap();
    let h = |t, ma, mi, len: u16| {
        let l = len.to_be_bytes();
        [t, ma, mi, l[0], l[1]]
    };
    let ok = h(TLS_RLTYPE_APP, 3, TLS_MINOR_VER_TWO, 100);
    assert_eq!(rx.check_header(&ok), Ok(HDR + 100));
    let bad = [
        (h(TLS_RLTYPE_APP, 2, TLS_MINOR_VER_TWO, 100), Errno::EINVAL),
        (
            h(TLS_RLTYPE_APP, 3, TLS_MINOR_VER_THREE, 100),
            Errno::EINVAL,
        ),
        (
            h(TLS_RLTYPE_ALERT, 3, TLS_MINOR_VER_TWO, 100),
            Errno::EINVAL,
        ),
        (
            h(TLS_RLTYPE_APP, 3, TLS_MINOR_VER_TWO, u16::MAX),
            Errno::EMSGSIZE,
        ),
    ];
    for (hdr, want) in bad {
        assert_eq!(rx.check_header(&hdr), Err(want));
    }
}

#[test]
fn tls10_chains_its_iv_and_sends_empty_records() {
    let e = en(CRYPTO_AES_CBC, 16, CRYPTO_SHA1_HMAC, TLS_MINOR_VER_ZERO);
    let mut tx = Tx::new(&e, &mut random).unwrap();
    assert!(tx.layout().permit_empty());
    let a = seal(&mut tx, TLS_RLTYPE_APP, &[]);
    assert_eq!(a.len(), HDR + 32);
    let m = tx.mark();
    let b = seal(&mut tx, TLS_RLTYPE_APP, &[]);
    assert_ne!(a[HDR..], b[HDR..]);
    // Going back gives the same record again.
    tx.reset(m);
    assert_eq!(seal(&mut tx, TLS_RLTYPE_APP, &[]), b);
}

#[test]
fn cbc_trailers_end_on_a_block() {
    let e = en(CRYPTO_AES_CBC, 16, CRYPTO_SHA2_256_HMAC, TLS_MINOR_VER_TWO);
    let tx = Tx::new(&e, &mut random).unwrap();
    let l = tx.layout();
    for n in 0..64 {
        let body = l.record_len(n) - l.hlen;
        assert_eq!(body % 16, 0);
        assert!(body > n + 32 && body <= n + 48);
    }
}

#[test]
fn suites_are_refused_as_freebsd_refuses_them() {
    let cbc = |k, a, m| en(CRYPTO_AES_CBC, k, a, m);
    let gcm = |k, m| en(CRYPTO_AES_NIST_GCM_16, k, 0, m);
    let rx = |e: &Enable<'_>| Session::new(e, Direction::Rx).map(|_| ());
    // Receive has no TLS 1.0, and send has it.
    let e = cbc(16, CRYPTO_SHA1_HMAC, TLS_MINOR_VER_ZERO);
    assert_eq!(rx(&e), Err(Errno::EPROTONOSUPPORT));
    assert!(Tx::new(&e, &mut random).is_ok());
    // SHA-2 HMAC is TLS 1.2 only.
    let e = cbc(16, CRYPTO_SHA2_256_HMAC, TLS_MINOR_VER_ONE);
    assert_eq!(rx(&e), Err(Errno::EINVAL));
    // GCM is TLS 1.2 and 1.3, with a 128- or 256-bit key.
    assert_eq!(rx(&gcm(16, TLS_MINOR_VER_ONE)), Err(Errno::EINVAL));
    assert_eq!(rx(&gcm(24, TLS_MINOR_VER_TWO)), Err(Errno::EINVAL));
    // ChaCha20 has one key size.
    let e = en(CRYPTO_CHACHA20_POLY1305, 16, 0, TLS_MINOR_VER_TWO);
    assert_eq!(rx(&e), Err(Errno::EINVAL));
    // A wrong version, flags, or cipher.
    let mut e = gcm(16, TLS_MINOR_VER_TWO);
    e.vmajor = 2;
    assert_eq!(rx(&e), Err(Errno::EINVAL));
    let mut e = gcm(16, TLS_MINOR_VER_TWO);
    e.flags = 1;
    assert_eq!(rx(&e), Err(Errno::EINVAL));
    let mut e = gcm(16, TLS_MINOR_VER_TWO);
    e.cipher_algorithm = 99;
    assert_eq!(rx(&e), Err(Errno::EINVAL));
}

#[test]
fn lengths_from_user_space_are_limited() {
    assert_eq!(check_lengths(16, 12, 0), Ok(()));
    for (k, iv, a) in [
        (0, 0, 0),
        (-1, 0, 0),
        (1025, 0, 0),
        (16, 33, 0),
        (16, -1, 0),
        (16, 0, 1025),
    ] {
        assert_eq!(check_lengths(k, iv, a), Err(Errno::EINVAL));
    }
}
