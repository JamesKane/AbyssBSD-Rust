// SPDX-License-Identifier: BSD-2-Clause
//! Poly1305 (RFC 8439 §2.5), in constant time with five 26-bit limbs.

/// Bytes in a tag.
pub const TAG: usize = 16;

const MASK: u32 = 0x3ff_ffff;

/// A MAC in progress.
#[derive(Clone)]
pub struct Poly1305 {
    r: [u32; 5],
    h: [u32; 5],
    pad: [u32; 4],
    buf: [u8; 16],
    fill: usize,
}

impl core::fmt::Debug for Poly1305 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("Poly1305").finish_non_exhaustive()
    }
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

impl Poly1305 {
    /// The one-time key: r (clamped) then s.
    #[must_use]
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            r: [
                le32(&key[0..]) & 0x3ff_ffff,
                (le32(&key[3..]) >> 2) & 0x3ff_ff03,
                (le32(&key[6..]) >> 4) & 0x3ff_c0ff,
                (le32(&key[9..]) >> 6) & 0x3f0_3fff,
                (le32(&key[12..]) >> 8) & 0x00f_ffff,
            ],
            h: [0; 5],
            pad: [
                le32(&key[16..]),
                le32(&key[20..]),
                le32(&key[24..]),
                le32(&key[28..]),
            ],
            buf: [0; 16],
            fill: 0,
        }
    }

    /// Adds a block (`hibit` is 1 << 24 for a full one) and multiplies by r.
    fn block(&mut self, m: &[u8; 16], hibit: u32) {
        let [r0, r1, r2, r3, r4] = self.r.map(u64::from);
        let (s1, s2, s3, s4) = (r1 * 5, r2 * 5, r3 * 5, r4 * 5);
        let mut h = self.h.map(u64::from);
        h[0] += u64::from(le32(&m[0..]) & MASK);
        h[1] += u64::from((le32(&m[3..]) >> 2) & MASK);
        h[2] += u64::from((le32(&m[6..]) >> 4) & MASK);
        h[3] += u64::from((le32(&m[9..]) >> 6) & MASK);
        h[4] += u64::from((le32(&m[12..]) >> 8) | hibit);
        let [h0, h1, h2, h3, h4] = h;
        let d0 = h0 * r0 + h1 * s4 + h2 * s3 + h3 * s2 + h4 * s1;
        let mut d1 = h0 * r1 + h1 * r0 + h2 * s4 + h3 * s3 + h4 * s2;
        let mut d2 = h0 * r2 + h1 * r1 + h2 * r0 + h3 * s4 + h4 * s3;
        let mut d3 = h0 * r3 + h1 * r2 + h2 * r1 + h3 * r0 + h4 * s4;
        let mut d4 = h0 * r4 + h1 * r3 + h2 * r2 + h3 * r1 + h4 * r0;
        let mut c = d0 >> 26;
        let mut n0 = d0 & u64::from(MASK);
        d1 += c;
        c = d1 >> 26;
        let n1 = d1 & u64::from(MASK);
        d2 += c;
        c = d2 >> 26;
        let n2 = d2 & u64::from(MASK);
        d3 += c;
        c = d3 >> 26;
        let n3 = d3 & u64::from(MASK);
        d4 += c;
        c = d4 >> 26;
        let n4 = d4 & u64::from(MASK);
        n0 += c * 5;
        c = n0 >> 26;
        n0 &= u64::from(MASK);
        let n1 = n1 + c;
        self.h = [n0, n1, n2, n3, n4].map(|v| v as u32);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        if self.fill != 0 {
            let n = (16 - self.fill).min(data.len());
            self.buf[self.fill..self.fill + n].copy_from_slice(&data[..n]);
            self.fill += n;
            data = &data[n..];
            if self.fill < 16 {
                return;
            }
            let b = self.buf;
            self.block(&b, 1 << 24);
            self.fill = 0;
        }
        let (blocks, rest) = data.as_chunks::<16>();
        for b in blocks {
            self.block(b, 1 << 24);
        }
        self.buf[..rest.len()].copy_from_slice(rest);
        self.fill = rest.len();
    }

    /// Pads what was added so far to a block boundary with zeros (the
    /// AEAD construction's padding).
    pub fn pad16(&mut self) {
        if self.fill != 0 {
            self.update(&[0; 16][..16 - self.fill]);
        }
    }

    #[must_use]
    pub fn finish(mut self) -> [u8; TAG] {
        if self.fill != 0 {
            let mut b = [0u8; 16];
            b[..self.fill].copy_from_slice(&self.buf[..self.fill]);
            b[self.fill] = 1;
            self.block(&b, 0);
        }
        // Fully carry h.
        let mut h = self.h;
        let mut c = h[1] >> 26;
        h[1] &= MASK;
        for v in &mut h[2..] {
            *v += c;
            c = *v >> 26;
            *v &= MASK;
        }
        h[0] += c * 5;
        c = h[0] >> 26;
        h[0] &= MASK;
        h[1] += c;
        // g = h + 5 - 2^130; take g if it did not borrow.
        let mut g = [0u32; 5];
        let mut c = 5;
        for i in 0..4 {
            g[i] = h[i] + c;
            c = g[i] >> 26;
            g[i] &= MASK;
        }
        g[4] = h[4].wrapping_add(c).wrapping_sub(1 << 26);
        let mask = (g[4] >> 31).wrapping_sub(1);
        for i in 0..5 {
            h[i] = (h[i] & !mask) | (g[i] & mask);
        }
        // h mod 2^128, plus s.
        let w = [
            h[0] | (h[1] << 26),
            (h[1] >> 6) | (h[2] << 20),
            (h[2] >> 12) | (h[3] << 14),
            (h[3] >> 18) | (h[4] << 8),
        ];
        let mut out = [0u8; TAG];
        let mut f = 0u64;
        for i in 0..4 {
            f = u64::from(w[i]) + u64::from(self.pad[i]) + (f >> 32);
            out[4 * i..4 * i + 4].copy_from_slice(&(f as u32).to_le_bytes());
        }
        out
    }
}

impl Drop for Poly1305 {
    fn drop(&mut self) {
        self.r = [0; 5];
        self.pad = [0; 4];
        self.h = [0; 5];
    }
}
