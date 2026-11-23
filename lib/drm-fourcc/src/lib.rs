// SPDX-License-Identifier: MIT
// Copyright 2011 Intel Corporation
// Copyright (c) 2026 The AbyssBSD Project
//! DRM pixel formats and framebuffer modifiers (`drm_fourcc.h`, MIT):
//! the four-character codes, how many planes and bytes per pixel each
//! has, and the validation `ADDFB2` applies to a framebuffer's pitches
//! and offsets.
//!
//! Only formats the drivers and their userland (Sway on `pixman`,
//! wlroots, Mesa) use are listed; the table grows with the drivers.

#![no_std]
#![forbid(unsafe_code)]

/// A pixel format's four-character code (`DRM_FORMAT_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fourcc(pub u32);

impl Fourcc {
    /// `fourcc_code(a, b, c, d)`.
    #[must_use]
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self((a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24))
    }

    /// Its four characters.
    #[must_use]
    pub const fn chars(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }
}

macro_rules! formats {
    ($($name:ident = $a:literal $b:literal $c:literal $d:literal;)*) => {
        $(
            #[doc = concat!("`DRM_FORMAT_", stringify!($name), "`.")]
            pub const $name: Fourcc = Fourcc::new($a, $b, $c, $d);
        )*
    };
}

/// The format constants, named as `DRM_FORMAT_*` less the prefix.
pub mod format {
    use super::Fourcc;
    formats! {
        C8 = b'C' b'8' b' ' b' ';
        R8 = b'R' b'8' b' ' b' ';
        XRGB4444 = b'X' b'R' b'1' b'2';
        ARGB4444 = b'A' b'R' b'1' b'2';
        XRGB1555 = b'X' b'R' b'1' b'5';
        ARGB1555 = b'A' b'R' b'1' b'5';
        RGB565 = b'R' b'G' b'1' b'6';
        BGR565 = b'B' b'G' b'1' b'6';
        RGB888 = b'R' b'G' b'2' b'4';
        BGR888 = b'B' b'G' b'2' b'4';
        XRGB8888 = b'X' b'R' b'2' b'4';
        XBGR8888 = b'X' b'B' b'2' b'4';
        RGBX8888 = b'R' b'X' b'2' b'4';
        BGRX8888 = b'B' b'X' b'2' b'4';
        ARGB8888 = b'A' b'R' b'2' b'4';
        ABGR8888 = b'A' b'B' b'2' b'4';
        RGBA8888 = b'R' b'A' b'2' b'4';
        BGRA8888 = b'B' b'A' b'2' b'4';
        XRGB2101010 = b'X' b'R' b'3' b'0';
        XBGR2101010 = b'X' b'B' b'3' b'0';
        ARGB2101010 = b'A' b'R' b'3' b'0';
        ABGR2101010 = b'A' b'B' b'3' b'0';
        XRGB16161616 = b'X' b'R' b'4' b'8';
        ARGB16161616 = b'A' b'R' b'4' b'8';
        XRGB16161616F = b'X' b'R' b'4' b'H';
        XBGR16161616F = b'X' b'B' b'4' b'H';
        ARGB16161616F = b'A' b'R' b'4' b'H';
        ABGR16161616F = b'A' b'B' b'4' b'H';
        YUYV = b'Y' b'U' b'Y' b'V';
        YVYU = b'Y' b'V' b'Y' b'U';
        UYVY = b'U' b'Y' b'V' b'Y';
        VYUY = b'V' b'Y' b'U' b'Y';
        NV12 = b'N' b'V' b'1' b'2';
        NV21 = b'N' b'V' b'2' b'1';
        NV16 = b'N' b'V' b'1' b'6';
        NV61 = b'N' b'V' b'6' b'1';
        P010 = b'P' b'0' b'1' b'0';
        YUV420 = b'Y' b'U' b'1' b'2';
        YVU420 = b'Y' b'V' b'1' b'2';
        YUV422 = b'Y' b'U' b'1' b'6';
        YUV444 = b'Y' b'U' b'2' b'4';
    }
}

/// What a format is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatInfo {
    pub format: Fourcc,
    /// Its name, as `DRM_FORMAT_` omits it.
    pub name: &'static str,
    /// Memory planes (1 to 3).
    pub planes: u8,
    /// Bytes per pixel of each plane, for the plane's own sampling grid
    /// (`cpp`; a chroma plane of NV12 holds a U/V pair, 2 bytes, per
    /// sample).
    pub cpp: [u8; 3],
    /// Horizontal and vertical subsampling of the chroma planes (and
    /// of packed YUV's pairs): 1, 2, or 4.
    pub hsub: u8,
    pub vsub: u8,
    pub has_alpha: bool,
    pub is_yuv: bool,
}

const fn rgb(format: Fourcc, name: &'static str, cpp: u8, has_alpha: bool) -> FormatInfo {
    FormatInfo {
        format,
        name,
        planes: 1,
        cpp: [cpp, 0, 0],
        hsub: 1,
        vsub: 1,
        has_alpha,
        is_yuv: false,
    }
}

const fn yuv(
    format: Fourcc,
    name: &'static str,
    planes: u8,
    cpp: [u8; 3],
    hsub: u8,
    vsub: u8,
) -> FormatInfo {
    FormatInfo {
        format,
        name,
        planes,
        cpp,
        hsub,
        vsub,
        has_alpha: false,
        is_yuv: true,
    }
}

/// Every format this table knows.
pub const FORMATS: &[FormatInfo] = &[
    rgb(format::C8, "C8", 1, false),
    rgb(format::R8, "R8", 1, false),
    rgb(format::XRGB4444, "XRGB4444", 2, false),
    rgb(format::ARGB4444, "ARGB4444", 2, true),
    rgb(format::XRGB1555, "XRGB1555", 2, false),
    rgb(format::ARGB1555, "ARGB1555", 2, true),
    rgb(format::RGB565, "RGB565", 2, false),
    rgb(format::BGR565, "BGR565", 2, false),
    rgb(format::RGB888, "RGB888", 3, false),
    rgb(format::BGR888, "BGR888", 3, false),
    rgb(format::XRGB8888, "XRGB8888", 4, false),
    rgb(format::XBGR8888, "XBGR8888", 4, false),
    rgb(format::RGBX8888, "RGBX8888", 4, false),
    rgb(format::BGRX8888, "BGRX8888", 4, false),
    rgb(format::ARGB8888, "ARGB8888", 4, true),
    rgb(format::ABGR8888, "ABGR8888", 4, true),
    rgb(format::RGBA8888, "RGBA8888", 4, true),
    rgb(format::BGRA8888, "BGRA8888", 4, true),
    rgb(format::XRGB2101010, "XRGB2101010", 4, false),
    rgb(format::XBGR2101010, "XBGR2101010", 4, false),
    rgb(format::ARGB2101010, "ARGB2101010", 4, true),
    rgb(format::ABGR2101010, "ABGR2101010", 4, true),
    rgb(format::XRGB16161616, "XRGB16161616", 8, false),
    rgb(format::ARGB16161616, "ARGB16161616", 8, true),
    rgb(format::XRGB16161616F, "XRGB16161616F", 8, false),
    rgb(format::XBGR16161616F, "XBGR16161616F", 8, false),
    rgb(format::ARGB16161616F, "ARGB16161616F", 8, true),
    rgb(format::ABGR16161616F, "ABGR16161616F", 8, true),
    yuv(format::YUYV, "YUYV", 1, [2, 0, 0], 2, 1),
    yuv(format::YVYU, "YVYU", 1, [2, 0, 0], 2, 1),
    yuv(format::UYVY, "UYVY", 1, [2, 0, 0], 2, 1),
    yuv(format::VYUY, "VYUY", 1, [2, 0, 0], 2, 1),
    yuv(format::NV12, "NV12", 2, [1, 2, 0], 2, 2),
    yuv(format::NV21, "NV21", 2, [1, 2, 0], 2, 2),
    yuv(format::NV16, "NV16", 2, [1, 2, 0], 2, 1),
    yuv(format::NV61, "NV61", 2, [1, 2, 0], 2, 1),
    yuv(format::P010, "P010", 2, [2, 4, 0], 2, 2),
    yuv(format::YUV420, "YUV420", 3, [1, 1, 1], 2, 2),
    yuv(format::YVU420, "YVU420", 3, [1, 1, 1], 2, 2),
    yuv(format::YUV422, "YUV422", 3, [1, 1, 1], 2, 1),
    yuv(format::YUV444, "YUV444", 3, [1, 1, 1], 1, 1),
];

/// The description of `format`, or `None` for one this table lacks.
#[must_use]
pub fn lookup(format: Fourcc) -> Option<&'static FormatInfo> {
    FORMATS.iter().find(|f| f.format == format)
}

impl FormatInfo {
    /// Width in pixels of plane `plane` for a framebuffer `width` wide.
    #[must_use]
    pub fn plane_width(&self, plane: usize, width: u32) -> u32 {
        if plane == 0 {
            width
        } else {
            width.div_ceil(u32::from(self.hsub))
        }
    }

    /// Height in lines of plane `plane`.
    #[must_use]
    pub fn plane_height(&self, plane: usize, height: u32) -> u32 {
        if plane == 0 {
            height
        } else {
            height.div_ceil(u32::from(self.vsub))
        }
    }

    /// The smallest pitch plane `plane` of a `width`-wide framebuffer
    /// can have, or `None` if the plane doesn't exist or it overflows.
    #[must_use]
    pub fn min_pitch(&self, plane: usize, width: u32) -> Option<u32> {
        if plane >= usize::from(self.planes) {
            return None;
        }
        let (w, cpp) = (self.plane_width(plane, width), u32::from(self.cpp[plane]));
        w.checked_mul(cpp)
    }
}

/// A framebuffer modifier (`DRM_FORMAT_MOD_*`): a vendor byte above 56
/// bits of vendor-defined layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Modifier(pub u64);

impl Modifier {
    /// `fourcc_mod_code(vendor, val)`.
    #[must_use]
    pub const fn new(vendor: u8, val: u64) -> Self {
        Self(((vendor as u64) << 56) | (val & ((1 << 56) - 1)))
    }

    /// The vendor byte.
    #[must_use]
    pub const fn vendor(self) -> u8 {
        (self.0 >> 56) as u8
    }
}

/// `DRM_FORMAT_MOD_LINEAR`: row-major, `pitch` bytes per line.
pub const MOD_LINEAR: Modifier = Modifier(0);
/// `DRM_FORMAT_MOD_INVALID`: "no modifier given", never valid in a
/// framebuffer that sets `DRM_MODE_FB_MODIFIERS`.
pub const MOD_INVALID: Modifier = Modifier::new(VENDOR_NONE, (1 << 56) - 1);
pub const VENDOR_NONE: u8 = 0;
pub const VENDOR_INTEL: u8 = 0x01;
pub const VENDOR_AMD: u8 = 0x02;
pub const VENDOR_NVIDIA: u8 = 0x03;
pub const VENDOR_QCOM: u8 = 0x05;
pub const VENDOR_ARM: u8 = 0x08;

/// What a framebuffer request (`ADDFB2`) names, to validate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbRequest {
    pub format: Fourcc,
    pub width: u32,
    pub height: u32,
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub handles: [u32; 4],
    pub modifier: Modifier,
}

/// Why a framebuffer request is refused (all map to `EINVAL`, except
/// `UnknownFormat`, which is too).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbError {
    UnknownFormat,
    ZeroSize,
    /// A plane the format has lacks a handle or pitch, or one it lacks
    /// has them set.
    PlaneMismatch,
    PitchTooSmall,
    /// The offset and the plane's bytes overflow, or the pitch is not
    /// aligned to the format's pixel size.
    Overflow,
    /// A linear framebuffer's pitch must be a multiple of its `cpp`.
    PitchUnaligned,
}

/// Validates a linear framebuffer request against `FormatInfo` and the
/// device's limits; returns each plane's size in bytes (pitch times
/// lines, past its offset).
///
/// # Errors
///
/// An [`FbError`] naming the first rule it breaks.
pub fn check_fb(req: &FbRequest, max_width: u32, max_height: u32) -> Result<[u64; 3], FbError> {
    let info = lookup(req.format).ok_or(FbError::UnknownFormat)?;
    if req.width == 0 || req.height == 0 || req.width > max_width || req.height > max_height {
        return Err(FbError::ZeroSize);
    }
    let mut sizes = [0u64; 3];
    #[allow(clippy::needless_range_loop, reason = "four parallel request arrays")]
    for plane in 0..4 {
        let present = plane < usize::from(info.planes);
        if present != (req.handles[plane] != 0) {
            return Err(FbError::PlaneMismatch);
        }
        if !present {
            if req.pitches[plane] != 0 || req.offsets[plane] != 0 {
                return Err(FbError::PlaneMismatch);
            }
            continue;
        }
        let min = info.min_pitch(plane, req.width).ok_or(FbError::Overflow)?;
        if req.pitches[plane] < min {
            return Err(FbError::PitchTooSmall);
        }
        if req.modifier == MOD_LINEAR
            && !req.pitches[plane].is_multiple_of(u32::from(info.cpp[plane]))
        {
            return Err(FbError::PitchUnaligned);
        }
        let lines = u64::from(info.plane_height(plane, req.height));
        let bytes = lines
            .checked_mul(u64::from(req.pitches[plane]))
            .and_then(|b| b.checked_add(u64::from(req.offsets[plane])))
            .ok_or(FbError::Overflow)?;
        sizes[plane] = bytes;
    }
    Ok(sizes)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_the_header() {
        assert_eq!(format::XRGB8888.0, 0x3432_5258);
        assert_eq!(format::ARGB8888.0, 0x3432_5241);
        assert_eq!(format::RGB565.0, 0x3631_4752);
        assert_eq!(format::NV12.0, 0x3231_564e);
        assert_eq!(format::XRGB8888.chars(), *b"XR24");
        assert_eq!(MOD_INVALID.0, 0x00ff_ffff_ffff_ffff);
        assert_eq!(Modifier::new(VENDOR_ARM, 1).vendor(), 8);
    }

    #[test]
    fn table_has_no_duplicates() {
        for (i, a) in FORMATS.iter().enumerate() {
            for b in &FORMATS[i + 1..] {
                assert_ne!(a.format, b.format, "{} vs {}", a.name, b.name);
            }
            assert!(!a.name.is_empty());
        }
    }

    fn rq(format: Fourcc, w: u32, h: u32) -> FbRequest {
        FbRequest {
            format,
            width: w,
            height: h,
            pitches: [0; 4],
            offsets: [0; 4],
            handles: [0; 4],
            modifier: MOD_LINEAR,
        }
    }

    #[test]
    fn xrgb_framebuffer() {
        let mut r = rq(format::XRGB8888, 1920, 1080);
        r.handles[0] = 1;
        r.pitches[0] = 1920 * 4;
        assert_eq!(check_fb(&r, 16384, 16384).unwrap()[0], 1920 * 4 * 1080);
        r.pitches[0] = 1920 * 4 - 4;
        assert_eq!(check_fb(&r, 16384, 16384), Err(FbError::PitchTooSmall));
        r.pitches[0] = 1920 * 4 + 2;
        assert_eq!(check_fb(&r, 16384, 16384), Err(FbError::PitchUnaligned));
    }

    #[test]
    fn nv12_has_two_planes() {
        let mut r = rq(format::NV12, 1280, 720);
        r.handles[0] = 1;
        r.handles[1] = 1;
        r.pitches[0] = 1280;
        r.pitches[1] = 1280;
        r.offsets[1] = 1280 * 720;
        let s = check_fb(&r, 8192, 8192).unwrap();
        assert_eq!(s[0], 1280 * 720);
        assert_eq!(s[1], 1280 * 360 + 1280 * 720);
        r.handles[2] = 1;
        assert_eq!(check_fb(&r, 8192, 8192), Err(FbError::PlaneMismatch));
    }

    #[test]
    fn rejects_unknown_and_empty() {
        let mut r = rq(Fourcc(0x1234_5678), 4, 4);
        assert_eq!(check_fb(&r, 64, 64), Err(FbError::UnknownFormat));
        r.format = format::XRGB8888;
        r.width = 0;
        assert_eq!(check_fb(&r, 64, 64), Err(FbError::ZeroSize));
        r.width = 65;
        assert_eq!(check_fb(&r, 64, 64), Err(FbError::ZeroSize));
    }

    #[test]
    fn yuyv_is_two_bytes_a_pixel() {
        let info = lookup(format::YUYV).unwrap();
        assert_eq!(info.min_pitch(0, 1280), Some(1280 * 2));
        assert_eq!(info.min_pitch(1, 1280), None);
    }
}
