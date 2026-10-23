// SPDX-License-Identifier: BSD-2-Clause
//! The default US keymap, keyed by HID keyboard usage, with what FreeBSD's
//! default keymap sends for each key (sys/dev/kbd/kbdtables.h's values:
//! Backspace is `^H`, Enter `^M`, Control with a letter its control
//! character), and the function and cursor keys as `teken` sends them in
//! xterm mode.

/// Modifier and lock state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    /// A Shift key is down.
    pub shift: bool,
    /// A Control key is down.
    pub ctrl: bool,
    /// An Alt key is down.
    pub alt: bool,
    /// Caps Lock is on.
    pub caps: bool,
    /// Num Lock is on.
    pub num: bool,
}

/// What a key does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing (an unmapped key).
    None,
    /// Sends one byte.
    Byte(u8),
    /// Sends a sequence.
    Seq(&'static [u8]),
    /// A modifier: held, not sent.
    Modifier(Modifier),
    /// A lock key: toggles.
    Lock(Lock),
    /// Alt-F*n*: switch to virtual terminal *n* (0-based).
    Switch(u8),
}

/// Modifier keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    /// Shift.
    Shift,
    /// Control.
    Ctrl,
    /// Alt.
    Alt,
    /// The GUI ("Windows") key: held, does nothing.
    Gui,
}

/// Lock keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    /// Caps Lock.
    Caps,
    /// Num Lock.
    Num,
    /// Scroll Lock.
    Scroll,
}

/// Letters and digits, with their shifted forms, from usage 0x04.
const MAIN: &[(u8, u8)] = &[
    (b'a', b'A'),
    (b'b', b'B'),
    (b'c', b'C'),
    (b'd', b'D'),
    (b'e', b'E'),
    (b'f', b'F'),
    (b'g', b'G'),
    (b'h', b'H'),
    (b'i', b'I'),
    (b'j', b'J'),
    (b'k', b'K'),
    (b'l', b'L'),
    (b'm', b'M'),
    (b'n', b'N'),
    (b'o', b'O'),
    (b'p', b'P'),
    (b'q', b'Q'),
    (b'r', b'R'),
    (b's', b'S'),
    (b't', b'T'),
    (b'u', b'U'),
    (b'v', b'V'),
    (b'w', b'W'),
    (b'x', b'X'),
    (b'y', b'Y'),
    (b'z', b'Z'),
    (b'1', b'!'),
    (b'2', b'@'),
    (b'3', b'#'),
    (b'4', b'$'),
    (b'5', b'%'),
    (b'6', b'^'),
    (b'7', b'&'),
    (b'8', b'*'),
    (b'9', b'('),
    (b'0', b')'),
];

/// Punctuation from usage 0x2d.
const PUNCT: &[(u8, u8)] = &[
    (b'-', b'_'),
    (b'=', b'+'),
    (b'[', b'{'),
    (b']', b'}'),
    (b'\\', b'|'),
    (b'\\', b'|'),
    (b';', b':'),
    (b'\'', b'"'),
    (b'`', b'~'),
    (b',', b'<'),
    (b'.', b'>'),
    (b'/', b'?'),
];

/// F1 to F12 in xterm mode.
const FKEYS: [&[u8]; 12] = [
    b"\x1bOP",
    b"\x1bOQ",
    b"\x1bOR",
    b"\x1bOS",
    b"\x1b[15~",
    b"\x1b[17~",
    b"\x1b[18~",
    b"\x1b[19~",
    b"\x1b[20~",
    b"\x1b[21~",
    b"\x1b[23~",
    b"\x1b[24~",
];

/// The control character for `c`, as FreeBSD's keymap gives it.
fn control(c: u8) -> Option<u8> {
    match c {
        b'a'..=b'z' => Some(c - b'a' + 1),
        b'@' | b'2' => Some(0),
        b'[' => Some(0x1b),
        b'\\' => Some(0x1c),
        b']' => Some(0x1d),
        b'^' | b'6' => Some(0x1e),
        b'_' | b'-' => Some(0x1f),
        _ => None,
    }
}

/// What key `usage` does with modifiers `m`.
#[must_use]
pub fn action(usage: u16, m: Mods) -> Action {
    let pick = |(lo, hi): (u8, u8), letter: bool| {
        let shifted = m.shift != (letter && m.caps);
        let c = if shifted { hi } else { lo };
        if m.ctrl {
            control(lo).map_or(Action::None, Action::Byte)
        } else {
            Action::Byte(c)
        }
    };
    match usage {
        0x04..=0x27 => {
            let i = usize::from(usage - 0x04);
            MAIN.get(i).map_or(Action::None, |&p| pick(p, i < 26))
        }
        0x28 => Action::Byte(if m.ctrl { 0x0a } else { 0x0d }),
        0x29 => Action::Byte(0x1b),
        0x2a => Action::Byte(if m.ctrl { 0x7f } else { 0x08 }),
        0x2b => {
            if m.shift {
                Action::Seq(b"\x1b[Z")
            } else {
                Action::Byte(0x09)
            }
        }
        0x2c => Action::Byte(if m.ctrl { 0 } else { b' ' }),
        0x2d..=0x38 => PUNCT
            .get(usize::from(usage - 0x2d))
            .map_or(Action::None, |&p| pick(p, false)),
        0x39 => Action::Lock(Lock::Caps),
        0x3a..=0x45 => {
            let n = usage - 0x3a;
            if m.alt {
                u8::try_from(n).map_or(Action::None, Action::Switch)
            } else {
                Action::Seq(FKEYS[usize::from(n)])
            }
        }
        0x47 => Action::Lock(Lock::Scroll),
        0x49 => Action::Seq(b"\x1b[2~"),
        0x4a => Action::Seq(b"\x1b[H"),
        0x4b => Action::Seq(b"\x1b[5~"),
        0x4c => Action::Seq(b"\x1b[3~"),
        0x4d => Action::Seq(b"\x1b[F"),
        0x4e => Action::Seq(b"\x1b[6~"),
        0x4f => Action::Seq(b"\x1b[C"),
        0x50 => Action::Seq(b"\x1b[D"),
        0x51 => Action::Seq(b"\x1b[B"),
        0x52 => Action::Seq(b"\x1b[A"),
        0x53 => Action::Lock(Lock::Num),
        0x54 => Action::Byte(b'/'),
        0x55 => Action::Byte(b'*'),
        0x56 => Action::Byte(b'-'),
        0x57 => Action::Byte(b'+'),
        0x58 => Action::Byte(0x0d),
        0x59..=0x63 => keypad(usage, m.num && !m.shift),
        0xe0 | 0xe4 => Action::Modifier(Modifier::Ctrl),
        0xe1 | 0xe5 => Action::Modifier(Modifier::Shift),
        0xe2 | 0xe6 => Action::Modifier(Modifier::Alt),
        0xe3 | 0xe7 => Action::Modifier(Modifier::Gui),
        _ => Action::None,
    }
}

/// The keypad's digits (with Num Lock) or cursor keys (without).
fn keypad(usage: u16, digits: bool) -> Action {
    const DIGITS: &[u8; 11] = b"1234567890.";
    const CURSOR: [&[u8]; 11] = [
        b"\x1b[F", b"\x1b[B", b"\x1b[6~", b"\x1b[D", b"\x1b[E", b"\x1b[C", b"\x1b[H", b"\x1b[A",
        b"\x1b[5~", b"\x1b[2~", b"\x1b[3~",
    ];
    let i = usize::from(usage - 0x59);
    if digits {
        DIGITS.get(i).map_or(Action::None, |&b| Action::Byte(b))
    } else {
        CURSOR.get(i).map_or(Action::None, |&s| Action::Seq(s))
    }
}
