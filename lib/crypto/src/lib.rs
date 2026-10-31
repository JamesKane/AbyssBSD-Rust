// SPDX-License-Identifier: BSD-2-Clause
//! Cryptographic primitives the kernel's random number generator is built
//! from: the ChaCha20 stream cipher (RFC 8439) and the BLAKE2b hash
//! (RFC 7693), as FreeBSD's fxrng uses them; and SHA-1, for identifiers
//! FreeBSD derives with it.
//!
//! Pure, constant-time where it matters (no data-dependent branches or
//! table lookups), and checked against the RFCs' test vectors.

#![no_std]
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "counters and lengths are split into machine words by design"
)]

pub mod blake2b;
pub mod chacha20;
pub mod sha1;

#[cfg(test)]
mod tests;
