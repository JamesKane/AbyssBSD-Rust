// SPDX-License-Identifier: BSD-2-Clause
//! Identification and the feature probe (03 §3.1, 02 §2.3.1).

use crate::Error;
use crate::regs::{self, Regs};

/// `GPU_ID`'s fields (02 §2.3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuId(pub u32);

impl GpuId {
    pub fn arch_major(self) -> u8 {
        (self.0 >> 28) as u8
    }
    pub fn arch_minor(self) -> u8 {
        (self.0 >> 24 & 0xF) as u8
    }
    pub fn arch_rev(self) -> u8 {
        (self.0 >> 20 & 0xF) as u8
    }
    pub fn product_major(self) -> u8 {
        (self.0 >> 16 & 0xF) as u8
    }
    pub fn version_major(self) -> u8 {
        (self.0 >> 12 & 0xF) as u8
    }
    pub fn version_minor(self) -> u8 {
        (self.0 >> 4 & 0xFF) as u8
    }
    pub fn version_status(self) -> u8 {
        (self.0 & 0xF) as u8
    }
}

/// How the host powers the GPU, chosen by architecture (03 §3.1 step 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    /// Architectures 10 to 13: the GPU-control block's `L2_PWRON`/`L2_PWROFF`.
    Legacy,
    /// Architecture 14: the PWR block, with delegation to firmware.
    PwrBlock,
}

/// Everything the probe reads (03 §3.1 step 3 and 4): kept for the driver and passed to
/// userspace unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub gpu_id: GpuId,
    pub csf_id: u32,
    pub revid: u32,
    pub core_features: u32,
    pub l2_features: u32,
    pub tiler_features: u32,
    pub mem_features: u32,
    pub mmu_features: u32,
    pub thread_features: u32,
    pub thread_max_threads: u32,
    pub thread_max_workgroup_size: u32,
    pub thread_max_barrier_size: u32,
    pub coherency_features: u32,
    pub texture_features: [u32; 4],
    pub as_present: u32,
    pub gpu_features: u64,
    pub shader_present: u64,
    pub tiler_present: u64,
    pub l2_present: u64,
}

/// `GPU_FEATURES` bit 2: ray intersection (02 §2.3).
pub const FEATURE_RAY_INTERSECTION: u64 = 1 << 2;
/// `GPU_FEATURES` bit 5: ray traversal.
pub const FEATURE_RAY_TRAVERSAL: u64 = 1 << 5;

impl Info {
    pub fn power(&self) -> Power {
        if self.gpu_id.arch_major() >= 14 {
            Power::PwrBlock
        } else {
            Power::Legacy
        }
    }

    /// GPU virtual address bits (`MMU_FEATURES[7:0]`), at most 48: the page tables have four
    /// levels at most (04 §4.6.1).
    pub fn va_bits(&self) -> u8 {
        ((self.mmu_features & 0xFF) as u8).min(48)
    }

    /// Physical address bits (`MMU_FEATURES[15:8]`): the DMA addressing limit.
    pub fn pa_bits(&self) -> u8 {
        (self.mmu_features >> 8 & 0xFF) as u8
    }

    /// log2 of the L2 cache line size (`L2_FEATURES[7:0]`).
    pub fn l2_line_log2(&self) -> u8 {
        (self.l2_features & 0xFF) as u8
    }

    pub fn shader_cores(&self) -> u32 {
        self.shader_present.count_ones()
    }

    /// The product's name, for logging only (02 §2.3.1).
    pub fn product(&self) -> &'static str {
        let cores = self.shader_cores();
        let ray = self.gpu_features & FEATURE_RAY_INTERSECTION != 0;
        let big = |immortalis, mid, small| {
            if cores >= 10 && ray {
                immortalis
            } else if cores >= 6 {
                mid
            } else {
                small
            }
        };
        match (self.gpu_id.arch_major(), self.gpu_id.product_major()) {
            (10, 2) => "Mali-G710",
            (10, 3) => "Mali-G510",
            (10, 4) => "Mali-G310",
            (10, 7) => "Mali-G610",
            (11, 2) => {
                if cores >= 10 && ray {
                    "Mali-G715-Immortalis"
                } else if cores >= 7 {
                    "Mali-G715"
                } else {
                    "Mali-G615"
                }
            }
            (11, 3) => "Mali-G615",
            (12, 0) => big("Mali-G720-Immortalis", "Mali-G720", "Mali-G620"),
            (12, 1) => "Mali-G620",
            (13, 0) => big("Mali-G925-Immortalis", "Mali-G725", "Mali-G625"),
            (13, 1) => "Mali-G625",
            (14, 0) => "Mali-G1-Ultra",
            (14, 1) => "Mali-G1-Premium",
            (14, 3) => "Mali-G1-Pro",
            _ => "unknown Mali",
        }
    }
}

/// Reads the identification and feature registers (03 §3.1). The GPU must be powered and
/// clocked. The PWR block's present masks (architecture 14) are not read: that path is not built.
pub fn probe(r: &dyn Regs) -> Result<Info, Error> {
    let gpu_id = GpuId(r.read32(regs::GPU_ID));
    if gpu_id.0 == 0 {
        return Err(Error::NoGpu);
    }
    match gpu_id.arch_major() {
        10..=13 => {}
        a => return Err(Error::UnsupportedArch(a)),
    }
    let info = Info {
        gpu_id,
        csf_id: r.read32(regs::CSF_ID),
        revid: r.read32(regs::REVID),
        core_features: r.read32(regs::CORE_FEATURES),
        l2_features: r.read32(regs::L2_FEATURES),
        tiler_features: r.read32(regs::TILER_FEATURES),
        mem_features: r.read32(regs::MEM_FEATURES),
        mmu_features: r.read32(regs::MMU_FEATURES),
        thread_features: r.read32(regs::THREAD_FEATURES),
        thread_max_threads: r.read32(regs::THREAD_MAX_THREADS),
        thread_max_workgroup_size: r.read32(regs::THREAD_MAX_WORKGROUP_SIZE),
        thread_max_barrier_size: r.read32(regs::THREAD_MAX_BARRIER_SIZE),
        coherency_features: r.read32(regs::COHERENCY_FEATURES),
        texture_features: core::array::from_fn(|i| r.read32(regs::TEXTURE_FEATURES + 4 * i)),
        as_present: r.read32(regs::AS_PRESENT),
        gpu_features: r.read64(regs::GPU_FEATURES),
        shader_present: r.read64(regs::SHADER_PRESENT),
        tiler_present: r.read64(regs::TILER_PRESENT),
        l2_present: r.read64(regs::L2_PRESENT),
    };
    if info.l2_present == 0 {
        return Err(Error::Bad("L2_PRESENT is 0"));
    }
    if info.as_present == 0 {
        return Err(Error::Bad("AS_PRESENT is 0"));
    }
    if !(32..=52).contains(&info.pa_bits()) || !(32..=52).contains(&info.va_bits()) {
        return Err(Error::Bad("MMU_FEATURES address sizes"));
    }
    Ok(info)
}
