// SPDX-License-Identifier: BSD-2-Clause
//! Reading a monitor's identification data (EDID 1.3 and 1.4): its size,
//! name and the timings of its detailed descriptors in the base block, and
//! the modes of its CTA-861 extension blocks (standard video codes and
//! detailed timings).
//!
//! The data is untrusted input from the monitor: every read is checked and
//! a malformed base block is an error, never a panic; an extension block
//! that is malformed gives no modes. DisplayID extensions are not read yet.
//! The sink's audio capabilities (the CTA-861 short audio descriptors and
//! speaker allocation) are read for the display drivers' audio endpoints.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod cta;

pub use cta::{Audio, FORMATS};

use alloc::string::String;
use alloc::vec::Vec;

use abi::drm::{
    DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_NVSYNC, DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PVSYNC,
    DRM_MODE_TYPE_DRIVER, DRM_MODE_TYPE_PREFERRED,
};
use kms_check::Mode;

/// A block is 128 bytes.
pub const BLOCK: usize = 128;

/// Why a block was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Fewer than 128 bytes.
    Short,
    /// The eight-byte header is not the EDID one.
    Header,
    /// The bytes do not sum to zero.
    Checksum,
}

/// What the base block says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edid {
    /// The monitor's name (descriptor `0xFC`), if it gives one.
    pub name: Option<String>,
    /// The image size in millimetres, 0 when it is a ratio or unknown.
    pub size_mm: (u32, u32),
    /// The detailed timings, the first of them preferred.
    pub modes: Vec<Mode>,
    /// The number of extension blocks that follow.
    pub extensions: u8,
    /// Bits per colour component of a digital input, if stated (EDID 1.4).
    pub bpc: Option<u8>,
    /// The manufacturer's id: bytes 8 and 9 as stored, read little-endian.
    pub manufacturer: u16,
    /// The product code (bytes 10 and 11, little-endian).
    pub product: u16,
    /// What the first CTA-861 extension says about audio.
    pub audio: Audio,
}

const HEADER: [u8; 8] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];

/// Parses the base block, and the CTA-861 extension blocks that follow it in
/// `data` (those present; a monitor's extension count may say more than was
/// read).
///
/// # Errors
///
/// [`Error`] for a block that is short, has the wrong header or does not
/// sum to zero.
pub fn parse(data: &[u8]) -> Result<Edid, Error> {
    let b = data.get(..BLOCK).ok_or(Error::Short)?;
    if b[..8] != HEADER {
        return Err(Error::Header);
    }
    if b.iter().fold(0u8, |a, x| a.wrapping_add(*x)) != 0 {
        return Err(Error::Checksum);
    }
    // Image size: centimetres, or an aspect ratio when either is zero.
    let size_mm = if b[21] != 0 && b[22] != 0 {
        (u32::from(b[21]) * 10, u32::from(b[22]) * 10)
    } else {
        (0, 0)
    };
    // EDID 1.4 digital input: bits 6:4 of byte 20 give the depth.
    let bpc = (b[20] & 0x80 != 0 && b[18] == 1 && b[19] >= 4)
        .then(|| match (b[20] >> 4) & 7 {
            1 => Some(6),
            2 => Some(8),
            3 => Some(10),
            4 => Some(12),
            5 => Some(14),
            6 => Some(16),
            _ => None,
        })
        .flatten();
    let mut modes = Vec::new();
    let mut name = None;
    for d in 0..4 {
        let at = 54 + 18 * d;
        let desc = &b[at..at + 18];
        let clock = u32::from(u16::from_le_bytes([desc[0], desc[1]]));
        if clock != 0 {
            if let Some(mut m) = detailed(desc, clock) {
                if modes.is_empty() {
                    m.kind |= DRM_MODE_TYPE_PREFERRED;
                }
                modes.push(m);
            }
        } else if desc[3] == 0xFC {
            let text: String = desc[5..18]
                .iter()
                .take_while(|c| **c != b'\n' && **c != 0)
                .map(|c| {
                    if (0x20..0x7F).contains(c) {
                        char::from(*c)
                    } else {
                        '?'
                    }
                })
                .collect();
            name = Some(String::from(text.trim_end()));
        }
    }
    // Extension blocks: their modes follow the base block's, without those
    // the monitor gave twice (the same size at the same rate).
    let mut audio = None;
    for ext in data[BLOCK..].as_chunks::<BLOCK>().0.iter().take(3) {
        if audio.is_none() {
            audio = cta::audio(ext);
        }
        for m in cta::modes(ext) {
            let seen = modes.iter().any(|o| {
                o.hdisplay == m.hdisplay
                    && o.vdisplay == m.vdisplay
                    && cta::refresh_hz(o) == cta::refresh_hz(&m)
            });
            if !seen {
                modes.push(m);
            }
        }
    }
    Ok(Edid {
        name,
        size_mm,
        modes,
        extensions: b[126],
        bpc,
        manufacturer: u16::from_le_bytes([b[8], b[9]]),
        product: u16::from_le_bytes([b[10], b[11]]),
        audio: audio.unwrap_or_default(),
    })
}

/// A detailed timing descriptor (EDID 1.3 §3.10.2). `None` for one whose
/// numbers cannot be a mode (zero or out of order).
fn detailed(d: &[u8], clock_10khz: u32) -> Option<Mode> {
    let hi = |b: u8| u32::from(b >> 4);
    let lo = |b: u8| u32::from(b & 0xF);
    let hactive = u32::from(d[2]) | hi(d[4]) << 8;
    let hblank = u32::from(d[3]) | lo(d[4]) << 8;
    let vactive = u32::from(d[5]) | hi(d[7]) << 8;
    let vblank = u32::from(d[6]) | lo(d[7]) << 8;
    let hfp = u32::from(d[8]) | u32::from(d[11] >> 6) << 8;
    let hsw = u32::from(d[9]) | u32::from((d[11] >> 4) & 3) << 8;
    let vfp = hi(d[10]) | u32::from((d[11] >> 2) & 3) << 4;
    let vsw = lo(d[10]) | u32::from(d[11] & 3) << 4;
    if hactive == 0 || vactive == 0 || hfp + hsw > hblank || vfp + vsw > vblank {
        return None;
    }
    let f = d[17];
    // Only digital separate sync carries polarities; the rest is
    // negative.
    let (hpos, vpos) = if f & 0x18 == 0x18 {
        (f & 2 != 0, f & 4 != 0)
    } else {
        (false, false)
    };
    let mut flags = if hpos {
        DRM_MODE_FLAG_PHSYNC
    } else {
        DRM_MODE_FLAG_NHSYNC
    };
    flags |= if vpos {
        DRM_MODE_FLAG_PVSYNC
    } else {
        DRM_MODE_FLAG_NVSYNC
    };
    if f & 0x80 != 0 {
        // Interlaced: the specification's totals are per field.
        flags |= abi::drm::DRM_MODE_FLAG_INTERLACE;
    }
    let u = |v: u32| u16::try_from(v).ok();
    Some(Mode {
        clock_khz: clock_10khz * 10,
        hdisplay: u(hactive)?,
        hsync_start: u(hactive + hfp)?,
        hsync_end: u(hactive + hfp + hsw)?,
        htotal: u(hactive + hblank)?,
        vdisplay: u(vactive)?,
        vsync_start: u(vactive + vfp)?,
        vsync_end: u(vactive + vfp + vsw)?,
        vtotal: u(vactive + vblank)?,
        flags,
        kind: DRM_MODE_TYPE_DRIVER,
        ..Mode::default()
    })
}

/// The modes to offer when a monitor gives none: the common ones up to
/// 1920 by 1080, with 1080p at 60 hertz preferred (what the specification
/// says the vendor driver does).
#[must_use]
pub fn fallback_modes() -> Vec<Mode> {
    let m = |clock_khz, h: [u16; 4], v: [u16; 4], pos: bool| {
        let sync = if pos {
            DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC
        } else {
            DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC
        };
        Mode {
            clock_khz,
            hdisplay: h[0],
            hsync_start: h[1],
            hsync_end: h[2],
            htotal: h[3],
            vdisplay: v[0],
            vsync_start: v[1],
            vsync_end: v[2],
            vtotal: v[3],
            flags: sync,
            kind: DRM_MODE_TYPE_DRIVER,
            ..Mode::default()
        }
    };
    let mut hd = m(
        148_500,
        [1920, 2008, 2052, 2200],
        [1080, 1084, 1089, 1125],
        true,
    );
    hd.kind |= DRM_MODE_TYPE_PREFERRED;
    alloc::vec![
        hd,
        m(74_250, [1280, 1390, 1430, 1650], [720, 725, 730, 750], true),
        m(
            65_000,
            [1024, 1048, 1184, 1344],
            [768, 771, 777, 806],
            false
        ),
        m(25_175, [640, 656, 752, 800], [480, 490, 492, 525], false),
    ]
}

#[cfg(test)]
mod tests;
