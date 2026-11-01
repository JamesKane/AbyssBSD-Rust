// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's ciphers for TLS: AES-GCM and AES-CBC on the CPU's
//! instructions where it has them (AES-NI and PCLMULQDQ; the ARMv8 AES and
//! PMULL instructions), as FreeBSD's `aesni` and `armv8crypto` drivers
//! provide, and `lib/crypto`'s constant-time software otherwise.
//! ChaCha20-Poly1305 and the hashes are software on every CPU, as in
//! FreeBSD's `cryptosoft`.
//!
//! The instructions run in a kernel FP/SIMD section ([`sched::simd`]),
//! opened once per call: [`Gcm::with`] and [`Cbc::with`] hand the engine to
//! a closure, which may encrypt a whole record, spread over buffers.
//!
//! Behavior reference: sys/crypto/aesni/aesni.c,
//! sys/crypto/armv8/armv8_crypto.c, sys/opencrypto/cryptosoft.c.

#![no_std]

pub use crypto::{cbc, chacha20poly1305, gcm, hmac, sha1, sha256, sha512};

use crypto::aes::Aes;

/// Whether this CPU runs AES and GHASH in instructions.
#[must_use]
pub fn accelerated() -> bool {
    hal::crypto::available()
}

/// AES-GCM through the CPU's instructions. Its methods run only inside a
/// kernel FP/SIMD section.
struct HwGcm {
    aes: Aes,
    h: [u8; 16],
}

impl gcm::Engine for HwGcm {
    fn encrypt_block(&self, ctr: &mut [u8; 16]) {
        let k = self.aes.round_keys();
        // SAFETY: `Gcm::with` holds a SIMD section and built this engine
        // only where the instructions exist; one block of 16 bytes.
        unsafe { hal::crypto::aes_ecb_encrypt(k.as_ptr(), self.aes.rounds(), ctr.as_mut_ptr(), 1) };
    }

    fn ctr32(&self, ctr: &mut [u8; 16], data: &mut [u8]) {
        let k = self.aes.round_keys();
        // SAFETY: as above; `data.len() / 16` whole blocks.
        unsafe {
            hal::crypto::aes_ctr32(
                k.as_ptr(),
                self.aes.rounds(),
                ctr.as_mut_ptr(),
                data.as_mut_ptr(),
                data.len() / 16,
            );
        }
    }

    fn ghash(&self, y: &mut [u8; 16], data: &[u8]) {
        // SAFETY: as above.
        unsafe {
            hal::crypto::ghash(
                self.h.as_ptr(),
                y.as_mut_ptr(),
                data.as_ptr(),
                data.len() / 16,
            )
        };
    }
}

impl Drop for HwGcm {
    fn drop(&mut self) {
        self.h.fill(0);
    }
}

enum GcmImpl {
    Soft(gcm::Soft),
    Hw(HwGcm),
}

/// An AES-GCM key.
pub struct Gcm(GcmImpl);

impl core::fmt::Debug for Gcm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("Gcm")
            .field("hw", &matches!(self.0, GcmImpl::Hw(_)))
            .finish_non_exhaustive()
    }
}

impl Gcm {
    /// A 16-, 24-, or 32-byte key; `None` for another length.
    #[must_use]
    pub fn new(key: &[u8]) -> Option<Self> {
        Self::with_engine(key, accelerated())
    }

    /// As [`Gcm::new`], in software even where the CPU has instructions.
    #[must_use]
    pub fn new_soft(key: &[u8]) -> Option<Self> {
        Self::with_engine(key, false)
    }

    fn with_engine(key: &[u8], hw: bool) -> Option<Self> {
        if !hw {
            return gcm::Soft::new(key).map(|e| Self(GcmImpl::Soft(e)));
        }
        let mut e = HwGcm {
            aes: Aes::new(key)?,
            h: [0; 16],
        };
        let mut h = [0u8; 16];
        {
            let _simd = sched::simd();
            gcm::Engine::encrypt_block(&e, &mut h);
        }
        e.h = h;
        h.fill(0);
        Some(Self(GcmImpl::Hw(e)))
    }

    /// Runs `f` with the engine, in a SIMD section when it is the CPU's.
    pub fn with<R>(&self, f: impl FnOnce(&dyn gcm::Engine) -> R) -> R {
        match &self.0 {
            GcmImpl::Soft(e) => f(e),
            GcmImpl::Hw(e) => {
                let _simd = sched::simd();
                f(e)
            }
        }
    }

    /// Encrypts `data` in place; returns the tag.
    pub fn seal(&self, nonce: &[u8; gcm::NONCE], aad: &[u8], data: &mut [u8]) -> [u8; gcm::TAG] {
        self.with(|e| gcm::seal(e, nonce, aad, data))
    }

    /// Checks `tag` and decrypts `data` in place; `false` (and `data`
    /// untouched) if the tag is wrong.
    #[must_use]
    pub fn open(
        &self,
        nonce: &[u8; gcm::NONCE],
        aad: &[u8],
        data: &mut [u8],
        tag: &[u8; gcm::TAG],
    ) -> bool {
        self.with(|e| gcm::open(e, nonce, aad, data, tag))
    }
}

/// AES-CBC through the CPU's instructions. Its methods run only inside a
/// kernel FP/SIMD section.
struct HwCbc<'a> {
    aes: &'a Aes,
    /// The equivalent inverse cipher's round keys.
    dk: &'a [u8; 240],
}

impl cbc::Engine for HwCbc<'_> {
    fn cbc_encrypt(&self, iv: &mut [u8; 16], data: &mut [u8]) {
        let k = self.aes.round_keys();
        // SAFETY: `Cbc::with` holds a SIMD section and has `dk` only where
        // the instructions exist; whole blocks.
        unsafe {
            hal::crypto::aes_cbc_encrypt(
                k.as_ptr(),
                self.aes.rounds(),
                iv.as_mut_ptr(),
                data.as_mut_ptr(),
                data.len() / 16,
            );
        }
    }

    fn cbc_decrypt(&self, iv: &mut [u8; 16], data: &mut [u8]) {
        // SAFETY: as above; `dk` holds `rounds + 1` keys.
        unsafe {
            hal::crypto::aes_cbc_decrypt(
                self.dk.as_ptr(),
                self.aes.rounds(),
                iv.as_mut_ptr(),
                data.as_mut_ptr(),
                data.len() / 16,
            );
        }
    }
}

/// An AES-CBC key.
pub struct Cbc {
    aes: Aes,
    /// The decryption round keys, for the CPU's instructions.
    dk: Option<[u8; 240]>,
}

impl core::fmt::Debug for Cbc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("Cbc")
            .field("hw", &self.dk.is_some())
            .finish_non_exhaustive()
    }
}

impl Cbc {
    /// A 16-, 24-, or 32-byte key; `None` for another length.
    #[must_use]
    pub fn new(key: &[u8]) -> Option<Self> {
        Self::with_engine(key, accelerated())
    }

    /// As [`Cbc::new`], in software even where the CPU has instructions.
    #[must_use]
    pub fn new_soft(key: &[u8]) -> Option<Self> {
        Self::with_engine(key, false)
    }

    fn with_engine(key: &[u8], hw: bool) -> Option<Self> {
        let aes = Aes::new(key)?;
        let dk = hw.then(|| aes.dec_round_keys());
        Some(Self { aes, dk })
    }

    /// Runs `f` with the engine, in a SIMD section when it is the CPU's.
    pub fn with<R>(&self, f: impl FnOnce(&dyn cbc::Engine) -> R) -> R {
        match &self.dk {
            None => f(&self.aes),
            Some(dk) => {
                let e = HwCbc { aes: &self.aes, dk };
                let _simd = sched::simd();
                f(&e)
            }
        }
    }
}

impl Drop for Cbc {
    fn drop(&mut self) {
        if let Some(dk) = &mut self.dk {
            dk.fill(0);
        }
    }
}
