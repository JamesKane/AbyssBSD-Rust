// SPDX-License-Identifier: BSD-2-Clause
//! AES (FIPS 197) with 128-, 192-, and 256-bit keys, in constant time.
//!
//! The S-box is computed, not looked up: the inverse in GF(2^8) as
//! x^254, eight bytes at a time in a `u64`, then the affine map. There are
//! no data-dependent branches or memory accesses. It is slow (the CPU's
//! instructions are the fast path, `sys/crypto`); the key schedule here
//! also feeds those instructions.

/// Bytes per block.
pub const BLOCK: usize = 16;

const LO: u64 = 0x0101_0101_0101_0101;

/// Multiplies each byte by x.
const fn xtime(x: u64) -> u64 {
    ((x & (0x7f * LO)) << 1) ^ (((x >> 7) & LO) * 0x1b)
}

/// Multiplies each byte of `a` by the matching byte of `b`.
const fn gmul(mut a: u64, b: u64) -> u64 {
    let mut r = 0;
    let mut i = 0;
    while i < 8 {
        r ^= a & (((b >> i) & LO) * 0xff);
        a = xtime(a);
        i += 1;
    }
    r
}

/// Each byte's inverse (0 stays 0): x^254 = x^2 x^4 ... x^128.
const fn inverse(x: u64) -> u64 {
    let mut t = gmul(x, x);
    let mut r = t;
    let mut i = 0;
    while i < 6 {
        t = gmul(t, t);
        r = gmul(r, t);
        i += 1;
    }
    r
}

/// Rotates each byte left by `k` (1 to 7).
const fn rotl(x: u64, k: u32) -> u64 {
    let hi = ((0xffu64 << k) & 0xff) * LO;
    ((x << k) & hi) | ((x >> (8 - k)) & !hi)
}

/// SubBytes on eight bytes.
const fn sub(x: u64) -> u64 {
    let b = inverse(x);
    b ^ rotl(b, 1) ^ rotl(b, 2) ^ rotl(b, 3) ^ rotl(b, 4) ^ (0x63 * LO)
}

/// InvSubBytes on eight bytes.
const fn inv_sub(x: u64) -> u64 {
    inverse(rotl(x, 1) ^ rotl(x, 3) ^ rotl(x, 6) ^ (0x05 * LO))
}

fn sub_state(s: &mut [u8; 16], f: fn(u64) -> u64) {
    for half in s.as_chunks_mut::<8>().0 {
        *half = f(u64::from_le_bytes(*half)).to_le_bytes();
    }
}

fn sub_word(w: u32) -> u32 {
    sub(u64::from(w)) as u32
}

/// Multiplies a byte by x.
const fn xt(b: u8) -> u8 {
    (b << 1) ^ ((b >> 7) * 0x1b)
}

/// Multiplies a byte by a constant.
const fn mulc(b: u8, c: u8) -> u8 {
    gmul(b as u64, c as u64) as u8
}

fn mix_column(c: &mut [u8; 4]) {
    let [a0, a1, a2, a3] = *c;
    let all = a0 ^ a1 ^ a2 ^ a3;
    c[0] ^= all ^ xt(a0 ^ a1);
    c[1] ^= all ^ xt(a1 ^ a2);
    c[2] ^= all ^ xt(a2 ^ a3);
    c[3] ^= all ^ xt(a3 ^ a0);
}

fn inv_mix_column(c: &mut [u8; 4]) {
    let [a0, a1, a2, a3] = *c;
    c[0] = mulc(a0, 14) ^ mulc(a1, 11) ^ mulc(a2, 13) ^ mulc(a3, 9);
    c[1] = mulc(a0, 9) ^ mulc(a1, 14) ^ mulc(a2, 11) ^ mulc(a3, 13);
    c[2] = mulc(a0, 13) ^ mulc(a1, 9) ^ mulc(a2, 14) ^ mulc(a3, 11);
    c[3] = mulc(a0, 11) ^ mulc(a1, 13) ^ mulc(a2, 9) ^ mulc(a3, 14);
}

fn shift_rows(s: &mut [u8; 16]) {
    let t = *s;
    for c in 0..4 {
        for r in 1..4 {
            s[4 * c + r] = t[4 * ((c + r) % 4) + r];
        }
    }
}

fn inv_shift_rows(s: &mut [u8; 16]) {
    let t = *s;
    for c in 0..4 {
        for r in 1..4 {
            s[4 * ((c + r) % 4) + r] = t[4 * c + r];
        }
    }
}

fn add(s: &mut [u8; 16], k: &[u8]) {
    for (s, k) in s.iter_mut().zip(k) {
        *s ^= k;
    }
}

/// An expanded key.
#[derive(Clone)]
pub struct Aes {
    rounds: usize,
    /// Round keys, 16 bytes each, in the order encryption uses them.
    rk: [u8; 240],
}

impl core::fmt::Debug for Aes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("Aes")
            .field("rounds", &self.rounds)
            .finish_non_exhaustive()
    }
}

impl Aes {
    /// Expands a 16-, 24-, or 32-byte key; `None` for another length.
    #[must_use]
    pub fn new(key: &[u8]) -> Option<Self> {
        let nk = match key.len() {
            16 | 24 | 32 => key.len() / 4,
            _ => return None,
        };
        let rounds = nk + 6;
        let mut w = [0u32; 60];
        for (w, k) in w.iter_mut().zip(key.as_chunks::<4>().0) {
            *w = u32::from_be_bytes(*k);
        }
        let mut rcon = 1u8;
        for i in nk..4 * (rounds + 1) {
            let mut t = w[i - 1];
            if i % nk == 0 {
                t = sub_word(t.rotate_left(8)) ^ (u32::from(rcon) << 24);
                rcon = xt(rcon);
            } else if nk > 6 && i % nk == 4 {
                t = sub_word(t);
            }
            w[i] = w[i - nk] ^ t;
        }
        let mut rk = [0u8; 240];
        for (b, w) in rk.as_chunks_mut::<4>().0.iter_mut().zip(&w) {
            *b = w.to_be_bytes();
        }
        w.fill(0);
        Some(Self { rounds, rk })
    }

    /// 10, 12, or 14.
    #[must_use]
    pub const fn rounds(&self) -> usize {
        self.rounds
    }

    /// The encryption round keys, `16 * (rounds + 1)` bytes.
    #[must_use]
    pub fn round_keys(&self) -> &[u8] {
        &self.rk[..16 * (self.rounds + 1)]
    }

    /// The equivalent inverse cipher's round keys (FIPS 197 §5.3.5), as
    /// AESDEC and AESD use them: the encryption keys reversed, with
    /// InvMixColumns applied to all but the first and last.
    #[must_use]
    pub fn dec_round_keys(&self) -> [u8; 240] {
        let n = self.rounds;
        let mut dk = [0u8; 240];
        for i in 0..=n {
            dk[16 * i..16 * i + 16].copy_from_slice(&self.rk[16 * (n - i)..16 * (n - i) + 16]);
            if i != 0 && i != n {
                for c in dk[16 * i..16 * i + 16].as_chunks_mut::<4>().0 {
                    inv_mix_column(c);
                }
            }
        }
        dk
    }

    pub fn encrypt_block(&self, s: &mut [u8; 16]) {
        add(s, &self.rk[..16]);
        for r in 1..=self.rounds {
            sub_state(s, sub);
            shift_rows(s);
            if r != self.rounds {
                for c in s.as_chunks_mut::<4>().0 {
                    mix_column(c);
                }
            }
            add(s, &self.rk[16 * r..16 * r + 16]);
        }
    }

    pub fn decrypt_block(&self, s: &mut [u8; 16]) {
        add(s, &self.rk[16 * self.rounds..16 * self.rounds + 16]);
        for r in (0..self.rounds).rev() {
            inv_shift_rows(s);
            sub_state(s, inv_sub);
            add(s, &self.rk[16 * r..16 * r + 16]);
            if r != 0 {
                for c in s.as_chunks_mut::<4>().0 {
                    inv_mix_column(c);
                }
            }
        }
    }
}

impl Drop for Aes {
    fn drop(&mut self) {
        self.rk.fill(0);
    }
}
