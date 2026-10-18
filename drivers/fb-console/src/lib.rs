// SPDX-License-Identifier: BSD-2-Clause
//! A text console on a linear 32-bit framebuffer: fixed 8×16 cells,
//! scrolling, a few ANSI-free colours.
//!
//! The pixel memory is borrowed as a slice so the renderer runs in host
//! tests against a vector; the kernel hands it the GOP framebuffer.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// How a 32-bit pixel is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    /// Byte 0 red, 1 green, 2 blue.
    Rgbx,
    /// Byte 0 blue, 1 green, 2 red.
    Bgrx,
    /// Arbitrary channel masks.
    Masks {
        /// Red channel bits.
        red: u32,
        /// Green channel bits.
        green: u32,
        /// Blue channel bits.
        blue: u32,
    },
}

impl PixelLayout {
    /// Encodes an RGB colour.
    #[must_use]
    pub fn encode(self, [r, g, b]: [u8; 3]) -> u32 {
        match self {
            Self::Rgbx => u32::from_le_bytes([r, g, b, 0]),
            Self::Bgrx => u32::from_le_bytes([b, g, r, 0]),
            Self::Masks { red, green, blue } => scale(r, red) | scale(g, green) | scale(b, blue),
        }
    }
}

/// Places an 8-bit channel value into `mask`, scaled to its width.
fn scale(v: u8, mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    let v = if bits >= 8 {
        u32::from(v) << (bits - 8)
    } else {
        u32::from(v) >> (8 - bits)
    };
    (v << shift) & mask
}

/// Why a framebuffer was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The pixel slice is smaller than `stride * height`.
    TooSmall,
    /// Smaller than one character cell.
    NoRoom,
}

/// A text console drawing into `pixels`.
#[derive(Debug)]
pub struct Console<'a> {
    pixels: &'a mut [u32],
    stride: usize,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: u32,
    bg: u32,
}

impl<'a> Console<'a> {
    /// A console over a `width`×`height` framebuffer with `stride` pixels
    /// per line. Clears the screen.
    pub fn new(
        pixels: &'a mut [u32],
        width: usize,
        height: usize,
        stride: usize,
        layout: PixelLayout,
    ) -> Result<Self, Error> {
        if stride < width || pixels.len() < stride * height {
            return Err(Error::TooSmall);
        }
        let (cols, rows) = (width / font::WIDTH, height / font::HEIGHT);
        if cols == 0 || rows == 0 {
            return Err(Error::NoRoom);
        }
        let mut c = Self {
            pixels,
            stride,
            cols,
            rows,
            col: 0,
            row: 0,
            fg: layout.encode([0xd8, 0xd8, 0xd8]),
            bg: layout.encode([0x10, 0x14, 0x1c]),
        };
        c.clear();
        Ok(c)
    }

    /// Columns and rows of text.
    #[must_use]
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// Clears the screen and homes the cursor.
    pub fn clear(&mut self) {
        let used = self.rows * font::HEIGHT * self.stride;
        self.pixels[..used].fill(self.bg);
        self.col = 0;
        self.row = 0;
    }

    /// Writes one character.
    pub fn put(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => self.col = 0,
            '\t' => {
                let next = (self.col / 8 + 1) * 8;
                while self.col < next.min(self.cols) {
                    self.put(' ');
                }
            }
            c => {
                if self.col == self.cols {
                    self.newline();
                }
                self.draw(c);
                self.col += 1;
            }
        }
    }

    fn draw(&mut self, c: char) {
        let glyph = font::glyph(c);
        let x0 = self.col * font::WIDTH;
        let y0 = self.row * font::HEIGHT;
        for (dy, bits) in glyph.iter().enumerate() {
            let line = (y0 + dy) * self.stride + x0;
            for (dx, px) in self.pixels[line..line + font::WIDTH].iter_mut().enumerate() {
                *px = if bits & (0x80 >> dx) != 0 {
                    self.fg
                } else {
                    self.bg
                };
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
            return;
        }
        // Scroll up one text row.
        let row_px = font::HEIGHT * self.stride;
        let used = self.rows * row_px;
        self.pixels.copy_within(row_px..used, 0);
        self.pixels[used - row_px..used].fill(self.bg);
    }
}

impl fmt::Write for Console<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        s.chars().for_each(|c| self.put(c));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use core::fmt::Write;
    use std::vec;

    use super::*;

    #[test]
    fn encodes_layouts() {
        assert_eq!(PixelLayout::Rgbx.encode([1, 2, 3]), 0x0003_0201);
        assert_eq!(PixelLayout::Bgrx.encode([1, 2, 3]), 0x0001_0203);
        let m = PixelLayout::Masks {
            red: 0xf800,
            green: 0x07e0,
            blue: 0x001f,
        };
        assert_eq!(m.encode([0xff, 0xff, 0xff]), 0xffff);
        assert_eq!(m.encode([0x80, 0, 0]), 0x8000);
    }

    #[test]
    fn draws_and_scrolls() {
        let (w, h, stride) = (8 * 4, 16 * 2, 40);
        let mut px = vec![0u32; stride * h];
        let fg = {
            let mut c = Console::new(&mut px, w, h, stride, PixelLayout::Rgbx).unwrap();
            assert_eq!(c.size(), (4, 2));
            write!(c, "|").unwrap();
            c.fg
        };
        // The bar glyph lights something in the first cell only.
        let lit = |px: &[u32], x0: usize, y0: usize| {
            (0..16).any(|y| (0..8).any(|x| px[(y0 + y) * stride + x0 + x] == fg))
        };
        assert!(lit(&px, 0, 0));
        assert!(!lit(&px, 8, 0));
        {
            let mut c = Console::new(&mut px, w, h, stride, PixelLayout::Rgbx).unwrap();
            // Fill two rows and wrap into a third: the first row scrolls away.
            write!(c, "|   \n |  \n").unwrap();
        }
        assert!(!lit(&px, 0, 0));
        assert!(lit(&px, 8, 0), "second row moved up");
        assert!(!lit(&px, 0, 16), "new last row is blank");
    }

    #[test]
    fn rejects_bad_geometry() {
        let mut px = vec![0u32; 10];
        assert_eq!(
            Console::new(&mut px, 8, 16, 8, PixelLayout::Rgbx).unwrap_err(),
            Error::TooSmall
        );
        let mut px = vec![0u32; 64];
        assert_eq!(
            Console::new(&mut px, 4, 16, 4, PixelLayout::Rgbx).unwrap_err(),
            Error::NoRoom
        );
    }
}
