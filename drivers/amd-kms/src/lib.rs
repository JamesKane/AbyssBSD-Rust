// SPDX-License-Identifier: BSD-2-Clause
//! Kernel mode setting over a Radeon display the firmware left running
//! (stage R1 of docs/GRAPHICS.md §16): one display controller, one primary
//! plane, one connector, and the one mode the firmware set.
//!
//! The display is adopted as it is: its timing, format, pitch and
//! watermarks are never written. What changes is the address of the
//! surface it scans out. Clients' buffers are in system memory (the buffer
//! objects of `sys/drm` are cached system pages), which the display cannot
//! read before the memory stage, so a commit copies the pixels into the
//! back one of two surfaces in video memory through the PCI aperture and
//! flips to it at the next vertical update: nothing tears. When nothing is
//! shown, the display returns to the firmware's surface, the first bytes of
//! video memory, which is the console's.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::drm::{
    DRM_MODE_CONNECTOR_UNKNOWN, DRM_MODE_ENCODER_TMDS, DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_NVSYNC,
    DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PVSYNC, DRM_MODE_TYPE_DRIVER, DRM_MODE_TYPE_PREFERRED,
};
use abi::errno::Errno;
use drm::kms::{
    Connection, ConnectorHw, CrtcHw, CrtcUpdate, EncoderHw, Kms, KmsLimits, PlaneHw, PlaneKind,
    PlaneUpdate,
};
use drm::{DriverInfo, DrmDriver, DumbLimits, KmsBuilder};
use drm_fourcc::{Fourcc, format};
use gop_kms::{FrameMemory, GemPixels, Rect, copy_region};
use kms_check::Mode;
use sync::SpinLock;

/// The display and the video memory it reads, as the card needs them.
pub trait Display: Send + Sync {
    /// Writes `data` at byte `offset` of video memory, through the
    /// aperture; a write that does not fit is cut.
    fn write(&self, offset: usize, data: &[u8]);
    /// Makes what was written visible to the display.
    fn flush(&self);
    /// Starts showing the surface at byte `offset` of video memory, from
    /// the next vertical update.
    fn show(&self, offset: usize);
    /// Whether a [`Display::show`] is still waiting for its update.
    fn pending(&self) -> bool;
    /// Waits `us` microseconds.
    fn delay_us(&self, us: u32);
    /// How many bytes of video memory the card may use from the start: the
    /// aperture's size, less what the firmware's display microcontroller
    /// still holds at the top.
    fn usable(&self) -> usize;
}

/// The size of the firmware's picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Width in pixels (also the pitch).
    pub width: u32,
    /// Height in lines.
    pub height: u32,
}

/// Why there is no card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A size that cannot be shown.
    Size,
}

/// The unit the surfaces are laid out in: each starts a whole number of
/// these past the last, which keeps them clear of the firmware's surface
/// at 0 and of each other.
const STEP: usize = 16 << 20;

/// Video memory the card uses for the two surfaces it draws on, past the
/// firmware's surface at 0 and well below the top two mebibytes (which the
/// platform's display microcontroller still uses), for a picture of
/// `bytes`. The first starts after the firmware's, the second after the
/// first.
fn surfaces(bytes: usize) -> (usize, usize) {
    let step = bytes.div_ceil(STEP).max(1) * STEP;
    (step, 2 * step)
}

/// The longest a flip is waited for: several frames.
const SETTLE_US: u32 = 200_000;

/// Which surface the display shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Front {
    /// The firmware's, the console's.
    Console,
    A,
    B,
}

impl Front {
    /// Where it starts in video memory, for surfaces at `(a, b)`.
    fn offset(self, (a, b): (usize, usize)) -> usize {
        match self {
            Self::Console => 0,
            Self::A => a,
            Self::B => b,
        }
    }

    /// The surface to draw the next picture on.
    fn back(self) -> Self {
        if self == Self::A { Self::B } else { Self::A }
    }
}

/// Writes pixels at one surface, forcing the unused byte of each to
/// opaque: the display's format has an alpha channel, which a client's
/// `XRGB` buffer leaves unspecified.
struct Opaque<'a> {
    display: &'a dyn Display,
    base: usize,
}

impl FrameMemory for Opaque<'_> {
    fn write(&self, offset: usize, data: &[u8]) {
        let mut buf = [0u8; 1024];
        let mut at = offset;
        for part in data.chunks(buf.len()) {
            let out = &mut buf[..part.len()];
            out.copy_from_slice(part);
            // Little-endian words: alpha is the fourth byte of each pixel.
            for px in out.as_chunks_mut::<4>().0 {
                px[3] = 0xff;
            }
            self.display.write(self.base + at, out);
            at += part.len();
        }
    }
}

/// The card's shared parts.
struct Screen {
    geometry: Geometry,
    /// Where the two surfaces to draw on start.
    at: (usize, usize),
    display: Arc<dyn Display>,
    front: SpinLock<Front>,
}

impl Screen {
    /// Waits for a pending flip, so the surface about to be drawn on is no
    /// longer scanned out.
    fn settle(&self) {
        let mut waited = 0;
        while self.display.pending() && waited < SETTLE_US {
            self.display.delay_us(100);
            waited += 100;
        }
    }

    /// Shows the firmware's surface again.
    fn restore(&self) {
        let mut front = self.front.lock();
        if *front != Front::Console {
            self.settle();
            self.display.show(0);
            *front = Front::Console;
        }
    }
}

/// The mode the firmware set. A 1920x1080 picture at 60 hertz has the
/// standard timing (the registers' totals are 2200 by 1125); any other
/// size has no blanking, as a framebuffer card's mode does.
fn mode_of(g: &Geometry) -> Mode {
    let (w, h) = (
        u16::try_from(g.width).unwrap_or(u16::MAX),
        u16::try_from(g.height).unwrap_or(u16::MAX),
    );
    let kind = DRM_MODE_TYPE_PREFERRED | DRM_MODE_TYPE_DRIVER;
    if (g.width, g.height) == (1920, 1080) {
        return Mode {
            clock_khz: 148_500,
            hdisplay: 1920,
            hsync_start: 2008,
            hsync_end: 2052,
            htotal: 2200,
            vdisplay: 1080,
            vsync_start: 1084,
            vsync_end: 1089,
            vtotal: 1125,
            flags: DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC,
            kind,
            ..Mode::default()
        };
    }
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
        flags: DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC,
        kind,
        ..Mode::default()
    }
}

struct Crtc(Arc<Screen>);

impl CrtcHw for Crtc {
    /// Only the mode the firmware set can be shown: its timing is not
    /// reprogrammed.
    fn check(&self, u: &CrtcUpdate<'_>) -> Result<(), Errno> {
        let Some(m) = u.new.mode.as_ref().filter(|_| u.new.active) else {
            return Ok(());
        };
        let want = mode_of(&self.0.geometry);
        if (m.hdisplay, m.vdisplay, m.htotal, m.vtotal, m.clock_khz)
            == (
                want.hdisplay,
                want.vdisplay,
                want.htotal,
                want.vtotal,
                want.clock_khz,
            )
        {
            Ok(())
        } else {
            Err(Errno::EINVAL)
        }
    }
}

struct Encoder;
impl EncoderHw for Encoder {
    fn kind(&self) -> u32 {
        DRM_MODE_ENCODER_TMDS
    }
}

struct Connector(Arc<Screen>);

impl ConnectorHw for Connector {
    fn kind(&self) -> u32 {
        DRM_MODE_CONNECTOR_UNKNOWN
    }

    fn detect(&self) -> Connection {
        Connection::Connected
    }

    fn modes(&self) -> Vec<Mode> {
        alloc::vec![mode_of(&self.0.geometry)]
    }
}

struct Plane(Arc<Screen>);

const FORMATS: &[Fourcc] = &[format::XRGB8888, format::ARGB8888];

impl PlaneHw for Plane {
    fn formats(&self) -> &[Fourcc] {
        FORMATS
    }

    /// The whole screen only: the picture is drawn into a surface the
    /// display then shows entire, and the pixels of the part a smaller
    /// plane leaves out would have to come from the one on show.
    fn check(&self, u: &PlaneUpdate<'_>) -> Result<(), Errno> {
        let g = self.0.geometry;
        if u.fb.is_some()
            && u.visible.shown
            && (
                u.visible.dst.x,
                u.visible.dst.y,
                u.visible.dst.w,
                u.visible.dst.h,
            ) != (0, 0, g.width, g.height)
        {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    fn commit(&self, u: &PlaneUpdate<'_>) {
        let s = &self.0;
        let (Some(fb), true) = (u.fb, u.visible.shown) else {
            // Nothing to show: the console's surface.
            s.restore();
            return;
        };
        let Some(gem) = fb.gems[0].as_ref() else {
            return;
        };
        let g = s.geometry;
        let v = u.visible;
        let mut front = s.front.lock();
        s.settle();
        let back = front.back();
        let mem = Opaque {
            display: &*s.display,
            base: back.offset(s.at),
        };
        let copied = copy_region(
            &GemPixels(gem),
            u64::from(fb.offsets[0]),
            u64::from(fb.pitches[0]),
            Rect {
                x: v.src.x >> 16,
                y: v.src.y >> 16,
                w: g.width,
                h: g.height,
            },
            (0, 0),
            usize::try_from(g.width).unwrap_or(0) * 4,
            &mem,
        );
        if !copied {
            // Part of the buffer could not be read: keep what is shown.
            return;
        }
        s.display.flush();
        s.display.show(back.offset(s.at));
        *front = back;
    }
}

/// The card: a display over the surface the firmware left.
pub struct AmdKms {
    kms: Kms,
    screen: Arc<Screen>,
}

impl core::fmt::Debug for AmdKms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "AmdKms")
    }
}

impl AmdKms {
    /// Shows the firmware's surface again (the card is going away, or the
    /// kernel has panicked and the console must be seen).
    pub fn restore(&self) {
        self.screen.restore();
    }
}

/// Makes the card for the display `display`, showing a picture of size `g`.
///
/// # Errors
///
/// [`Error::Size`] for a size with no area, or one whose surfaces do not
/// fit in the video memory the card may use.
pub fn create(g: Geometry, display: Arc<dyn Display>) -> Result<Arc<AmdKms>, Error> {
    let bytes =
        usize::try_from(u64::from(g.width) * u64::from(g.height) * 4).map_err(|_| Error::Size)?;
    let at = surfaces(bytes);
    // Both surfaces must be in the aperture, below the top two mebibytes.
    if g.width == 0
        || g.height == 0
        || at
            .1
            .checked_add(bytes)
            .is_none_or(|end| end > display.usable())
    {
        return Err(Error::Size);
    }
    let screen = Arc::new(Screen {
        geometry: g,
        at,
        display,
        front: SpinLock::new(Front::Console),
    });
    let mut b = KmsBuilder::new(KmsLimits {
        min_width: 1,
        max_width: g.width,
        min_height: 1,
        max_height: g.height,
    });
    let c = b.crtc(Arc::new(Crtc(screen.clone())));
    b.plane(Arc::new(Plane(screen.clone())), PlaneKind::Primary, 1 << c);
    let e = b.encoder(Arc::new(Encoder), 1 << c, 0);
    b.connector(Arc::new(Connector(screen.clone())), &[e]);
    let kms = b.build().map_err(|_| Error::Size)?;
    Ok(Arc::new(AmdKms { kms, screen }))
}

impl DrmDriver for AmdKms {
    fn info(&self) -> DriverInfo {
        // Not "amdgpu": there is no render side yet (GRAPHICS.md §16.3).
        DriverInfo {
            name: "amd-display",
            desc: "AMD display, firmware mode",
            date: "20261124",
            major: 0,
            minor: 1,
            patch: 0,
        }
    }

    fn kms(&self) -> Option<&Kms> {
        Some(&self.kms)
    }

    fn dumb(&self) -> Option<DumbLimits> {
        Some(DumbLimits::default())
    }

    /// The display goes back to the console's surface at once: no lock
    /// (the one that guards which surface is on show may be held by a
    /// stopped processor), no wait for a flip, no flush. Two register
    /// writes, which the display takes at its next update.
    fn panic_restore(&self) {
        self.screen.display.show(0);
    }
}

#[cfg(test)]
mod tests;
