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

/// The registers read before and after (name, byte offset): read-only.
pub(crate) const SNAPSHOT: &[(&str, usize)] = &[("CP_STAT", 0x8680)];

/// Logs the registers in [`SNAPSHOT`].
pub(crate) fn snapshot(gpu: &Arc<Aperture>, what: &str, log: &dyn Fn(core::fmt::Arguments<'_>)) {
    let w = Bar5(gpu);
    log(format_args!("GC: {what}:"));
    for (name, at) in SNAPSHOT {
        log(format_args!(
            "  {name} @{at:#07x} = {:#010x}",
            w.read32(*at)
        ));
    }
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
