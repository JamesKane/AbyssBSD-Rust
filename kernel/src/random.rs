// SPDX-License-Identifier: BSD-2-Clause
//! Feeding the random number generator (`sys/random`): the loader's
//! EFI_RNG bytes and the CPU's RNG seed it at boot, and a harvester thread
//! keeps mixing in the CPU's RNG, entropy devices (virtio-rng), and timing.

use core::sync::atomic::{AtomicU32, Ordering};

use abi::errno::Errno;
use bootproto::EntropySource;

/// How often the harvester runs.
const HARVEST_MS: u64 = 1000;

/// FreeBSD's `enum random_entropy_source` names, by bit
/// (`random_source_descr`).
const SOURCES: [&str; 29] = [
    "CACHED",
    "ATTACH",
    "KEYBOARD",
    "MOUSE",
    "NET_TUN",
    "NET_ETHER",
    "NET_NG",
    "INTERRUPT",
    "SWI",
    "FS_ATIME",
    "UMA",
    "CALLOUT",
    "RANDOMDEV",
    "PURE_TPM",
    "PURE_RDRAND",
    "PURE_RDSEED",
    "PURE_NEHEMIAH",
    "PURE_RNDTEST",
    "PURE_VIRTIO",
    "PURE_BROADCOM",
    "PURE_CCP",
    "PURE_DARN",
    "PURE_VMGENID",
    "PURE_QUALCOMM",
    "PURE_ARMV8",
    "PURE_ARM_TRNG",
    "PURE_SAFE",
    "PURE_GLXSB",
    "PURE_HIFN",
];
/// `ENTROPYSOURCE`.
const ENTROPYSOURCE: u32 = 29;
/// `RANDOM_PURE_START`.
const PURE_START: u32 = 13;
/// `RANDOM_NET_ETHER` and `RANDOM_UMA`, off by default and fixed.
const ETHER_UMA_OFF: u32 = (1 << 5) | (1 << 10);
#[cfg(target_arch = "x86_64")]
const PURE_RDRAND: u32 = 14;
#[cfg(target_arch = "x86_64")]
const PURE_RDSEED: u32 = 15;
const PURE_VIRTIO: u32 = 18;
#[cfg(target_arch = "aarch64")]
const PURE_ARMV8: u32 = 24;

/// `hc_source_mask`: the environmental sources harvested (all but the
/// Ethernet and UMA ones, `almost_everything_mask`), and the pure sources
/// registered: the CPU's RNG and an entropy device once found.
static MASK: AtomicU32 = AtomicU32::new(((1 << PURE_START) - 1) & !ETHER_UMA_OFF);

/// The bit of the CPU's RNG: `RDSEED` where the CPU has it, else
/// `RDRAND` (as `hardware_random` picks), or arm64's `RNDR`.
fn cpu_source() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if core::arch::x86_64::__cpuid_count(7, 0).ebx & (1 << 18) != 0 {
            PURE_RDSEED
        } else {
            PURE_RDRAND
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        PURE_ARMV8
    }
}

/// `kern.random.harvest.mask`, `mask_bin`, and `mask_symbolic`
/// (`random_harvestq.c`).
pub(crate) fn sysctl_init(s: &sched::Sleepable) {
    use alloc::boxed::Box;
    use sysctl::{Number, Value};
    if cpu_bytes().is_some() {
        MASK.fetch_or(1 << cpu_source(), Ordering::Relaxed);
    }
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        let random = t
            .child(kern, "random")
            .or_else(|| {
                t.add_node(
                    kern,
                    Number::Auto,
                    "random",
                    "Cryptographically Secure Random Number Generator",
                )
                .ok()
            })
            .expect("kern.random");
        let harvest = t
            .add_node(random, Number::Auto, "harvest", "Entropy Device Parameters")
            .expect("kern.random.harvest");
        t.add_writable(
            harvest,
            Number::Auto,
            "mask",
            "Entropy harvesting mask",
            0,
            Value::Uint(Box::new(|| MASK.load(Ordering::Relaxed))),
            // `random_check_uint_harvestmask`: the pure sources and the
            // fixed-off ones are not the user's to change.
            sysctl::int_writer(|v| {
                let v = v.cast_unsigned();
                if 32 - v.leading_zeros() > ENTROPYSOURCE {
                    return Err(Errno::EINVAL);
                }
                let fixed =
                    (((1u32 << ENTROPYSOURCE) - 1) & (u32::MAX << PURE_START)) | ETHER_UMA_OFF;
                let old = MASK.load(Ordering::Relaxed);
                MASK.store((v & !fixed) | (old & fixed), Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.random.harvest.mask");
        t.add_value(
            harvest,
            Number::Auto,
            "mask_bin",
            "Entropy harvesting mask (printable)",
            Value::Str(Box::new(|out| {
                let m = MASK.load(Ordering::Relaxed);
                for i in (0..ENTROPYSOURCE).rev() {
                    out.push(if m & (1 << i) != 0 { '1' } else { '0' });
                }
            })),
        )
        .expect("kern.random.harvest.mask_bin");
        t.add_value(
            harvest,
            Number::Auto,
            "mask_symbolic",
            "Entropy harvesting mask (symbolic)",
            Value::Str(Box::new(|out| {
                let m = MASK.load(Ordering::Relaxed);
                let mut first = true;
                for i in (0..ENTROPYSOURCE).rev() {
                    let present = m & (1 << i) != 0;
                    if i >= PURE_START && !present {
                        continue;
                    }
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    let name = SOURCES[i as usize];
                    if present {
                        out.push_str(name);
                    } else {
                        out.push('[');
                        out.push_str(name);
                        out.push(']');
                    }
                }
            })),
        )
        .expect("kern.random.harvest.mask_symbolic");
    });
}

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
                MASK.fetch_or(1 << PURE_VIRTIO, Ordering::Relaxed);
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
