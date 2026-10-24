// SPDX-License-Identifier: BSD-2-Clause
#![allow(clippy::cast_possible_truncation)] // test sizes are small

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::*;

fn term(rows: u32, cols: u32) -> Terminal {
    Terminal::new(Pos::new(rows, cols), DEFAULT_HISTORY)
}

fn text(cells: &[Cell]) -> String {
    let s: String = cells
        .iter()
        .map(|&c| char::from_u32(cell_char(c)).unwrap_or('?'))
        .collect();
    String::from(s.trim_end())
}

fn row(t: &Terminal, r: u32) -> String {
    text(t.buf.screen_row(r))
}

fn cell(t: &Terminal, r: u32, c: u32) -> Cell {
    t.buf.screen_row(r)[c as usize]
}

/// Records what the emulator asks for besides drawing.
#[derive(Default)]
struct Rec {
    responses: Vec<Vec<u8>>,
    params: Vec<(Param, u32)>,
    bells: u32,
}

impl Sink for Rec {
    fn bell(&mut self) {
        self.bells += 1;
    }
    fn cursor(&mut self, _: Pos) {}
    fn putchar(&mut self, _: Pos, _: Char, _: &Attr) {}
    fn fill(&mut self, _: Rect, _: Char, _: &Attr) {}
    fn copy(&mut self, _: Rect, _: Pos) {}
    fn param(&mut self, cmd: Param, value: u32) {
        self.params.push((cmd, value));
    }
    fn respond(&mut self, b: &[u8]) {
        self.responses.push(b.to_vec());
    }
}

#[test]
fn text_wraps_at_the_last_column() {
    let mut t = term(4, 10);
    t.write(b"0123456789");
    // In the wrapped state, the cursor stays on the last column.
    assert_eq!(t.emu.cursor(), Pos::new(0, 9));
    t.write(b"ab\r\ncd");
    assert_eq!(row(&t, 0), "0123456789");
    assert_eq!(row(&t, 1), "ab");
    assert_eq!(row(&t, 2), "cd");
    assert_eq!(t.emu.cursor(), Pos::new(2, 2));
    // Without autowrap the last column is overwritten.
    t.write(b"\x1b[?7l\r0123456789XY");
    assert_eq!(row(&t, 2), "012345678Y");
}

#[test]
fn newlines_scroll_into_history() {
    let mut t = term(3, 8);
    for i in 0..6 {
        t.write(alloc::format!("line{i}\r\n").as_bytes());
    }
    assert_eq!(row(&t, 0), "line4");
    assert_eq!(row(&t, 1), "line5");
    assert_eq!(row(&t, 2), "");
    // Scroll mode: the view goes back, the screen stays.
    t.buf.set_scroll_mode(true);
    assert!(t.buf.seek(-2, Seek::Cur));
    assert_eq!(text(t.buf.view_row(0)), "line2");
    assert!(t.buf.scrolled_back());
    t.write(b"more\r\n");
    assert_eq!(text(t.buf.view_row(0)), "line2");
    assert_eq!(row(&t, 1), "more");
    assert!(!t.buf.is_cursor(t.buf.cursor()));
    // Back to the top of the history: line0 is the oldest row.
    t.buf.seek(0, Seek::Set);
    assert_eq!(text(t.buf.view_row(0)), "line0");
    t.buf.set_scroll_mode(false);
    assert!(t.buf.seek(0, Seek::End));
    assert_eq!(text(t.buf.view_row(1)), "more");
}

#[test]
fn history_ring_wraps() {
    let mut t = Terminal::new(Pos::new(2, 4), 5);
    for i in 0..13 {
        t.write(alloc::format!("{i}\r\n").as_bytes());
    }
    assert_eq!(row(&t, 0), "12");
    t.buf.set_scroll_mode(true);
    t.buf.seek(0, Seek::Set);
    // Five rows: three of history and the screen.
    assert_eq!(text(t.buf.view_row(0)), "9");
    assert_eq!(text(t.buf.view_row(1)), "10");
    t.buf.seek(-100, Seek::Cur);
    assert_eq!(text(t.buf.view_row(0)), "9");
    t.buf.seek(100, Seek::Cur);
    assert_eq!(text(t.buf.view_row(0)), "12");
}

#[test]
fn cursor_movement_and_erasure() {
    let mut t = term(5, 10);
    t.write(b"aaaaaaaaaa\r\nbbbbbbbbbb\r\ncccccccccc");
    t.write(b"\x1b[2;4H\x1b[K");
    assert_eq!(row(&t, 1), "bbb");
    t.write(b"\x1b[1;5H\x1b[1K");
    assert_eq!(row(&t, 0), "     aaaaa");
    t.write(b"\x1b[3;3H\x1b[X");
    assert_eq!(row(&t, 2), "cc ccccccc");
    t.write(b"\x1b[J");
    assert_eq!(row(&t, 2), "cc");
    t.write(b"\x1b[2J");
    assert!((0..5).all(|r| row(&t, r).is_empty()));
    // Out-of-range positions clamp; 0 counts as 1.
    t.write(b"\x1b[99;99H");
    assert_eq!(t.emu.cursor(), Pos::new(4, 9));
    t.write(b"\x1b[0;0H");
    assert_eq!(t.emu.cursor(), Pos::new(0, 0));
    t.write(b"\x1b[3B\x1b[2C\x1b[A\x1b[D");
    assert_eq!(t.emu.cursor(), Pos::new(2, 1));
    t.write(b"\x1b[7G\x1b[2d");
    assert_eq!(t.emu.cursor(), Pos::new(1, 6));
}

#[test]
fn insert_and_delete() {
    let mut t = term(4, 10);
    t.write(b"abcdefghij\r\x1b[2C\x1b[2@");
    assert_eq!(row(&t, 0), "ab  cdefgh");
    t.write(b"\x1b[3P");
    assert_eq!(row(&t, 0), "abdefgh");
    t.write(b"\x1b[4hXY\x1b[4l");
    assert_eq!(row(&t, 0), "abXYdefgh");
    t.write(b"\r\n1\r\n2\r\n3\x1b[2;1H\x1b[L");
    assert_eq!(
        (row(&t, 1), row(&t, 2), row(&t, 3)),
        ("".into(), "1".into(), "2".into())
    );
    t.write(b"\x1b[2M");
    assert_eq!((row(&t, 1), row(&t, 2)), ("2".into(), "".into()));
}

#[test]
fn scrolling_region() {
    let mut t = term(5, 4);
    t.write(b"0\r\n1\r\n2\r\n3\r\n4");
    t.write(b"\x1b[2;4r");
    assert_eq!(t.emu.cursor(), Pos::new(0, 0));
    t.write(b"\x1b[4;1H\nX");
    assert_eq!(
        (0..5).map(|r| row(&t, r)).collect::<Vec<_>>(),
        ["0", "2", "3", "X", "4"]
    );
    // Reverse index at the top of the region scrolls it down.
    t.write(b"\x1b[2;1H\x1bMY");
    assert_eq!(
        (0..5).map(|r| row(&t, r)).collect::<Vec<_>>(),
        ["0", "Y", "2", "3", "4"]
    );
    // Region scrolls don't feed the history.
    assert_eq!(t.buf.view_offset(), 0);
    // SU and SD.
    t.write(b"\x1b[S");
    assert_eq!(row(&t, 1), "2");
    t.write(b"\x1b[2T");
    assert_eq!(
        (row(&t, 1), row(&t, 2), row(&t, 3)),
        ("".into(), "".into(), "2".into())
    );
}

#[test]
fn origin_mode_counts_from_the_region() {
    let mut t = term(6, 6);
    t.write(b"\x1b[3;5r\x1b[?6h");
    assert_eq!(t.emu.cursor(), Pos::new(2, 0));
    t.write(b"\x1b[9;2H");
    assert_eq!(t.emu.cursor(), Pos::new(4, 1));
    let mut r = Rec::default();
    t.emu
        .input(&mut r, b"\x1b[6n\x1b[?6n\x1b[5n\x1b[26n\x1b[99n");
    assert_eq!(
        r.responses,
        [&b"\x1b[3;2R"[..], b"\x1b[?3;2R", b"\x1b[0n", b"\x1b[27;1n"]
    );
}

#[test]
fn attributes_and_colors() {
    let mut t = term(2, 10);
    t.write(
        b"\x1b[1;31;44ma\x1b[0mb\x1b[7;93;101mc\x1b[38;5;196;48;5;21md\x1b[38;5mz\x1b[39;49;27me",
    );
    let a = cell(&t, 0, 0);
    assert_eq!(
        (cell_char(a), cell_format(a), cell_fg(a), cell_bg(a)),
        ('a' as u32, format::BOLD, 1, 4)
    );
    assert_eq!(colors(a, false), (9, 4));
    let b = cell(&t, 0, 1);
    assert_eq!(b, pack('b' as u32, &NORMAL));
    // Bright colors become bold and blink.
    let c = cell(&t, 0, 2);
    assert_eq!(
        cell_format(c),
        format::REVERSE | format::BOLD | format::BLINK
    );
    assert_eq!(colors(c, false), (9, 11));
    assert_eq!(colors(c, true), (11, 9));
    let d = cell(&t, 0, 3);
    // Still reversed; 196 and 21 are plain red and blue of the 16.
    assert_eq!((cell_fg(d), cell_bg(d)), (color::RED, color::BLUE));
    assert_eq!(cell_format(d), format::REVERSE);
    assert_eq!(color_256to16(196), color::RED);
    assert_eq!(color_256to16(231), color::WHITE | color::LIGHT);
    // `38;5` without a color is ignored.
    assert_eq!(cell_fg(cell(&t, 0, 4)), cell_fg(d));
    // So is `38;5` alone, and the 5 then means blink, as in `teken`.
    let e = cell(&t, 0, 5);
    let blink = Attr {
        format: format::BLINK,
        ..NORMAL
    };
    assert_eq!(e, pack('e' as u32, &blink));
}

#[test]
fn kernel_output_uses_the_kernel_rendition() {
    let mut t = term(2, 10);
    t.write(b"\x1b[32m");
    t.write_kernel(b"k");
    t.write(b"u");
    assert_eq!(colors(cell(&t, 0, 0), false), (15, 0));
    assert_eq!(colors(cell(&t, 0, 1), false), (2, 0));
}

#[test]
fn tabs() {
    let mut t = term(2, 30);
    t.write(b"\tx");
    assert_eq!(t.emu.cursor(), Pos::new(0, 9));
    t.write(b"\x1b[3g\r\x1b[5C\x1bH\r\tA\x1b[2I");
    assert_eq!(row(&t, 0), "     A  x");
    assert_eq!(t.emu.cursor(), Pos::new(0, 29));
    t.write(b"\x1b[Z");
    assert_eq!(t.emu.cursor(), Pos::new(0, 5));
}

#[test]
fn utf8_wide_and_line_drawing() {
    let mut t = term(2, 6);
    t.write("é漢".as_bytes());
    assert_eq!(cell_char(cell(&t, 0, 0)), 0xe9);
    assert_eq!(cell_char(cell(&t, 0, 1)), 0x6f22);
    assert_eq!(
        cell_format(cell(&t, 0, 2)) & format::CJK_RIGHT,
        format::CJK_RIGHT
    );
    assert_eq!(t.emu.cursor(), Pos::new(0, 3));
    // Combining marks and stray continuation bytes draw nothing.
    t.write(b"\xcc\x81\x80");
    assert_eq!(t.emu.cursor(), Pos::new(0, 3));
    // A wide character that doesn't fit wraps.
    t.write("ab漢".as_bytes());
    assert_eq!(cell_char(cell(&t, 1, 0)), 0x6f22);
    t.write(b"\x1b(0q\x0e\x1b)0x\x0fq\x1b(Bq");
    assert_eq!(text(t.buf.screen_row(1)), "漢漢─│─q");
}

#[test]
fn strings_and_bad_sequences_are_dropped() {
    let mut t = term(2, 20);
    t.write(b"\x1b]0;title\x07a\x1bPdcs\x1b\\b");
    assert_eq!(row(&t, 0), "ab");
    // Eight arguments are taken; a ninth aborts the sequence.
    t.write(b"\x1b[1;1;1;1;1;1;1;1mX\x1b[1;1;1;1;1;1;1;1;1mY");
    assert_eq!(row(&t, 0), "abX1mY");
    // Controls act inside a sequence.
    t.write(b"\r\x1b[2\nC|");
    assert_eq!(row(&t, 1), "  |");
    // An unknown final ends it; ESC inside one is lost.
    t.write(b"\x1b[5y\x1b[\x1b[Az");
    assert_eq!(row(&t, 1), "  |[Az");
}

#[test]
fn repeat_last_character() {
    let mut t = term(2, 20);
    t.write(b"ab\x1b[3b\x1b[b");
    assert_eq!(row(&t, 0), "abbbb");
    t.write(b"\x1b[m\x1b[2b");
    assert_eq!(row(&t, 0), "abbbb");
    // At most a screenful: 40 more, scrolling once.
    t.write(b"c\x1b[99999b");
    assert_eq!(row(&t, 0), "cccccccccccccccccccc");
    assert_eq!(row(&t, 1), "cccccc");
}

#[test]
fn queries_and_parameters() {
    let mut t = Teken::new();
    let mut r = Rec::default();
    t.input(
        &mut r,
        b"\x1b[c\x1b[>c\x1b[1c\x07\x1b[?25l\x1b[?1000h\x1b[=3;100B\x1b[=1S\x1b[3z",
    );
    assert_eq!(r.responses, [&b"\x1b[?1;2c"[..], b"\x1b[>0;10;0c"]);
    assert_eq!(r.bells, 1);
    assert_eq!(
        r.params,
        [
            (Param::ShowCursor, 0),
            (Param::Mouse, 1),
            (Param::SetBellPd, (3 << 16) | 100),
            (Param::SetLocalCursor, 1),
            (Param::SwitchVt, 3),
        ]
    );
    assert_eq!(t.sequence(Key::Up), Some(&b"\x1b[A"[..]));
    t.input(&mut r, b"\x1b[?1h");
    assert_eq!(t.sequence(Key::Up), Some(&b"\x1bOA"[..]));
    assert_eq!(t.sequence(Key::F(4)), Some(&b"\x1b[15~"[..]));
    t.input(&mut r, b"\x1b[=1T");
    assert!(t.cons25());
    assert_eq!(t.sequence(Key::F(0)), Some(&b"\x1b[M"[..]));
    t.set_cons25keys();
    assert_eq!(t.sequence(Key::F(0)), None);
}

#[test]
fn cursor_visibility_follows_the_emulator() {
    let mut t = term(2, 4);
    assert!(t.buf.cursor_visible());
    t.write(b"\x1b[?25l");
    assert!(!t.buf.cursor_visible());
    t.write(b"\x1b[=0S");
    assert!(t.buf.cursor_visible());
    t.write(b"\x1b[=1S");
    assert!(!t.buf.cursor_visible());
    t.write(b"\x1bc");
    assert!(t.buf.cursor_visible());
}

#[test]
fn cons25_mode() {
    let mut t = term(3, 4);
    t.write(b"\x1b[=1Tabcd");
    // cons25 wraps at once.
    assert_eq!(t.emu.cursor(), Pos::new(1, 0));
    t.write(b"\x08x");
    assert_eq!(row(&t, 0), "abcx");
    t.write(b"\x0c");
    assert_eq!(row(&t, 0), "");
    t.write(b"\x1b[=4F\x1b[=9Gq");
    let q = cell(&t, 0, 0);
    assert_eq!(colors(q, false), (color::RED, color::BLUE | color::LIGHT));
    assert_eq!(t.emu.defattr_cons25(), (4, 9 & 7));
}

#[test]
fn damage_is_per_row() {
    let mut t = term(4, 10);
    t.buf.take_damage(|_, _, _| {});
    t.write(b"\x1b[2;3Hab");
    let mut d = Vec::new();
    t.buf.take_damage(|r, b, e| d.push((r, b, e)));
    // The cursor's old cell, the text, and the cursor's new cell.
    assert_eq!(d, [(0, 0, 1), (1, 2, 5)]);
    assert!(!t.buf.damaged());
    // Rewriting the same cell is no change, except for the cursor.
    t.write(b"\x1b[2;3Ha");
    d.clear();
    t.buf.take_damage(|r, b, e| d.push((r, b, e)));
    assert_eq!(d, [(1, 2, 5)]);
    // A full-screen scroll damages every row.
    t.write(b"\x1b[4;1H\n");
    d.clear();
    t.buf.take_damage(|r, b, e| d.push((r, b, e)));
    assert_eq!(d, [(0, 0, 10), (1, 0, 10), (2, 0, 10), (3, 0, 10)]);
}

#[test]
fn grow_keeps_the_newest_rows() {
    let mut t = Terminal::new(Pos::new(3, 4), 6);
    for i in 0..10 {
        t.write(alloc::format!("{i}\r\n").as_bytes());
    }
    assert_eq!(row(&t, 0), "8");
    t.buf.grow(Pos::new(3, 6), 20, &NORMAL);
    assert_eq!(t.buf.history_size(), 20);
    assert_eq!(row(&t, 0), "8");
    assert_eq!(row(&t, 1), "9");
    t.buf.set_scroll_mode(true);
    t.buf.seek(0, Seek::Set);
    assert_eq!(text(t.buf.view_row(0)), "5");
    t.buf.set_scroll_mode(false);
    t.buf.grow(Pos::new(3, 2), 3, &NORMAL);
    t.buf.seek(0, Seek::End);
    assert_eq!(row(&t, 0), "8");
    assert_eq!(t.buf.size(), Pos::new(3, 2));
}

#[test]
fn widths() {
    assert_eq!(width(0), 0);
    assert_eq!(width(0x1b), -1);
    assert_eq!(width(0x85), -1);
    assert_eq!(width(u32::from(b'a')), 1);
    assert_eq!(width(0x301), 0);
    assert_eq!(width(0xe0100), 0);
    assert_eq!(width(0x4e00), 2);
    assert_eq!(width(0x303f), 1);
    assert_eq!(width(0x1f600), 1);
}

#[test]
fn fuzzed_input_never_panics() {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    const PIECES: &[&[u8]] = &[
        b"\x1b[",
        b"\x1b[?",
        b"\x1b[=",
        b"\x1b[>",
        b"\x1b[ ",
        b"\x1b#",
        b"\x1b(",
        b"\x1b)",
        b"\x1b",
        b";",
        b"9999999999",
        b"0",
        b"1",
        b"\r",
        b"\n",
        b"\t",
        b"\x08",
        b"\x0e",
        b"\x0f",
        "漢".as_bytes(),
        b"\xe2",
        b"\x80",
        b"\xff",
        b"\x1b]",
        b"\x07",
    ];
    for round in 0..300 {
        let rows = (rnd() % 30 + 1) as u32;
        let cols = (rnd() % 170 + 1) as u32;
        let mut t = Terminal::new(Pos::new(rows, cols), (rnd() % 40) as u32);
        let mut bytes = Vec::new();
        for _ in 0..2000 {
            if rnd() % 3 == 0 {
                bytes.extend_from_slice(PIECES[(rnd() as usize) % PIECES.len()]);
            } else {
                bytes.push(
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz@`~"
                        [(rnd() % 55) as usize],
                );
            }
        }
        t.write(&bytes);
        t.buf.set_scroll_mode(round % 2 == 0);
        t.buf.seek((rnd() % 100) as i32 - 50, Seek::Cur);
        if round % 5 == 0 {
            t.buf.grow(
                Pos::new(rows / 2 + 1, cols + 3),
                (rnd() % 50) as u32,
                &NORMAL,
            );
            t.emu.set_winsize_noreset(t.buf.size());
            t.write(&bytes);
        }
        let c = t.emu.cursor();
        let w = t.emu.winsize();
        assert!(
            c.row < w.row && c.col < w.col,
            "round {round}: {c:?} in {w:?}"
        );
        let size = t.buf.size();
        t.buf
            .take_damage(|r, b, e| assert!(r < size.row && b < e && e <= size.col));
    }
}

/// The streams `tests/corpus/vtterm/teken-drive.c` feeds FreeBSD's
/// `teken`, with the screens it ended with: ours must end the same.
#[test]
fn teken_differential() {
    const DIGESTS: &str = include_str!("../../../tests/corpus/vtterm/teken-digests.txt");
    const PIECES: &[&[u8]] = &[
        b"\x1b[",
        b"\x1b[?",
        b"\x1b[=",
        b"\x1b[>",
        b"\x1b[ ",
        b"\x1b#",
        b"\x1b(",
        b"\x1b)",
        b"\x1b",
        b";",
        b"1",
        b"2",
        b"3",
        b"4",
        b"7",
        b"25",
        b"0",
        b"99",
        b"\r",
        b"\n",
        b"\t",
        b"\x08",
        b"\x0e",
        b"\x0f",
        b"\xe6\xbc\xa2",
        b"\xcc\x81",
        b"\xe2",
        b"\x80",
        b"\xff",
        b"\x1b]",
        b"\x07",
        b"m",
        b"H",
        b"J",
        b"K",
        b"r",
        b"h",
        b"l",
        b"@",
        b"P",
        b"L",
        b"M",
        b"X",
        b"b",
        b"S",
        b"T",
        b"Z",
        b"I",
        b"g",
        b"d",
        b"G",
        b"A",
        b"B",
        b"C",
        b"D",
        b"E",
        b"F",
        b"s",
        b"u",
        b"7",
        b"8",
        b"c",
        b"x",
    ];
    const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz@`~#q";
    let mut x = 0x0123_4567_89ab_cdefu64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut rounds = 0;
    for line in DIGESTS.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let rows = (rnd() % 30 + 1) as u32;
        let cols = (rnd() % 98 + 3) as u32;
        assert_eq!(
            (f[1], f[2]),
            (&*rows.to_string(), &*cols.to_string()),
            "{line}"
        );
        let mut t = term(rows, cols);
        let len = rnd() % 3000;
        let mut bytes = Vec::new();
        for _ in 0..len {
            if rnd() % 2 == 0 {
                bytes.extend_from_slice(PIECES[(rnd() % PIECES.len() as u64) as usize]);
            } else {
                bytes.push(LETTERS[(rnd() % LETTERS.len() as u64) as usize]);
            }
        }
        t.write(&bytes);
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for r in 0..rows {
            for &c in t.buf.screen_row(r) {
                for b in c.to_le_bytes() {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
        let c = t.emu.cursor();
        let ours = alloc::format!("{} {rows} {cols} {} {} {h:016x}", f[0], c.row, c.col);
        assert_eq!(ours, line, "stream {}", f[0]);
        rounds += 1;
    }
    assert_eq!(rounds, 1000);
}

/// Host timing of `vt.cat`'s lines through the emulator alone (not a
/// gate: `cargo test -p vtterm --release -- --ignored --nocapture`).
#[test]
#[ignore = "timing"]
fn time_cat_lines() {
    extern crate std;
    let mut lines = Vec::new();
    for l in 0..64 {
        for c in 0..80 {
            lines.push(b' ' + u8::try_from((l + c) % 95).unwrap());
        }
        lines.extend_from_slice(b"\r\n");
    }
    let mut t = term(48, 128);
    let start = std::time::Instant::now();
    let n = 20_000;
    for _ in 0..n {
        t.write(&lines);
    }
    let ns = start.elapsed().as_nanos() as f64 / f64::from(n * 64);
    std::println!("vtterm: {ns:.1} ns per 80-column line");
}
