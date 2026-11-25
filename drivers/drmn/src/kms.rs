// SPDX-License-Identifier: BSD-2-Clause
//! The Radeon's display as a graphics card (`drivers/amd-kms`): the
//! firmware's running display adopted, its video memory written through
//! BAR0, and the card registered in place of the firmware framebuffer's
//! (docs/GRAPHICS.md §16.2).

use alloc::sync::{Arc, Weak};

use amd_kms::{AmdKms, Display, Geometry};
use amdgpu::flip;
use amdgpu::regs::Window;
use sync::SpinLock;
use vgapci::Aperture;

use crate::amd::Bar5;

/// `HUBP0_DCSURF_PRI_VIEWPORT_DIMENSION`: the height in the upper half, the
/// width in the lower.
const VIEWPORT_DIMENSION: usize = 0xeaa8;

/// Where the two scan-out slots are in the GART's address space (0 to 1
/// GiB): above the first half, which nothing else uses, 16 MiB apiece.
const SCANOUT_GART_BASE: u64 = 0x2000_0000;
const SLOT_BYTES: u64 = 16 << 20;
/// The pages in a slot.
const SLOT_PAGES: usize = (SLOT_BYTES / 4096) as usize;

/// The display, as the card drives it.
struct Scanout {
    gpu: Weak<Aperture>,
    /// The GPU address of the first byte of video memory.
    base: u64,
    hdp_ready: SpinLock<bool>,
    /// The interrupts: while they run, frames come from them.
    irq: Arc<crate::interrupt::Irq>,
    /// The driver, for a mode set.
    amd: Weak<crate::amd::Amd>,
}

impl Display for Scanout {
    fn write(&self, offset: usize, data: &[u8]) {
        let Some(gpu) = self.gpu.upgrade() else {
            return;
        };
        for (i, w) in data.as_chunks::<4>().0.iter().enumerate() {
            gpu.vram_write32(offset + 4 * i, u32::from_le_bytes(*w));
        }
    }

    fn flush(&self) {
        let Some(gpu) = self.gpu.upgrade() else {
            return;
        };
        let w = Bar5(&gpu);
        {
            let mut ready = self.hdp_ready.lock();
            if !*ready {
                flip::hdp_init(&w);
                *ready = true;
            }
        }
        // The last word back through the aperture completes the posted
        // writes before the flush asks for them to be visible.
        let _ = gpu.vram_read32(0);
        flip::hdp_flush(&w);
    }

    fn show(&self, offset: usize) {
        if let Some(gpu) = self.gpu.upgrade() {
            flip::flip(&Bar5(&gpu), self.base + offset as u64);
        }
    }

    fn pending(&self) -> bool {
        self.gpu
            .upgrade()
            .is_some_and(|gpu| flip::pending(&Bar5(&gpu)))
    }

    fn delay_us(&self, us: u32) {
        bus::manager().delay_us(u64::from(us));
    }

    fn vblank_interrupt(&self) -> bool {
        self.irq.active()
    }

    fn usable(&self) -> usize {
        self.gpu.upgrade().and_then(|g| g.vram_len()).unwrap_or(0)
    }

    fn connected(&self) -> bool {
        self.amd.upgrade().is_none_or(|a| a.connected())
    }

    /// Shows the pages as the surface, through the GART (see
    /// [`show_system_pages`]).
    fn show_pages(&self, pages: &[u64], slot: u8) -> Result<(), abi::errno::Errno> {
        let amd = self.amd.upgrade().ok_or(abi::errno::Errno::ENXIO)?;
        let gpu = self.gpu.upgrade().ok_or(abi::errno::Errno::ENXIO)?;
        show_system_pages(&amd, &gpu, pages, slot)
    }

    fn modes(&self) -> alloc::vec::Vec<kms_check::Mode> {
        // What the monitor's EDID allows of the modes the card can set.
        match self.amd.upgrade() {
            Some(amd) => amd.offered_modes(),
            None => crate::sink::settable(),
        }
        .into_iter()
        .map(|(_, m)| m)
        .collect()
    }

    fn set_mode(
        &self,
        s: Option<&sched::Sleepable>,
        mode: Option<&kms_check::Mode>,
    ) -> Result<(), abi::errno::Errno> {
        self.amd
            .upgrade()
            .ok_or(abi::errno::Errno::ENXIO)?
            .set_mode_for(s, mode)
    }
}

/// Why the display was not made a card.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "the variant's data is shown through Debug in the log"
)]
pub(crate) enum Refused {
    NoAperture,
    NotAdoptable(flip::NotAdoptable),
    /// The console's framebuffer is not the start of this device's
    /// aperture, so the picture on the screen is not this device's.
    NotTheConsole,
    Size,
}

/// Shows `pages` (physical addresses of 4 KiB pages) as the display's
/// surface, through the GART: entries for them in slot `slot` of the table
/// (uncached, not snooped), the pages written back from the processor's
/// caches, the hubs' translation caches invalidated, then a flip to the
/// slot's GART address.
pub(crate) fn show_system_pages(
    amd: &Arc<crate::amd::Amd>,
    gpu: &Arc<Aperture>,
    pages: &[u64],
    slot: u8,
) -> Result<(), abi::errno::Errno> {
    use abi::errno::Errno;
    use amdgpu::gmc::{self, Hub};
    if slot > 1 || pages.is_empty() || pages.len() > SLOT_PAGES {
        return Err(Errno::EINVAL);
    }
    let tag = gpu.dma_tag().ok_or(Errno::ENXIO)?;
    amd.scanout_prepare()?;
    let at = SCANOUT_GART_BASE + u64::from(slot) * SLOT_BYTES;
    let first = usize::try_from(at >> 12).map_err(|_| Errno::EINVAL)?;
    let mem = tag.memory();
    for (i, pa) in pages.iter().enumerate() {
        // The page is written back and dropped from the caches: the display
        // reads memory without snooping them.
        mem.clean(mem.virt(*pa).as_ptr(), 4096);
        let e = amdgpu::scanout::entry(*pa);
        let off = crate::amd::GART_AT + (first + i) * 8;
        gpu.vram_write32(off, u32::try_from(e & 0xffff_ffff).unwrap_or(0));
        gpu.vram_write32(off + 4, u32::try_from(e >> 32).unwrap_or(0));
    }
    let w = Bar5(gpu);
    // The table is in video memory: the writes complete, then are made
    // visible; the hubs forget what they had translated.
    let _ = gpu.vram_read32(crate::amd::GART_AT);
    flip::hdp_flush(&w);
    let pause = || bus::manager().delay_us(10);
    for hub in [Hub::Gfx, Hub::Mm] {
        let _ = gmc::invalidate(&w, hub, 0, 1000, &pause);
    }
    flip::flip(&w, at);
    amd.scanout_frames
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// The card for `gpu`'s display, if it is the one the console shows.
pub(crate) fn card(
    gpu: &Arc<Aperture>,
    irq: Arc<crate::interrupt::Irq>,
    amd: Weak<crate::amd::Amd>,
) -> Result<Arc<AmdKms>, Refused> {
    let phys = gpu.vram_phys().ok_or(Refused::NoAperture)?;
    let w = Bar5(gpu);
    let adopted = flip::adopt(&w).map_err(Refused::NotAdoptable)?;
    let width = w.read32(VIEWPORT_DIMENSION) & 0x3fff;
    let height = (w.read32(VIEWPORT_DIMENSION) >> 16) & 0x3fff;
    // The console draws on the first bytes of this device's video memory,
    // as large as the picture.
    let console = bus::manager().console_framebuffer();
    if !console
        .is_some_and(|(base, len)| base == phys && len >= u64::from(width) * u64::from(height) * 4)
    {
        return Err(Refused::NotTheConsole);
    }
    if width != adopted.pitch_pixels || width == 0 || height == 0 {
        return Err(Refused::Size);
    }
    let display = Arc::new(Scanout {
        gpu: Arc::downgrade(gpu),
        base: adopted.vram_base,
        hdp_ready: SpinLock::new(false),
        irq,
        amd,
    });
    amd_kms::create(Geometry { width, height }, display).map_err(|_| Refused::Size)
}
