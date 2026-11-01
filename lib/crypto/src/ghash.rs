// SPDX-License-Identifier: MIT
// Copyright (c) 2016 Thomas Pornin <pornin@bolet.org>
// Copyright (c) 2026 The AbyssBSD Project
//! GHASH (NIST SP 800-38D §6.4) in constant time, after BearSSL's
//! `ghash_ctmul64`: carry-less multiplication done with integer
//! multiplications whose operands have every fourth bit set, so carries
//! land in the holes and are masked off.

/// The low 64 bits of the carry-less product of `x` and `y`.
const fn bmul64(x: u64, y: u64) -> u64 {
    const M0: u64 = 0x1111_1111_1111_1111;
    const M1: u64 = M0 << 1;
    const M2: u64 = M0 << 2;
    const M3: u64 = M0 << 3;
    let (x0, x1, x2, x3) = (x & M0, x & M1, x & M2, x & M3);
    let (y0, y1, y2, y3) = (y & M0, y & M1, y & M2, y & M3);
    let z0 = x0.wrapping_mul(y0) ^ x1.wrapping_mul(y3) ^ x2.wrapping_mul(y2) ^ x3.wrapping_mul(y1);
    let z1 = x0.wrapping_mul(y1) ^ x1.wrapping_mul(y0) ^ x2.wrapping_mul(y3) ^ x3.wrapping_mul(y2);
    let z2 = x0.wrapping_mul(y2) ^ x1.wrapping_mul(y1) ^ x2.wrapping_mul(y0) ^ x3.wrapping_mul(y3);
    let z3 = x0.wrapping_mul(y3) ^ x1.wrapping_mul(y2) ^ x2.wrapping_mul(y1) ^ x3.wrapping_mul(y0);
    (z0 & M0) | (z1 & M1) | (z2 & M2) | (z3 & M3)
}

/// The hash key H, with its bit-reversed halves.
#[derive(Clone)]
pub struct Ghash {
    h0: u64,
    h1: u64,
    h0r: u64,
    h1r: u64,
}

impl core::fmt::Debug for Ghash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("Ghash").finish_non_exhaustive()
    }
}

impl Ghash {
    /// The hash with key `h` (the block cipher's encryption of zero).
    #[must_use]
    pub fn new(h: &[u8; 16]) -> Self {
        let h1 = u64::from_be_bytes(h[..8].try_into().unwrap_or_default());
        let h0 = u64::from_be_bytes(h[8..].try_into().unwrap_or_default());
        Self {
            h0,
            h1,
            h0r: h0.reverse_bits(),
            h1r: h1.reverse_bits(),
        }
    }

    /// Folds `data` into `y`, zero-padding a partial last block.
    pub fn update(&self, y: &mut [u8; 16], data: &[u8]) {
        let (blocks, rest) = data.as_chunks::<16>();
        let mut y1 = u64::from_be_bytes(y[..8].try_into().unwrap_or_default());
        let mut y0 = u64::from_be_bytes(y[8..].try_into().unwrap_or_default());
        let mut last = [0u8; 16];
        last[..rest.len()].copy_from_slice(rest);
        let tail = if rest.is_empty() { None } else { Some(&last) };
        for b in blocks.iter().chain(tail) {
            y1 ^= u64::from_be_bytes(b[..8].try_into().unwrap_or_default());
            y0 ^= u64::from_be_bytes(b[8..].try_into().unwrap_or_default());
            (y1, y0) = self.mul(y1, y0);
        }
        y[..8].copy_from_slice(&y1.to_be_bytes());
        y[8..].copy_from_slice(&y0.to_be_bytes());
    }

    /// `(y1, y0)` times H.
    fn mul(&self, y1: u64, y0: u64) -> (u64, u64) {
        let (h0, h1, h0r, h1r) = (self.h0, self.h1, self.h0r, self.h1r);
        let (h2, h2r) = (h0 ^ h1, h0r ^ h1r);
        let (y0r, y1r) = (y0.reverse_bits(), y1.reverse_bits());
        let (y2, y2r) = (y0 ^ y1, y0r ^ y1r);
        // Karatsuba: the low halves of the products directly, the high
        // halves from the bit-reversed operands.
        let z0 = bmul64(y0, h0);
        let z1 = bmul64(y1, h1);
        let mut z2 = bmul64(y2, h2);
        let mut z0h = bmul64(y0r, h0r);
        let mut z1h = bmul64(y1r, h1r);
        let mut z2h = bmul64(y2r, h2r);
        z2 ^= z0 ^ z1;
        z2h ^= z0h ^ z1h;
        z0h = z0h.reverse_bits() >> 1;
        z1h = z1h.reverse_bits() >> 1;
        z2h = z2h.reverse_bits() >> 1;
        let mut v0 = z0;
        let mut v1 = z0h ^ z2;
        let mut v2 = z1 ^ z2h;
        let mut v3 = z1h;
        // The product of the bit-reflected values is one bit short.
        v3 = (v3 << 1) | (v2 >> 63);
        v2 = (v2 << 1) | (v1 >> 63);
        v1 = (v1 << 1) | (v0 >> 63);
        v0 <<= 1;
        // Reduce modulo x^128 + x^7 + x^2 + x + 1, reflected.
        v2 ^= v0 ^ (v0 >> 1) ^ (v0 >> 2) ^ (v0 >> 7);
        v1 ^= (v0 << 63) ^ (v0 << 62) ^ (v0 << 57);
        v3 ^= v1 ^ (v1 >> 1) ^ (v1 >> 2) ^ (v1 >> 7);
        v2 ^= (v1 << 63) ^ (v1 << 62) ^ (v1 << 57);
        (v3, v2)
    }
}

impl Drop for Ghash {
    fn drop(&mut self) {
        self.h0 = 0;
        self.h1 = 0;
        self.h0r = 0;
        self.h1r = 0;
    }
}
