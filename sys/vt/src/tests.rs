// SPDX-License-Identifier: BSD-2-Clause

extern crate std;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use fb_console::PixelLayout;
use vtterm::{NORMAL, Pos, Terminal, pack};

use alloc::sync::Arc;

use abi::errno::Errno;

use crate::font::{Face, Font, Header, Map, validate};
use crate::render::{Display, Renderer, text_size};

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
    assert_eq!(r.display_size(), (20, 36));
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

fn header(width: u32, height: u32, glyph_count: u32) -> Header {
    Header {
        width,
        height,
        glyph_count,
        map_count: [0; 4],
    }
}

#[test]
fn a_font_is_checked_as_vtfont_load_checks_it() {
    assert_eq!(header(0, 16, 1).check(), Err(Errno::EINVAL));
    assert_eq!(header(8, 0, 1).check(), Err(Errno::EINVAL));
    assert_eq!(header(129, 16, 1).check(), Err(Errno::E2BIG));
    assert_eq!(header(8, 16, 131_073).check(), Err(Errno::E2BIG));
    assert_eq!(header(8, 16, 0).check(), Err(Errno::EINVAL));
    // 2 MiB of glyphs at most.
    assert_eq!(header(16, 128, 8192).check(), Ok(2_097_152));
    assert_eq!(header(16, 128, 8193).check(), Err(Errno::E2BIG));
    // A 12-pixel row takes two bytes.
    assert_eq!(header(12, 6, 3).check(), Ok(36));
    let mut h = header(8, 16, 1);
    h.map_count[2] = 65537;
    assert_eq!(h.check(), Err(Errno::E2BIG));

    let m = |src, dst, len| Map { src, dst, len };
    assert_eq!(validate(&[m(32, 1, 94), m(160, 96, 0)], 97), Ok(()));
    // Out of order, or overlapping the last range.
    assert_eq!(
        validate(&[m(32, 1, 94), m(126, 96, 0)], 97),
        Err(Errno::EINVAL)
    );
    // Past the glyphs: `dst + len` must be below the count too.
    assert_eq!(validate(&[m(32, 1, 96)], 97), Err(Errno::EINVAL));
    assert_eq!(validate(&[m(32, 97, 0)], 97), Err(Errno::EINVAL));
    // Map entries are 8 bytes in the machine's order.
    let b = [0x41, 0, 0, 0, 2, 0, 1, 0].map(u8::to_le);
    assert_eq!(Map::from_bytes(b), m(0x41, 2, 1));
}

/// A 4×6 font of three glyphs: glyph `n` is `n` lit columns. `A`
/// and `B` are glyphs 1 and 2, a bold `A` glyph 2, a right half of `A`
/// glyph 1.
fn small_font() -> Face {
    let mut glyphs = Vec::new();
    for n in 0..3u8 {
        glyphs.extend([0xf0u8 << (4 - n); 6]);
    }
    let m = |src, dst, len| Map { src, dst, len };
    let maps = [
        vec![m(0x41, 1, 1)],
        vec![m(0x41, 1, 0)],
        vec![m(0x41, 2, 0)],
        vec![],
    ];
    let h = Header {
        width: 4,
        height: 6,
        glyph_count: 3,
        map_count: [1, 1, 1, 0],
    };
    assert!(Font::new(&h, maps.clone(), vec![0; 17]).is_err());
    Face::Loaded(Arc::new(Font::new(&h, maps, glyphs).unwrap()))
}

#[test]
fn glyphs_are_found_through_the_maps() {
    let f = small_font();
    assert_eq!(f.size(), (4, 6));
    assert_eq!(f.glyph(u32::from('A'), false, false), [0x80; 6]);
    assert_eq!(f.glyph(u32::from('B'), false, false), [0xc0; 6]);
    // Bold first from the bold map, else the normal one.
    assert_eq!(f.glyph(u32::from('A'), true, false), [0xc0; 6]);
    assert_eq!(f.glyph(u32::from('B'), true, false), [0xc0; 6]);
    // Not in a map: glyph 0.
    assert_eq!(f.glyph(u32::from('C'), false, false), [0x00; 6]);
    assert_eq!(f.glyph(0x10_ffff, false, false), [0x00; 6]);
    assert_eq!(f.glyph(u32::from('A'), false, true), [0x80; 6]);
    assert_eq!(f.glyph(u32::from('B'), false, true), [0x00; 6]);
    assert!(f.same(&f.clone()));
    assert!(!f.same(&Face::Builtin));
}

#[test]
fn a_loaded_font_sizes_and_draws_the_text_area() {
    let mut r = renderer();
    let f = small_font();
    // 20×36 pixels hold 5 columns and 6 rows of 4×6 cells.
    assert_eq!(text_size((20, 36), &f), Pos::new(6, 5));
    assert_eq!(text_size((20, 36), &Face::Builtin), Pos::new(2, 2));
    // At most 512 columns and 150 rows.
    assert_eq!(text_size((8192, 4800), &Face::Builtin), Pos::new(150, 512));
    assert!(r.set_face(&f));
    assert!(!r.set_face(&f.clone()));
    assert_eq!(r.size(), Pos::new(6, 5));
    r.invalidate();
    let mut view = vec![pack(u32::from(b' '), &NORMAL); 30];
    view[5 + 1] = pack(u32::from(b'B'), &NORMAL);
    r.draw(&view, None);
    let px = r.pixels();
    // Cell (1, 1) is at x = 4, y = 6: two lit columns, two dark.
    for dy in 0..6 {
        let at = (6 + dy) * 24 + 4;
        assert_eq!(px[at..at + 4], [WHITE, WHITE, BLACK, BLACK], "row {dy}");
    }
    assert!(r.set_face(&Face::Builtin));
    assert_eq!(r.size(), Pos::new(2, 2));
}
