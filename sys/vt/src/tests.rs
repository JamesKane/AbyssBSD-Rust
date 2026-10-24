// SPDX-License-Identifier: BSD-2-Clause

extern crate std;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use fb_console::PixelLayout;
use vtterm::{NORMAL, Pos, Terminal, pack};

use crate::render::{Display, Renderer};

/// A 20×36-pixel screen with a stride of 24: two rows of two cells,
/// centered, with a border.
fn renderer() -> Renderer {
    let pixels: &'static mut [u32] = Box::leak(vec![0x1234_5678u32; 24 * 36].into_boxed_slice());
    let d = Display {
        pixels,
        width: 20,
        height: 36,
        stride: 24,
        layout: PixelLayout::Rgbx,
    };
    Renderer::new(d).unwrap()
}

const WHITE: u32 = 0x00bf_bfbf; // light grey: 75% of 255 is 191.
const BLACK: u32 = 0;

#[test]
fn text_area_is_centered_and_glyphs_drawn() {
    let mut r = renderer();
    assert_eq!(r.size(), Pos::new(2, 2));
    assert_eq!(r.pixel_size(), (16, 32));
    r.invalidate();
    let px = r.pixels().to_vec();
    // Blanked, past the width (the stride's slack) left alone.
    assert_eq!(px[0], BLACK);
    assert_eq!(px[20], 0x1234_5678);
    let mut view = vec![pack(u32::from(b' '), &NORMAL); 4];
    view[3] = pack(u32::from(b'|'), &NORMAL);
    r.draw(&view, None);
    let px = r.pixels().to_vec();
    // Cell (1, 1) starts at x = 2 + 8, y = 2 + 16.
    let glyph = font::glyph('|');
    for (dy, &bits) in glyph.iter().enumerate() {
        for dx in 0..8 {
            let want = if bits & (0x80 >> dx) != 0 {
                WHITE
            } else {
                BLACK
            };
            assert_eq!(px[(18 + dy) * 24 + 10 + dx], want, "({dx}, {dy})");
        }
    }
}

#[test]
fn only_changed_cells_are_drawn_and_the_cursor_is_reversed() {
    let mut r = renderer();
    r.invalidate();
    let view = vec![pack(u32::from(b' '), &NORMAL); 4];
    r.draw(&view, None);
    // Scribble on a cell: unchanged, it isn't drawn again.
    r.pixels()[2 * 24 + 2] = 0xdead;
    r.draw(&view, None);
    assert_eq!(r.pixels()[2 * 24 + 2], 0xdead);
    // The cursor there reverses the blank: it's drawn in light grey.
    r.draw(&view, Some(Pos::new(0, 0)));
    assert_eq!(r.pixels()[2 * 24 + 2], WHITE);
    r.draw(&view, None);
    assert_eq!(r.pixels()[2 * 24 + 2], BLACK);
}

#[test]
fn a_terminal_view_draws() {
    let mut r = renderer();
    let mut t = Terminal::new(r.size(), 10);
    t.write(b"\x1b[1;34mAB");
    let view: Vec<_> = (0..2)
        .flat_map(|row| t.buf.view_row(row).to_vec())
        .collect();
    r.invalidate();
    r.draw(&view, Some(t.buf.cursor()));
    let px = r.pixels();
    // A bright blue pixel of the A: 45%, 62%, 81% is (114, 158, 206).
    let blue = u32::from_le_bytes([114, 158, 206, 0]);
    assert!(px[2 * 24..20 * 24].contains(&blue));
}
