// SPDX-License-Identifier: BSD-2-Clause
//! `teken`: UTF-8 decoding, the escape-sequence state machine its
//! `sequences` table generates, and the subroutines each sequence runs.
//!
//! Behavior reference: sys/teken/teken.c, teken_subr.h,
//! teken_subr_compat.h, teken_scs.h, sequences, gensequences.

use crate::{Attr, Char, Key, Param, Pos, Rect, Sink, color, format, width};

/// `t_stateflags`.
const FIRSTDIGIT: u32 = 0x0001;
const INSERT: u32 = 0x0002;
const AUTOWRAP: u32 = 0x0004;
const ORIGIN: u32 = 0x0008;
const WRAPPED: u32 = 0x0010;
const BIT8: u32 = 0x0020;
const CONS25: u32 = 0x0040;
const INSTRING: u32 = 0x0080;
const CURSORKEYS: u32 = 0x0100;
const CONS25KEYS: u32 = 0x0400;

/// `T_NUMSIZE`: arguments per sequence.
const NUMSIZE: usize = 8;
/// `T_NUMCOL`: columns with settable tab stops.
const NUMCOL: u32 = 160;
/// Words of tab-stop bits.
const TABWORDS: usize = 5;

const BLANK: Char = 0x20;
const ESC: Char = 0x1b;

/// The prefixes of the sequences: where the parser is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Init,
    /// `ESC`
    Esc,
    /// `ESC [`
    Csi,
    /// `ESC [ ?`
    CsiQuestion,
    /// `ESC [ >`
    CsiGreater,
    /// `ESC [ SP`
    CsiSpace,
    /// `ESC [ =`
    CsiEquals,
    /// `ESC #`
    EscHash,
    /// `ESC (`
    EscG0,
    /// `ESC )`
    EscG1,
}

impl State {
    /// Whether a sequence under this prefix takes arguments.
    fn numbers(self) -> bool {
        matches!(
            self,
            Self::Csi | Self::CsiQuestion | Self::CsiGreater | Self::CsiSpace | Self::CsiEquals
        )
    }
}

/// A character set (`teken_scs_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scs {
    UsAscii,
    SpecialGraphics,
    UkNational,
}

/// A span of rows, `begin` up to `end`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Span {
    begin: u32,
    end: u32,
}

/// VT100 line drawing as Unicode, for `` ` `` to `~`.
const BOX_UNICODE: [u16; 31] = [
    0x25c6, 0x2592, 0x2409, 0x240c, 0x240d, 0x240a, 0x00b0, 0x00b1, 0x2424, 0x240b, 0x2518, 0x2510,
    0x250c, 0x2514, 0x253c, 0x23ba, 0x23bb, 0x2500, 0x23bc, 0x23bd, 0x251c, 0x2524, 0x2534, 0x252c,
    0x2502, 0x2264, 0x2265, 0x03c0, 0x2260, 0x00a3, 0x00b7,
];

/// VT100 line drawing in ASCII, for 8-bit mode.
const BOX_8BIT: &[u8; 31] = b"??HFCL??NV+++++-----++++|??????";

/// cons25's color numbers to `teken`'s.
const CONS25_COLORS: [u8; 8] = [
    color::BLACK,
    color::BLUE,
    color::GREEN,
    color::CYAN,
    color::RED,
    color::MAGENTA,
    color::YELLOW,
    color::WHITE,
];
const CONS25_REVCOLORS: [u8; 8] = [0, 4, 2, 6, 1, 5, 3, 7];

/// The emulator (`teken_t`).
#[derive(Clone, Debug)]
pub struct Teken {
    state: State,
    flags: u32,
    nums: [u32; NUMSIZE],
    curnum: usize,

    cursor: Pos,
    curattr: Attr,
    saved_cursor: Pos,
    saved_curattr: Attr,

    defattr: Attr,
    winsize: Pos,

    /// DECSTBM.
    scrollreg: Span,
    /// DECOM.
    originreg: Span,

    tabstops: [u32; TABWORDS],

    utf8_left: u32,
    utf8_partial: Char,
    last: Char,

    curscs: usize,
    saved_curscs: Scs,
    scs: [Scs; 2],
}

impl Default for Teken {
    fn default() -> Self {
        Self::new()
    }
}

impl Teken {
    /// An 80×24 xterm, white on black (`teken_init`).
    #[must_use]
    pub fn new() -> Self {
        let mut t = Self {
            state: State::Init,
            flags: 0,
            nums: [0; NUMSIZE],
            curnum: 0,
            cursor: Pos::default(),
            curattr: Attr::default(),
            saved_cursor: Pos::default(),
            saved_curattr: Attr::default(),
            defattr: Attr {
                format: 0,
                fg: color::WHITE,
                bg: color::BLACK,
            },
            winsize: Pos::default(),
            scrollreg: Span::default(),
            originreg: Span::default(),
            tabstops: [0; TABWORDS],
            utf8_left: 0,
            utf8_partial: 0,
            last: 0,
            curscs: 0,
            saved_curscs: Scs::UsAscii,
            scs: [Scs::UsAscii; 2],
        };
        t.do_reset();
        t.set_winsize(Pos::new(24, 80));
        t
    }

    /// Runs `bytes` (`teken_input`).
    pub fn input<S: Sink>(&mut self, s: &mut S, bytes: &[u8]) {
        for &b in bytes {
            self.input_byte(s, b);
        }
    }

    #[must_use]
    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    /// Moves the cursor (`teken_set_cursor`); clamped to the window.
    pub fn set_cursor(&mut self, p: Pos) {
        self.cursor = Pos::new(
            p.row.min(self.winsize.row - 1),
            p.col.min(self.winsize.col - 1),
        );
    }

    #[must_use]
    pub fn curattr(&self) -> &Attr {
        &self.curattr
    }

    pub fn set_curattr(&mut self, a: &Attr) {
        self.curattr = *a;
    }

    #[must_use]
    pub fn defattr(&self) -> &Attr {
        &self.defattr
    }

    /// Sets the default rendition, and the current and saved ones with
    /// it (`teken_set_defattr`).
    pub fn set_defattr(&mut self, a: &Attr) {
        self.curattr = *a;
        self.saved_curattr = *a;
        self.defattr = *a;
    }

    /// The default colors as cons25 numbers, foreground then background
    /// (`teken_get_defattr_cons25`).
    #[must_use]
    pub fn defattr_cons25(&self) -> (u8, u8) {
        let mut fg = CONS25_REVCOLORS[usize::from(crate::color_256to8(self.defattr.fg))];
        if self.defattr.format & format::BOLD != 0 {
            fg += 8;
        }
        let bg = CONS25_REVCOLORS[usize::from(crate::color_256to8(self.defattr.bg))];
        (fg, bg)
    }

    #[must_use]
    pub fn winsize(&self) -> Pos {
        self.winsize
    }

    /// Resizes and resets (`teken_set_winsize`).
    pub fn set_winsize(&mut self, p: Pos) {
        let p = Pos::new(p.row.max(1), p.col.max(1));
        self.trim_cursor_pos(p);
        self.winsize = p;
        self.do_reset();
    }

    /// Resizes, keeping the state but the regions
    /// (`teken_set_winsize_noreset`).
    pub fn set_winsize_noreset(&mut self, p: Pos) {
        let p = Pos::new(p.row.max(1), p.col.max(1));
        self.trim_cursor_pos(p);
        self.winsize = p;
        self.do_resize();
    }

    /// Takes bytes as characters, without UTF-8 (`teken_set_8bit`).
    pub fn set_8bit(&mut self) {
        self.flags |= BIT8;
    }

    /// cons25 emulation (`teken_set_cons25`).
    pub fn set_cons25(&mut self) {
        self.flags |= CONS25;
    }

    /// cons25 function keys (`teken_set_cons25keys`).
    pub fn set_cons25keys(&mut self) {
        self.flags |= CONS25KEYS;
    }

    /// Whether cons25 emulation is on (`ESC [ = 1 T`).
    #[must_use]
    pub fn cons25(&self) -> bool {
        self.flags & CONS25 != 0
    }

    /// What a special key sends (`teken_get_sequence`); `None` when the
    /// keyboard's own strings stand.
    #[must_use]
    pub fn sequence(&self, k: Key) -> Option<&'static [u8]> {
        if self.flags & (CONS25 | CONS25KEYS) == CONS25 | CONS25KEYS {
            return None;
        }
        if self.flags & CONS25 != 0 {
            return Some(match k {
                Key::Up => b"\x1b[A",
                Key::Down => b"\x1b[B",
                Key::Left => b"\x1b[D",
                Key::Right => b"\x1b[C",
                Key::Home => b"\x1b[H",
                Key::End => b"\x1b[F",
                Key::Insert => b"\x1b[L",
                Key::Delete => b"\x7f",
                Key::PageUp => b"\x1b[I",
                Key::PageDown => b"\x1b[G",
                Key::F(n) => match n {
                    0 => b"\x1b[M",
                    1 => b"\x1b[N",
                    2 => b"\x1b[O",
                    3 => b"\x1b[P",
                    4 => b"\x1b[Q",
                    5 => b"\x1b[R",
                    6 => b"\x1b[S",
                    7 => b"\x1b[T",
                    8 => b"\x1b[U",
                    9 => b"\x1b[V",
                    10 => b"\x1b[W",
                    11 => b"\x1b[X",
                    _ => return None,
                },
                Key::BackTab => b"\x1b[Z",
            });
        }
        if self.flags & CURSORKEYS != 0 {
            match k {
                Key::Up => return Some(b"\x1bOA"),
                Key::Down => return Some(b"\x1bOB"),
                Key::Left => return Some(b"\x1bOD"),
                Key::Right => return Some(b"\x1bOC"),
                Key::Home => return Some(b"\x1bOH"),
                Key::End => return Some(b"\x1bOF"),
                _ => {}
            }
        }
        Some(match k {
            Key::Up => b"\x1b[A",
            Key::Down => b"\x1b[B",
            Key::Left => b"\x1b[D",
            Key::Right => b"\x1b[C",
            Key::Home => b"\x1b[H",
            Key::End => b"\x1b[F",
            Key::Insert => b"\x1b[2~",
            Key::Delete => b"\x1b[3~",
            Key::PageUp => b"\x1b[5~",
            Key::PageDown => b"\x1b[6~",
            Key::F(n) => match n {
                0 => b"\x1bOP",
                1 => b"\x1bOQ",
                2 => b"\x1bOR",
                3 => b"\x1bOS",
                4 => b"\x1b[15~",
                5 => b"\x1b[17~",
                6 => b"\x1b[18~",
                7 => b"\x1b[19~",
                8 => b"\x1b[20~",
                9 => b"\x1b[21~",
                10 => b"\x1b[23~",
                11 => b"\x1b[24~",
                _ => return None,
            },
            Key::BackTab => b"\x1b[Z",
        })
    }

    /// Keeps the cursor inside a window of `new`. `teken` leaves it be
    /// when either dimension grows, which can leave it outside the
    /// other; here each is trimmed.
    fn trim_cursor_pos(&mut self, new: Pos) {
        if self.cursor.row >= new.row {
            self.cursor.row = new.row - 1;
        }
        if self.cursor.col >= new.col {
            self.cursor.col = new.col - 1;
        }
    }

    fn input_byte<S: Sink>(&mut self, s: &mut S, c: u8) {
        if c & 0x80 == 0 || self.flags & BIT8 != 0 {
            self.utf8_left = 0;
            self.input_char(s, Char::from(c));
        } else if c & 0xe0 == 0xc0 {
            self.utf8_left = 1;
            self.utf8_partial = Char::from(c & 0x1f);
        } else if c & 0xf0 == 0xe0 {
            self.utf8_left = 2;
            self.utf8_partial = Char::from(c & 0x0f);
        } else if c & 0xf8 == 0xf0 {
            self.utf8_left = 3;
            self.utf8_partial = Char::from(c & 0x07);
        } else if c & 0xc0 == 0x80 {
            if self.utf8_left == 0 {
                return;
            }
            self.utf8_left -= 1;
            self.utf8_partial = (self.utf8_partial << 6) | Char::from(c & 0x3f);
            if self.utf8_left == 0 {
                self.input_char(s, self.utf8_partial);
            }
        }
    }

    fn input_char<S: Sink>(&mut self, s: &mut S, c: Char) {
        // DCS and OSC strings are dropped up to ESC or BEL.
        if self.flags & INSTRING != 0 {
            match c {
                ESC => self.flags &= !INSTRING,
                0x07 => {
                    self.flags &= !INSTRING;
                    return;
                }
                _ => return,
            }
        }
        match c {
            0 => {}
            0x07 => s.bell(),
            0x08 => self.backspace(s),
            0x0a | 0x0b => self.newline(s),
            0x0c => self.newpage(s),
            0x0e if self.flags & CONS25 == 0 => self.curscs = 1,
            0x0f if self.flags & CONS25 == 0 => self.curscs = 0,
            0x0d => self.carriage_return(s),
            0x09 => self.cursor_forward_tabulation(s, 1),
            _ => self.next(s, c),
        }
    }

    fn switch(&mut self, st: State) {
        self.state = st;
        self.curnum = 0;
        self.flags |= FIRSTDIGIT;
    }

    /// Takes a digit or `;` of an argument list (`teken_state_numbers`);
    /// false for anything else, after closing the list.
    fn numbers(&mut self, c: Char) -> bool {
        if (Char::from(b'0')..=Char::from(b'9')).contains(&c) {
            let d = c - Char::from(b'0');
            let n = &mut self.nums[self.curnum];
            if self.flags & FIRSTDIGIT != 0 {
                self.flags &= !FIRSTDIGIT;
                *n = d;
            } else if *n < u32::MAX / 100 {
                // Past the size of any screen, digits change nothing.
                *n = *n * 10 + d;
            }
            true
        } else if c == Char::from(b';') {
            if self.flags & FIRSTDIGIT != 0 {
                self.nums[self.curnum] = 0;
            }
            self.curnum += 1;
            if self.curnum == NUMSIZE {
                self.switch(State::Init);
                return true;
            }
            self.flags |= FIRSTDIGIT;
            true
        } else {
            if self.flags & FIRSTDIGIT != 0 && self.curnum > 0 {
                self.nums[self.curnum] = 0;
                self.curnum += 1;
            } else if self.flags & FIRSTDIGIT == 0 {
                self.curnum += 1;
            }
            false
        }
    }

    /// Argument `a` (1-based), with 0 or none taken as 1.
    fn n(&self, a: usize) -> u32 {
        if self.curnum < a || self.nums[a - 1] == 0 {
            1
        } else {
            self.nums[a - 1]
        }
    }

    /// Argument `a` (1-based), none taken as 0.
    fn r(&self, a: usize) -> u32 {
        if self.curnum < a { 0 } else { self.nums[a - 1] }
    }

    /// The state machine `gensequences` makes of the `sequences` table.
    fn next<S: Sink>(&mut self, s: &mut S, c: Char) {
        let st = self.state;
        if st == State::Init {
            if c == ESC {
                self.switch(State::Esc);
            } else {
                self.regular_character(s, c);
            }
            return;
        }
        if st.numbers() && self.numbers(c) {
            return;
        }
        let Ok(b) = u8::try_from(c) else {
            self.last = 0;
            self.switch(State::Init);
            return;
        };
        match st {
            State::Init => {}
            State::Esc => match b {
                b'[' => return self.switch(State::Csi),
                b'#' => return self.switch(State::EscHash),
                b'(' => return self.switch(State::EscG0),
                b')' => return self.switch(State::EscG1),
                b'7' => self.save_cursor(),
                b'8' => self.restore_cursor(s),
                b'=' => s.param(Param::KeypadApp, 1),
                b'>' => s.param(Param::KeypadApp, 0),
                b'D' => self.index(s),
                b'E' => self.next_line(s),
                b'H' => self.tab_set(self.cursor.col),
                b'M' => self.reverse_index(s),
                b'P' | b']' => self.flags |= INSTRING,
                b'\\' => {}
                b'c' => self.reset_to_initial_state(s),
                _ => {}
            },
            State::Csi => match b {
                b'?' => return self.switch(State::CsiQuestion),
                b'>' => return self.switch(State::CsiGreater),
                b' ' => return self.switch(State::CsiSpace),
                b'=' => return self.switch(State::CsiEquals),
                b'Z' => self.cursor_backward_tabulation(s, self.n(1)),
                b'I' => self.cursor_forward_tabulation(s, self.n(1)),
                b'E' => {
                    self.cursor.col = 0;
                    self.cursor_down(s, self.n(1));
                }
                b'F' => {
                    self.cursor.col = 0;
                    self.cursor_up(s, self.n(1));
                }
                b'n' => self.cursor_position_report(s, self.r(1), false),
                b'D' => self.cursor_backward(s, self.n(1)),
                b'B' | b'e' => self.cursor_down(s, self.n(1)),
                b'C' | b'a' => self.cursor_forward(s, self.n(1)),
                b'H' | b'f' => self.cursor_position(s, self.n(1), self.n(2)),
                b'A' => self.cursor_up(s, self.n(1)),
                b'c' => {
                    if self.r(1) == 0 {
                        s.respond(b"\x1b[?1;2c");
                    }
                }
                b'P' => self.delete_character(s, self.n(1)),
                b'u' => self.restore_cursor(s),
                b's' => self.save_cursor(),
                b'r' => self.set_top_and_bottom_margins(s, self.r(1), self.r(2)),
                b'M' => self.delete_line(s, self.n(1)),
                b'X' => self.erase_character(s, self.n(1)),
                b'J' => self.erase_display(s, self.r(1)),
                b'K' => self.erase_line(s, self.r(1)),
                b'G' | b'`' => self.horizontal_position_absolute(s, self.n(1)),
                b'@' => self.insert_character(s, self.n(1)),
                b'L' => self.insert_line(s, self.n(1)),
                b'l' => {
                    if self.r(1) == 4 {
                        self.flags &= !INSERT;
                    }
                }
                b'h' => {
                    if self.r(1) == 4 {
                        self.flags |= INSERT;
                    }
                }
                // SD ("pan up") and SU ("pan down").
                b'T' => self.do_scroll(s, -arg_i32(self.n(1))),
                b'S' => self.do_scroll(s, arg_i32(self.n(1))),
                b'm' => {
                    let (n, nums) = (self.curnum, self.nums);
                    self.set_graphic_rendition(&nums[..n]);
                }
                b'g' => self.tab_clear(self.r(1)),
                b'd' => self.vertical_position_absolute(s, self.n(1)),
                b'x' => {
                    // cons25 SGR: only 0, reset.
                    if self.r(1) == 0 {
                        self.curattr = self.defattr;
                    }
                }
                b'z' => s.param(Param::SwitchVt, self.r(1)),
                b'b' => self.repeat_last_graphic_char(s, self.n(1)),
                _ => {}
            },
            State::CsiQuestion => match b {
                b'l' => self.reset_dec_mode(s, self.r(1)),
                b'h' => self.set_dec_mode(s, self.r(1)),
                b'n' => self.cursor_position_report(s, self.r(1), true),
                _ => {}
            },
            State::CsiGreater => {
                if b == b'c' && self.r(1) == 0 {
                    s.respond(b"\x1b[>0;10;0c");
                }
            }
            // DECSCUSR, which `teken` doesn't implement.
            State::CsiSpace => {}
            State::CsiEquals => match b {
                b'B' => s.param(Param::SetBellPd, (self.r(1) << 16) | (self.r(2) & 0xffff)),
                b'A' => s.param(Param::SetBorder, self.r(1)),
                b'G' => {
                    let c = self.r(1);
                    let bg = cons25_color(c);
                    self.defattr.bg = bg;
                    self.curattr.bg = bg;
                }
                b'F' => {
                    let c = self.r(1);
                    let fg = cons25_color(c);
                    self.defattr.fg = fg;
                    self.curattr.fg = fg;
                }
                b'C' => self.cons25_set_global_cursor_shape(s),
                b'S' => s.param(Param::SetLocalCursor, self.r(1)),
                b'T' => match self.r(1) {
                    0 => self.flags &= !CONS25,
                    1 => self.flags |= CONS25,
                    _ => {}
                },
                _ => {}
            },
            State::EscHash => {
                // Double-height and double-width lines do nothing.
                if b == b'8' {
                    self.alignment_test(s);
                }
            }
            State::EscG0 | State::EscG1 => {
                let set = match b {
                    b'0' | b'2' => Some(Scs::SpecialGraphics),
                    b'1' | b'B' => Some(Scs::UsAscii),
                    b'A' => Some(Scs::UkNational),
                    _ => None,
                };
                if let Some(set) = set {
                    self.scs[usize::from(st == State::EscG1)] = set;
                }
            }
        }
        self.last = 0;
        self.switch(State::Init);
    }

    fn scs_process(&self, c: Char) -> Char {
        match self.scs[self.curscs] {
            Scs::UsAscii => c,
            Scs::SpecialGraphics => {
                if (Char::from(b'`')..=Char::from(b'~')).contains(&c) {
                    let i = ix(c - Char::from(b'`'));
                    if self.flags & BIT8 != 0 {
                        Char::from(BOX_8BIT[i])
                    } else {
                        Char::from(BOX_UNICODE[i])
                    }
                } else {
                    c
                }
            }
            Scs::UkNational => {
                if c == Char::from(b'#') {
                    if self.flags & BIT8 != 0 { 0x9c } else { 0xa3 }
                } else {
                    c
                }
            }
        }
    }

    // Tab stops.

    fn tab_isset(&self, col: u32) -> bool {
        if col >= NUMCOL {
            return col.is_multiple_of(8);
        }
        self.tabstops[ix(col / 32)] & (1 << (col % 32)) != 0
    }

    fn tab_clear_at(&mut self, col: u32) {
        if col < NUMCOL {
            self.tabstops[ix(col / 32)] &= !(1 << (col % 32));
        }
    }

    fn tab_set(&mut self, col: u32) {
        if col < NUMCOL {
            self.tabstops[ix(col / 32)] |= 1 << (col % 32);
        }
    }

    fn tab_default(&mut self) {
        self.tabstops = [0; TABWORDS];
        for i in (8..NUMCOL).step_by(8) {
            self.tab_set(i);
        }
    }

    fn tab_clear(&mut self, cmd: u32) {
        match cmd {
            0 => self.tab_clear_at(self.cursor.col),
            3 => self.tabstops = [0; TABWORDS],
            _ => {}
        }
    }

    // The subroutines (teken_subr.h).

    fn full_width(&self, row: u32, rows: u32) -> Rect {
        Rect::new(Pos::new(row, 0), Pos::new(rows, self.winsize.col))
    }

    /// Scrolls the scrolling region up by `amount` rows, or down by
    /// `-amount`.
    fn do_scroll<S: Sink>(&mut self, s: &mut S, amount: i32) {
        let (begin, end) = (self.scrollreg.begin, self.scrollreg.end);
        let a = amount.unsigned_abs();
        if amount > 0 {
            let first = if begin + a < end {
                s.copy(self.full_width(begin + a, end), Pos::new(begin, 0));
                end - a
            } else {
                begin
            };
            s.fill(self.full_width(first, end), BLANK, &self.curattr);
        } else if amount < 0 {
            let last = if begin + a < end {
                s.copy(self.full_width(begin, end - a), Pos::new(begin + a, 0));
                begin + a
            } else {
                end
            };
            s.fill(self.full_width(begin, last), BLANK, &self.curattr);
        }
    }

    fn cursor_position_report<S: Sink>(&self, s: &mut S, cmd: u32, dec: bool) {
        let mut out = [0u8; 32];
        let mut len = 0;
        let mut put = |b: &[u8]| {
            out[len..len + b.len()].copy_from_slice(b);
            len += b.len();
        };
        put(if dec { b"\x1b[?" } else { b"\x1b[" });
        match cmd {
            5 => put(b"0n"),
            6 => {
                let mut digits = [0u8; 10];
                put(decimal(
                    &mut digits,
                    self.cursor.row - self.originreg.begin + 1,
                ));
                put(b";");
                put(decimal(&mut digits, self.cursor.col + 1));
                put(b"R");
            }
            15 => put(b"13n"),
            25 => put(b"20n"),
            26 => put(b"27;1n"),
            _ => return,
        }
        s.respond(&out[..len]);
    }

    fn alignment_test<S: Sink>(&mut self, s: &mut S) {
        self.cursor = Pos::new(0, 0);
        self.scrollreg = Span {
            begin: 0,
            end: self.winsize.row,
        };
        self.originreg = self.scrollreg;
        self.flags &= !(WRAPPED | ORIGIN);
        s.cursor(self.cursor);
        s.fill(
            Rect::new(Pos::new(0, 0), self.winsize),
            Char::from(b'E'),
            &self.defattr,
        );
    }

    fn backspace<S: Sink>(&mut self, s: &mut S) {
        if self.flags & CONS25 != 0 {
            if self.cursor.col == 0 {
                if self.cursor.row == self.originreg.begin {
                    return;
                }
                self.cursor.row -= 1;
                self.cursor.col = self.winsize.col - 1;
            } else {
                self.cursor.col -= 1;
            }
        } else {
            if self.cursor.col == 0 {
                return;
            }
            self.cursor.col -= 1;
            self.flags &= !WRAPPED;
        }
        s.cursor(self.cursor);
    }

    fn carriage_return<S: Sink>(&mut self, s: &mut S) {
        self.cursor.col = 0;
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn cursor_backward<S: Sink>(&mut self, s: &mut S, ncols: u32) {
        self.cursor.col = self.cursor.col.saturating_sub(ncols);
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn cursor_backward_tabulation<S: Sink>(&mut self, s: &mut S, mut ntabs: u32) {
        loop {
            if self.cursor.col == 0 {
                break;
            }
            self.cursor.col -= 1;
            if self.tab_isset(self.cursor.col) {
                ntabs -= 1;
            }
            if ntabs == 0 {
                break;
            }
        }
        s.cursor(self.cursor);
    }

    fn cursor_down<S: Sink>(&mut self, s: &mut S, nrows: u32) {
        if self.cursor.row + nrows >= self.scrollreg.end {
            self.cursor.row = self.scrollreg.end - 1;
        } else {
            self.cursor.row += nrows;
        }
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn cursor_forward<S: Sink>(&mut self, s: &mut S, ncols: u32) {
        if self.cursor.col + ncols >= self.winsize.col {
            self.cursor.col = self.winsize.col - 1;
        } else {
            self.cursor.col += ncols;
        }
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn cursor_forward_tabulation<S: Sink>(&mut self, s: &mut S, mut ntabs: u32) {
        loop {
            if self.cursor.col == self.winsize.col - 1 {
                break;
            }
            self.cursor.col += 1;
            if self.tab_isset(self.cursor.col) {
                ntabs -= 1;
            }
            if ntabs == 0 {
                break;
            }
        }
        s.cursor(self.cursor);
    }

    fn cursor_position<S: Sink>(&mut self, s: &mut S, row: u32, col: u32) {
        let row = (row - 1) + self.originreg.begin;
        self.cursor.row = row.min(self.originreg.end - 1);
        self.cursor.col = (col - 1).min(self.winsize.col - 1);
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn cursor_up<S: Sink>(&mut self, s: &mut S, nrows: u32) {
        if self.scrollreg.begin + nrows >= self.cursor.row {
            self.cursor.row = self.scrollreg.begin;
        } else {
            self.cursor.row -= nrows;
        }
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn delete_character<S: Sink>(&mut self, s: &mut S, ncols: u32) {
        let row = self.cursor.row;
        let mut r = Rect::new(Pos::new(row, 0), Pos::new(row + 1, self.winsize.col));
        if self.cursor.col + ncols >= self.winsize.col {
            r.begin.col = self.cursor.col;
        } else {
            r.begin.col = self.cursor.col + ncols;
            s.copy(r, self.cursor);
            r.begin.col = self.winsize.col - ncols;
        }
        s.fill(r, BLANK, &self.curattr);
    }

    fn delete_line<S: Sink>(&mut self, s: &mut S, nrows: u32) {
        let row = self.cursor.row;
        if row < self.scrollreg.begin || row >= self.scrollreg.end {
            return;
        }
        let end = self.scrollreg.end;
        let first = if row + nrows >= end {
            row
        } else {
            s.copy(self.full_width(row + nrows, end), Pos::new(row, 0));
            end - nrows
        };
        s.fill(self.full_width(first, end), BLANK, &self.curattr);
    }

    fn erase_character<S: Sink>(&mut self, s: &mut S, ncols: u32) {
        let c = self.cursor;
        let end = if c.col + ncols >= self.winsize.col {
            self.winsize.col
        } else {
            c.col + ncols
        };
        s.fill(Rect::new(c, Pos::new(c.row + 1, end)), BLANK, &self.curattr);
    }

    fn erase_display<S: Sink>(&mut self, s: &mut S, mode: u32) {
        let (begin, end) = match mode {
            1 => {
                self.erase_line(s, 1);
                if self.cursor.row == 0 {
                    return;
                }
                (0, self.cursor.row)
            }
            2 => (0, self.winsize.row),
            _ => {
                self.erase_line(s, 0);
                if self.cursor.row == self.winsize.row - 1 {
                    return;
                }
                (self.cursor.row + 1, self.winsize.row)
            }
        };
        s.fill(self.full_width(begin, end), BLANK, &self.curattr);
    }

    fn erase_line<S: Sink>(&mut self, s: &mut S, mode: u32) {
        let c = self.cursor;
        let (begin, end) = match mode {
            1 => (0, c.col + 1),
            2 => (0, self.winsize.col),
            _ => (c.col, self.winsize.col),
        };
        s.fill(
            Rect::new(Pos::new(c.row, begin), Pos::new(c.row + 1, end)),
            BLANK,
            &self.curattr,
        );
    }

    fn horizontal_position_absolute<S: Sink>(&mut self, s: &mut S, col: u32) {
        self.cursor.col = (col - 1).min(self.winsize.col - 1);
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn index<S: Sink>(&mut self, s: &mut S) {
        if self.cursor.row < self.scrollreg.end - 1 {
            self.cursor.row += 1;
            self.flags &= !WRAPPED;
            s.cursor(self.cursor);
        } else {
            self.do_scroll(s, 1);
        }
    }

    fn insert_character<S: Sink>(&mut self, s: &mut S, ncols: u32) {
        let c = self.cursor;
        let mut r = Rect::new(c, Pos::new(c.row + 1, self.winsize.col));
        if c.col + ncols < self.winsize.col {
            r.end.col = self.winsize.col - ncols;
            s.copy(r, Pos::new(c.row, c.col + ncols));
            r.end.col = c.col + ncols;
        }
        s.fill(r, BLANK, &self.curattr);
    }

    fn insert_line<S: Sink>(&mut self, s: &mut S, nrows: u32) {
        let row = self.cursor.row;
        if row < self.scrollreg.begin || row >= self.scrollreg.end {
            return;
        }
        let end = self.scrollreg.end;
        let last = if row + nrows >= end {
            end
        } else {
            s.copy(self.full_width(row, end - nrows), Pos::new(row + nrows, 0));
            row + nrows
        };
        s.fill(self.full_width(row, last), BLANK, &self.curattr);
    }

    fn newline<S: Sink>(&mut self, s: &mut S) {
        self.cursor.row += 1;
        if self.cursor.row >= self.scrollreg.end {
            self.do_scroll(s, 1);
            self.cursor.row = self.scrollreg.end - 1;
        }
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn newpage<S: Sink>(&mut self, s: &mut S) {
        if self.flags & CONS25 != 0 {
            let r = Rect::new(
                Pos::new(self.originreg.begin, 0),
                Pos::new(self.originreg.end, self.winsize.col),
            );
            s.fill(r, BLANK, &self.curattr);
            self.cursor = Pos::new(self.originreg.begin, 0);
            self.flags &= !WRAPPED;
            s.cursor(self.cursor);
        } else {
            self.newline(s);
        }
    }

    fn next_line<S: Sink>(&mut self, s: &mut S) {
        self.cursor.col = 0;
        self.newline(s);
    }

    fn do_putchar<S: Sink>(&mut self, s: &mut S, p: Pos, c: Char, width: u32) {
        self.last = c;
        if self.flags & INSERT != 0 && p.col + width < self.winsize.col {
            // Insert mode: move the rest of the line right.
            let r = Rect::new(p, Pos::new(p.row + 1, self.winsize.col - width));
            s.copy(r, Pos::new(p.row, p.col + width));
        }
        s.putchar(p, c, &self.curattr);
        if width == 2 && p.col + 1 < self.winsize.col {
            let mut a = self.curattr;
            a.format |= format::CJK_RIGHT;
            s.putchar(Pos::new(p.row, p.col + 1), c, &a);
        }
    }

    fn regular_character<S: Sink>(&mut self, s: &mut S, c: Char) {
        let (c, width) = if self.flags & BIT8 != 0 {
            if self.flags & CONS25 == 0 && (c <= 0x1b || c == 0x7f) {
                return;
            }
            (self.scs_process(c), 1)
        } else {
            let c = self.scs_process(c);
            let Ok(w @ 1..) = u32::try_from(width(c)) else {
                // Zero-width characters aren't drawn.
                return;
            };
            (c, w)
        };

        if self.flags & CONS25 != 0 {
            self.do_putchar(s, self.cursor, c, width);
            self.cursor.col += width;
            if self.cursor.col >= self.winsize.col {
                if self.cursor.row == self.scrollreg.end - 1 {
                    self.do_scroll(s, 1);
                } else if self.cursor.row < self.winsize.row - 1 {
                    self.cursor.row += 1;
                }
                self.cursor.col = 0;
            }
        } else if self.flags & AUTOWRAP != 0
            && ((self.flags & WRAPPED != 0 && self.cursor.col + 1 == self.winsize.col)
                || self.cursor.col + width > self.winsize.col)
        {
            // Wrap: in the wrapped state at the last column, or when the
            // character doesn't fit.
            let row = if self.cursor.row == self.scrollreg.end - 1 {
                self.do_scroll(s, 1);
                self.scrollreg.end - 1
            } else {
                let row = self.cursor.row + 1;
                if row == self.winsize.row {
                    // Below the scrolling region, on the last row.
                    self.do_putchar(s, self.cursor, c, width);
                    return;
                }
                row
            };
            self.do_putchar(s, Pos::new(row, 0), c, width);
            self.cursor = Pos::new(row, width);
            self.flags &= !WRAPPED;
            if width >= self.winsize.col {
                // A window no wider than the character: `teken` would
                // leave the cursor outside it.
                self.flags |= WRAPPED;
                self.cursor.col = self.winsize.col - 1;
            }
        } else {
            self.do_putchar(s, self.cursor, c, width);
            self.cursor.col += width;
            if self.cursor.col >= self.winsize.col {
                self.flags |= WRAPPED;
                self.cursor.col = self.winsize.col - 1;
            } else {
                self.flags &= !WRAPPED;
            }
        }
        s.cursor(self.cursor);
    }

    fn reset_dec_mode<S: Sink>(&mut self, s: &mut S, cmd: u32) {
        match cmd {
            1 => self.flags &= !CURSORKEYS,
            3 => {
                s.param(Param::Cols132, 0);
                self.reset_to_initial_state(s);
            }
            6 => {
                self.flags &= !ORIGIN;
                self.originreg = Span {
                    begin: 0,
                    end: self.winsize.row,
                };
                self.cursor = Pos::new(0, 0);
                self.flags &= !WRAPPED;
                s.cursor(self.cursor);
            }
            7 => self.flags &= !AUTOWRAP,
            8 => s.param(Param::AutoRepeat, 0),
            25 => s.param(Param::ShowCursor, 0),
            1000 => s.param(Param::Mouse, 0),
            _ => {}
        }
    }

    fn set_dec_mode<S: Sink>(&mut self, s: &mut S, cmd: u32) {
        match cmd {
            1 => self.flags |= CURSORKEYS,
            3 => {
                s.param(Param::Cols132, 1);
                self.reset_to_initial_state(s);
            }
            6 => {
                self.flags |= ORIGIN;
                self.originreg = self.scrollreg;
                self.cursor = Pos::new(self.scrollreg.begin, 0);
                self.flags &= !WRAPPED;
                s.cursor(self.cursor);
            }
            7 => self.flags |= AUTOWRAP,
            8 => s.param(Param::AutoRepeat, 1),
            25 => s.param(Param::ShowCursor, 1),
            1000 => s.param(Param::Mouse, 1),
            _ => {}
        }
    }

    fn do_resize(&mut self) {
        self.scrollreg = Span {
            begin: 0,
            end: self.winsize.row,
        };
        self.originreg = self.scrollreg;
    }

    fn do_reset(&mut self) {
        self.curattr = self.defattr;
        self.cursor = Pos::new(0, 0);
        self.do_resize();
        self.flags &= BIT8 | CONS25 | CONS25KEYS;
        self.flags |= AUTOWRAP;
        self.scs = [Scs::UsAscii; 2];
        self.curscs = 0;
        self.save_cursor();
        self.tab_default();
    }

    fn reset_to_initial_state<S: Sink>(&mut self, s: &mut S) {
        self.do_reset();
        self.erase_display(s, 2);
        s.param(Param::ShowCursor, 1);
        s.cursor(self.cursor);
    }

    fn restore_cursor<S: Sink>(&mut self, s: &mut S) {
        self.cursor = self.saved_cursor;
        self.curattr = self.saved_curattr;
        self.scs[self.curscs] = self.saved_curscs;
        self.flags &= !WRAPPED;
        // Out of origin mode when the cursor lands outside its region.
        if self.cursor.row < self.originreg.begin || self.cursor.row >= self.originreg.end {
            self.flags &= !ORIGIN;
            self.originreg = Span {
                begin: 0,
                end: self.winsize.row,
            };
        }
        s.cursor(self.cursor);
    }

    fn reverse_index<S: Sink>(&mut self, s: &mut S) {
        if self.cursor.row > self.scrollreg.begin {
            self.cursor.row -= 1;
            self.flags &= !WRAPPED;
            s.cursor(self.cursor);
        } else {
            self.do_scroll(s, -1);
        }
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
        self.saved_curattr = self.curattr;
        self.saved_curscs = self.scs[self.curscs];
    }

    fn set_graphic_rendition(&mut self, cmds: &[u32]) {
        if cmds.is_empty() {
            self.curattr = self.defattr;
            return;
        }
        let mut i = 0;
        while i < cmds.len() {
            let n = cmds[i];
            match n {
                0 => self.curattr = self.defattr,
                1 => self.curattr.format |= format::BOLD,
                4 => self.curattr.format |= format::UNDERLINE,
                5 => self.curattr.format |= format::BLINK,
                7 => self.curattr.format |= format::REVERSE,
                22 => self.curattr.format &= !format::BOLD,
                24 => self.curattr.format &= !format::UNDERLINE,
                25 => self.curattr.format &= !format::BLINK,
                27 => self.curattr.format &= !format::REVERSE,
                30..=37 => self.curattr.fg = low_byte(n - 30),
                38 | 48 => {
                    if i + 2 < cmds.len() && cmds[i + 1] == 5 {
                        let c = low_byte(cmds[i + 2]);
                        if n == 38 {
                            self.curattr.fg = c;
                        } else {
                            self.curattr.bg = c;
                        }
                        i += 2;
                    }
                }
                39 => self.curattr.fg = self.defattr.fg,
                40..=47 => self.curattr.bg = low_byte(n - 40),
                49 => self.curattr.bg = self.defattr.bg,
                90..=97 => self.curattr.fg = low_byte(n - 90) + 8,
                100..=107 => self.curattr.bg = low_byte(n - 100) + 8,
                _ => {}
            }
            i += 1;
        }
    }

    fn set_top_and_bottom_margins<S: Sink>(&mut self, s: &mut S, top: u32, bottom: u32) {
        let mut top = top.saturating_sub(1);
        let mut bottom = if bottom == 0 || bottom > self.winsize.row {
            self.winsize.row
        } else {
            bottom
        };
        if top >= bottom - 1 {
            top = 0;
            bottom = self.winsize.row;
        }
        self.scrollreg = Span {
            begin: top,
            end: bottom,
        };
        if self.flags & ORIGIN != 0 {
            self.originreg = self.scrollreg;
        }
        self.cursor = Pos::new(self.originreg.begin, 0);
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn vertical_position_absolute<S: Sink>(&mut self, s: &mut S, row: u32) {
        let row = (row - 1) + self.originreg.begin;
        self.cursor.row = row.min(self.originreg.end - 1);
        self.flags &= !WRAPPED;
        s.cursor(self.cursor);
    }

    fn repeat_last_graphic_char<S: Sink>(&mut self, s: &mut S, rpts: u32) {
        let mut rpts = rpts.min(self.winsize.row * self.winsize.col);
        while self.last != 0 && rpts > 0 {
            self.regular_character(s, self.last);
            rpts -= 1;
        }
    }

    fn cons25_set_global_cursor_shape<S: Sink>(&mut self, s: &mut S) {
        // The arguments packed a byte each, then their count.
        let n = self.curnum;
        if n > 3 {
            return;
        }
        let mut code = 0u32;
        for i in (0..n).rev() {
            code = (code << 8) | (self.nums[i] & 0xff);
        }
        code = (code << 8) | u32::try_from(n).unwrap_or(0);
        s.param(Param::SetGlobalCursor, code);
    }
}

/// An argument as a row count; arguments stop growing well below
/// `i32::MAX`.
fn arg_i32(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn low_byte(n: u32) -> u8 {
    n.to_le_bytes()[0]
}

/// cons25's color `c` (`ESC [ = F`, `ESC [ = G`), with its bright bit.
fn cons25_color(c: u32) -> u8 {
    CONS25_COLORS[ix(c % 8)] | low_byte(c & 8)
}

/// `n` in decimal, in `buf`.
fn decimal(buf: &mut [u8; 10], mut n: u32) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + low_byte(n % 10);
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &buf[i..]
}

fn ix(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
