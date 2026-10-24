// SPDX-License-Identifier: BSD-2-Clause
//! What the console makes of `kbdmux0`'s key actions, as `vt` does
//! (`vt_processkey`): characters as UTF-8, with ESC before them while
//! Alt is down (`VT_ALT_TO_ESC_HACK`); function, cursor, and editing
//! keys as `teken` sends them as `TERM=xterm` (cursor keys in normal
//! mode); screen switches; and the machine keys. In `K_RAW` and
//! `K_CODE` the codes go through as bytes.
//!
//! Until `vt` (U.4), this is all of it: there is no scroll-lock
//! scrollback, and cursor keys never take application mode.
//!
//! Behavior reference: sys/dev/vt/vt_core.c (`vt_processkey`,
//! `vt_machine_kbdevent`), sys/teken/teken.c (`teken_get_sequence`),
//! sys/kern/subr_terminal.c (`terminal_input_char`).

use alloc::vec::Vec;

use abi::kbio::spcl::*;
use abi::kbio::{BKEY, FKEY, K_XLATE, RELKEY, SPCLKEY, keychar, keyflags};

/// `VT_MAXWINDOWS`.
pub const WINDOWS: u32 = 12;

/// A key the console acts on rather than types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Switch to virtual terminal *n* (0-based).
    Switch(u32),
    /// The next or previous virtual terminal.
    Next,
    Prev,
    /// `vt_machine_kbdevent`'s keys: enter the debugger, halt, power
    /// off, reboot (Ctrl-Alt-Delete), suspend, standby.
    Debug,
    Halt,
    PowerOff,
    Reboot,
    Suspend,
    Standby,
}

/// The console's keyboard state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Console {
    /// An Alt key is down (`vd_kbstate & ALKED`).
    alt: bool,
}

/// `teken`'s xterm strings for `vt`'s special keys, by function key.
fn special(fkey: u32) -> Option<&'static [u8]> {
    const F: [&[u8]; 12] = [
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
    Some(match fkey {
        x if (f(1)..=f(12)).contains(&x) => F[usize::try_from(x - f(1)).ok()?],
        x if x == f(49) => b"\x1b[H",
        x if x == f(50) => b"\x1b[A",
        x if x == f(51) => b"\x1b[5~",
        x if x == f(53) => b"\x1b[D",
        x if x == f(55) => b"\x1b[C",
        x if x == f(57) => b"\x1b[F",
        x if x == f(58) => b"\x1b[B",
        x if x == f(59) => b"\x1b[6~",
        x if x == f(60) => b"\x1b[2~",
        x if x == f(61) => b"\x1b[3~",
        _ => return None,
    })
}

impl Console {
    /// Alt up.
    #[must_use]
    pub const fn new() -> Self {
        Self { alt: false }
    }

    /// One value read from the keyboard in `mode`: appends what it types
    /// to `out`, or returns what it does.
    pub fn key(&mut self, c: u32, mode: i32, out: &mut Vec<u8>) -> Option<Event> {
        // Right Alt is AltGr (`kern.vt.enable_altgr`), not Alt.
        if c & RELKEY != 0 {
            if c & !RELKEY == SPCLKEY | LALT {
                self.alt = false;
            }
            return None;
        }
        if c == SPCLKEY | LALT {
            self.alt = true;
        }
        // `kern.vt.kbd_*`: all on but the panic key.
        let machine = match c {
            x if x == SPCLKEY | DBG => Some(Event::Debug),
            x if x == SPCLKEY | HALT => Some(Event::Halt),
            x if x == SPCLKEY | PDWN => Some(Event::PowerOff),
            x if x == SPCLKEY | RBT => Some(Event::Reboot),
            x if x == SPCLKEY | SUSP => Some(Event::Suspend),
            x if x == SPCLKEY | STBY => Some(Event::Standby),
            x if x == SPCLKEY | PNC || x == SPCLKEY | SPSC => return None,
            _ => None,
        };
        if machine.is_some() {
            return machine;
        }
        if c & SPCLKEY != 0 {
            let k = c & !SPCLKEY;
            if (F_SCR..=L_SCR.min(F_SCR + WINDOWS - 1)).contains(&k) {
                return Some(Event::Switch(k - F_SCR));
            }
            match k {
                NEXT => return Some(Event::Next),
                PREV => return Some(Event::Prev),
                x if x == BKEY | BTAB => out.extend_from_slice(b"\x1b[Z"),
                x if x & FKEY != 0 => {
                    if let Some(s) = special(x & !FKEY) {
                        out.extend_from_slice(s);
                    }
                }
                _ => {}
            }
        } else if keyflags(c) == 0 {
            if mode == K_XLATE {
                if self.alt {
                    out.push(0x1b);
                }
                utf8(keychar(c), out);
            } else {
                out.push(c.to_le_bytes()[0]);
            }
        }
        None
    }
}

/// Appends `c` as UTF-8.
fn utf8(c: u32, out: &mut Vec<u8>) {
    let mut buf = [0u8; 4];
    match char::from_u32(c) {
        Some(ch) => out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()),
        // `terminal_input_char` encodes surrogates too, as three bytes.
        None if c < 0x10000 => out.extend_from_slice(&[
            0xe0 | (c >> 12).to_le_bytes()[0],
            0x80 | ((c >> 6) & 0x3f).to_le_bytes()[0],
            0x80 | (c & 0x3f).to_le_bytes()[0],
        ]),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::kbio::{MKEY, RELKEY};

    fn typed(c: &mut Console, keys: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for &k in keys {
            assert_eq!(c.key(k, K_XLATE, &mut out), None);
        }
        out
    }

    #[test]
    fn characters_function_keys_and_alt() {
        let mut c = Console::default();
        assert_eq!(typed(&mut c, &[u32::from(b'a'), 0xe9]), b"a\xc3\xa9");
        assert_eq!(typed(&mut c, &[SPCLKEY | FKEY | f(1)]), b"\x1bOP");
        assert_eq!(typed(&mut c, &[SPCLKEY | FKEY | f(50)]), b"\x1b[A");
        assert_eq!(typed(&mut c, &[SPCLKEY | FKEY | f(54)]), b"", "keypad 5");
        assert_eq!(typed(&mut c, &[SPCLKEY | BKEY | BTAB]), b"\x1b[Z");
        assert_eq!(
            typed(
                &mut c,
                &[SPCLKEY | LALT, u32::from(b'x'), SPCLKEY | RELKEY | LALT]
            ),
            b"\x1bx"
        );
        assert_eq!(
            typed(&mut c, &[MKEY | u32::from(b'x')]),
            b"",
            "vt drops META"
        );
    }

    #[test]
    fn screens_and_machine_keys() {
        let mut c = Console::default();
        let mut out = Vec::new();
        assert_eq!(
            c.key(SPCLKEY | s(3), K_XLATE, &mut out),
            Some(Event::Switch(2))
        );
        assert_eq!(c.key(SPCLKEY | RBT, K_XLATE, &mut out), Some(Event::Reboot));
        assert_eq!(c.key(SPCLKEY | PNC, K_XLATE, &mut out), None);
        assert!(out.is_empty());
    }
}
