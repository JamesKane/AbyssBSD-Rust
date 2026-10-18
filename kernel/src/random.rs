// SPDX-License-Identifier: BSD-2-Clause
//! Feeding the random number generator (`sys/random`): the loader's
//! EFI_RNG bytes and the CPU's RNG seed it at boot, and a harvester thread
//! keeps mixing in the CPU's RNG, entropy devices (virtio-rng), and timing.

use bootproto::EntropySource;

/// How often the harvester runs.
const HARVEST_MS: u64 = 1000;

/// 32 bytes from the CPU's RNG, if it has one.
fn cpu_bytes() -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    for chunk in out.chunks_mut(8) {
        chunk.copy_from_slice(&crate::arch::cpu::hardware_random()?.to_le_bytes());
    }
    Some(out)
}

/// Seeds the generator from first-time sources. Call once, early.
pub(crate) fn init() {
    let e = &crate::boot::info().entropy;
    let mut first = false;
    // Timer jitter alone is not good enough to seed from.
    if matches!(e.source, EntropySource::UefiRng | EntropySource::Cpu) {
        first |= random::seed(&e.bytes);
    }
    if let Some(b) = cpu_bytes() {
        first |= random::seed(&b);
    }
    random::harvest(
        random::Source::Timing,
        &crate::time::counter().to_le_bytes(),
    );
    if first {
        kprintln!("random: unblocking device.");
    } else if !random::seeded() {
        kprintln!("random: no seed source yet; waiting for an entropy device");
    }
}

/// Starts the harvester. Call once devices are up.
pub(crate) fn start_harvester() {
    let _ = sched::spawn("random", sched::Priority::KERNEL, |s| {
        loop {
            if let Some(b) = cpu_bytes() {
                random::harvest(random::Source::Cpu, &b);
            }
            let mut dev = [0u8; 32];
            if let Some(n) = virtio_random::read(s, &mut dev)
                && n > 0
            {
                let was = random::seeded();
                if !was && random::seed(&dev[..n]) {
                    kprintln!("random: unblocking device.");
                } else {
                    random::harvest(random::Source::Device, &dev[..n]);
                }
            }
            random::harvest(
                random::Source::Timing,
                &crate::time::counter().to_le_bytes(),
            );
            crate::time::sleep_ns(s, HARVEST_MS * 1_000_000);
        }
    });
}

/// Fills `out` with random bytes, once the generator is seeded.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn fill(out: &mut [u8]) -> Result<(), random::Unseeded> {
    random::fill(sched::this_cpu().id(), out)
}
