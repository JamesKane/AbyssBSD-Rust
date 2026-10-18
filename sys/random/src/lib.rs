// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's random number generator, after FreeBSD's fxrng
//! ("Fenestras X").
//!
//! - A **root generator**: a ChaCha20 keystream that rekeys itself from
//!   its own first output after every request (fast key erasure), so a
//!   later compromise reveals nothing already produced.
//! - **Reseeding** hashes the root's next 32 bytes together with new
//!   entropy through BLAKE2b and takes the digest as the new key, so
//!   entropy accumulates and cannot be steered.
//! - **Per-CPU generators**, keyed from the root, serve requests without a
//!   shared lock; one rekeys from the root when the root's generation has
//!   moved on (a reseed).
//! - **Sources**: first-time sources (the loader's EFI_RNG bytes, the
//!   CPU's RDSEED/RDRAND or RNDR) seed the root directly; later
//!   contributions (those, and entropy devices such as virtio-rng) mix
//!   into a pool, which reseeds the root once it has gathered enough.
//!
//! Until the root is seeded, [`fill`] refuses: nothing reads randomness
//! the generator cannot vouch for.
//!
//! Behavior reference: sys/dev/random/fenestrasX/.

#![no_std]
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crypto::blake2b::Blake2b;
use crypto::chacha20::Stream;
use hal::{CpuId, MAX_CPUS};
use sync::SpinLock;

/// Where entropy came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The loader (UEFI's EFI_RNG_PROTOCOL).
    Loader,
    /// The CPU's instruction (RDSEED, RDRAND, RNDR).
    Cpu,
    /// An entropy device (virtio-rng).
    Device,
    /// Event timing (interrupts), weak but free.
    Timing,
}

/// The generator has not been seeded yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unseeded;

/// Bytes the pool gathers from strong sources before reseeding the root.
const RESEED_BYTES: u64 = 64;

struct Root {
    stream: Stream,
}

impl Root {
    /// Fills `out`, then rekeys from the keystream's first block.
    fn generate(&mut self, out: &mut [u8]) {
        let mut next = [0u8; 32];
        self.stream.fill(&mut next);
        self.stream.fill(out);
        self.stream.rekey(next);
    }

    /// Rekeys from the hash of its own next output and `entropy`.
    fn reseed(&mut self, seeded: bool, entropy: &[u8]) {
        let mut mix = Blake2b::new(64);
        if seeded {
            let mut own = [0u8; 32];
            self.stream.fill(&mut own);
            mix.update(&own);
        }
        mix.update(entropy);
        let mut digest = [0u8; 64];
        mix.finish(&mut digest);
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest[..32]);
        self.stream.rekey(key);
    }
}

static ROOT: SpinLock<Root> = SpinLock::new(Root {
    stream: Stream::new([0; 32], 0),
});
static SEEDED: AtomicBool = AtomicBool::new(false);
/// The root's reseed count: per-CPU generators rekey when it moves.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The entropy pool: a running BLAKE2b digest, and strong bytes gathered
/// since the last reseed.
static POOL: SpinLock<([u8; 64], u64)> = SpinLock::new(([0; 64], 0));

/// One CPU's generator and the root generation it was keyed at.
static PER_CPU: [SpinLock<Option<(Stream, u64)>>; MAX_CPUS] =
    [const { SpinLock::new(None) }; MAX_CPUS];

/// Whether the generator has been seeded.
#[must_use]
pub fn seeded() -> bool {
    SEEDED.load(Ordering::Acquire)
}

/// Reseeds the root with `entropy` directly: first-time sources, trusted
/// to be good enough to seed with (fxrng's direct reseed). Returns whether
/// this seeded the generator for the first time.
pub fn seed(entropy: &[u8]) -> bool {
    if entropy.iter().all(|&b| b == 0) {
        return false;
    }
    ROOT.lock().reseed(seeded(), entropy);
    GENERATION.fetch_add(1, Ordering::AcqRel);
    !SEEDED.swap(true, Ordering::AcqRel)
}

/// Mixes `data` from `source` into the pool, reseeding the root once
/// strong sources have contributed enough.
pub fn harvest(source: Source, data: &[u8]) {
    let mut pool = POOL.lock();
    let mut h = Blake2b::new(64);
    h.update(&pool.0);
    h.update(&[source as u8]);
    h.update(data);
    h.finish(&mut pool.0);
    if source != Source::Timing {
        pool.1 += data.len() as u64;
    }
    if pool.1 >= RESEED_BYTES && seeded() {
        let digest = pool.0;
        pool.1 = 0;
        drop(pool);
        ROOT.lock().reseed(true, &digest);
        GENERATION.fetch_add(1, Ordering::AcqRel);
    }
}

/// Fills `out` with random bytes, from this CPU's generator.
pub fn fill(cpu: CpuId, out: &mut [u8]) -> Result<(), Unseeded> {
    if !seeded() {
        return Err(Unseeded);
    }
    let generation = GENERATION.load(Ordering::Acquire);
    let mut mine = PER_CPU[cpu.index() % MAX_CPUS].lock();
    if mine.as_ref().is_none_or(|(_, g)| *g != generation) {
        let mut key = [0u8; 32];
        ROOT.lock().generate(&mut key);
        *mine = Some((Stream::new(key, 0), generation));
    }
    let Some((stream, _)) = mine.as_mut() else {
        return Err(Unseeded);
    };
    let mut next = [0u8; 32];
    stream.fill(&mut next);
    stream.fill(out);
    stream.rekey(next);
    Ok(())
}

/// The root's reseed count (diagnostics and tests).
#[must_use]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests;
