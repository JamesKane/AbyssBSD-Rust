// SPDX-License-Identifier: BSD-2-Clause
//! The Radeon's display as a graphics card (`drivers/amd-kms`): the
//! firmware's running display adopted, its video memory written through
//! BAR0, and the card registered in place of the firmware framebuffer's
//! (docs/GRAPHICS.md §16.3).

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

    fn modes(&self) -> alloc::vec::Vec<kms_check::Mode> {
        crate::amd::Amd::supported_modes()
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
