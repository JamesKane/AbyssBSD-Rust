// SPDX-License-Identifier: BSD-2-Clause
//! CBC mode (NIST SP 800-38A §6.2) over any [`Engine`], for TLS 1.0–1.2's
//! AES-CBC suites. Data is whole blocks; padding is TLS's business.

use crate::aes::Aes;

/// A keyed block cipher in CBC mode.
pub trait Engine {
    /// Encrypts `data`, whole blocks, chaining from `iv`; leaves the last
    /// ciphertext block in `iv`.
    fn cbc_encrypt(&self, iv: &mut [u8; 16], data: &mut [u8]);
    /// Decrypts `data`, whole blocks, chaining from `iv`; leaves the last
    /// ciphertext block in `iv`.
    fn cbc_decrypt(&self, iv: &mut [u8; 16], data: &mut [u8]);
}

impl Engine for Aes {
    fn cbc_encrypt(&self, iv: &mut [u8; 16], data: &mut [u8]) {
        for b in data.as_chunks_mut::<16>().0 {
            for (b, v) in b.iter_mut().zip(*iv) {
                *b ^= v;
            }
            self.encrypt_block(b);
            *iv = *b;
        }
    }

    fn cbc_decrypt(&self, iv: &mut [u8; 16], data: &mut [u8]) {
        for b in data.as_chunks_mut::<16>().0 {
            let c = *b;
            self.decrypt_block(b);
            for (b, v) in b.iter_mut().zip(*iv) {
                *b ^= v;
            }
            *iv = c;
        }
    }
}
