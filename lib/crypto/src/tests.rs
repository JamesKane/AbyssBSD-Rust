// SPDX-License-Identifier: BSD-2-Clause

use crate::blake2b::Blake2b;
use crate::chacha20::{Stream, block};

fn hex(s: &str) -> [u8; 64] {
    let s: String = s.split_whitespace().collect();
    let mut out = [0u8; 64];
    for (i, b) in out.iter_mut().enumerate().take(s.len() / 2) {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

extern crate std;
use std::string::String;

#[test]
fn chacha20_block_rfc8439() {
    // RFC 8439 §2.3.2: key 00..1f, block counter 1, nonce
    // 00:00:00:09:00:00:00:4a:00:00:00:00, as one 128-bit counter.
    let key: [u8; 32] = core::array::from_fn(|i| i as u8);
    let counter = 1u128 | (0x0900_0000u128 << 32) | (0x4a00_0000u128 << 64);
    let want = hex(
        "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e
         d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e",
    );
    assert_eq!(block(&key, counter), want);
}

#[test]
fn chacha20_stream_continues_and_rekeys() {
    let key = [7u8; 32];
    let mut s = Stream::new(key, 5);
    let mut out = [0u8; 100];
    s.fill(&mut out);
    assert_eq!(out[..64], block(&key, 5));
    assert_eq!(out[64..], block(&key, 6)[..36]);
    // The partial block is discarded: the next fill starts at block 7.
    let mut next = [0u8; 64];
    s.fill(&mut next);
    assert_eq!(next, block(&key, 7));
    s.rekey([9; 32]);
    s.fill(&mut next);
    assert_eq!(next, block(&[9; 32], 8));
}

#[test]
fn blake2b_rfc7693() {
    // RFC 7693 appendix A: BLAKE2b-512("abc").
    let want = hex(
        "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d1
         7d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923",
    );
    let mut h = Blake2b::new(64);
    h.update(b"abc");
    let mut out = [0u8; 64];
    h.finish(&mut out);
    assert_eq!(out, want);
}

#[test]
fn blake2b_incremental_matches_one_shot() {
    let data: std::vec::Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
    let mut one = [0u8; 32];
    let mut h = Blake2b::new(32);
    h.update(&data);
    h.finish(&mut one);
    for split in [0, 1, 127, 128, 129, 256, 999, 1000] {
        let mut h = Blake2b::new(32);
        h.update(&data[..split]);
        h.update(&data[split..]);
        let mut out = [0u8; 32];
        h.finish(&mut out);
        assert_eq!(out, one, "split at {split}");
    }
    // The empty message (BLAKE2b-512 of "").
    let mut out = [0u8; 64];
    Blake2b::new(64).finish(&mut out);
    assert_eq!(
        out,
        hex(
            "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f5419
             d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9be2ce"
        )
    );
}

#[test]
fn sha1_fips180() {
    use crate::sha1::{Sha1, digest};
    let h = |d: [u8; 20]| {
        d.iter()
            .map(|b| alloc_hex(*b))
            .collect::<std::string::String>()
    };
    assert_eq!(
        h(digest(b"abc")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(h(digest(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(
        h(digest(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    let mut s = Sha1::new();
    for _ in 0..1000 {
        s.update(&[b'a'; 1000]);
    }
    assert_eq!(h(s.finish()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
}

fn alloc_hex(b: u8) -> std::string::String {
    std::format!("{b:02x}")
}
