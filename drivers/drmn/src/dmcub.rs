// SPDX-License-Identifier: BSD-2-Clause
//! The Radeon's display microcontroller (stage R3.3 of docs/GRAPHICS.md
//! §16.6): reset the unit the platform's firmware is running on, have the
//! security processor load the driver's display firmware (image type 51),
//! give the unit the driver's windows and mailboxes, and let it run.
//!
//! Each step is logged so a stall shows where it stopped. The picture is
//! scanned out by the timing generator and the plane, not by the unit, so
//! the frame count is logged around the sequence to show the display kept
//! going.

use alloc::sync::Arc;

use amdgpu::dmcub::{self, Layout};
use amdgpu::flip;
use amdgpu::psp::{self, Command};
use amdgpu::regs::Window;
use vgapci::Aperture;

use crate::amd::Bar5;
use crate::psp::STAGING_AT;

/// The display firmware's file.
pub(crate) const PATH: &str = "/lib/firmware/amdgpu/dcn_3_1_5_dmcub.bin";
/// The file's header: 256 bytes before the instruction region (02 section 6).
const FILE_HEADER: usize = 0x100;
/// Where the instruction region's size is, in the file's header.
const SIZE_AT: usize = 0x20;
/// The windows' places in video memory (BAR0 offsets, 1 MiB each, after the
/// security processor's staging buffer at 161 MiB).
const VBIOS_AT: usize = 162 << 20;
const MAILBOXES_AT: usize = 163 << 20;
const TRACE_AT: usize = 164 << 20;
const STATE_AT: usize = 165 << 20;
/// The copy of the video BIOS: its size (the platform's own is 128 KiB).
const VBIOS_BYTES: u32 = 0x2_0000;
/// How long to wait for the firmware: 2 s in 1 ms steps.
const BOOT_POLLS: u32 = 2000;
/// The stop request's waits: 100 ms each in 1 ms steps (08 section 3.2).
const STOP_POLLS: u32 = 100;

/// Why it stopped.
#[derive(Debug)]
#[allow(dead_code, reason = "the data is shown through Debug in the log")]
pub(crate) enum Stopped {
    /// The file is not an image this driver knows.
    BadImage(usize),
    /// The unit is not present on this chip.
    NotPresent,
    /// The security processor.
    Psp(psp::Error),
    /// The load answered with a status.
    Load(u32),
    /// The firmware did not come up; the last `SCRATCH0`.
    NoBoot(u32),
}

/// Zeroes `bytes` of video memory at BAR0 offset `at`.
fn zero(gpu: &Aperture, at: usize, bytes: usize) {
    for o in (0..bytes).step_by(4) {
        gpu.vram_write32(at + o, 0);
    }
}

/// Runs the whole sequence. `file` is the firmware file; `psp` the ring
/// from the security processor's bring-up.
///
/// # Errors
///
/// [`Stopped`] says where it stopped; the unit is left as that step left it.
pub(crate) fn load(
    gpu: &Arc<Aperture>,
    psp: &mut psp::Psp,
    file: &[u8],
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> Result<(), Stopped> {
    let w = Bar5(gpu);
    let pause = |us: u32| bus::manager().delay_us(u64::from(us));
    let size = file
        .get(SIZE_AT..SIZE_AT + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .ok_or(Stopped::BadImage(file.len()))?;
    let region = file
        .get(FILE_HEADER..FILE_HEADER + size)
        .ok_or(Stopped::BadImage(file.len()))?;
    let needs = dmcub::needs(region).ok_or(Stopped::BadImage(file.len()))?;
    log(format_args!(
        "DMCUB: image {} bytes, version {:#x}, state window {:#x}, trace window {:#x}",
        region.len(),
        needs.version,
        needs.state_bytes,
        needs.trace_bytes
    ));
    if !dmcub::present(&w) {
        return Err(Stopped::NotPresent);
    }
    let base = u64::from(w.read32(flip::FB_LOCATION_BASE) & 0xffff) << 24;
    let state = |what: &str| {
        log(format_args!(
            "DMCUB: {what}: control {:#x}, control2 {:#x}, scratch0 {:#x}, reset {:#x}, frames {}",
            w.read32(dmcub::CNTL),
            w.read32(dmcub::CNTL2),
            w.read32(dmcub::SCRATCH0),
            w.read32(dmcub::MMHUBBUB_SOFT_RESET),
            flip::frame_count(&w),
        ));
    };
    state("before");
    // The windows' memory and the image, written and flushed before the
    // unit or the security processor sees them.
    zero(gpu, VBIOS_AT, VBIOS_BYTES as usize);
    zero(gpu, MAILBOXES_AT, 0x4000);
    zero(gpu, TRACE_AT, needs.trace_bytes as usize);
    zero(gpu, STATE_AT, needs.state_bytes as usize);
    for (i, c) in region.chunks(4).enumerate() {
        let mut b = [0u8; 4];
        b[..c.len()].copy_from_slice(c);
        gpu.vram_write32(STAGING_AT + 4 * i, u32::from_le_bytes(b));
    }
    let _ = gpu.vram_read32(0);
    flip::hdp_flush(&w);

    let stopped = dmcub::stop(&w, STOP_POLLS, 1000, &pause);
    log(format_args!(
        "DMCUB: the platform's firmware {} the stop request",
        if stopped {
            "answered"
        } else {
            "did not answer"
        }
    ));
    dmcub::hold(&w);
    dmcub::clean(&w);
    state("held");

    let code = u32::try_from(size).unwrap_or(0).saturating_sub(512);
    let r = psp
        .submit(
            &w,
            &crate::psp::vram_memory(gpu, base),
            &Command::load_ip_firmware(
                base + STAGING_AT as u64,
                u32::try_from(size).unwrap_or(0),
                dmcub::IMAGE_TYPE,
            ),
            psp::COMMAND_WAIT,
            &pause,
        )
        .map_err(Stopped::Psp)?;
    log(format_args!(
        "DMCUB: the security processor's load: status {:#x}",
        r.status
    ));
    if r.status != 0 {
        return Err(Stopped::Load(r.status));
    }
    // What the security processor wrote into windows 0 and 1.
    for n in [0usize, 1] {
        log(format_args!(
            "DMCUB: window {n}: base {:#x}, top {:#x}, offset {:#x}:{:#x}",
            w.read32(dmcub::CW_BASE + 4 * n),
            w.read32(dmcub::CW_TOP + 4 * n),
            w.read32(dmcub::CW_OFFSET + 8 * n + 4),
            w.read32(dmcub::CW_OFFSET + 8 * n),
        ));
    }
    let layout = Layout {
        vbios: base + VBIOS_AT as u64,
        vbios_bytes: VBIOS_BYTES,
        mailboxes: base + MAILBOXES_AT as u64,
        trace: base + TRACE_AT as u64,
        trace_bytes: needs.trace_bytes,
        state: base + STATE_AT as u64,
        state_bytes: needs.state_bytes,
        code_bytes: code,
    };
    dmcub::configure(&w, &layout);
    state("configured");
    dmcub::release(&w, w.read32(psp::C2PMSG_58));
    state("released");
    match dmcub::wait_running(&w, BOOT_POLLS, 1000, &pause) {
        Ok(v) => {
            state("running");
            log(format_args!(
                "DMCUB: the display firmware is up: scratch0 {v:#x}"
            ));
            round_trips(gpu, &w, base, log);
            Ok(())
        }
        Err(v) => {
            state("stalled");
            Err(Stopped::NoBoot(v))
        }
    }
}

/// The first conversations with the running firmware: its version by the
/// general-purpose interrupt register, then a no-operation command through
/// inbox 1.
fn round_trips(
    gpu: &Arc<Aperture>,
    w: &Bar5<'_>,
    base: u64,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) {
    let pause = |us: u32| bus::manager().delay_us(u64::from(us));
    match dmcub::version(w, 100, 1000, &pause) {
        Some(v) => log(format_args!(
            "DMCUB: the firmware says its version is {v:#x}"
        )),
        None => log(format_args!("DMCUB: the version request was not taken")),
    }
    let inbox = dmcub::Inbox::new(base + MAILBOXES_AT as u64);
    let mem = crate::psp::vram_memory(gpu, base);
    let nop = dmcub::header(dmcub::CMD_NOP, 0, false, 0).to_le_bytes();
    match inbox.submit(w, &mem, &nop, 100, 1000, &pause) {
        Ok(()) => log(format_args!(
            "DMCUB: a no-operation command through inbox 1 was taken (read pointer {:#x})",
            w.read32(dmcub::INBOX1 + 12)
        )),
        Err(e) => log(format_args!("DMCUB: inbox 1: {e:?}")),
    }
    match inbox.submit(w, &mem, &dmcub::enable_outbox1(true), 100, 1000, &pause) {
        Ok(()) => log(format_args!(
            "DMCUB: outbox 1 notifications enabled (read pointer {:#x}, outbox 1 write pointer {:#x})",
            w.read32(dmcub::INBOX1 + 12),
            w.read32(dmcub::OUTBOX1 + 8)
        )),
        Err(e) => log(format_args!("DMCUB: enable outbox 1: {e:?}")),
    }
}
