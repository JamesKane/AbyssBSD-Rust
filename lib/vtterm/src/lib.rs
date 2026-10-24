// SPDX-License-Identifier: BSD-2-Clause
//! The virtual terminal's emulator and screen, as FreeBSD's `vt` has
//! them: [`Teken`] is `teken` (the ECMA-48 parser with xterm's and
//! cons25's behavior), drawing through a [`Sink`]; [`Buffer`] is
//! `vt_buf` (the screen as cells in a ring of history rows, with the
//! damage since the last draw); and [`Terminal`] joins them as
//! `subr_terminal.c` and `vt_core.c` do for one window.
//!
//! Behavior reference: sys/teken (teken.c, teken_subr.h,
//! teken_subr_compat.h, teken_scs.h, the `sequences` table),
//! sys/dev/vt/vt_buf.c, sys/kern/subr_terminal.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod buf;
mod teken;
mod wcwidth;

#[cfg(test)]
mod tests;

pub use buf::{Buffer, DEFAULT_HISTORY, Seek};
pub use teken::Teken;
pub use wcwidth::width;

/// A character (`teken_char_t`): a Unicode code point.
pub type Char = u32;

/// A cell position (`teken_pos_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pos {
    pub row: u32,
    pub col: u32,
}

impl Pos {
    #[must_use]
    pub const fn new(row: u32, col: u32) -> Self {
        Self { row, col }
    }
}

/// Cells `begin` up to, not including, `end` in both directions
/// (`teken_rect_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub begin: Pos,
    pub end: Pos,
}

impl Rect {
    #[must_use]
    pub const fn new(begin: Pos, end: Pos) -> Self {
        Self { begin, end }
    }
}

/// `teken_format_t` bits.
pub mod format {
    pub const BOLD: u8 = 0x01;
    pub const UNDERLINE: u8 = 0x02;
    pub const BLINK: u8 = 0x04;
    pub const REVERSE: u8 = 0x08;
    /// The right half of a double-width character.
    pub const CJK_RIGHT: u8 = 0x10;
    pub const IMAGE: u8 = 0x20;
}

/// `teken_color_t`: the eight colors, [`color::LIGHT`] ORed in for the
/// bright ones; 16 to 255 are xterm's 256-color map.
pub mod color {
    pub const BLACK: u8 = 0;
    pub const RED: u8 = 1;
    pub const GREEN: u8 = 2;
    pub const YELLOW: u8 = 3;
    pub const BLUE: u8 = 4;
    pub const MAGENTA: u8 = 5;
    pub const CYAN: u8 = 6;
    pub const WHITE: u8 = 7;
    pub const LIGHT: u8 = 8;
}

/// A character's rendition (`teken_attr_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub format: u8,
    pub fg: u8,
    pub bg: u8,
}

/// `subr_terminal.c`'s `default_message`: light grey on black.
pub const NORMAL: Attr = Attr {
    format: 0,
    fg: color::WHITE,
    bg: color::BLACK,
};

/// `subr_terminal.c`'s `kernel_message`: bright white on black.
pub const KERNEL: Attr = Attr {
    format: format::BOLD,
    fg: color::WHITE | color::LIGHT,
    bg: color::BLACK,
};

/// What `teken` asks of the terminal beyond drawing (`TP_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    ShowCursor,
    KeypadApp,
    AutoRepeat,
    SwitchVt,
    Cols132,
    /// Pitch in the high 16 bits, duration in the low.
    SetBellPd,
    Mouse,
    SetBorder,
    SetLocalCursor,
    SetGlobalCursor,
}

/// Where `teken` draws (`teken_funcs_t`).
pub trait Sink {
    fn bell(&mut self);
    fn cursor(&mut self, p: Pos);
    fn putchar(&mut self, p: Pos, c: Char, a: &Attr);
    fn fill(&mut self, r: Rect, c: Char, a: &Attr);
    fn copy(&mut self, r: Rect, p: Pos);
    fn param(&mut self, cmd: Param, value: u32);
    /// An answer to a query (DA, DSR); `vt` drops them.
    fn respond(&mut self, _bytes: &[u8]) {}
}

/// A cell as `vt` stores it (`term_char_t`): the character in bits 0 to
/// 20, the format in 21 to 25, the foreground in 26 to 28, and the
/// background in 29 to 31.
pub type Cell = u32;

/// The character of a cell.
#[must_use]
pub const fn cell_char(c: Cell) -> Char {
    c & 0x1f_ffff
}

/// The format bits of a cell.
#[must_use]
pub const fn cell_format(c: Cell) -> u8 {
    ((c >> 21) & 0x1f).to_le_bytes()[0]
}

/// The foreground color of a cell, 0 to 7.
#[must_use]
pub const fn cell_fg(c: Cell) -> u8 {
    ((c >> 26) & 7).to_le_bytes()[0]
}

/// The background color of a cell, 0 to 7.
#[must_use]
pub const fn cell_bg(c: Cell) -> u8 {
    ((c >> 29) & 7).to_le_bytes()[0]
}

/// A cell from a character and a rendition (`TCHAR_CREATE`): colors
/// past 16 become their nearest of the 16, a bright foreground becomes
/// bold, and a bright background becomes blink.
#[must_use]
pub fn pack(c: Char, a: &Attr) -> Cell {
    let fg = to16(a.fg);
    let bg = to16(a.bg);
    (c & 0x1f_ffff)
        | (u32::from(a.format & 0x1f) << 21)
        | (u32::from(fg & 7) << 26)
        | (u32::from(fg & 8) << 18)
        | (u32::from(bg & 7) << 29)
        | (u32::from(bg & 8) << 20)
}

fn to16(c: u8) -> u8 {
    if c >= 16 { color_256to16(c) } else { c }
}

/// The colors a cell is drawn in, foreground then background, 0 to 15
/// (`vt_determine_colors`): bold brightens the foreground, blink the
/// background, and reverse (or the cursor) swaps them.
#[must_use]
pub fn colors(c: Cell, cursor: bool) -> (u8, u8) {
    let f = cell_format(c);
    let mut fg = cell_fg(c);
    if f & format::BOLD != 0 {
        fg |= color::LIGHT;
    }
    let mut bg = cell_bg(c);
    if f & format::BLINK != 0 {
        bg |= color::LIGHT;
    }
    if (f & format::REVERSE != 0) != cursor {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

const K: u8 = color::BLACK;
const R: u8 = color::RED;
const G: u8 = color::GREEN;
const Y: u8 = color::YELLOW;
const B: u8 = color::BLUE;
const M: u8 = color::MAGENTA;
const C: u8 = color::CYAN;
const W: u8 = color::WHITE;
const LK: u8 = K | color::LIGHT;
const LR: u8 = R | color::LIGHT;
const LG: u8 = G | color::LIGHT;
const LY: u8 = Y | color::LIGHT;
const LB: u8 = B | color::LIGHT;
const LM: u8 = M | color::LIGHT;
const LC: u8 = C | color::LIGHT;
const LW: u8 = W | color::LIGHT;

/// xterm's 256 colors to the eight (`teken_256to8tab`).
#[rustfmt::skip]
const TO8: [u8; 256] = [
    K, R, G, Y, B, M, C, W,
    K, R, G, Y, B, M, C, W,
    // Red 0.
    K, B, B, B, B, B,  G, C, C, B, B, B,  G, C, C, C, B, B,
    G, G, C, C, C, B,  G, G, G, C, C, C,  G, G, G, G, C, C,
    // Red 2.
    R, M, M, B, B, B,  Y, K, B, B, B, B,  Y, G, C, C, B, B,
    G, G, C, C, C, B,  G, G, G, C, C, C,  G, G, G, G, C, C,
    // Red 3.
    R, M, M, M, B, B,  Y, R, M, M, B, B,  Y, Y, W, B, B, B,
    Y, Y, G, C, C, B,  G, G, G, C, C, C,  G, G, G, G, C, C,
    // Red 4.
    R, R, M, M, M, B,  R, R, M, M, M, B,  Y, Y, R, M, M, B,
    Y, Y, Y, W, B, B,  Y, Y, Y, G, C, C,  G, G, G, G, C, C,
    // Red 5.
    R, R, R, M, M, M,  R, R, R, M, M, M,  R, R, R, M, M, M,
    Y, Y, Y, R, M, M,  Y, Y, Y, Y, W, B,  Y, Y, Y, Y, G, C,
    // Red 6.
    R, R, R, R, M, M,  R, R, R, R, M, M,  R, R, R, R, M, M,
    R, R, R, R, M, M,  Y, Y, Y, Y, R, M,  Y, Y, Y, Y, Y, W,
    // Grey.
    K, K, K, K, K, K,  K, K, K, K, K, K,
    W, W, W, W, W, W,  W, W, W, W, W, W,
];

/// xterm's 256 colors to the 16 (`teken_256to16tab`).
#[rustfmt::skip]
const TO16: [u8; 256] = [
    K, R, G, Y, B, M, C, W,
    LK, LR, LG, LY, LB, LM, LC, LW,
    // Red 0.
    K, B, B, B, B, B,  G, C, C, B, B, B,  G, C, C, C, B, B,
    G, G, C, C, C, B,  G, G, G, C, C, C,  G, G, G, G, C, C,
    // Red 2.
    R, M, M, B, B, B,  Y, LK, B, B, LB, LB,  Y, G, C, C, LB, LB,
    G, G, C, C, LC, LB,  G, LG, LG, LC, LC, LC,  G, LG, LG, LG, LC, LC,
    // Red 3.
    R, M, M, M, B, B,  Y, R, M, M, LB, LB,  Y, Y, W, LB, LB, LB,
    Y, Y, LG, LC, LC, LB,  G, LG, LG, LC, LC, LC,  G, LG, LG, LG, LC, LC,
    // Red 4.
    R, R, M, M, M, B,  R, R, M, M, LM, LB,  Y, Y, LR, LM, LM, LB,
    Y, Y, LY, LW, LB, LB,  Y, LY, LY, LG, LC, LC,  G, LG, LG, LG, LC, LC,
    // Red 5.
    R, R, R, M, M, M,  R, LR, LR, LM, LM, LM,  R, LR, LR, LM, LM, LM,
    Y, LY, LY, LR, LM, LM,  Y, LY, LY, LY, LW, LB,  Y, LY, LY, LY, LG, LC,
    // Red 6.
    R, R, R, R, M, M,  R, LR, LR, LR, LM, LM,  R, LR, LR, LR, LM, LM,
    R, LR, LR, LR, LM, LM,  Y, LY, LY, LY, LR, LM,  Y, LY, LY, LY, LY, LW,
    // Grey.
    K, K, K, K, K, K,  LK, LK, LK, LK, LK, LK,
    W, W, W, W, W, W,  LW, LW, LW, LW, LW, LW,
];

/// The nearest of the eight colors (`teken_256to8`).
#[must_use]
pub fn color_256to8(c: u8) -> u8 {
    TO8[usize::from(c)]
}

/// The nearest of the 16 colors (`teken_256to16`).
#[must_use]
pub fn color_256to16(c: u8) -> u8 {
    TO16[usize::from(c)]
}

/// The keys `teken` has strings for (`TKEY_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    /// F1 to F12, 0-based.
    F(u8),
    BackTab,
}

/// A terminal: the emulator and its screen, with what the emulator
/// asked of the window (`struct terminal` and `vtterm_*`).
#[derive(Debug)]
pub struct Terminal {
    pub emu: Teken,
    pub buf: Buffer,
    /// A bell is due (`TF_BELL`).
    pub bell: bool,
    /// `vw_mouse_level` (DEC mode 1000).
    pub mouse_level: u32,
    /// `vw_bell_pitch` and the duration in ticks (cons25 `ESC [ = B`).
    pub bell_pitch: u32,
    pub bell_duration: u32,
}

impl Terminal {
    /// A terminal of `size` with `history` rows of history, blank in the
    /// normal rendition (`terminal_alloc`, `vtbuf_init`,
    /// `terminal_set_winsize`).
    #[must_use]
    pub fn new(size: Pos, history: u32) -> Self {
        let mut emu = Teken::new();
        emu.set_defattr(&NORMAL);
        let mut t = Self {
            emu,
            buf: Buffer::new(size, history, &NORMAL),
            bell: false,
            mouse_level: 0,
            bell_pitch: 0,
            bell_duration: 0,
        };
        t.emu.set_winsize(size);
        let blank = pack(u32::from(b' '), &NORMAL);
        t.buf.fill(Rect::new(Pos::new(0, 0), t.buf.size()), blank);
        t
    }

    /// Runs `bytes` through the emulator (`termtty_outwakeup`).
    pub fn write(&mut self, bytes: &[u8]) {
        let Self {
            emu,
            buf,
            bell,
            mouse_level,
            bell_pitch,
            bell_duration,
        } = self;
        let mut out = Window {
            buf,
            bell,
            mouse_level,
            bell_pitch,
            bell_duration,
        };
        emu.input(&mut out, bytes);
    }

    /// Writes kernel output in the kernel rendition (`termcn_cnputc`).
    pub fn write_kernel(&mut self, bytes: &[u8]) {
        let saved = *self.emu.curattr();
        self.emu.set_curattr(&KERNEL);
        self.write(bytes);
        self.emu.set_curattr(&saved);
    }
}

/// One window's half of `vtterm_*`: the screen, and the parameters
/// `vtterm_param` keeps.
struct Window<'a> {
    buf: &'a mut Buffer,
    bell: &'a mut bool,
    mouse_level: &'a mut u32,
    bell_pitch: &'a mut u32,
    bell_duration: &'a mut u32,
}

impl Sink for Window<'_> {
    fn bell(&mut self) {
        *self.bell = true;
    }

    fn cursor(&mut self, p: Pos) {
        self.buf.cursor_position(p);
    }

    fn putchar(&mut self, p: Pos, c: Char, a: &Attr) {
        self.buf.putchar(p, pack(c, a));
    }

    fn fill(&mut self, r: Rect, c: Char, a: &Attr) {
        self.buf.fill(r, pack(c, a));
    }

    fn copy(&mut self, r: Rect, p: Pos) {
        self.buf.copy(r, p);
    }

    fn param(&mut self, cmd: Param, value: u32) {
        match cmd {
            // syscons' 0 (normal) and 2 (blinking) both show it; 1 hides.
            Param::SetLocalCursor => self.buf.cursor_visibility(value != 1),
            Param::ShowCursor => self.buf.cursor_visibility(value != 0),
            Param::Mouse => *self.mouse_level = value,
            Param::SetBellPd => {
                *self.bell_pitch = value >> 16;
                *self.bell_duration = value & 0xffff;
            }
            _ => {}
        }
    }
}
