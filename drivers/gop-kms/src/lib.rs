// SPDX-License-Identifier: BSD-2-Clause
//! Kernel mode setting over the firmware's framebuffer: one display
//! controller, one primary plane, one connector, and the one mode the
//! firmware left running.
//!
//! This gives every machine that boots through UEFI (and QEMU) a
//! `/dev/dri/card<N>` a compositor can use, before any native display
//! driver exists, as Linux's simple framebuffer driver does. There is no
//! scanout engine: the framebuffer is the screen, so showing a buffer
//! means copying its pixels there. A commit does that for the part of the
//! plane that shows; a flip to the buffer already shown, or a dirty call,
//! copies it again, because that is how the screen learns the buffer
//! changed. A native driver for the same output adopts the display later
//! and this one goes away.
//!
//! Behavior reference: `docs/GRAPHICS.md` §7.5.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::sync::Arc;

use abi::drm::{
    DRM_MODE_CONNECTOR_UNKNOWN, DRM_MODE_ENCODER_NONE, DRM_MODE_TYPE_DRIVER,
    DRM_MODE_TYPE_PREFERRED,
};
use drm::kms::{
    Connection, ConnectorHw, CrtcHw, EncoderHw, Kms, KmsLimits, PlaneHw, PlaneKind, PlaneUpdate,
};
use drm::{DriverInfo, DrmDriver, DumbLimits, KmsBuilder};
use drm_fourcc::{Fourcc, format};
use kms_check::Mode;

/// The framebuffer's memory, written without being read (it is write
/// combining memory, slow to read).
pub trait FrameMemory: Send + Sync {
    /// Writes `data` at byte `offset`; a write that does not fit is cut.
    fn write(&self, offset: usize, data: &[u8]);
}

/// How the firmware's framebuffer lays out a pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    /// Bytes red, green, blue, unused.
    Rgbx,
    /// Bytes blue, green, red, unused.
    Bgrx,
    /// Channels at arbitrary bit positions.
    Masks { red: u32, green: u32, blue: u32 },
}

/// The framebuffer as the loader found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    /// Bytes from one line to the next.
    pub stride_bytes: u32,
    pub layout: PixelLayout,
}

/// Why there is no card for a framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A pixel layout no standard format matches.
    Layout,
    /// A size, or a stride that is not whole pixels, that cannot be shown.
    Size,
}

/// The standard format of a layout: what a buffer must be in to be
/// copied as it is.
#[must_use]
pub fn native_format(l: PixelLayout) -> Option<Fourcc> {
    match l {
        PixelLayout::Bgrx
        | PixelLayout::Masks {
            red: 0x00ff_0000,
            green: 0x0000_ff00,
            blue: 0x0000_00ff,
        } => Some(format::XRGB8888),
        PixelLayout::Rgbx
        | PixelLayout::Masks {
            red: 0x0000_00ff,
            green: 0x0000_ff00,
            blue: 0x00ff_0000,
        } => Some(format::XBGR8888),
        PixelLayout::Masks { .. } => None,
    }
}

const BGR_FORMATS: &[Fourcc] = &[format::XRGB8888, format::ARGB8888];
const RGB_FORMATS: &[Fourcc] = &[format::XBGR8888, format::ABGR8888];

/// The screen, shared by the objects.
struct Screen {
    geometry: Geometry,
    formats: &'static [Fourcc],
    mem: &'static dyn FrameMemory,
}

struct Crtc;
impl CrtcHw for Crtc {}

struct Plane(Arc<Screen>);

struct Encoder;
impl EncoderHw for Encoder {
    fn kind(&self) -> u32 {
        DRM_MODE_ENCODER_NONE
    }
}

struct Connector(Arc<Screen>);

/// The one mode: the firmware's size at 60 hertz, with no blanking
/// (there is no scan to time, only the pace flips complete at).
fn mode_of(g: &Geometry) -> Mode {
    let (w, h) = (
        u16::try_from(g.width).unwrap_or(u16::MAX),
        u16::try_from(g.height).unwrap_or(u16::MAX),
    );
    let khz = u64::from(g.width) * u64::from(g.height) * 60 / 1000;
    Mode {
        clock_khz: u32::try_from(khz).unwrap_or(u32::MAX),
        hdisplay: w,
        hsync_start: w,
        hsync_end: w,
        htotal: w,
        vdisplay: h,
        vsync_start: h,
        vsync_end: h,
        vtotal: h,
        kind: DRM_MODE_TYPE_PREFERRED | DRM_MODE_TYPE_DRIVER,
        ..Mode::default()
    }
}

impl ConnectorHw for Connector {
    fn kind(&self) -> u32 {
        DRM_MODE_CONNECTOR_UNKNOWN
    }

    fn detect(&self) -> Connection {
        Connection::Connected
    }

    fn modes(&self) -> alloc::vec::Vec<Mode> {
        alloc::vec![mode_of(&self.0.geometry)]
    }
}

/// Source pixels, a page's worth at a time.
pub trait Pixels {
    /// Calls `f` with the bytes from `offset` to the end of the page it
    /// is in; `false` if there is no such page.
    fn chunk(&self, offset: u64, f: &mut dyn FnMut(&[u8])) -> bool;
}

/// A buffer object's pixels, for [`copy_region`].
#[derive(Debug)]
pub struct GemPixels<'a>(pub &'a drm::gem::Gem);

impl Pixels for GemPixels<'_> {
    fn chunk(&self, offset: u64, f: &mut dyn FnMut(&[u8])) -> bool {
        const PAGE: u64 = 4096;
        let (page, within) = (offset / PAGE, usize::try_from(offset % PAGE).unwrap_or(0));
        self.0.with_page(page, &mut |p| f(&p[within..])).is_ok()
    }
}

/// A rectangle of 32-bit pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Copies the pixels of `src` (a buffer of `pitch` bytes a line, whose
/// first line is at byte `base`) in rectangle `from` to rectangle `to`'s
/// top left on a screen whose lines are `stride` bytes. Returns whether
/// every line was copied.
pub fn copy_region(
    src: &dyn Pixels,
    base: u64,
    pitch: u64,
    from: Rect,
    to: (u32, u32),
    stride: usize,
    mem: &dyn FrameMemory,
) -> bool {
    let mut ok = true;
    let px = |v: u32| usize::try_from(v).unwrap_or(usize::MAX);
    for row in 0..from.h {
        let mut at = base + (u64::from(from.y) + u64::from(row)) * pitch + u64::from(from.x) * 4;
        let mut dst = (px(to.1) + px(row)).saturating_mul(stride) + px(to.0) * 4;
        let mut left = px(from.w) * 4;
        while left > 0 {
            let mut took = 0;
            let got = src.chunk(at, &mut |c| {
                let n = c.len().min(left);
                mem.write(dst, &c[..n]);
                took = n;
            });
            if !got || took == 0 {
                ok = false;
                break;
            }
            at += took as u64;
            dst += took;
            left -= took;
        }
    }
    ok
}

impl PlaneHw for Plane {
    fn formats(&self) -> &[Fourcc] {
        self.0.formats
    }

    fn commit(&self, u: &PlaneUpdate<'_>) {
        let (Some(fb), true) = (u.fb, u.visible.shown) else {
            return;
        };
        let Some(gem) = fb.gems[0].as_ref() else {
            return;
        };
        let v = u.visible;
        let from = Rect {
            x: v.src.x >> 16,
            y: v.src.y >> 16,
            w: v.dst.w,
            h: v.dst.h,
        };
        let to = (
            u32::try_from(v.dst.x).unwrap_or(0),
            u32::try_from(v.dst.y).unwrap_or(0),
        );
        let stride = usize::try_from(self.0.geometry.stride_bytes).unwrap_or(0);
        copy_region(
            &GemPixels(gem),
            u64::from(fb.offsets[0]),
            u64::from(fb.pitches[0]),
            from,
            to,
            stride,
            self.0.mem,
        );
    }
}

/// The card: a display over the firmware framebuffer.
pub struct GopKms {
    kms: Kms,
}

impl core::fmt::Debug for GopKms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "GopKms")
    }
}

/// Makes the card for a framebuffer.
///
/// # Errors
///
/// [`Error::Layout`] for pixels no standard format describes;
/// [`Error::Size`] for a framebuffer with no area, a stride that is not
/// whole pixels, or a line shorter than its pixels.
pub fn create(g: Geometry, mem: &'static dyn FrameMemory) -> Result<Arc<GopKms>, Error> {
    let native = native_format(g.layout).ok_or(Error::Layout)?;
    if g.width == 0
        || g.height == 0
        || !g.stride_bytes.is_multiple_of(4)
        || u64::from(g.stride_bytes) < u64::from(g.width) * 4
    {
        return Err(Error::Size);
    }
    let formats = if native == format::XRGB8888 {
        BGR_FORMATS
    } else {
        RGB_FORMATS
    };
    let screen = Arc::new(Screen {
        geometry: g,
        formats,
        mem,
    });
    let mut b = KmsBuilder::new(KmsLimits {
        min_width: 1,
        max_width: g.width,
        min_height: 1,
        max_height: g.height,
    });
    let c = b.crtc(Arc::new(Crtc));
    b.plane(Arc::new(Plane(screen.clone())), PlaneKind::Primary, 1 << c);
    let e = b.encoder(Arc::new(Encoder), 1 << c, 0);
    b.connector(Arc::new(Connector(screen)), &[e]);
    let kms = b.build().map_err(|_| Error::Size)?;
    Ok(Arc::new(GopKms { kms }))
}

impl DrmDriver for GopKms {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "simpledrm",
            desc: "Firmware framebuffer",
            date: "20261122",
            major: 1,
            minor: 0,
            patch: 0,
        }
    }

    fn kms(&self) -> Option<&Kms> {
        Some(&self.kms)
    }

    fn firmware_framebuffer(&self) -> bool {
        true
    }

    fn dumb(&self) -> Option<DumbLimits> {
        Some(DumbLimits::default())
    }
}

#[cfg(test)]
mod tests;
