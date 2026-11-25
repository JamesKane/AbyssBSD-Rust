// SPDX-License-Identifier: BSD-2-Clause
//! The graphics, compute and DMA engines' firmware, handed to the
//! security processor (stage R4.1 of docs/GRAPHICS.md §16), then the RLC's
//! autoload started: the processor verifies each image into the trusted
//! memory zone and the RLC brings the command processors up from there.

use alloc::sync::Arc;

use amdgpu::gcfw::{self, Files};
use amdgpu::psp::{self, Command, Psp};
use amdgpu::regs::Window;
use vgapci::Aperture;

use crate::amd::Bar5;
use crate::psp::STAGING_AT;

/// Where the files are, and which.
pub(crate) const DIR: &str = "/lib/firmware/amdgpu/";
/// The seven files: SDMA, CE, PFP, ME, MEC, MEC2, RLC.
pub(crate) const FILES: [&str; 7] = [
    "sdma_5_2_6.bin",
    "gc_10_3_6_ce.bin",
    "gc_10_3_6_pfp.bin",
    "gc_10_3_6_me.bin",
    "gc_10_3_6_mec.bin",
    "gc_10_3_6_mec2.bin",
    "gc_10_3_6_rlc.bin",
];

/// Why it stopped.
#[derive(Debug)]
#[allow(dead_code, reason = "the data is shown through Debug in the log")]
pub(crate) enum Stopped {
    /// A file did not read as its container.
    Firmware(gcfw::Error),
    /// The image does not fit the staging buffer.
    TooBig(&'static str, usize),
    /// The security processor.
    Psp(psp::Error),
    /// A command answered with a status; the name of the step.
    Status(&'static str, u32),
}

/// `RLC_GPM_STAT` (Raphael byte): bit 1 the graphics power is on, bit 2 its
/// clock is on. Read first: the rest of the graphics registers only if both
/// are set (06 section 2.1).
const RLC_GPM_STAT: usize = 0x3b9b8;
/// `RLC_RLCS_BOOTLOAD_STATUS`: bit 31 boot complete, bit 0 the IRAM loaded.
pub(crate) const BOOTLOAD_STATUS: usize = 0x3ba34;
/// `CP_STAT`: 0 when the command processor is idle.
pub(crate) const CP_STAT: usize = 0x8680;

/// The registers read before and after (name, byte offset): read-only.
pub(crate) const SNAPSHOT: &[(&str, usize)] = &[
    ("GRBM_STATUS", 0x8010),
    ("GRBM_STATUS2", 0x8008),
    ("GRBM_STATUS_SE0", 0x8014),
    ("GRBM_STATUS_SE1", 0x8018),
    ("CP_STAT", CP_STAT),
    ("CP_BUSY_STAT", 0x867c),
    ("CPF_STATUS", 0x821c),
    ("CPC_STATUS", 0x8210),
    ("CP_ME_CNTL", 0x86d8),
    ("CP_MEC_CNTL", 0x86d4),
    ("RLC_CNTL", 0x3b000),
    ("RLC_STAT", 0x3b010),
    ("RLC_PG_CNTL", 0x3b10c),
    ("RLC_CGCG_CGLS_CTRL", 0x3b124),
    ("RLC_SAFE_MODE", 0x3b280),
    ("RLC_RLCS_BOOTLOAD_STATUS", BOOTLOAD_STATUS),
    ("BOOTLOAD_ID_STATUS1", 0x3bbb0),
    ("BOOTLOAD_ID_STATUS2", 0x3bbb4),
    ("CP_RB0_BASE", 0xc100),
    ("CP_RB0_CNTL", 0xc104),
    ("CP_RB0_RPTR", 0x8700),
    ("CP_RB0_WPTR", 0xc150),
    ("SDMA0_F32_CNTL", 0x4a28),
    ("SDMA0_STATUS_REG", 0x4a14),
];

/// Logs the graphics power and clock state and, if both are on and nothing
/// reads as all ones, the registers in [`SNAPSHOT`] (06 section 2.1's read
/// order: a read of a block that is off must not be the first).
pub(crate) fn snapshot(gpu: &Arc<Aperture>, what: &str, log: &dyn Fn(core::fmt::Arguments<'_>)) {
    let w = Bar5(gpu);
    let gpm = w.read32(RLC_GPM_STAT);
    log(format_args!(
        "GC: {what}: RLC_GPM_STAT @{RLC_GPM_STAT:#07x} = {gpm:#010x} (power {}, clock {})",
        if gpm & 2 != 0 { "on" } else { "off" },
        if gpm & 4 != 0 { "on" } else { "off" },
    ));
    if gpm == 0xffff_ffff || gpm & 6 != 6 {
        log(format_args!(
            "  the rest of the graphics registers not read"
        ));
        return;
    }
    for (name, at) in SNAPSHOT {
        let v = w.read32(*at);
        log(format_args!("  {name} @{at:#07x} = {v:#010x}"));
        if v == 0xffff_ffff {
            log(format_args!("  all ones: stopping the reads"));
            return;
        }
    }
}

/// Waits for the autoload: `CP_STAT` 0 and the boot-complete bit, in one
/// read pass (06 section 2.1). Polls every millisecond for up to `ms`.
/// Returns the last `(CP_STAT, BOOTLOAD_STATUS)` and whether it finished.
pub(crate) fn wait_autoload(gpu: &Arc<Aperture>, ms: u32) -> (u32, u32, bool) {
    let w = Bar5(gpu);
    let (mut cp, mut boot) = (0xffff_ffff, 0);
    for _ in 0..ms {
        cp = w.read32(CP_STAT);
        boot = w.read32(BOOTLOAD_STATUS);
        if cp == 0 && boot >> 31 != 0 {
            return (cp, boot, true);
        }
        bus::manager().delay_us(1000);
    }
    (cp, boot, false)
}

/// Hands each image to the security processor and starts the autoload.
///
/// # Errors
///
/// [`Stopped`] says where it stopped; the processor's state is whatever the
/// last command left.
pub(crate) fn load(
    gpu: &Arc<Aperture>,
    psp: &mut Psp,
    files: &Files<'_>,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> Result<(), Stopped> {
    let w = Bar5(gpu);
    let base = u64::from(w.read32(amdgpu::flip::FB_LOCATION_BASE) & 0xffff) << 24;
    let mem = crate::psp::vram_memory(gpu, base);
    let pause = |us: u32| bus::manager().delay_us(u64::from(us));
    let list = gcfw::load_list(files).map_err(Stopped::Firmware)?;
    for image in &list {
        // The staging buffer is 1 MiB, 4 KiB-aligned, and each image goes
        // through it in turn (the processor copies it into its zone).
        if image.data.len() > 1 << 20 {
            return Err(Stopped::TooBig(image.name, image.data.len()));
        }
        for (i, c) in image.data.chunks(4).enumerate() {
            let mut b = [0u8; 4];
            b[..c.len()].copy_from_slice(c);
            gpu.vram_write32(STAGING_AT + 4 * i, u32::from_le_bytes(b));
        }
        let _ = gpu.vram_read32(0);
        amdgpu::flip::hdp_flush(&w);
        let size = u32::try_from(image.data.len()).unwrap_or(0);
        let r = psp
            .submit(
                &w,
                &mem,
                &Command::load_ip_firmware(base + STAGING_AT as u64, size, image.kind),
                psp::COMMAND_WAIT,
                &pause,
            )
            .map_err(Stopped::Psp)?;
        log(format_args!(
            "GC: {} (type {}, {} bytes): status {:#x}",
            image.name,
            image.kind,
            image.data.len(),
            r.status
        ));
        if r.status != 0 {
            return Err(Stopped::Status(image.name, r.status));
        }
    }
    let r = psp
        .submit(
            &w,
            &mem,
            &Command::start_rlc_autoload(),
            psp::COMMAND_WAIT,
            &pause,
        )
        .map_err(Stopped::Psp)?;
    log(format_args!(
        "GC: RLC autoload started: status {:#x}",
        r.status
    ));
    if r.status != 0 {
        return Err(Stopped::Status("start RLC autoload", r.status));
    }
    Ok(())
}
