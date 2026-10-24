// SPDX-License-Identifier: BSD-2-Clause
//! A window's font (`struct vt_font`): the built-in 8×16 one, or one a
//! program loaded (`PIO_VFONT`, `vidcontrol -f`), whose glyphs are
//! found through four sorted maps of character ranges (normal, bold,
//! and the right halves of double-width characters).
//!
//! Behavior reference: sys/dev/vt/vt_font.c, sys/sys/font.h.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;

/// `VFNT_MAPS`, in their order: normal, normal right, bold, bold
/// right.
pub const MAPS: usize = 4;
const NORMAL: usize = 0;
const NORMAL_RIGHT: usize = 1;
const BOLD: usize = 2;
const BOLD_RIGHT: usize = 3;

/// `VTFONT_MAXMAPPINGS`, `VTFONT_MAXGLYPHS`, `VTFONT_MAXGLYPHSIZE`,
/// `VTFONT_MAXDIMENSION`: what a loaded font may have at most.
pub const MAX_MAPPINGS: u32 = 65536;
pub const MAX_GLYPHS: u32 = 131_072;
pub const MAX_GLYPH_BYTES: u64 = 2_097_152;
pub const MAX_DIMENSION: u32 = 128;

/// `struct vfnt_map`: characters `src` to `src + len` are glyphs `dst`
/// to `dst + len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Map {
    pub src: u32,
    pub dst: u16,
    pub len: u16,
}

impl Map {
    /// `sizeof(struct vfnt_map)`.
    pub const SIZE: usize = 8;

    /// From its bytes.
    #[must_use]
    pub fn from_bytes(b: [u8; Self::SIZE]) -> Self {
        Self {
            src: u32::from_ne_bytes([b[0], b[1], b[2], b[3]]),
            dst: u16::from_ne_bytes([b[4], b[5]]),
            len: u16::from_ne_bytes([b[6], b[7]]),
        }
    }
}

/// A loaded font.
#[derive(Debug)]
pub struct Font {
    width: u32,
    height: u32,
    maps: [Vec<Map>; MAPS],
    /// The glyphs, each `height` rows of `width.div_ceil(8)` bytes.
    glyphs: Vec<u8>,
}

/// A font's size and counts, as `vfnt_t` gives them.
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    pub glyph_count: u32,
    pub map_count: [u32; MAPS],
}

impl Header {
    /// The checks `vtfont_load` makes before it reads the maps and the
    /// glyphs; the glyphs' size in bytes.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a dimension of 0 or no glyph 0; `E2BIG` for more
    /// than a font may have.
    pub fn check(&self) -> Result<usize, Errno> {
        if self.width < 1 || self.height < 1 {
            return Err(Errno::EINVAL);
        }
        if self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
            || self.glyph_count > MAX_GLYPHS
        {
            return Err(Errno::E2BIG);
        }
        if self.map_count.iter().any(|&n| n > MAX_MAPPINGS) {
            return Err(Errno::E2BIG);
        }
        // Character 0 must always be present.
        if self.glyph_count < 1 {
            return Err(Errno::EINVAL);
        }
        let size = u64::from(self.width.div_ceil(8))
            * u64::from(self.height)
            * u64::from(self.glyph_count);
        if size > MAX_GLYPH_BYTES {
            return Err(Errno::E2BIG);
        }
        usize::try_from(size).map_err(|_| Errno::E2BIG)
    }
}

impl Font {
    /// A font from `h`, already [checked](Header::check), its maps and
    /// its glyphs (`vtfont_load`).
    ///
    /// # Errors
    ///
    /// `EINVAL` if a map is out of order or names glyphs the font
    /// doesn't have (`vtfont_validate_map`).
    pub fn new(h: &Header, maps: [Vec<Map>; MAPS], glyphs: Vec<u8>) -> Result<Self, Errno> {
        if Some(glyphs.len()) != h.check().ok() {
            return Err(Errno::EINVAL);
        }
        for m in &maps {
            validate(m, h.glyph_count)?;
        }
        Ok(Self {
            width: h.width,
            height: h.height,
            maps,
            glyphs,
        })
    }
}

/// `vtfont_validate_map`.
///
/// # Errors
///
/// `EINVAL` if `map` is out of order or names glyphs past
/// `glyph_count`.
pub fn validate(map: &[Map], glyph_count: u32) -> Result<(), Errno> {
    let mut last = 0u32;
    for (i, m) in map.iter().enumerate() {
        // Not ordered.
        if i > 0 && m.src <= last {
            return Err(Errno::EINVAL);
        }
        // Past the glyphs.
        if u32::from(m.dst) >= glyph_count || u32::from(m.dst) + u32::from(m.len) >= glyph_count {
            return Err(Errno::EINVAL);
        }
        last = m.src.wrapping_add(u32::from(m.len));
    }
    Ok(())
}

/// `vtfont_bisearch`: the glyph for `src`, 0 if the map has none.
fn bisearch(map: &[Map], src: u32) -> u32 {
    let glyph = |m: &Map| (src.wrapping_sub(m.src).wrapping_add(u32::from(m.dst))) & 0xffff;
    let (Some(first), Some(last)) = (map.first(), map.last()) else {
        return 0;
    };
    if src < first.src {
        return 0;
    }
    // ASCII characters occur very often.
    if src <= first.src.wrapping_add(u32::from(first.len)) {
        return glyph(first);
    }
    if src > last.src.wrapping_add(u32::from(last.len)) {
        return 0;
    }
    let (mut lo, mut hi) = (0, map.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let m = &map[mid];
        if src < m.src {
            hi = mid;
        } else if src > m.src.wrapping_add(u32::from(m.len)) {
            lo = mid + 1;
        } else {
            return glyph(m);
        }
    }
    0
}

/// A window's font: the built-in one or a loaded one.
#[derive(Clone, Debug, Default)]
pub enum Face {
    #[default]
    Builtin,
    Loaded(Arc<Font>),
}

static BLANK: [u8; font::HEIGHT] = [0; font::HEIGHT];

impl Face {
    /// The glyph cell's width and height in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        match self {
            // 8 and 16.
            Self::Builtin => (
                u32::try_from(font::WIDTH).unwrap_or(8),
                u32::try_from(font::HEIGHT).unwrap_or(16),
            ),
            Self::Loaded(f) => (f.width, f.height),
        }
    }

    /// Whether `self` and `other` are the same font.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Builtin, Self::Builtin) => true,
            (Self::Loaded(a), Self::Loaded(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Character `ch`'s glyph (`vtfont_lookup`): the bold map's first
    /// for bold, then the normal one's, and glyph 0 if neither has it;
    /// with `right`, the right half of a double-width character.
    #[must_use]
    pub fn glyph(&self, ch: u32, bold: bool, right: bool) -> &[u8] {
        match self {
            // The built-in font has no double-width glyphs: a wide
            // character's right half is blank.
            Self::Builtin if right => &BLANK,
            Self::Builtin => {
                char::from_u32(ch).map_or(font::replacement_glyph(), font::glyph) as &[u8]
            }
            Self::Loaded(f) => {
                let (normal, bold_map) = if right {
                    (NORMAL_RIGHT, BOLD_RIGHT)
                } else {
                    (NORMAL, BOLD)
                };
                let mut dst = if bold {
                    bisearch(&f.maps[bold_map], ch)
                } else {
                    0
                };
                if dst == 0 {
                    dst = bisearch(&f.maps[normal], ch);
                }
                let stride = ix(f.width.div_ceil(8)) * ix(f.height);
                let at = ix(dst) * stride;
                f.glyphs.get(at..at + stride).unwrap_or(&f.glyphs[..stride])
            }
        }
    }
}

fn ix(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
