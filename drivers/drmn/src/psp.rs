// SPDX-License-Identifier: BSD-2-Clause
//! The Radeon's platform security processor (stage R3.1 and R3.2 of
//! docs/GRAPHICS.md §16.6): its ring, the table of contents, and the trusted
//! memory zone, with every buffer in video memory.

use alloc::sync::Arc;

use amdgpu::flip;
use amdgpu::psp::{self, Buffers, Command, Memory};
use amdgpu::regs::Window;
use vgapci::Aperture;

use crate::amd::Bar5;

/// Where the PSP's buffers are in video memory (BAR0 offsets): the ring, the
/// command buffer and the fence, 4 KiB each, then the 1 MiB-aligned staging
/// buffer, past the translation tables.
pub(crate) const BUFFERS_AT: usize = 160 << 20;
/// The staging buffer (1 MiB, 1 MiB-aligned).
pub(crate) const STAGING_AT: usize = 161 << 20;
/// The trusted memory zone (a video memory offset): where the reference put
/// it, 10 MiB, 1 MiB-aligned, clear of the firmware's surface and of the top
/// 2 MiB the platform's display microcontroller holds.
pub(crate) const TMR_AT: u64 = 0x1e00_0000;
/// The table of contents' file, and what is in it (02 section 8.1): 1,792
/// bytes with 1,536 of table from offset `0x100`.
pub(crate) const TOC_PATH: &str = "/lib/firmware/amdgpu/psp_13_0_5_toc.bin";
const TOC_FILE_BYTES: usize = 1792;
const TOC_OFFSET: usize = 0x100;
const TOC_BYTES: usize = 1536;
/// `HDP_READ_CACHE_INVALIDATE`.
const HDP_READ_CACHE_INVALIDATE: usize = 0x3fc4;

/// Video memory as the PSP's buffers use it: by GPU address, through BAR0.
struct VramMemory {
    gpu: Arc<Aperture>,
    /// The GPU address of the first byte of video memory.
    base: u64,
}

impl VramMemory {
    fn offset(&self, gpu: u64) -> usize {
        usize::try_from(gpu.wrapping_sub(self.base)).unwrap_or(usize::MAX)
    }
}

impl Memory for VramMemory {
    fn write(&self, gpu: u64, data: &[u8]) {
        let at = self.offset(gpu);
        for (i, w) in data.as_chunks::<4>().0.iter().enumerate() {
            self.gpu.vram_write32(at + 4 * i, u32::from_le_bytes(*w));
        }
    }

    fn read32(&self, gpu: u64) -> u32 {
        self.gpu.vram_read32(self.offset(gpu)).unwrap_or(u32::MAX)
    }

    fn flush(&self) {
        // The last word back completes the posted writes; then the host
        // data path's flush (05 section 7).
        let _ = self.gpu.vram_read32(0);
        flip::hdp_flush(&Bar5(&self.gpu));
    }

    fn invalidate(&self) {
        let w = Bar5(&self.gpu);
        w.write32(HDP_READ_CACHE_INVALIDATE, 1);
        let _ = w.read32(HDP_READ_CACHE_INVALIDATE);
    }
}

/// What a bring-up reached.
#[derive(Debug)]
pub(crate) struct Brought {
    /// The ring and its frame counter, for the commands after (the
    /// display microcontroller's load, stage R3.3).
    #[allow(dead_code, reason = "used by the display microcontroller's load")]
    pub(crate) psp: psp::Psp,
    /// The trusted memory zone's size the PSP asked for.
    pub(crate) tmr_size: u32,
}

/// Why it stopped.
#[derive(Debug)]
#[allow(dead_code, reason = "the data is shown through Debug in the log")]
pub(crate) enum Stopped {
    BadToc(usize),
    Psp(psp::Error),
    /// A command answered with a status.
    Status(&'static str, u32),
}

/// Creates the ring, loads the table of contents, sets up the trusted memory
/// zone. `log` says each step; `read` reads a firmware file.
///
/// # Errors
///
/// [`Stopped`] says where it stopped; the PSP's state is whatever the last
/// step left.
pub(crate) fn bring_up(
    gpu: &Arc<Aperture>,
    toc: &[u8],
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> Result<Brought, Stopped> {
    use flip::FB_LOCATION_BASE;
    let w = Bar5(gpu);
    if toc.len() != TOC_FILE_BYTES {
        return Err(Stopped::BadToc(toc.len()));
    }
    let base = u64::from(w.read32(FB_LOCATION_BASE) & 0xffff) << 24;
    let mem = VramMemory {
        gpu: Arc::clone(gpu),
        base,
    };
    let pause = |us: u32| bus::manager().delay_us(u64::from(us));
    log(format_args!(
        "PSP: secure OS {:#x}, boot firmware {}, version register {:#x}",
        w.read32(psp::C2PMSG_81),
        if w.read32(psp::C2PMSG_33) >> 31 != 0 {
            "initialised"
        } else {
            "NOT initialised"
        },
        w.read32(psp::C2PMSG_58),
    ));
    let buffers = Buffers {
        ring: base + BUFFERS_AT as u64,
        command: base + BUFFERS_AT as u64 + 0x1000,
        fence: base + BUFFERS_AT as u64 + 0x2000,
    };
    // The table, in the staging buffer, flushed before the PSP is told.
    let staging = base + STAGING_AT as u64;
    mem.write(staging, &toc[TOC_OFFSET..TOC_OFFSET + TOC_BYTES]);
    mem.flush();
    let mut p = psp::Psp::create_ring(&w, &mem, buffers, psp::COMMAND_WAIT, &pause)
        .map_err(Stopped::Psp)?;
    log(format_args!("PSP: ring created at {:#x}", buffers.ring));
    let r = p
        .submit(
            &w,
            &mem,
            &Command::load_toc(staging, u32::try_from(TOC_BYTES).unwrap_or(0)),
            psp::COMMAND_WAIT,
            &pause,
        )
        .map_err(Stopped::Psp)?;
    log(format_args!(
        "PSP: table of contents loaded: status {:#x}, trusted memory zone {:#x} bytes",
        r.status, r.tmr_size
    ));
    if r.status != 0 {
        return Err(Stopped::Status("load TOC", r.status));
    }
    // The zone: its size from the PSP, at a 1 MiB-aligned place of ours.
    let physical =
        (u64::from(w.read32(amdgpu::gmc::Hub::Gfx.reg(amdgpu::gmc::Reg::FbOffset))) << 24) + TMR_AT;
    let r2 = p
        .submit(
            &w,
            &mem,
            &Command::setup_tmr(base + TMR_AT, r.tmr_size, physical),
            psp::COMMAND_WAIT,
            &pause,
        )
        .map_err(Stopped::Psp)?;
    log(format_args!(
        "PSP: trusted memory zone set up at {:#x} (physical {physical:#x}): status {:#x}",
        base + TMR_AT,
        r2.status
    ));
    if r2.status != 0 {
        return Err(Stopped::Status("set up TMR", r2.status));
    }
    Ok(Brought {
        psp: p,
        tmr_size: r.tmr_size,
    })
}
