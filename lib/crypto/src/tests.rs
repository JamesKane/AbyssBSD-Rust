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

fn unhex(s: &str) -> std::vec::Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

#[test]
fn sha2_fips180() {
    use crate::{sha256, sha512};
    assert_eq!(
        sha256::digest(b"abc").to_vec(),
        unhex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    let mut s = sha512::Sha384::new();
    s.update(b"abc");
    assert_eq!(
        s.finish().to_vec(),
        unhex(
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed
             8086072ba1e7cc2358baeca134c825a7"
        )
    );
    let mut s = sha512::Sha512::new();
    s.update(b"abc");
    assert_eq!(
        s.finish().to_vec(),
        unhex(
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        )
    );
    // Padding that spills into a second block, fed in uneven pieces.
    let data: std::vec::Vec<u8> = (0..1000u32).map(|i| (i * 13) as u8).collect();
    for n in [55, 56, 63, 64, 111, 112, 127, 128, 1000] {
        let mut a = sha256::Sha256::new();
        let mut b = sha512::Sha384::new();
        for c in data[..n].chunks(7) {
            a.update(c);
            b.update(c);
        }
        let mut a1 = sha256::Sha256::new();
        a1.update(&data[..n]);
        let mut b1 = sha512::Sha384::new();
        b1.update(&data[..n]);
        assert_eq!(a.finish(), a1.finish(), "{n}");
        assert_eq!(b.finish(), b1.finish(), "{n}");
    }
}

#[test]
fn hmac_rfc4231() {
    use crate::hmac::Key;
    use crate::{sha1::Sha1, sha256::Sha256, sha512::Sha384};
    assert_eq!(
        Key::<Sha256>::new(&[0x0b; 20]).mac(b"Hi There").to_vec(),
        unhex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
    );
    assert_eq!(
        Key::<Sha384>::new(&[0x0b; 20]).mac(b"Hi There").to_vec(),
        unhex(
            "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59c
             faea9ea9076ede7f4af152e8b2fa9cb6"
        )
    );
    assert_eq!(
        Key::<Sha256>::new(b"Jefe")
            .mac(b"what do ya want for nothing?")
            .to_vec(),
        unhex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
    );
    // Test case 6: a key longer than the block is hashed first.
    assert_eq!(
        Key::<Sha384>::new(&[0xaa; 131])
            .mac(b"Test Using Larger Than Block-Size Key - Hash Key First")
            .to_vec(),
        unhex(
            "4ece084485813e9088d2c63a041bc5b44f9ef1012a2b588f3cd11f05033ac4c6
             0c2ef6ab4030fe8296248df163f44952"
        )
    );
    // RFC 2202 test case 1.
    assert_eq!(
        Key::<Sha1>::new(&[0x0b; 20]).mac(b"Hi There").to_vec(),
        unhex("b617318655057264e28bc0b6fb378c8ef146be00")
    );
}

#[test]
fn aes_fips197() {
    use crate::aes::Aes;
    for (n, want) in [
        (16u8, "69c4e0d86a7b0430d8cdb78070b4c55a"),
        (24, "dda97ca4864cdfe06eaf70a0ec0d7191"),
        (32, "8ea2b7ca516745bfeafc49904b496089"),
    ] {
        let key: std::vec::Vec<u8> = (0..n).collect();
        let aes = Aes::new(&key).unwrap();
        let pt: [u8; 16] = unhex("00112233445566778899aabbccddeeff")
            .try_into()
            .unwrap();
        let mut b = pt;
        aes.encrypt_block(&mut b);
        assert_eq!(b.to_vec(), unhex(want), "AES-{}", n * 8);
        aes.decrypt_block(&mut b);
        assert_eq!(b, pt);
    }
    assert!(Aes::new(&[0; 20]).is_none());
}

#[test]
fn aes_dec_round_keys_are_the_equivalent_inverse_cipher() {
    // FIPS 197 §5.3.5 for AES-128 with key 000102..0f: the decryption
    // schedule starts with the last round key and ends with the first.
    let aes = crate::aes::Aes::new(&(0..16).collect::<std::vec::Vec<u8>>()).unwrap();
    let dk = aes.dec_round_keys();
    assert_eq!(dk[..16], aes.round_keys()[160..176]);
    assert_eq!(dk[160..176], aes.round_keys()[..16]);
    assert_eq!(
        aes.round_keys()[160..176].to_vec(),
        unhex("13111d7fe3944a17f307a78b4d2b30c5")
    );
}

#[test]
fn cbc_sp800_38a() {
    use crate::{aes::Aes, cbc::Engine};
    let aes = Aes::new(&unhex("2b7e151628aed2a6abf7158809cf4f3c")).unwrap();
    let pt = unhex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
    let mut data = pt.clone();
    let mut iv: [u8; 16] = core::array::from_fn(|i| i as u8);
    aes.cbc_encrypt(&mut iv, &mut data);
    assert_eq!(
        data,
        unhex("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2")
    );
    assert_eq!(iv[..], data[16..]);
    let mut iv: [u8; 16] = core::array::from_fn(|i| i as u8);
    // Chained across two calls.
    aes.cbc_decrypt(&mut iv, &mut data[..16]);
    aes.cbc_decrypt(&mut iv, &mut data[16..]);
    assert_eq!(data, pt);
}

/// Key, nonce, additional data, plaintext, ciphertext, tag.
type GcmCase = (
    std::vec::Vec<u8>,
    std::vec::Vec<u8>,
    std::vec::Vec<u8>,
    std::vec::Vec<u8>,
    std::vec::Vec<u8>,
    std::vec::Vec<u8>,
);

/// GCM test cases 2, 3, and 4 from McGrew and Viega's specification, and
/// test case 4's text under a 256-bit key.
fn gcm_cases() -> [GcmCase; 4] {
    let k = "feffe9928665731c6d6a8f9467308308";
    let iv = "cafebabefacedbaddecaf888";
    let p = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72
             1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255";
    let c = "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e
             21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985";
    let aad = "feedfacedeadbeeffeedfacedeadbeefabaddad2";
    [
        (
            vec_of(16),
            vec_of(12),
            std::vec::Vec::new(),
            vec_of(16),
            unhex("0388dace60b6a392f328c2b971b2fe78"),
            unhex("ab6e47d42cec13bdf53a67b21257bddf"),
        ),
        (
            unhex(k),
            unhex(iv),
            std::vec::Vec::new(),
            unhex(p),
            unhex(c),
            unhex("4d5c2af327cd64a62cf35abd2ba6fab4"),
        ),
        (
            unhex(k),
            unhex(iv),
            unhex(aad),
            unhex(p)[..60].to_vec(),
            unhex(c)[..60].to_vec(),
            unhex("5bc94fbc3221a5db94fae95ae7121a47"),
        ),
        (
            unhex(&std::format!("{k}{k}")),
            unhex(iv),
            unhex(aad),
            unhex(p)[..60].to_vec(),
            unhex(
                "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa
                 8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662",
            ),
            unhex("76fc6ece0f4e1768cddf8853bb2d551b"),
        ),
    ]
}

fn vec_of(n: usize) -> std::vec::Vec<u8> {
    std::vec![0; n]
}

#[test]
fn gcm_mcgrew_viega() {
    use crate::gcm::{self, Soft};
    for (key, iv, aad, p, c, t) in gcm_cases() {
        let e = Soft::new(&key).unwrap();
        let iv: [u8; 12] = iv.try_into().unwrap();
        let mut data = p.clone();
        let tag = gcm::seal(&e, &iv, &aad, &mut data);
        assert_eq!(data, c);
        assert_eq!(tag.to_vec(), t);
        // Any split of the text gives the same result.
        for split in [1, 15, 16, 17, 33] {
            let mut d = p.clone();
            let mut s = gcm::Stream::new(&e, &iv, &aad);
            let (a, b) = d.split_at_mut(split.min(p.len()));
            s.encrypt(a);
            s.encrypt(b);
            assert_eq!(s.finish().to_vec(), t, "split {split}");
            assert_eq!(d, c);
        }
        let tag: [u8; 16] = t.clone().try_into().unwrap();
        assert!(gcm::open(&e, &iv, &aad, &mut data, &tag));
        assert_eq!(data, p);
        let mut bad = tag;
        bad[15] ^= 1;
        let mut data = c.clone();
        assert!(!gcm::open(&e, &iv, &aad, &mut data, &bad));
        assert_eq!(data, c, "a forged record is left alone");
    }
}

#[test]
fn poly1305_rfc8439() {
    let key: [u8; 32] = unhex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b")
        .try_into()
        .unwrap();
    let mut m = crate::poly1305::Poly1305::new(&key);
    m.update(b"Cryptographic Forum ");
    m.update(b"Research Group");
    assert_eq!(
        m.finish().to_vec(),
        unhex("a8061dc1305136c6c22b8baf0c0127a9")
    );
}

#[test]
fn chacha20poly1305_rfc8439() {
    use crate::chacha20poly1305::{Stream, open, seal};
    let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    let key: [u8; 32] = core::array::from_fn(|i| 0x80 + i as u8);
    let nonce: [u8; 12] = unhex("070000004041424344454647").try_into().unwrap();
    let aad = unhex("50515253c0c1c2c3c4c5c6c7");
    let ct = unhex(
        "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6
         3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36
         92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc
         3ff4def08e4b7a9de576d26586cec64b6116",
    );
    let t: [u8; 16] = unhex("1ae10b594f09e26a7e902ecbd0600691")
        .try_into()
        .unwrap();
    let mut d = pt.to_vec();
    assert_eq!(seal(&key, &nonce, &aad, &mut d), t);
    assert_eq!(d, ct);
    for split in [1, 63, 64, 65, 100] {
        let mut d = pt.to_vec();
        let mut s = Stream::new(&key, &nonce, &aad);
        let (a, b) = d.split_at_mut(split);
        s.encrypt(a);
        s.encrypt(b);
        assert_eq!(s.finish(), t, "split {split}");
        assert_eq!(d, ct);
    }
    let mut d = ct.clone();
    assert!(open(&key, &nonce, &aad, &mut d, &t));
    assert_eq!(d, pt);
    let mut d = ct.clone();
    d[0] ^= 1;
    assert!(!open(&key, &nonce, &aad, &mut d, &t));
}

#[test]
fn poly1305_final_reduction() {
    // Accumulators at and past 2^130 - 5 (RFC 8439 appendix A.3 test 6
    // among them), checked against a big-integer reference.
    for (key, msg, want) in [
        (
            [0xff; 32],
            [0xffu8; 64].as_slice(),
            "900fe32bc15fa8d7bca8efe4c7e37eb1",
        ),
        (
            core::array::from_fn(|i| u8::from(i == 0) * 2),
            &[0xff; 16],
            "03000000000000000000000000000000",
        ),
        (
            core::array::from_fn(|i| {
                if i == 0 {
                    1
                } else if i >= 16 {
                    0xff
                } else {
                    0
                }
            }),
            &[0xff; 48],
            "01000000000000000000000000000000",
        ),
        ([0xff; 32], &[0xff; 17], "7cfe7ff768f81f2763f8bf565df85f86"),
    ] {
        let mut m = crate::poly1305::Poly1305::new(&key);
        m.update(msg);
        assert_eq!(m.finish().to_vec(), unhex(want), "{} bytes", msg.len());
    }
}
