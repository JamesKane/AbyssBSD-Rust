// SPDX-License-Identifier: BSD-2-Clause
//! Entries in the GART for the card's own system-memory buffers (the fence
//! words, the interrupt handler's ring): memory the processor reads from its
//! cache in a few nanoseconds, where a read of video memory across the bus
//! takes most of a microsecond.
//!
//! The GART is a flat table in video memory, shared by the two translation
//! hubs (`Amd::gmc`), covering the first gigabyte of the engines' address
//! space. These addresses are above the two scan-out slots (`kms.rs`).

use alloc::sync::Arc;

use amdgpu::gmc::{self, Hub};
use amdgpu::vm::{Flags, MemoryType};
use vgapci::Aperture;

use crate::amd::{Bar5, GART_AT};

/// The fence words' page.
pub(crate) const FENCES: u64 = 0x3000_0000;
/// The interrupt handler's ring and its write-back word.
pub(crate) const INTERRUPT_RING: u64 = 0x3100_0000;

/// Enters the pages at bus addresses `first`, `first + 4096`, ... (`pages`
/// of them, contiguous) in the GART at GPU address `at`: readable and
/// writeable, snooped (the processor's cache sees what an engine writes), and
/// empties the hubs' translation caches. The hubs must be set up.
pub(crate) fn map_system(gpu: &Arc<Aperture>, at: u64, first: u64, pages: usize) {
    let flags = Flags {
        readable: true,
        writeable: true,
        executable: false,
        system: true,
        snooped: true,
        memory_type: MemoryType::Uc,
    };
    let index = usize::try_from(at >> 12).unwrap_or(0);
    for i in 0..pages {
        let entry = amdgpu::vm::pte(first + 4096 * i as u64, flags);
        let off = GART_AT + (index + i) * 8;
        gpu.vram_write32(off, u32::try_from(entry & 0xffff_ffff).unwrap_or(0));
        gpu.vram_write32(off + 4, u32::try_from(entry >> 32).unwrap_or(0));
    }
    // The table is in video memory: the writes complete and are made
    // visible, and the hubs forget what they had translated.
    let _ = gpu.vram_read32(GART_AT + index * 8);
    let w = Bar5(gpu);
    amdgpu::flip::hdp_flush(&w);
    for hub in [Hub::Gfx, Hub::Mm] {
        let _ = gmc::invalidate(&w, hub, 0, 1000, &|| bus::manager().delay_us(10));
    }
}
