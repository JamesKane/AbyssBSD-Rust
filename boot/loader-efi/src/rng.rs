// SPDX-License-Identifier: BSD-2-Clause
//! Seed material for KASLR and the kernel RNG.

use bootproto::{Entropy, EntropySource};
use efi::{SystemTable, raw};

/// Gathers 32 random bytes from the best source available.
pub(crate) fn gather(st: &SystemTable) -> Entropy {
    let mut bytes = [0u8; 32];
    if let Ok(rng) = st.locate_protocol::<raw::Rng>() {
        // SAFETY: `rng` is a live protocol and `bytes` is writable; a null
        // algorithm selects the firmware default.
        let status =
            unsafe { ((*rng).get_rng)(rng, core::ptr::null(), bytes.len(), bytes.as_mut_ptr()) };
        if !status.is_error() {
            return Entropy {
                bytes,
                source: EntropySource::UefiRng,
            };
        }
    }
    #[cfg(target_arch = "x86_64")]
    if cpu_random(&mut bytes) {
        return Entropy {
            bytes,
            source: EntropySource::Cpu,
        };
    }
    timer_mix(st, &mut bytes);
    Entropy {
        bytes,
        source: EntropySource::Timer,
    }
}

#[cfg(target_arch = "x86_64")]
fn cpu_random(out: &mut [u8; 32]) -> bool {
    use core::arch::x86_64::{__cpuid, _rdrand64_step};

    if __cpuid(1).ecx & (1 << 30) == 0 {
        return false;
    }
    #[target_feature(enable = "rdrand")]
    fn fill(out: &mut [u8; 32]) -> bool {
        for chunk in out.as_chunks_mut::<8>().0 {
            let mut v = 0;
            // RDRAND may transiently fail; retry a few times.
            if !(0..10).any(|_| _rdrand64_step(&mut v) == 1) {
                return false;
            }
            *chunk = v.to_le_bytes();
        }
        true
    }
    // SAFETY: CPUID reported RDRAND support.
    unsafe { fill(out) }
}

/// Mixes stall-loop jitter into a CRC. Weak, but better than a fixed slide.
fn timer_mix(st: &SystemTable, out: &mut [u8; 32]) {
    let mut c = crc::Crc32::new();
    for chunk in out.as_chunks_mut::<4>().0 {
        for i in 0..64u32 {
            st.stall(1);
            c.update(&i.to_le_bytes());
            c.update(&(&raw const c as usize).to_le_bytes());
        }
        *chunk = c.finish().to_le_bytes();
    }
}
