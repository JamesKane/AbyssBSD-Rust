// SPDX-License-Identifier: BSD-2-Clause
//! An original 8x16 bitmap font for the framebuffer text console.
//!
//! Every glyph was drawn for this project; none is derived from an existing
//! font. The bitmaps live in [`art`] as human-readable ASCII art and are
//! parsed into row bitmaps during constant evaluation, so a malformed glyph is
//! a compile error rather than a runtime surprise.
//!
//! Metrics: the cell is [`WIDTH`] x [`HEIGHT`] pixels. Glyphs use columns 0..=6
//! and leave column 7 blank as inter-character spacing. Capitals and digits
//! occupy rows 2..=11 (row 11 is the baseline row), the x-height starts at row
//! 5, descenders reach row 14, and row 15 is always blank so that lines of text
//! never touch. Box-drawing glyphs are the exception: they fill the whole cell
//! edge to edge so that adjacent cells join up.

#![no_std]
#![forbid(unsafe_code)]

mod art;

/// Glyph width in pixels.
pub const WIDTH: usize = 8;
/// Glyph height in pixels.
pub const HEIGHT: usize = 16;

/// A glyph bitmap: one byte per row, top to bottom, MSB = leftmost pixel.
pub type Bitmap = [u8; HEIGHT];

/// First code point of the dense ASCII table.
const ASCII_FIRST: u32 = 0x20;
/// Number of printable ASCII glyphs (0x20..=0x7E).
const ASCII_COUNT: usize = 95;
/// Number of non-ASCII extras.
const EXTRA_COUNT: usize = 14;

static ASCII: [Bitmap; ASCII_COUNT] = ascii_table();
static EXTRA: ([u32; EXTRA_COUNT], [Bitmap; EXTRA_COUNT]) = parse_set(art::EXTRA);
static REPLACEMENT: Bitmap = replacement();

/// Bitmap rows for `c`, MSB = leftmost pixel; the replacement glyph for
/// unknown chars.
pub fn glyph(c: char) -> &'static Bitmap {
    lookup(c).unwrap_or(&REPLACEMENT)
}

/// Whether the font has a real glyph (not the replacement) for `c`.
pub fn has_glyph(c: char) -> bool {
    lookup(c).is_some()
}

/// The glyph drawn for characters the font does not cover.
pub fn replacement_glyph() -> &'static Bitmap {
    &REPLACEMENT
}

fn lookup(c: char) -> Option<&'static Bitmap> {
    let cp = u32::from(c);
    if let Some(i) = cp.checked_sub(ASCII_FIRST)
        && let Some(g) = usize::try_from(i).ok().and_then(|i| ASCII.get(i))
    {
        return Some(g);
    }
    let (codes, glyphs) = &EXTRA;
    codes.iter().position(|&x| x == cp).map(|i| &glyphs[i])
}

const fn ascii_table() -> [Bitmap; ASCII_COUNT] {
    let (codes, glyphs) = parse_set::<ASCII_COUNT>(art::ASCII);
    let mut i = 0;
    let mut expected = ASCII_FIRST;
    while i < ASCII_COUNT {
        if codes[i] != expected {
            panic!("font: ASCII glyphs must be listed in order from 0x20 to 0x7E");
        }
        i += 1;
        expected += 1;
    }
    glyphs
}

const fn replacement() -> Bitmap {
    let (codes, glyphs) = parse_set::<1>(art::REPLACEMENT);
    if codes[0] != 0xFFFD {
        panic!("font: the replacement glyph must be labelled [U+FFFD]");
    }
    glyphs[0]
}

/// Returns the next line of `s` at or after `pos`, with leading and trailing
/// blanks removed, as `(start, end, next_pos)`; blank lines are skipped.
/// `None` at end of input.
const fn next_line(s: &[u8], mut pos: usize) -> Option<(usize, usize, usize)> {
    while pos < s.len() {
        let mut end = pos;
        while end < s.len() && s[end] != b'\n' {
            end += 1;
        }
        let next = if end < s.len() { end + 1 } else { end };
        let mut a = pos;
        let mut b = end;
        while a < b && is_blank(s[a]) {
            a += 1;
        }
        while b > a && is_blank(s[b - 1]) {
            b -= 1;
        }
        if a < b {
            return Some((a, b, next));
        }
        pos = next;
    }
    None
}

const fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r')
}

/// Parses a glyph header line: `[c]` for a single ASCII character or
/// `[U+XXXX]` for any code point.
const fn parse_header(s: &[u8], a: usize, b: usize) -> u32 {
    let len = b - a;
    if len < 3 || s[a] != b'[' || s[b - 1] != b']' {
        panic!("font: expected a glyph header like [A] or [U+2500]");
    }
    if len == 3 {
        let c = s[a + 1];
        if c < 0x20 || c > 0x7E {
            panic!("font: a one-character header must be printable ASCII");
        }
        return c as u32;
    }
    if len < 5 || s[a + 1] != b'U' || s[a + 2] != b'+' || len > 10 {
        panic!("font: malformed [U+XXXX] header");
    }
    let mut cp: u32 = 0;
    let mut i = a + 3;
    while i < b - 1 {
        let d = match s[i] {
            c @ b'0'..=b'9' => c - b'0',
            c @ b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("font: [U+XXXX] header needs uppercase hex digits"),
        };
        cp = cp * 16 + d as u32;
        i += 1;
    }
    if char::from_u32(cp).is_none() {
        panic!("font: header is not a Unicode scalar value");
    }
    cp
}

/// Parses one 8-pixel row: `#` is on, `.` is off, MSB = leftmost.
const fn parse_row(s: &[u8], a: usize, b: usize) -> u8 {
    if b - a != WIDTH {
        panic!("font: every glyph row must be exactly 8 pixels wide");
    }
    let mut bits = 0u8;
    let mut i = 0;
    while i < WIDTH {
        bits <<= 1;
        match s[a + i] {
            b'#' => bits |= 1,
            b'.' => {}
            _ => panic!("font: glyph rows may only contain '#' and '.'"),
        }
        i += 1;
    }
    bits
}

/// Parses exactly `N` glyphs (a header line followed by 16 rows each) from
/// `art`. Blank lines and indentation are ignored.
const fn parse_set<const N: usize>(art: &str) -> ([u32; N], [Bitmap; N]) {
    let s = art.as_bytes();
    let mut codes = [0u32; N];
    let mut glyphs = [[0u8; HEIGHT]; N];
    let mut pos = 0;
    let mut n = 0;
    while let Some((a, b, next)) = next_line(s, pos) {
        if n == N {
            panic!("font: more glyphs than the table holds");
        }
        let cp = parse_header(s, a, b);
        let mut k = 0;
        while k < n {
            if codes[k] == cp {
                panic!("font: duplicate glyph");
            }
            k += 1;
        }
        codes[n] = cp;
        pos = next;
        let mut row = 0;
        while row < HEIGHT {
            let Some((a, b, next)) = next_line(s, pos) else {
                panic!("font: glyph has fewer than 16 rows");
            };
            if s[a] == b'[' {
                panic!("font: glyph has fewer than 16 rows");
            }
            glyphs[n][row] = parse_row(s, a, b);
            pos = next;
            row += 1;
        }
        n += 1;
    }
    if n != N {
        panic!("font: fewer glyphs than the table holds");
    }
    (codes, glyphs)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::String;
    use std::vec::Vec;

    fn printable() -> impl Iterator<Item = char> {
        (0x20u8..=0x7E).map(char::from)
    }

    fn render(text: &str) -> String {
        let mut out = String::new();
        for row in 0..HEIGHT {
            for c in text.chars() {
                let bits = glyph(c)[row];
                for col in 0..WIDTH {
                    out.push(if bits & (0x80 >> col) != 0 { '#' } else { '.' });
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn ascii_glyphs_present_and_distinct() {
        let all: Vec<char> = printable().collect();
        assert_eq!(all.len(), ASCII_COUNT);
        for (i, &a) in all.iter().enumerate() {
            assert!(has_glyph(a), "{a:?} missing");
            assert_ne!(glyph(a), replacement_glyph(), "{a:?} is the replacement");
            for &b in &all[i + 1..] {
                assert_ne!(glyph(a), glyph(b), "{a:?} and {b:?} are identical");
            }
        }
    }

    #[test]
    fn space_is_blank_and_others_are_not() {
        assert_eq!(glyph(' '), &[0u8; HEIGHT]);
        for c in printable().filter(|&c| c != ' ') {
            assert!(glyph(c).iter().any(|&r| r != 0), "{c:?} is blank");
        }
    }

    #[test]
    fn last_row_and_column_blank() {
        for c in printable() {
            assert_eq!(glyph(c)[HEIGHT - 1], 0, "{c:?} uses row 15");
            assert!(glyph(c).iter().all(|&r| r & 1 == 0), "{c:?} uses column 7");
        }
        assert_eq!(replacement_glyph()[HEIGHT - 1], 0);
    }

    #[test]
    fn unknown_chars_get_replacement() {
        for c in ['\0', '\n', '\u{7F}', '\u{E9}', '\u{FFFD}', '\u{1F600}'] {
            assert!(!has_glyph(c));
            assert_eq!(glyph(c), replacement_glyph());
        }
    }

    #[test]
    fn extras_present_and_distinct() {
        let (codes, glyphs) = &EXTRA;
        for (i, &cp) in codes.iter().enumerate() {
            let c = char::from_u32(cp).unwrap();
            assert!(has_glyph(c));
            assert_eq!(glyph(c), &glyphs[i]);
            assert_ne!(glyph(c), replacement_glyph());
            for c2 in printable() {
                assert_ne!(glyph(c), glyph(c2));
            }
        }
    }

    #[test]
    fn box_drawing_joins_at_cell_edges() {
        // Horizontal arms reach both side edges; vertical arms reach top and bottom.
        assert_eq!(glyph('─')[7], 0xFF);
        assert!(glyph('│').iter().all(|&r| r == 0x10));
        assert_eq!(glyph('┼')[0], 0x10);
        assert_eq!(glyph('┼')[HEIGHT - 1], 0x10);
    }

    #[test]
    fn parser_examples() {
        let (codes, glyphs) = parse_set::<1>(
            "[U+41]\n#.......\n.#......\n..#.....\n...#....\n....#...\n.....#..\n\
             ......#.\n.......#\n########\n........\n........\n........\n\
             ........\n........\n........\n#......#\n",
        );
        assert_eq!(codes[0], 0x41);
        assert_eq!(glyphs[0][0], 0x80);
        assert_eq!(glyphs[0][7], 0x01);
        assert_eq!(glyphs[0][8], 0xFF);
        assert_eq!(glyphs[0][15], 0x81);
    }

    /// Visual review: `cargo test -p font -- --nocapture render`.
    #[test]
    fn render_sample() {
        let sample = "The quick brown fox jumps over the lazy dog 0123456789 Il1| O0 {}[]()<>";
        let chars: Vec<char> = sample.chars().collect();
        for line in chars.chunks(18) {
            let s: String = line.iter().collect();
            std::println!("{}", render(&s));
        }
    }

    /// Visual review of every glyph, 16 per line.
    #[test]
    fn render_all() {
        let mut chars: Vec<char> = printable().collect();
        chars.extend(EXTRA.0.iter().map(|&cp| char::from_u32(cp).unwrap()));
        chars.push('\u{FFFD}');
        for line in chars.chunks(16) {
            let s: String = line.iter().collect();
            std::println!("{}", render(&s));
        }
    }
}
