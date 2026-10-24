// SPDX-License-Identifier: BSD-2-Clause
//! Drawing a window's cells into the framebuffer (`vt_fb`): the text
//! area centered on the screen, each cell its glyph in the window's
//! font in the colors
//! `vt_determine_colors` gives it, with the cursor as reverse video.
//! What is on the screen is remembered per cell (`vd_drawn`), so a
//! frame draws only the cells that differ from it.
//!
//! Behavior reference: sys/dev/vt/hw/fb/vt_fb.c (`vt_fb_bitblt_text`),
//! sys/dev/vt/colors/vt_termcolors.c, sys/dev/vt/vt_core.c
//! (`vt_compute_drawable_area`).

use alloc::vec;
use alloc::vec::Vec;

use abi::fbio::{FBTYPE_EFIFB, FbRgboffs, Fbtype};
use fb_console::PixelLayout;
use vtterm::{Cell, Pos, cell_char, cell_format, colors, format};

use crate::font::Face;

/// `VT_FB_MAX_WIDTH`, `VT_FB_MAX_HEIGHT` in 8×16 cells
/// (`PIXEL_WIDTH`, `PIXEL_HEIGHT`): the most cells a window has.
const MAX_COLS: u32 = 4096 / 8;
const MAX_ROWS: u32 = 2400 / 16;
/// Pixel rows blanked at a time.
const BAND: usize = 64;

/// `color_def`: the 16 colors as percentages of full red, green, blue.
const PALETTE: [[u32; 3]; 16] = [
    [0, 0, 0],
    [50, 0, 0],
    [0, 50, 0],
    [77, 63, 0],
    [20, 40, 64],
    [50, 0, 50],
    [0, 50, 50],
    [75, 75, 75],
    [18, 20, 21],
    [100, 0, 0],
    [0, 100, 0],
    [100, 100, 0],
    [45, 62, 81],
    [100, 0, 100],
    [0, 100, 100],
    [100, 100, 100],
];

/// A linear 32-bit framebuffer.
#[derive(Debug)]
pub struct Display {
    pub pixels: &'static mut [u32],
    pub width: u32,
    pub height: u32,
    /// Pixels per scan line.
    pub stride: u32,
    pub layout: PixelLayout,
}

/// What a cell was drawn as: its character (with the right-half and
/// bold bits) and its two colors.
type Drawn = u64;

const NOTHING: Drawn = u64::MAX;

/// The screen as drawn.
#[derive(Debug)]
pub struct Renderer {
    display: Display,
    /// The font drawn in.
    face: Face,
    /// The text area, in cells.
    size: Pos,
    /// The text area's top left pixel.
    x0: u32,
    y0: u32,
    cmap: [u32; 16],
    drawn: Vec<Drawn>,
    /// The four pixels of each nibble of a glyph row, for the colors
    /// last drawn in.
    lut: [[u32; 4]; 16],
    lut_colors: (u8, u8),
}

impl Renderer {
    /// A renderer for `display` in the built-in font; `None` if not
    /// one cell fits or the pixels don't cover it.
    #[must_use]
    pub fn new(display: Display) -> Option<Self> {
        let face = Face::Builtin;
        let (fw, fh) = face.size();
        let need = ix(display.stride).checked_mul(ix(display.height))?;
        if display.width < fw
            || display.height < fh
            || display.stride < display.width
            || display.pixels.len() < need
        {
            return None;
        }
        let mut cmap = [0; 16];
        for (c, rgb) in cmap.iter_mut().zip(PALETTE) {
            // 0 to 100 percent of 255 fits a byte.
            let [r, g, b] = rgb.map(|p| (255 * p / 100).to_le_bytes()[0]);
            *c = display.layout.encode([r, g, b]);
        }
        let mut r = Self {
            face: Face::Builtin,
            size: Pos::new(0, 0),
            x0: 0,
            y0: 0,
            cmap,
            drawn: Vec::new(),
            lut: [[0; 4]; 16],
            lut_colors: (16, 16),
            display,
        };
        r.layout();
        Some(r)
    }

    /// The display's size in pixels.
    #[must_use]
    pub fn display_size(&self) -> (u32, u32) {
        (self.display.width, self.display.height)
    }

    /// The frame buffer as `FBIOGTYPE` gives it, with its scan line in
    /// bytes and its colors' lowest bits (`vt_efifb_init`): 32 bits a
    /// pixel, 16 colors.
    #[must_use]
    pub fn fb_info(&self) -> (Fbtype, u32, FbRgboffs) {
        let d = &self.display;
        let (red, green, blue, reserved) = match d.layout {
            PixelLayout::Rgbx => (0xff, 0xff00, 0xff_0000, 0xff00_0000),
            PixelLayout::Bgrx => (0xff_0000, 0xff00, 0xff, 0xff00_0000),
            PixelLayout::Masks { red, green, blue } => (red, green, blue, 0),
        };
        let all: u32 = red | green | blue | reserved;
        let depth = 32 - all.leading_zeros();
        let stride = d.stride.saturating_mul(4);
        let int = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        // `ffs(mask) - 1`.
        let off = |m: u32| if m == 0 { -1 } else { int(m.trailing_zeros()) };
        let t = Fbtype {
            fb_type: FBTYPE_EFIFB,
            fb_height: int(d.height),
            fb_width: int(d.width),
            fb_depth: int(depth),
            fb_cmsize: 16,
            fb_size: int(d.height.saturating_mul(stride)),
        };
        let o = FbRgboffs {
            red: off(red),
            green: off(green),
            blue: off(blue),
        };
        (t, stride, o)
    }

    /// Draws in `face` from now on; `true` if that is a change, after
    /// which the screen must be [invalidated](Self::invalidate).
    pub fn set_face(&mut self, face: &Face) -> bool {
        if self.face.same(face) {
            return false;
        }
        self.face = face.clone();
        self.layout();
        true
    }

    /// The text area for the font (`vt_compute_drawable_area`).
    fn layout(&mut self) {
        let (fw, fh) = self.face.size();
        self.size = text_size(self.display_size(), &self.face);
        self.x0 = (self.display.width % fw) / 2;
        self.y0 = (self.display.height % fh) / 2;
        self.drawn = vec![NOTHING; ix(self.size.row) * ix(self.size.col)];
    }

    /// The text area, in cells.
    #[must_use]
    pub fn size(&self) -> Pos {
        self.size
    }

    /// The framebuffer's pixels.
    #[cfg(test)]
    pub(crate) fn pixels(&mut self) -> &mut [u32] {
        self.display.pixels
    }

    /// Forgets what is on the screen and blanks it, so the next frame
    /// draws every cell (`VDF_INVALID`).
    pub fn invalidate(&mut self) {
        self.forget();
        let mut band = 0;
        while self.blank(band) {
            band += 1;
        }
    }

    /// Forgets what was drawn: the next frame draws every cell.
    pub fn forget(&mut self) {
        self.drawn.fill(NOTHING);
    }

    /// Blanks band `band` of [`BAND`] pixel rows (`vt_fb_blank` in
    /// parts); `false` past the display.
    pub fn blank(&mut self, band: usize) -> bool {
        let black = self.cmap[0];
        let (w, h, stride) = (
            ix(self.display.width),
            ix(self.display.height),
            ix(self.display.stride),
        );
        if band * BAND >= h {
            return false;
        }
        let rows = self.display.pixels.chunks_mut(stride).take(h);
        for row in rows.skip(band * BAND).take(BAND) {
            row[..w].fill(black);
        }
        true
    }

    /// Draws the cells of `view` (rows of `size.col` cells) that differ
    /// from the screen; `cursor` is where the cursor is drawn, if it is.
    pub fn draw(&mut self, view: &[Cell], cursor: Option<Pos>) {
        let cols = ix(self.size.col);
        for (r, row) in (0u32..).zip(view.chunks(cols).take(ix(self.size.row))) {
            self.draw_row(r, row, cursor);
        }
    }

    /// Draws row `r` of the view, `row`, as [`Renderer::draw`] does.
    pub fn draw_row(&mut self, r: u32, row: &[Cell], cursor: Option<Pos>) {
        if r >= self.size.row {
            return;
        }
        let cols = ix(self.size.col);
        for (c, &cell) in (0u32..).zip(row.iter().take(cols)) {
            let at_cursor = cursor == Some(Pos::new(r, c));
            let (fg, bg) = colors(cell, at_cursor);
            let right = cell_format(cell) & format::CJK_RIGHT != 0;
            let bold = cell_format(cell) & format::BOLD != 0;
            let key = u64::from(cell_char(cell))
                | u64::from(right) << 21
                | u64::from(bold) << 22
                | u64::from(fg) << 24
                | u64::from(bg) << 28;
            let i = ix(r) * cols + ix(c);
            if self.drawn[i] == key {
                continue;
            }
            self.drawn[i] = key;
            self.glyph(r, c, (cell_char(cell), bold, right), fg, bg);
        }
    }

    fn glyph(&mut self, row: u32, col: u32, ch: (u32, bool, bool), fg: u8, bg: u8) {
        if self.lut_colors != (fg, bg) {
            let (fgc, bgc) = (
                self.cmap[usize::from(fg & 15)],
                self.cmap[usize::from(bg & 15)],
            );
            for (n, px) in self.lut.iter_mut().enumerate() {
                for (i, p) in px.iter_mut().enumerate() {
                    *p = if n & (8 >> i) != 0 { fgc } else { bgc };
                }
            }
            self.lut_colors = (fg, bg);
        }
        let (fw, fh) = self.face.size();
        let (x, y) = (self.x0 + col * fw, self.y0 + row * fh);
        // A font wider or taller than the display draws what fits.
        if x + fw > self.display.width || y + fh > self.display.height {
            return;
        }
        let bitmap = self.face.glyph(ch.0, ch.1, ch.2);
        let stride = ix(self.display.stride);
        let (x, y, fw) = (ix(x), ix(y), ix(fw));
        let lut = &self.lut;
        if fw == 8 {
            for (dy, &bits) in bitmap.iter().enumerate() {
                let at = (y + dy) * stride + x;
                let line = &mut self.display.pixels[at..at + 8];
                line[..4].copy_from_slice(&lut[usize::from(bits >> 4)]);
                line[4..].copy_from_slice(&lut[usize::from(bits & 15)]);
            }
            return;
        }
        for (dy, bits) in bitmap.chunks(fw.div_ceil(8)).enumerate() {
            let at = (y + dy) * stride + x;
            let line = &mut self.display.pixels[at..at + fw];
            for (i, px) in line.chunks_mut(4).enumerate() {
                let b = bits[i / 2];
                let n = if i % 2 == 0 { b >> 4 } else { b & 15 };
                px.copy_from_slice(&lut[usize::from(n)][..px.len()]);
            }
        }
    }
}

/// A window's size in cells on a display of `pixels` in font `face`
/// (`vt_termsize`).
#[must_use]
pub fn text_size(pixels: (u32, u32), face: &Face) -> Pos {
    let (fw, fh) = face.size();
    Pos::new(
        (pixels.1 / fh).clamp(1, MAX_ROWS),
        (pixels.0 / fw).clamp(1, MAX_COLS),
    )
}

fn ix(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
