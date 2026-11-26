// SPDX-License-Identifier: BSD-2-Clause
//! The CIX Sky1's integration of its Immortalis-G720 (specification 11): the RCSU window's
//! registers, core harvesting, the bus protocol, and the system-cache allocation values.

use crate::id::Info;
use crate::regs::{self, Regs};

/// RCSU `PGCTRL`: bit 0 enables Q-channel clock gating (11 §11.5 step 5).
pub const RCSU_PGCTRL: usize = 0x218;
/// RCSU: the shader-core fuses, in bits `[27:4]` (11 §11.6).
pub const RCSU_FUSES: usize = 0x304;

/// The full part's shader-present pattern: 10 cores (11 §11.6).
pub const FULL_SHADER_PRESENT: u64 = 0x0055_0555;

/// The shader-present mask with the fused-off cores removed (11 §11.6). Userspace sees this one.
pub fn harvested(shader_present: u64, fuses: u32) -> u64 {
    let available = !(u64::from(fuses & 0x0FFF_FFF0) >> 4) & FULL_SHADER_PRESENT;
    shader_present & available
}

/// The `COHERENCY_PROTOCOL` value: ACE-Lite, plus the shareable-cache bit if the GPU reports it
/// (11 §11.7).
pub fn coherency_protocol(info: &Info) -> u32 {
    regs::COHERENCY_ACE_LITE | (info.coherency_features & regs::COHERENCY_SHAREABLE_CACHE)
}

/// `SYSC_PBHA_OVERRIDE3` (11 §11.8.2).
pub const PBHA_OVERRIDE3: u32 = 0x2200_0000;
/// `SYSC_ALLOC0` to `SYSC_ALLOC7` (11 §11.8.2): one policy byte per AXI ID.
pub const SYSC_ALLOC: [u32; 8] = [
    0x0023_0000,
    0x0000_0023,
    0,
    0,
    0x0052_3222,
    0x0052_3200,
    0x0000_0022,
    0x0000_0032,
];

/// The `SYSC_ALLOC` words from the firmware's `int_id_override` pairs (AXI ID, policy byte),
/// as the board's DSDT gives them under the GPU's `pbha` data extension.
pub fn sysc_alloc_from_pairs(pairs: &[(u8, u8)]) -> Option<[u32; 8]> {
    let mut words = [0u32; 8];
    for &(id, policy) in pairs {
        let (n, k) = (usize::from(id / 4), u32::from(id % 4));
        *words.get_mut(n)? |= u32::from(policy) << (8 * k);
    }
    Some(words)
}

/// Writes the system-cache allocation registers; before the L2 is powered (11 §11.8.2).
pub fn program_system_cache(r: &dyn Regs) {
    r.write32(regs::SYSC_PBHA_OVERRIDE + 4 * 3, PBHA_OVERRIDE3);
    for (n, v) in SYSC_ALLOC.iter().enumerate() {
        r.write32(regs::SYSC_ALLOC + 4 * n, *v);
    }
}
