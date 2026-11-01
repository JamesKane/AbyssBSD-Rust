// SPDX-License-Identifier: BSD-2-Clause
//! ChaCha20-Poly1305 (RFC 8439 §2.8), with [`Stream`] taking the text in
//! pieces of any length.

use crate::chacha20::{BLOCK, block};
use crate::ct_eq;
use crate::poly1305::Poly1305;

/// Bytes in a tag.
pub const TAG: usize = 16;
/// Bytes in a nonce.
pub const NONCE: usize = 12;

/// The 128-bit block counter for `nonce` at block `n`.
fn counter(nonce: &[u8; NONCE], n: u32) -> u128 {
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&n.to_le_bytes());
    b[4..].copy_from_slice(nonce);
    u128::from_le_bytes(b)
}

/// An encryption or decryption in progress.
pub struct Stream {
    key: [u8; 32],
    ctr: u128,
    ks: [u8; BLOCK],
    used: usize,
    mac: Poly1305,
    aad: u64,
    len: u64,
}

impl core::fmt::Debug for Stream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("chacha20poly1305::Stream")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Stream {
    /// Starts with `key`, `nonce`, and the additional data `aad`.
    #[must_use]
    pub fn new(key: &[u8; 32], nonce: &[u8; NONCE], aad: &[u8]) -> Self {
        let mut k0 = block(key, counter(nonce, 0));
        let mut mac = Poly1305::new(k0[..32].try_into().unwrap_or(&[0; 32]));
        k0.fill(0);
        mac.update(aad);
        mac.pad16();
        Self {
            key: *key,
            ctr: counter(nonce, 1),
            ks: [0; BLOCK],
            used: BLOCK,
            mac,
            aad: aad.len() as u64,
            len: 0,
        }
    }

    /// Encrypts the next piece of plaintext in place.
    pub fn encrypt(&mut self, data: &mut [u8]) {
        self.crypt(data);
        self.mac.update(data);
    }

    /// Decrypts the next piece of ciphertext in place.
    pub fn decrypt(&mut self, data: &mut [u8]) {
        self.mac.update(data);
        self.crypt(data);
    }

    fn crypt(&mut self, data: &mut [u8]) {
        self.len += data.len() as u64;
        for b in data {
            if self.used == BLOCK {
                self.ks = block(&self.key, self.ctr);
                // The 32-bit block counter wraps alone (RFC 8439 §2.4).
                let low = (self.ctr as u32).wrapping_add(1);
                self.ctr = (self.ctr & !u128::from(u32::MAX)) | u128::from(low);
                self.used = 0;
            }
            *b ^= self.ks[self.used];
            self.used += 1;
        }
    }

    /// The tag.
    #[must_use]
    pub fn finish(mut self) -> [u8; TAG] {
        self.mac.pad16();
        let mut lens = [0u8; 16];
        lens[..8].copy_from_slice(&self.aad.to_le_bytes());
        lens[8..].copy_from_slice(&self.len.to_le_bytes());
        self.mac.update(&lens);
        self.mac.clone().finish()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.key.fill(0);
        self.ks.fill(0);
    }
}

/// Encrypts `data` in place; returns the tag.
#[must_use]
pub fn seal(key: &[u8; 32], nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8]) -> [u8; TAG] {
    let mut s = Stream::new(key, nonce, aad);
    s.encrypt(data);
    s.finish()
}

/// Checks `tag` and decrypts `data` in place; `false` (and `data`
/// untouched) if the tag is wrong.
#[must_use]
pub fn open(
    key: &[u8; 32],
    nonce: &[u8; NONCE],
    aad: &[u8],
    data: &mut [u8],
    tag: &[u8; TAG],
) -> bool {
    let mut s = Stream::new(key, nonce, aad);
    s.mac.update(data);
    s.len = data.len() as u64;
    if !ct_eq(&s.finish(), tag) {
        return false;
    }
    Stream::new(key, nonce, &[]).crypt(data);
    true
}
