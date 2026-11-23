// SPDX-License-Identifier: BSD-2-Clause
//! The video stream: what the core's main-stream registers hold for a
//! mode, and starting and stopping a source (clean-room specification
//! `sky1-dp-tx` 02 §2.5.6, §2.6).

use abi::drm::{DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PVSYNC};
use kms_check::Mode;

use crate::link::Link;
use crate::regs::{self, Words};

/// The core needs this many pixels of horizontal back porch at least.
pub const MIN_BACK_PORCH: u32 = 12;

/// A mode as the core can use it: too short a back porch is lengthened
/// and the pixel clock raised in proportion, which keeps the refresh rate
/// (section 2.6.1). The display processor is given the same timing.
#[must_use]
pub fn adjust_mode(m: &Mode) -> Mode {
    let porch = u32::from(m.htotal.saturating_sub(m.hsync_end));
    if porch >= MIN_BACK_PORCH {
        return *m;
    }
    let add = MIN_BACK_PORCH - porch;
    let old = u64::from(m.htotal);
    let new = old + u64::from(add);
    let mut out = *m;
    out.htotal = u16::try_from(new).unwrap_or(u16::MAX);
    // Same refresh rate: the clock grows with the line.
    out.clock_khz = u32::try_from((u64::from(m.clock_khz) * new).div_ceil(old)).unwrap_or(u32::MAX);
    out
}

/// What the sync polarities mean at the core (section 2.6.1): the vendor
/// inverts both before programming the display processor and the core. The
/// reason is not known, so the choice is the board's to confirm.
#[must_use]
pub fn with_inverted_sync(m: &Mode) -> Mode {
    use abi::drm::{DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_NVSYNC};
    let mut out = *m;
    out.flags &= !(DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_NHSYNC);
    out.flags &= !(DRM_MODE_FLAG_PVSYNC | DRM_MODE_FLAG_NVSYNC);
    out.flags |= if m.flags & DRM_MODE_FLAG_PHSYNC != 0 {
        DRM_MODE_FLAG_NHSYNC
    } else {
        DRM_MODE_FLAG_PHSYNC
    };
    out.flags |= if m.flags & DRM_MODE_FLAG_PVSYNC != 0 {
        DRM_MODE_FLAG_NVSYNC
    } else {
        DRM_MODE_FLAG_PVSYNC
    };
    out
}

/// Pixels per clock the display processor sends (section 2.6.5), for RGB.
#[must_use]
pub fn pixels_per_clock(pixel_khz: u32) -> u32 {
    if pixel_khz > 600_000 { 2 } else { 1 }
}

/// The `MISC0` code of a colour depth (RGB, asynchronous clock).
#[must_use]
pub fn misc0_rgb(bpc: u32) -> u32 {
    let code = match bpc {
        6 => 0,
        10 => 2,
        12 => 3,
        16 => 4,
        // 8, and what is not supported, falls back to 8.
        _ => 1,
    };
    code << 5
}

/// Register values for one source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamRegs {
    /// Offsets within the source's block, and values, in the order the
    /// specification programs them.
    pub values: [(usize, u32); 15],
}

/// The transfer-unit word (section 2.6.3): size 64, and the average of
/// valid symbols per unit as an integer and sixteenths.
#[must_use]
pub fn tu_config(pixel_khz: u32, bpp: u32, l: Link) -> u32 {
    let num = u64::from(pixel_khz) * u64::from(bpp) * 64 * 16;
    let den = 8 * u64::from(l.lanes) * u64::from(l.rate.khz());
    let a16 = u32::try_from(num / den).unwrap_or(0);
    64 | (a16 / 16) << 16 | (a16 % 16) << 24
}

/// What the registers of a stream hold for `m` (already adjusted and, if
/// wanted, with its sync inverted), at `bpc` bits per component of RGB, on
/// link `l`, with `ppc` pixels per clock.
#[must_use]
pub fn stream_regs(m: &Mode, bpc: u32, l: Link, ppc: u32) -> StreamRegs {
    let bpp = 3 * bpc;
    let (ha, ht) = (u32::from(m.hdisplay), u32::from(m.htotal));
    let (hs, he) = (u32::from(m.hsync_start), u32::from(m.hsync_end));
    let (va, vt) = (u32::from(m.vdisplay), u32::from(m.vtotal));
    let (vs, ve) = (u32::from(m.vsync_start), u32::from(m.vsync_end));
    let p = m.clock_khz;
    let mut pol = 0;
    if m.flags & DRM_MODE_FLAG_PHSYNC != 0 {
        pol |= 1;
    }
    if m.flags & DRM_MODE_FLAG_PVSYNC != 0 {
        pol |= 2;
    }
    let lanes = u32::from(l.lanes);
    let data_count = (ha * bpp).div_ceil(8).div_ceil(lanes);
    // Blanking capacity for secondary data, in symbols; integer
    // arithmetic in the specified order.
    let window = u64::from(ht - ha) * u64::from(l.rate.khz() / 40) * 9 / u64::from(p.max(1));
    let data_control = if ppc == 2 && l.lanes == 4 && l.rate == crate::dpcd::Rate::Hbr3 {
        0x0004
    } else {
        0x2004
    };
    StreamRegs {
        values: [
            (regs::MAIN_STREAM_HTOTAL, ht),
            (regs::MAIN_STREAM_VTOTAL, vt),
            (regs::MAIN_STREAM_POLARITY, pol),
            (regs::MAIN_STREAM_HSWIDTH, he - hs),
            (regs::MAIN_STREAM_VSWIDTH, ve - vs),
            (regs::MAIN_STREAM_HRES, ha),
            (regs::MAIN_STREAM_VRES, va),
            (regs::MAIN_STREAM_HSTART, ht - hs),
            (regs::MAIN_STREAM_VSTART, vt - vs),
            (regs::MAIN_STREAM_MISC0, misc0_rgb(bpc)),
            (regs::TU_CONFIG, tu_config(p, bpp, l)),
            (regs::USER_PIXEL_COUNT, ppc),
            (regs::USER_DATA_COUNT, data_count),
            (
                regs::SECONDARY_DATA_WINDOW,
                u32::try_from(window).unwrap_or(u32::MAX),
            ),
            (regs::DATA_CONTROL, data_control),
        ],
    }
}

/// Starts source `s` (section 2.6.6 from step 2; its video clock is the
/// caller's). `sync_input` is the polarity word of the input side, written
/// for source 0 only.
pub fn start(w: &dyn Words, s: usize, r: &StreamRegs, polarity: u32) {
    let base = regs::source(s);
    w.write(base + regs::MAIN_STREAM_MISC1, 0);
    w.write(base + regs::MAIN_STREAM_OVERRIDE, 0);
    for &(off, v) in &r.values {
        w.write(base + off, v);
    }
    if s == 0 {
        w.write(base + regs::USER_SYNC_POLARITY, 0xC | polarity);
    }
    w.write(base + regs::VIDEO_STREAM_ENABLE, 1);
    w.write(base + regs::SECONDARY_STREAM_ENABLE, 1);
}

/// Stops source `s`: secondary data first, then the video.
pub fn stop(w: &dyn Words, s: usize) {
    let base = regs::source(s);
    w.write(base + regs::SECONDARY_STREAM_ENABLE, 0);
    w.write(base + regs::VIDEO_STREAM_ENABLE, 0);
}

/// Enables the sources in `mask` after training (section 2.5.6).
pub fn enable_sources(w: &dyn Words, mask: u32) {
    w.write(regs::SOFT_RESET, mask);
    w.write(regs::SOURCE_ENABLE, mask);
    w.write(regs::FORCE_SCRAMBLER_RESET, mask);
}

/// Shuts the link's sources down (section 2.11.2 step 4).
pub fn disable_sources(w: &dyn Words) {
    w.write(regs::SOURCE_ENABLE, 0);
    w.write(regs::SOFT_RESET, 0);
    w.write(regs::FORCE_SCRAMBLER_RESET, 0);
}
