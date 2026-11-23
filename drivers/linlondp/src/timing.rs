// SPDX-License-Identifier: BSD-2-Clause
//! From a display mode to the timing generator's registers (specification
//! `linlon-dpu` 06 §6.4), and the clocks a mode needs.

use abi::drm::{
    DRM_MODE_FLAG_DBLSCAN, DRM_MODE_FLAG_INTERLACE, DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PVSYNC,
};
use kms_check::Mode;

use crate::regs::{self, bs};

/// A pipeline at one pixel per clock handles at most this pixel clock
/// (section 08 §8.6).
pub const MAX_PIXEL_KHZ_1PPC: u32 = 600_000;

/// The line at which the programmed-line interrupt fires, and from which
/// pixels are fetched (section 06 §6.4.2: defaults, not requirements).
pub const PROG_LINE: u32 = 4;
pub const PREFETCH_LINE: u32 = 5;

/// Why a mode cannot be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeError {
    /// Interlaced and double-scan modes are not supported.
    Interlaced,
    /// A timing value does not fit its register field, which the
    /// hardware would truncate silently.
    Field,
    /// A pixel clock the pipeline cannot take, or none.
    Clock,
}

/// The values of the timing generator's registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BsRegs {
    pub active: u32,
    pub hintervals: u32,
    pub vintervals: u32,
    pub sync: u32,
    pub prog_line: u32,
    pub prefetch_line: u32,
    pub control: u32,
}

/// Pixels per clock the timing generator emits for `mode`: one up to
/// 600 MHz. Faster modes need two pixels per clock with the screen
/// composed in halves, which this driver does not build.
///
/// # Errors
///
/// [`ModeError::Clock`] for a mode that needs more than one.
pub fn pixels_per_clock(mode: &Mode) -> Result<u32, ModeError> {
    if mode.clock_khz == 0 {
        return Err(ModeError::Clock);
    }
    if mode.clock_khz > MAX_PIXEL_KHZ_1PPC {
        return Err(ModeError::Clock);
    }
    Ok(1)
}

/// The registers for `mode` on a single link at one pixel per clock.
///
/// The specification notes that the vendor behaviour is to truncate a
/// field that does not fit; this refuses instead.
///
/// # Errors
///
/// [`ModeError`] for what cannot be shown.
pub fn bs_regs(mode: &Mode) -> Result<BsRegs, ModeError> {
    if mode.flags & (DRM_MODE_FLAG_INTERLACE | DRM_MODE_FLAG_DBLSCAN) != 0 {
        return Err(ModeError::Interlaced);
    }
    pixels_per_clock(mode)?;
    let (ha, hs, he, ht) = (
        u32::from(mode.hdisplay),
        u32::from(mode.hsync_start),
        u32::from(mode.hsync_end),
        u32::from(mode.htotal),
    );
    let (va, vs, ve, vt) = (
        u32::from(mode.vdisplay),
        u32::from(mode.vsync_start),
        u32::from(mode.vsync_end),
        u32::from(mode.vtotal),
    );
    if ha == 0 || va == 0 || hs < ha || he < hs || ht < he || vs < va || ve < vs || vt < ve {
        return Err(ModeError::Field);
    }
    let (hfp, hsw, hbp) = (hs - ha, he - hs, ht - he);
    let (vfp, vsw, vbp) = (vs - va, ve - vs, vt - ve);
    // Field widths (section 06 §6.4.2).
    if ha > 8191
        || va > 8191
        || hfp > 4095
        || hbp > 1023
        || hsw > 1023
        || vfp > 16383
        || vbp > 255
        || vsw > 255
    {
        return Err(ModeError::Field);
    }
    let active = regs::hv_size(ha, va).ok_or(ModeError::Field)?;
    let mut sync = hsw | vsw << 16;
    if mode.flags & DRM_MODE_FLAG_PHSYNC != 0 {
        sync |= bs::SYNC_HSP;
    }
    if mode.flags & DRM_MODE_FLAG_PVSYNC != 0 {
        sync |= bs::SYNC_VSP;
    }
    Ok(BsRegs {
        active,
        hintervals: hfp | hbp << 16,
        vintervals: vfp | vbp << 16,
        sync,
        prog_line: PROG_LINE,
        prefetch_line: PREFETCH_LINE,
        control: bs::CTRL_EN | bs::CTRL_VM,
    })
}

/// The main clock the pipeline needs for `mode`, in kHz: at least the
/// pixel clock, doubled on a dual-link output (section 06 §6.5). The
/// platform may pin it higher (Sky1: 800 MHz).
#[must_use]
pub fn min_aclk_khz(mode: &Mode, dual_link: bool) -> u64 {
    u64::from(mode.clock_khz) * if dual_link { 2 } else { 1 }
}

/// Whether a scaler that takes `h_in` by `v_in` pixels to `v_out` lines
/// can be fed at `aclk_khz` for `mode` (section 05 §5.3.5), in integers.
#[must_use]
pub fn downscale_fits(mode: &Mode, aclk_khz: u64, h_in: u32, v_in: u32, v_out: u32) -> bool {
    let px = u64::from(mode.clock_khz);
    let (h_in, v_in, v_out) = (u64::from(h_in), u64::from(v_in), u64::from(v_out));
    if v_in <= v_out {
        // Horizontal-only downscale: aclk / pxclk >= h_in / (hdisplay - 3).
        let den = u64::from(mode.hdisplay).saturating_sub(3);
        den != 0 && aclk_khz * den >= px * h_in
    } else {
        // aclk / pxclk >= (h_in * v_in) / ((htotal - 1) * v_out - 2 * v_in)
        let t = u64::from(mode.htotal).saturating_sub(1) * v_out;
        let Some(den) = t.checked_sub(2 * v_in).filter(|d| *d != 0) else {
            return false;
        };
        aclk_khz * den >= px * h_in * v_in
    }
}
