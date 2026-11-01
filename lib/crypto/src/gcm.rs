// SPDX-License-Identifier: BSD-2-Clause
//! GCM (NIST SP 800-38D) with 96-bit nonces and 16-byte tags, the form
//! TLS uses, over any [`Engine`]: this file's software one, or the CPU's
//! instructions (`sys/crypto`).
//!
//! [`Stream`] takes the text in pieces of any length, so a record spread
//! over buffers is encrypted where it lies.

use crate::aes::Aes;
use crate::ct_eq;
use crate::ghash::Ghash;

/// Bytes in a tag.
pub const TAG: usize = 16;
/// Bytes in a nonce.
pub const NONCE: usize = 12;

/// A keyed block cipher in counter mode and GHASH.
pub trait Engine {
    /// Encrypts `ctr` in place.
    fn encrypt_block(&self, ctr: &mut [u8; 16]);
    /// XORs `data`, whole blocks, with the keystream from `ctr`, adding
    /// one to its last 32 bits (big-endian, wrapping) for each block.
    fn ctr32(&self, ctr: &mut [u8; 16], data: &mut [u8]);
    /// Folds `data`, whole blocks, into `y`.
    fn ghash(&self, y: &mut [u8; 16], data: &[u8]);
}

/// AES-GCM in software.
#[derive(Clone, Debug)]
pub struct Soft {
    aes: Aes,
    ghash: Ghash,
}

impl Soft {
    /// A 16-, 24-, or 32-byte key; `None` for another length.
    #[must_use]
    pub fn new(key: &[u8]) -> Option<Self> {
        let aes = Aes::new(key)?;
        let mut h = [0u8; 16];
        aes.encrypt_block(&mut h);
        let ghash = Ghash::new(&h);
        h.fill(0);
        Some(Self { aes, ghash })
    }
}

impl Engine for Soft {
    fn encrypt_block(&self, ctr: &mut [u8; 16]) {
        self.aes.encrypt_block(ctr);
    }

    fn ctr32(&self, ctr: &mut [u8; 16], data: &mut [u8]) {
        for b in data.as_chunks_mut::<16>().0 {
            let mut k = *ctr;
            self.aes.encrypt_block(&mut k);
            for (b, k) in b.iter_mut().zip(k) {
                *b ^= k;
            }
            inc32(ctr);
        }
    }

    fn ghash(&self, y: &mut [u8; 16], data: &[u8]) {
        self.ghash.update(y, data);
    }
}

/// Adds one to the last 32 bits of `ctr`.
pub fn inc32(ctr: &mut [u8; 16]) {
    let n = u32::from_be_bytes([ctr[12], ctr[13], ctr[14], ctr[15]]).wrapping_add(1);
    ctr[12..].copy_from_slice(&n.to_be_bytes());
}

/// An encryption or decryption in progress.
pub struct Stream<'a, E: Engine + ?Sized> {
    e: &'a E,
    /// The next counter block.
    ctr: [u8; 16],
    /// The first counter block, which encrypts the tag.
    j0: [u8; 16],
    y: [u8; 16],
    /// Keystream for a partial block, from `ks[used..]`.
    ks: [u8; 16],
    used: usize,
    /// Ciphertext of a partial block, not yet hashed.
    part: [u8; 16],
    aad: u64,
    len: u64,
}

impl<E: Engine + ?Sized> core::fmt::Debug for Stream<'_, E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("gcm::Stream")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl<'a, E: Engine + ?Sized> Stream<'a, E> {
    /// Starts with `nonce` and the additional data `aad`.
    pub fn new(e: &'a E, nonce: &[u8; NONCE], aad: &[u8]) -> Self {
        let mut j0 = [0u8; 16];
        j0[..NONCE].copy_from_slice(nonce);
        j0[15] = 1;
        let mut ctr = j0;
        inc32(&mut ctr);
        let mut y = [0u8; 16];
        let (blocks, rest) = aad.as_chunks::<16>();
        e.ghash(&mut y, blocks.as_flattened());
        if !rest.is_empty() {
            let mut last = [0u8; 16];
            last[..rest.len()].copy_from_slice(rest);
            e.ghash(&mut y, &last);
        }
        Self {
            e,
            ctr,
            j0,
            y,
            ks: [0; 16],
            used: 16,
            part: [0; 16],
            aad: aad.len() as u64,
            len: 0,
        }
    }

    /// Encrypts the next piece of plaintext in place.
    pub fn encrypt(&mut self, data: &mut [u8]) {
        self.crypt(data, true);
    }

    /// Decrypts the next piece of ciphertext in place.
    pub fn decrypt(&mut self, data: &mut [u8]) {
        self.crypt(data, false);
    }

    fn crypt(&mut self, mut data: &mut [u8], encrypt: bool) {
        self.len += data.len() as u64;
        // The rest of a partial block.
        if self.used < 16 {
            let n = (16 - self.used).min(data.len());
            let (head, rest) = data.split_at_mut(n);
            for (i, b) in head.iter_mut().enumerate() {
                let c = if encrypt {
                    *b ^ self.ks[self.used + i]
                } else {
                    *b
                };
                *b ^= self.ks[self.used + i];
                self.part[self.used + i] = c;
            }
            self.used += n;
            if self.used == 16 {
                let p = self.part;
                self.e.ghash(&mut self.y, &p);
            }
            data = rest;
        }
        let whole = data.len() & !15;
        let (blocks, tail) = data.split_at_mut(whole);
        if !blocks.is_empty() {
            if !encrypt {
                self.e.ghash(&mut self.y, blocks);
            }
            self.e.ctr32(&mut self.ctr, blocks);
            if encrypt {
                self.e.ghash(&mut self.y, blocks);
            }
        }
        if !tail.is_empty() {
            self.ks = [0; 16];
            self.e.ctr32(&mut self.ctr, &mut self.ks);
            self.part = [0; 16];
            for (i, b) in tail.iter_mut().enumerate() {
                let c = if encrypt { *b ^ self.ks[i] } else { *b };
                *b ^= self.ks[i];
                self.part[i] = c;
            }
            self.used = tail.len();
        }
    }

    /// The tag.
    #[must_use]
    pub fn finish(mut self) -> [u8; TAG] {
        if self.used < 16 {
            self.part[self.used..].fill(0);
            let p = self.part;
            self.e.ghash(&mut self.y, &p);
        }
        let mut lens = [0u8; 16];
        lens[..8].copy_from_slice(&(self.aad * 8).to_be_bytes());
        lens[8..].copy_from_slice(&(self.len * 8).to_be_bytes());
        self.e.ghash(&mut self.y, &lens);
        let mut t = self.j0;
        self.e.encrypt_block(&mut t);
        for (t, y) in t.iter_mut().zip(self.y) {
            *t ^= y;
        }
        t
    }
}

impl<E: Engine + ?Sized> Drop for Stream<'_, E> {
    fn drop(&mut self) {
        self.ks.fill(0);
        self.y.fill(0);
    }
}

/// Encrypts `data` in place; returns the tag.
pub fn seal<E: Engine + ?Sized>(
    e: &E,
    nonce: &[u8; NONCE],
    aad: &[u8],
    data: &mut [u8],
) -> [u8; TAG] {
    let mut s = Stream::new(e, nonce, aad);
    s.encrypt(data);
    s.finish()
}

/// Checks `tag` and decrypts `data` in place; `false` (and `data`
/// untouched) if the tag is wrong.
#[must_use]
pub fn open<E: Engine + ?Sized>(
    e: &E,
    nonce: &[u8; NONCE],
    aad: &[u8],
    data: &mut [u8],
    tag: &[u8; TAG],
) -> bool {
    // Authenticate before releasing any plaintext.
    let mut s = Stream::new(e, nonce, aad);
    let (blocks, rest) = data.as_chunks::<16>();
    e.ghash(&mut s.y, blocks.as_flattened());
    s.part[..rest.len()].copy_from_slice(rest);
    s.used = if rest.is_empty() { 16 } else { rest.len() };
    s.len = data.len() as u64;
    let ctr = s.ctr;
    if !ct_eq(&s.finish(), tag) {
        return false;
    }
    let whole = data.len() & !15;
    let (blocks, tail) = data.split_at_mut(whole);
    let mut ctr = ctr;
    e.ctr32(&mut ctr, blocks);
    if !tail.is_empty() {
        let mut ks = [0u8; 16];
        e.ctr32(&mut ctr, &mut ks);
        for (b, k) in tail.iter_mut().zip(ks) {
            *b ^= k;
        }
        ks.fill(0);
    }
    true
}
