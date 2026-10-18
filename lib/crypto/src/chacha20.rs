// SPDX-License-Identifier: BSD-2-Clause
//! ChaCha20 (RFC 8439 §2.3), with the original layout's 128-bit block
//! counter in the last four state words (FreeBSD's fxrng treats the
//! counter and nonce as one 128-bit counter, which never wraps in
//! practice).

/// "expand 32-byte k".
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Bytes per block.
pub const BLOCK: usize = 64;

fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// The block function: 64 bytes of keystream for `key` at `counter`.
#[must_use]
pub fn block(key: &[u8; 32], counter: u128) -> [u8; BLOCK] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&SIGMA);
    for (i, w) in init[4..12].iter_mut().enumerate() {
        *w = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    for (i, w) in init[12..].iter_mut().enumerate() {
        *w = (counter >> (32 * i)) as u32;
    }
    let mut s = init;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; BLOCK];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

/// A keystream: `key` from block `counter` on.
#[derive(Clone)]
pub struct Stream {
    key: [u8; 32],
    counter: u128,
}

impl core::fmt::Debug for Stream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("Stream").finish_non_exhaustive()
    }
}

impl Stream {
    /// The keystream of `key` from block `counter`.
    #[must_use]
    pub const fn new(key: [u8; 32], counter: u128) -> Self {
        Self { key, counter }
    }

    /// Fills `out` with the next keystream bytes, whole blocks at a time
    /// (a partial last block's rest is discarded).
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(BLOCK) {
            let b = block(&self.key, self.counter);
            chunk.copy_from_slice(&b[..chunk.len()]);
            self.counter = self.counter.wrapping_add(1);
        }
    }

    /// Replaces the key, keeping the counter (as fxrng's rekeying does).
    pub fn rekey(&mut self, key: [u8; 32]) {
        self.key = key;
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.key = [0; 32];
    }
}
