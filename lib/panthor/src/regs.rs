// SPDX-License-Identifier: BSD-2-Clause
//! The register window (specification 02): offsets, and the 32-bit access rules for the 64-bit
//! registers.

/// The GPU's register window. All registers are 32 bits and accessed as such (02 §2.1).
pub trait Regs {
    fn read32(&self, off: usize) -> u32;
    fn write32(&self, off: usize, v: u32);

    /// A 64-bit configuration register: low word, then high (02 §2.1).
    fn read64(&self, off: usize) -> u64 {
        let lo = self.read32(off);
        let hi = self.read32(off + 4);
        u64::from(hi) << 32 | u64::from(lo)
    }

    /// A 64-bit write: low word, then high (02 §2.1).
    fn write64(&self, off: usize, v: u64) {
        self.write32(off, (v & 0xFFFF_FFFF) as u32);
        self.write32(off + 4, (v >> 32) as u32);
    }

    /// A free-running 64-bit counter: high, low, high, again until both highs agree, so a carry
    /// between the halves is not missed (02 §2.1).
    fn read_counter(&self, off: usize) -> u64 {
        loop {
            let hi = self.read32(off + 4);
            let lo = self.read32(off);
            if self.read32(off + 4) == hi {
                return u64::from(hi) << 32 | u64::from(lo);
            }
        }
    }
}

// GPU control (02 §2.3).
pub const GPU_ID: usize = 0x000;
pub const L2_FEATURES: usize = 0x004;
pub const CORE_FEATURES: usize = 0x008;
pub const TILER_FEATURES: usize = 0x00C;
pub const MEM_FEATURES: usize = 0x010;
pub const MMU_FEATURES: usize = 0x014;
pub const AS_PRESENT: usize = 0x018;
pub const CSF_ID: usize = 0x01C;
pub const GPU_INT: usize = 0x020;
pub const GPU_CMD: usize = 0x030;
pub const GPU_STATUS: usize = 0x034;
pub const GPU_FAULT_STATUS: usize = 0x03C;
pub const GPU_FAULT_ADDR: usize = 0x040;
pub const L2_CONFIG: usize = 0x048;
pub const GPU_FEATURES: usize = 0x060;
pub const TIMESTAMP_OFFSET: usize = 0x088;
pub const CYCLE_COUNT: usize = 0x090;
pub const TIMESTAMP: usize = 0x098;
pub const THREAD_MAX_THREADS: usize = 0x0A0;
pub const THREAD_MAX_WORKGROUP_SIZE: usize = 0x0A4;
pub const THREAD_MAX_BARRIER_SIZE: usize = 0x0A8;
pub const THREAD_FEATURES: usize = 0x0AC;
pub const TEXTURE_FEATURES: usize = 0x0B0;
pub const SHADER_PRESENT: usize = 0x100;
pub const TILER_PRESENT: usize = 0x110;
pub const L2_PRESENT: usize = 0x120;
pub const SHADER_READY: usize = 0x140;
pub const TILER_READY: usize = 0x150;
pub const L2_READY: usize = 0x160;
pub const L2_PWRON: usize = 0x1A0;
pub const L2_PWROFF: usize = 0x1E0;
pub const L2_PWRTRANS: usize = 0x220;
pub const REVID: usize = 0x280;
pub const COHERENCY_FEATURES: usize = 0x300;
pub const COHERENCY_PROTOCOL: usize = 0x304;

// Architecture 12 system-cache registers (11 §11.8.1).
pub const SYSC_PBHA_OVERRIDE: usize = 0x320;
pub const SYSC_ALLOC: usize = 0x340;

// MCU control (02 §2.4).
pub const MCU_CONTROL: usize = 0x700;
pub const MCU_STATUS: usize = 0x704;

// Interrupt blocks (02 §2.1.1, 2.6, 2.7.1).
pub const JOB_INT: usize = 0x1000;
pub const MMU_INT: usize = 0x2000;

/// Offsets inside an interrupt block (02 §2.1.1).
pub const INT_RAWSTAT: usize = 0x0;
pub const INT_CLEAR: usize = 0x4;
pub const INT_MASK: usize = 0x8;
pub const INT_STAT: usize = 0xC;

// GPU interrupt bits (02 §2.3.2).
pub const INT_FAULT: u32 = 1 << 0;
pub const INT_PROTM_FAULT: u32 = 1 << 1;
pub const INT_RESET_COMPLETED: u32 = 1 << 8;
pub const INT_POWER_CHANGED: u32 = 1 << 9;
pub const INT_POWER_CHANGED_ALL: u32 = 1 << 10;
pub const INT_CLEAN_CACHES_COMPLETED: u32 = 1 << 17;
pub const INT_MCU_STATUS_CHANGED: u32 = 1 << 19;

/// The GPU-control events a driver enables (03 §3.8.4).
pub const GPU_EVENTS: u32 =
    INT_FAULT | INT_PROTM_FAULT | INT_RESET_COMPLETED | INT_CLEAN_CACHES_COMPLETED;

// `GPU_CMD` values (02 §2.3.3).
pub const CMD_SOFT_RESET: u32 = 0x101;
pub const CMD_FLUSH_CACHES: u32 = 0x04;

/// `COHERENCY_PROTOCOL` values (02 §2.3).
pub const COHERENCY_ACE_LITE: u32 = 0;
pub const COHERENCY_NONE: u32 = 31;
/// `COHERENCY_FEATURES` and `COHERENCY_PROTOCOL` bit 5: the shareable cache (11 §11.7).
pub const COHERENCY_SHAREABLE_CACHE: u32 = 1 << 5;
