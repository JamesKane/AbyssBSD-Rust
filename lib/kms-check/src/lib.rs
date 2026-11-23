// SPDX-License-Identifier: BSD-2-Clause
//! The generic rules of an atomic modeset, as pure functions: display
//! modes, plane geometry and clipping, plane and connector routing. A
//! driver's own `check` adds its hardware limits on top.
//!
//! Behavior reference: the atomic kernel mode setting model of `docs/GRAPHICS.md` §7.3;
//! the rules are those every compositor already meets (a plane's source
//! lies inside its framebuffer, an active display controller has a mode, a connector
//! reaches only a display controller its encoder can).

#![no_std]
#![forbid(unsafe_code)]

use abi::drm::{DRM_MODE_FLAG_DBLSCAN, DRM_MODE_FLAG_INTERLACE, DrmModeModeinfo};
use drm_fourcc::{Fourcc, Modifier};

/// A display object's number in a device (a display controller, plane,
/// connector, or framebuffer): 1 or more, 0 is "none".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjId(pub u32);

/// A display mode's timings (`drm_mode_modeinfo`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mode {
    /// Pixel clock in kHz.
    pub clock_khz: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub flags: u32,
    pub kind: u32,
}

/// Why a mode is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeError {
    /// A zero pixel clock, size, or total.
    Empty,
    /// The sync and total fields are not in increasing order.
    Order,
    /// Interlaced and double-scan modes, which no driver here scans out.
    Unsupported,
}

impl Mode {
    /// The mode a caller's `drm_mode_modeinfo` describes.
    #[must_use]
    pub fn from_info(i: &DrmModeModeinfo) -> Self {
        Self {
            clock_khz: i.clock,
            hdisplay: i.hdisplay,
            hsync_start: i.hsync_start,
            hsync_end: i.hsync_end,
            htotal: i.htotal,
            hskew: i.hskew,
            vdisplay: i.vdisplay,
            vsync_start: i.vsync_start,
            vsync_end: i.vsync_end,
            vtotal: i.vtotal,
            vscan: i.vscan,
            flags: i.flags,
            kind: i.kind,
        }
    }

    /// Its `drm_mode_modeinfo`, named `<h>x<v>` and with its refresh in
    /// whole hertz.
    #[must_use]
    pub fn to_info(&self) -> DrmModeModeinfo {
        let mut name = [0u8; abi::drm::DRM_DISPLAY_MODE_LEN];
        let mut n = 0;
        for v in [u32::from(self.hdisplay), u32::from(self.vdisplay)] {
            if n != 0 {
                name[n] = b'x';
                n += 1;
            }
            n += put_decimal(&mut name[n..], v);
        }
        DrmModeModeinfo {
            clock: self.clock_khz,
            hdisplay: self.hdisplay,
            hsync_start: self.hsync_start,
            hsync_end: self.hsync_end,
            htotal: self.htotal,
            hskew: self.hskew,
            vdisplay: self.vdisplay,
            vsync_start: self.vsync_start,
            vsync_end: self.vsync_end,
            vtotal: self.vtotal,
            vscan: self.vscan,
            vrefresh: self.refresh_hz(),
            flags: self.flags,
            kind: self.kind,
            name,
        }
    }

    /// The frame rate in millihertz (`clock / (htotal * vtotal)`), 0 if
    /// the totals are.
    #[must_use]
    pub fn refresh_mhz(&self) -> u32 {
        let total = u64::from(self.htotal) * u64::from(self.vtotal);
        if total == 0 {
            return 0;
        }
        let mhz = u64::from(self.clock_khz) * 1_000_000 / total;
        u32::try_from(mhz).unwrap_or(u32::MAX)
    }

    /// The frame rate in whole hertz, rounded.
    #[must_use]
    pub fn refresh_hz(&self) -> u32 {
        self.refresh_mhz().saturating_add(500) / 1000
    }

    /// The nanoseconds between frames, 0 for a mode without a rate.
    #[must_use]
    pub fn frame_ns(&self) -> u64 {
        let total = u64::from(self.htotal) * u64::from(self.vtotal);
        if self.clock_khz == 0 || total == 0 {
            return 0;
        }
        total * 1_000_000 / u64::from(self.clock_khz)
    }

    /// Whether the timings are well formed (`drm_mode_validate_basic`).
    ///
    /// # Errors
    ///
    /// A [`ModeError`] for the first rule broken.
    pub fn validate(&self) -> Result<(), ModeError> {
        if self.clock_khz == 0 || self.hdisplay == 0 || self.vdisplay == 0 {
            return Err(ModeError::Empty);
        }
        if self.hdisplay > self.hsync_start
            || self.hsync_start > self.hsync_end
            || self.hsync_end > self.htotal
            || self.vdisplay > self.vsync_start
            || self.vsync_start > self.vsync_end
            || self.vsync_end > self.vtotal
        {
            return Err(ModeError::Order);
        }
        if self.htotal == 0 || self.vtotal == 0 {
            return Err(ModeError::Empty);
        }
        if self.flags & (DRM_MODE_FLAG_INTERLACE | DRM_MODE_FLAG_DBLSCAN) != 0 {
            return Err(ModeError::Unsupported);
        }
        Ok(())
    }
}

/// Writes `v` in decimal at the start of `out`; returns the digits.
fn put_decimal(out: &mut [u8], mut v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = 0;
    loop {
        tmp[n] = b'0' + u8::try_from(v % 10).unwrap_or(0);
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for i in 0..n {
        out[i] = tmp[n - 1 - i];
    }
    n
}

/// What a framebuffer is, as planes see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbInfo {
    pub id: ObjId,
    pub format: Fourcc,
    pub modifier: Modifier,
    pub width: u32,
    pub height: u32,
}

/// A source rectangle in 16.16 fixed point (`SRC_X`, `SRC_Y`, `SRC_W`,
/// `SRC_H`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SrcRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// A destination rectangle on the display controller, in pixels (`CRTC_X` and so on).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DstRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// A plane's requested state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneState {
    pub crtc: Option<ObjId>,
    pub fb: Option<FbInfo>,
    pub src: SrcRect,
    pub dst: DstRect,
}

/// A display controller's requested state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrtcState {
    pub active: bool,
    pub mode: Option<Mode>,
}

/// What a plane can do.
#[derive(Clone, Copy, Debug)]
pub struct PlaneCaps<'a> {
    /// Bit `i` set: it can scan out on display controller `i` (in index order).
    pub possible_crtcs: u32,
    pub formats: &'a [Fourcc],
    /// Scaling it supports, as source-to-destination ratios in 16.16
    /// `(min, max)`; `None` is 1:1 only.
    pub scale: Option<(u32, u32)>,
}

/// Why a state is refused. Each is `EINVAL` to userland.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// A framebuffer without a display controller, or a display controller without a framebuffer.
    HalfBound,
    /// The plane cannot scan out on that display controller.
    CrtcNotPossible,
    /// The plane is on a display controller that is off.
    CrtcInactive,
    FormatUnsupported,
    /// The source is empty or leaves its framebuffer.
    BadSource,
    /// The destination is empty or its size does not scale.
    BadDestination,
    /// A scale the plane cannot do.
    Scaling,
    /// An active display controller without a mode, or a mode set on an inactive one.
    ModeMismatch,
    Mode(ModeError),
    /// A connector routed to a display controller its encoder cannot reach.
    ConnectorCrtc,
    /// Two connectors on one display controller (no cloning).
    CloneUnsupported,
}

impl From<ModeError> for CheckError {
    fn from(e: ModeError) -> Self {
        Self::Mode(e)
    }
}

/// The part of a plane on screen (`drm_atomic_helper_check_plane_state`'s
/// clipped state).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visible {
    /// Whether any of it shows. If not, the plane is off in effect.
    pub shown: bool,
    pub src: SrcRect,
    pub dst: DstRect,
}

/// Checks plane state `st` against its `caps` and, if it names one, the
/// display controller's index and state; returns the clipped geometry.
///
/// # Errors
///
/// The first [`CheckError`] the state earns.
pub fn check_plane(
    caps: &PlaneCaps<'_>,
    st: &PlaneState,
    crtc: Option<(u32, &CrtcState)>,
) -> Result<Visible, CheckError> {
    let off = Visible {
        shown: false,
        src: SrcRect::default(),
        dst: DstRect::default(),
    };
    let (Some(fb), Some(_)) = (st.fb, st.crtc) else {
        return if st.fb.is_none() && st.crtc.is_none() {
            Ok(off)
        } else {
            Err(CheckError::HalfBound)
        };
    };
    let Some((index, cs)) = crtc else {
        return Err(CheckError::CrtcNotPossible);
    };
    if index >= 32 || caps.possible_crtcs & (1 << index) == 0 {
        return Err(CheckError::CrtcNotPossible);
    }
    if !cs.active {
        return Err(CheckError::CrtcInactive);
    }
    if !caps.formats.contains(&fb.format) {
        return Err(CheckError::FormatUnsupported);
    }
    // The source lies inside the framebuffer.
    let (fw, fh) = (u64::from(fb.width) << 16, u64::from(fb.height) << 16);
    let s = st.src;
    if s.w == 0
        || s.h == 0
        || u64::from(s.x) + u64::from(s.w) > fw
        || u64::from(s.y) + u64::from(s.h) > fh
    {
        return Err(CheckError::BadSource);
    }
    let d = st.dst;
    if d.w == 0 || d.h == 0 {
        // A plane with a framebuffer and nothing to show is off.
        return Ok(off);
    }
    // Scale ratios, source over destination, in 16.16.
    let rx = (u64::from(s.w)) / u64::from(d.w);
    let ry = (u64::from(s.h)) / u64::from(d.h);
    match caps.scale {
        None => {
            if rx != 1 << 16 || ry != 1 << 16 || u64::from(s.w) % u64::from(d.w) != 0 {
                return Err(CheckError::Scaling);
            }
        }
        Some((lo, hi)) => {
            if rx < u64::from(lo) || rx > u64::from(hi) || ry < u64::from(lo) || ry > u64::from(hi)
            {
                return Err(CheckError::Scaling);
            }
        }
    }
    Ok(clip(st, cs.mode.as_ref()))
}

/// Clips `st` to the mode's area, moving the source with the cut edge.
fn clip(st: &PlaneState, mode: Option<&Mode>) -> Visible {
    let none = Visible {
        shown: false,
        src: st.src,
        dst: st.dst,
    };
    let Some(m) = mode else {
        return none;
    };
    let (cw, ch) = (i64::from(m.hdisplay), i64::from(m.vdisplay));
    let d = st.dst;
    let (x1, y1) = (i64::from(d.x), i64::from(d.y));
    let (x2, y2) = (x1 + i64::from(d.w), y1 + i64::from(d.h));
    let (cx1, cy1, cx2, cy2) = (x1.max(0), y1.max(0), x2.min(cw), y2.min(ch));
    if cx1 >= cx2 || cy1 >= cy2 {
        return none;
    }
    // Source units per destination pixel.
    let sw = i64::from(st.src.w);
    let sh = i64::from(st.src.h);
    let dw = i64::from(d.w);
    let dh = i64::from(d.h);
    let nx = i64::from(st.src.x) + (cx1 - x1) * sw / dw;
    let ny = i64::from(st.src.y) + (cy1 - y1) * sh / dh;
    let nw = (cx2 - cx1) * sw / dw;
    let nh = (cy2 - cy1) * sh / dh;
    let to = |v: i64| u32::try_from(v).unwrap_or(0);
    Visible {
        shown: nw > 0 && nh > 0,
        src: SrcRect {
            x: to(nx),
            y: to(ny),
            w: to(nw),
            h: to(nh),
        },
        dst: DstRect {
            x: i32::try_from(cx1).unwrap_or(0),
            y: i32::try_from(cy1).unwrap_or(0),
            w: u32::try_from(cx2 - cx1).unwrap_or(0),
            h: u32::try_from(cy2 - cy1).unwrap_or(0),
        },
    }
}

/// Checks a display controller's state: active means a good mode, inactive means none
/// is needed (a mode left on an inactive display controller is fine, as userland
/// writes it back).
///
/// # Errors
///
/// [`CheckError::ModeMismatch`] or the mode's own error.
pub fn check_crtc(st: &CrtcState) -> Result<(), CheckError> {
    match (st.active, st.mode) {
        (true, None) => Err(CheckError::ModeMismatch),
        (true, Some(m)) => Ok(m.validate()?),
        (false, _) => Ok(()),
    }
}

/// Checks connector routing: each connector's `(mask, crtc index)` has
/// its display controller in the mask of display controllers its encoders reach, and no two share a
/// display controller.
///
/// # Errors
///
/// [`CheckError::ConnectorCrtc`] or [`CheckError::CloneUnsupported`].
pub fn check_routing(conns: &[(u32, Option<u32>)]) -> Result<(), CheckError> {
    let mut used = 0u32;
    for &(mask, crtc) in conns {
        let Some(i) = crtc else { continue };
        if i >= 32 || mask & (1 << i) == 0 {
            return Err(CheckError::ConnectorCrtc);
        }
        if used & (1 << i) != 0 {
            return Err(CheckError::CloneUnsupported);
        }
        used |= 1 << i;
    }
    Ok(())
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use drm_fourcc::{MOD_LINEAR, format};

    fn m1080() -> Mode {
        Mode {
            clock_khz: 148_500,
            hdisplay: 1920,
            hsync_start: 2008,
            hsync_end: 2052,
            htotal: 2200,
            vdisplay: 1080,
            vsync_start: 1084,
            vsync_end: 1089,
            vtotal: 1125,
            ..Mode::default()
        }
    }

    fn fb(w: u32, h: u32) -> FbInfo {
        FbInfo {
            id: ObjId(9),
            format: format::XRGB8888,
            modifier: MOD_LINEAR,
            width: w,
            height: h,
        }
    }

    const FORMATS: &[Fourcc] = &[format::XRGB8888, format::ARGB8888];
    const CAPS: PlaneCaps<'static> = PlaneCaps {
        possible_crtcs: 0b01,
        formats: FORMATS,
        scale: None,
    };

    fn state(w: u32, h: u32, dst: DstRect) -> PlaneState {
        PlaneState {
            crtc: Some(ObjId(2)),
            fb: Some(fb(w, h)),
            src: SrcRect {
                x: 0,
                y: 0,
                w: w << 16,
                h: h << 16,
            },
            dst,
        }
    }

    fn on() -> CrtcState {
        CrtcState {
            active: true,
            mode: Some(m1080()),
        }
    }

    #[test]
    fn modes() {
        let m = m1080();
        assert_eq!(m.validate(), Ok(()));
        assert_eq!(m.refresh_hz(), 60);
        assert_eq!(m.refresh_mhz(), 60_000);
        assert_eq!(m.frame_ns(), 16_666_666);
        let i = m.to_info();
        assert_eq!(&i.name[..9], b"1920x1080");
        assert_eq!(i.vrefresh, 60);
        assert_eq!(Mode::from_info(&i), m);

        let mut b = m;
        b.hsync_start = 1900;
        assert_eq!(b.validate(), Err(ModeError::Order));
        b = m;
        b.clock_khz = 0;
        assert_eq!(b.validate(), Err(ModeError::Empty));
        b = m;
        b.flags = DRM_MODE_FLAG_INTERLACE;
        assert_eq!(b.validate(), Err(ModeError::Unsupported));
    }

    #[test]
    fn a_full_screen_plane() {
        let st = state(
            1920,
            1080,
            DstRect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
        );
        let v = check_plane(&CAPS, &st, Some((0, &on()))).unwrap();
        assert!(v.shown);
        assert_eq!(v.dst, st.dst);
        assert_eq!(v.src, st.src);
    }

    #[test]
    fn planes_off_and_half_bound() {
        let off = PlaneState::default();
        assert!(!check_plane(&CAPS, &off, None).unwrap().shown);
        let mut st = state(
            64,
            64,
            DstRect {
                x: 0,
                y: 0,
                w: 64,
                h: 64,
            },
        );
        st.crtc = None;
        assert_eq!(check_plane(&CAPS, &st, None), Err(CheckError::HalfBound));
        st.crtc = Some(ObjId(2));
        st.fb = None;
        assert_eq!(
            check_plane(&CAPS, &st, Some((0, &on()))),
            Err(CheckError::HalfBound)
        );
    }

    #[test]
    fn routing_format_and_activity() {
        let st = state(
            64,
            64,
            DstRect {
                x: 0,
                y: 0,
                w: 64,
                h: 64,
            },
        );
        assert_eq!(
            check_plane(&CAPS, &st, Some((1, &on()))),
            Err(CheckError::CrtcNotPossible)
        );
        let idle = CrtcState {
            active: false,
            mode: Some(m1080()),
        };
        assert_eq!(
            check_plane(&CAPS, &st, Some((0, &idle))),
            Err(CheckError::CrtcInactive)
        );
        let mut bad = st;
        bad.fb = Some(FbInfo {
            format: format::NV12,
            ..fb(64, 64)
        });
        assert_eq!(
            check_plane(&CAPS, &bad, Some((0, &on()))),
            Err(CheckError::FormatUnsupported)
        );
    }

    #[test]
    fn sources_stay_in_the_framebuffer() {
        let mut st = state(
            64,
            64,
            DstRect {
                x: 0,
                y: 0,
                w: 32,
                h: 32,
            },
        );
        st.src = SrcRect {
            x: 33 << 16,
            y: 0,
            w: 32 << 16,
            h: 32 << 16,
        };
        assert_eq!(
            check_plane(&CAPS, &st, Some((0, &on()))),
            Err(CheckError::BadSource)
        );
        st.src = SrcRect {
            x: 0,
            y: 0,
            w: 0,
            h: 32 << 16,
        };
        assert_eq!(
            check_plane(&CAPS, &st, Some((0, &on()))),
            Err(CheckError::BadSource)
        );
        st.src = SrcRect {
            x: 32 << 16,
            y: 32 << 16,
            w: 32 << 16,
            h: 32 << 16,
        };
        assert!(check_plane(&CAPS, &st, Some((0, &on()))).is_ok());
    }

    #[test]
    fn scaling_is_checked_against_the_plane() {
        let st = state(
            100,
            100,
            DstRect {
                x: 0,
                y: 0,
                w: 50,
                h: 50,
            },
        );
        assert_eq!(
            check_plane(&CAPS, &st, Some((0, &on()))),
            Err(CheckError::Scaling)
        );
        let scaler = PlaneCaps {
            scale: Some((1 << 15, 4 << 16)),
            ..CAPS
        };
        assert!(check_plane(&scaler, &st, Some((0, &on()))).is_ok());
        let tiny = state(
            100,
            100,
            DstRect {
                x: 0,
                y: 0,
                w: 400,
                h: 400,
            },
        );
        assert_eq!(
            check_plane(&scaler, &tiny, Some((0, &on()))),
            Err(CheckError::Scaling)
        );
    }

    #[test]
    fn clipping_moves_the_source_with_the_edge() {
        // A 200x100 buffer shown at x=-50, so its left quarter is cut.
        let st = state(
            200,
            100,
            DstRect {
                x: -50,
                y: 0,
                w: 200,
                h: 100,
            },
        );
        let v = check_plane(&CAPS, &st, Some((0, &on()))).unwrap();
        assert!(v.shown);
        assert_eq!(
            v.dst,
            DstRect {
                x: 0,
                y: 0,
                w: 150,
                h: 100
            }
        );
        assert_eq!(
            v.src,
            SrcRect {
                x: 50 << 16,
                y: 0,
                w: 150 << 16,
                h: 100 << 16
            }
        );
        // Past the right edge too.
        let st = state(
            200,
            100,
            DstRect {
                x: 1820,
                y: 0,
                w: 200,
                h: 100,
            },
        );
        let v = check_plane(&CAPS, &st, Some((0, &on()))).unwrap();
        assert_eq!(v.dst.w, 100);
        assert_eq!(v.src.w, 100 << 16);
        // Wholly off screen.
        let st = state(
            64,
            64,
            DstRect {
                x: 5000,
                y: 0,
                w: 64,
                h: 64,
            },
        );
        assert!(!check_plane(&CAPS, &st, Some((0, &on()))).unwrap().shown);
    }

    #[test]
    fn crtcs() {
        assert_eq!(
            check_crtc(&CrtcState {
                active: true,
                mode: None
            }),
            Err(CheckError::ModeMismatch)
        );
        assert_eq!(check_crtc(&on()), Ok(()));
        assert_eq!(
            check_crtc(&CrtcState {
                active: false,
                mode: None
            }),
            Ok(())
        );
        let mut bad = m1080();
        bad.vtotal = 1000;
        assert_eq!(
            check_crtc(&CrtcState {
                active: true,
                mode: Some(bad)
            }),
            Err(CheckError::Mode(ModeError::Order))
        );
    }

    #[test]
    fn connector_routing() {
        assert_eq!(
            check_routing(&[(0b11, Some(0)), (0b11, Some(1)), (0b11, None)]),
            Ok(())
        );
        assert_eq!(
            check_routing(&[(0b01, Some(1))]),
            Err(CheckError::ConnectorCrtc)
        );
        assert_eq!(
            check_routing(&[(0b11, Some(0)), (0b11, Some(0))]),
            Err(CheckError::CloneUnsupported)
        );
    }
}
