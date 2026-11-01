// SPDX-License-Identifier: BSD-2-Clause
//! Cryptographic primitives: the ChaCha20 stream cipher (RFC 8439) and
//! the BLAKE2b hash (RFC 7693), as FreeBSD's fxrng uses them; SHA-1, for
//! identifiers FreeBSD derives with it; and what kernel TLS needs: AES
//! (FIPS 197) in CBC and GCM modes, ChaCha20-Poly1305, SHA-256, SHA-384,
//! and HMAC.
//!
//! Pure, constant-time where it matters (no data-dependent branches or
//! table lookups), and checked against the published test vectors. The
//! CPU's AES and carry-less multiply instructions are `sys/crypto`'s,
//! through the [`gcm::Engine`] and [`cbc::Engine`] traits.

#![no_std]
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "counters and lengths are split into machine words by design"
)]

pub mod aes;
pub mod blake2b;
pub mod cbc;
pub mod chacha20;
pub mod chacha20poly1305;
pub mod gcm;
pub mod ghash;
pub mod hmac;
pub mod poly1305;
pub mod sha1;
pub mod sha256;
pub mod sha512;

/// A hash function, for [`hmac`].
pub trait Hash: Clone {
    /// Bytes per input block.
    const BLOCK: usize;
    /// The digest.
    type Digest: AsRef<[u8]>;
    fn new() -> Self;
    fn update(&mut self, data: &[u8]);
    fn finish(self) -> Self::Digest;
}

impl Hash for sha1::Sha1 {
    const BLOCK: usize = 64;
    type Digest = [u8; 20];
    fn new() -> Self {
        Self::new()
    }
    fn update(&mut self, data: &[u8]) {
        self.update(data);
    }
    fn finish(self) -> [u8; 20] {
        self.finish()
    }
}

/// Whether `a` and `b` are equal, in time that depends only on their
/// lengths.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let d = a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y));
    core::hint::black_box(d) == 0
}

#[cfg(test)]
mod tests;
