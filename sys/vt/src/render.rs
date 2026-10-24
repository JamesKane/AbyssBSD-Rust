// SPDX-License-Identifier: BSD-2-Clause
//! Drawing a window's cells into the framebuffer (`vt_fb`): the text
//! area centered on the screen, each cell its glyph in the colors
//! `vt_determine_colors` gives it, with the cursor as reverse video.
//! What is on the screen is remembered per cell (`vd_drawn`), so a
//! frame draws only the cells that differ from it.
//!
//! Behavior reference: sys/dev/vt/hw/fb/vt_fb.c (`vt_fb_bitblt_text`),
//! sys/dev/vt/colors/vt_termcolors.c, sys/dev/vt/vt_core.c
//! (`vt_compute_drawable_area`).

use alloc::vec;
use alloc::vec::Vec;

use fb_console::PixelLayout;
use vtterm::{Cell, Pos, cell_char, cell_format, colors, format};

/// The font's cell.
pub const FONT_WIDTH: u32 = 8;
pub const FONT_HEIGHT: u32 = 16;
const _: () = assert!(font::WIDTH == 8 && font::HEIGHT == 16);

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

/// What a cell was drawn as: its character (with the right-half bit)
/// and its two colors.
type Drawn = u64;

const NOTHING: Drawn = u64::MAX;

/// The screen as drawn.
#[derive(Debug)]
pub struct Renderer {
    display: Display,
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
    /// A renderer for `display`; `None` if not one cell fits or the
    /// pixels don't cover it.
    #[must_use]
    pub fn new(display: Display) -> Option<Self> {
        let cols = display.width / FONT_WIDTH;
        let rows = display.height / FONT_HEIGHT;
        let need = ix(display.stride).checked_mul(ix(display.height))?;
        if cols == 0 || rows == 0 || display.stride < display.width || display.pixels.len() < need {
            return None;
        }
        let mut cmap = [0; 16];
        for (c, rgb) in cmap.iter_mut().zip(PALETTE) {
            // 0 to 100 percent of 255 fits a byte.
            let [r, g, b] = rgb.map(|p| (255 * p / 100).to_le_bytes()[0]);
            *c = display.layout.encode([r, g, b]);
        }
        Some(Self {
            x0: (display.width - cols * FONT_WIDTH) / 2,
            y0: (display.height - rows * FONT_HEIGHT) / 2,
            size: Pos::new(rows, cols),
            cmap,
            drawn: vec![NOTHING; ix(rows) * ix(cols)],
            lut: [[0; 4]; 16],
            lut_colors: (16, 16),
            display,
        })
    }

    /// The text area, in cells.
    #[must_use]
    pub fn size(&self) -> Pos {
        self.size
    }

    /// The text area, in pixels.
    #[must_use]
    pub fn pixel_size(&self) -> (u32, u32) {
        (self.size.col * FONT_WIDTH, self.size.row * FONT_HEIGHT)
    }

    /// The framebuffer's pixels.
    #[cfg(test)]
    pub(crate) fn pixels(&mut self) -> &mut [u32] {
        self.display.pixels
    }

    /// Forgets what is on the screen and blanks it, so the next frame
    /// draws every cell (`VDF_INVALID`).
    pub fn invalidate(&mut self) {
        let black = self.cmap[0];
        let (w, h, stride) = (
            ix(self.display.width),
            ix(self.display.height),
            ix(self.display.stride),
        );
        for row in self.display.pixels.chunks_mut(stride).take(h) {
            row[..w].fill(black);
        }
        self.drawn.fill(NOTHING);
    }

    /// Draws the cells of `view` (rows of `size.col` cells) that differ
    /// from the screen; `cursor` is where the cursor is drawn, if it is.
    pub fn draw(&mut self, view: &[Cell], cursor: Option<Pos>) {
        let cols = ix(self.size.col);
        for (r, row) in (0u32..).zip(view.chunks(cols).take(ix(self.size.row))) {
            for (c, &cell) in (0u32..).zip(row) {
                let at_cursor = cursor == Some(Pos::new(r, c));
                let (fg, bg) = colors(cell, at_cursor);
                let right = cell_format(cell) & format::CJK_RIGHT != 0;
                let key = u64::from(cell_char(cell))
                    | u64::from(right) << 21
                    | u64::from(fg) << 24
                    | u64::from(bg) << 28;
                let i = ix(r) * cols + ix(c);
                if self.drawn[i] == key {
                    continue;
                }
                self.drawn[i] = key;
                self.glyph(r, c, cell_char(cell), right, fg, bg);
            }
        }
    }

    fn glyph(&mut self, row: u32, col: u32, ch: u32, right: bool, fg: u8, bg: u8) {
        // The font has no double-width glyphs: a wide character's right
        // half is blank.
        let bitmap = if right {
            &[0u8; font::HEIGHT]
        } else {
            char::from_u32(ch).map_or(font::replacement_glyph(), font::glyph)
        };
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
        let stride = ix(self.display.stride);
        let x = ix(self.x0 + col * FONT_WIDTH);
        let y = ix(self.y0 + row * FONT_HEIGHT);
        for (dy, &bits) in bitmap.iter().enumerate() {
            let at = (y + dy) * stride + x;
            let line = &mut self.display.pixels[at..at + font::WIDTH];
            line[..4].copy_from_slice(&self.lut[usize::from(bits >> 4)]);
            line[4..].copy_from_slice(&self.lut[usize::from(bits & 15)]);
        }
    }
}

fn ix(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
