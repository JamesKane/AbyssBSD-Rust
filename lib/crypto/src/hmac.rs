// SPDX-License-Identifier: BSD-2-Clause
//! HMAC (RFC 2104) over any [`Hash`].
//!
//! The keyed inner and outer states are computed once, so a MAC per TLS
//! record starts from clones of them.

use crate::Hash;

/// The longest hash block HMAC handles (SHA-384's and SHA-512's).
const MAX_BLOCK: usize = 128;

/// A key's precomputed inner and outer hash states.
#[derive(Clone)]
pub struct Key<H: Hash> {
    inner: H,
    outer: H,
}

impl<H: Hash> core::fmt::Debug for Key<H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("hmac::Key").finish_non_exhaustive()
    }
}

impl<H: Hash> Key<H> {
    /// The states for `key`; a key longer than the block is hashed first.
    #[must_use]
    pub fn new(key: &[u8]) -> Self {
        let mut k = [0u8; MAX_BLOCK];
        if key.len() > H::BLOCK {
            let mut h = H::new();
            h.update(key);
            let d = h.finish();
            k[..d.as_ref().len()].copy_from_slice(d.as_ref());
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut pad = [0u8; MAX_BLOCK];
        let mut inner = H::new();
        for (p, k) in pad.iter_mut().zip(&k).take(H::BLOCK) {
            *p = k ^ 0x36;
        }
        inner.update(&pad[..H::BLOCK]);
        let mut outer = H::new();
        for (p, k) in pad.iter_mut().zip(&k).take(H::BLOCK) {
            *p = k ^ 0x5c;
        }
        outer.update(&pad[..H::BLOCK]);
        k.fill(0);
        pad.fill(0);
        Self { inner, outer }
    }

    /// Starts a MAC.
    #[must_use]
    pub fn start(&self) -> Mac<H> {
        Mac {
            inner: self.inner.clone(),
            outer: self.outer.clone(),
        }
    }

    /// The MAC of `data`.
    #[must_use]
    pub fn mac(&self, data: &[u8]) -> H::Digest {
        let mut m = self.start();
        m.update(data);
        m.finish()
    }
}

/// A MAC in progress.
#[derive(Clone)]
pub struct Mac<H: Hash> {
    inner: H,
    outer: H,
}

impl<H: Hash> core::fmt::Debug for Mac<H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("hmac::Mac").finish_non_exhaustive()
    }
}

impl<H: Hash> Mac<H> {
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    #[must_use]
    pub fn finish(self) -> H::Digest {
        let mut outer = self.outer;
        outer.update(self.inner.finish().as_ref());
        outer.finish()
    }
}
