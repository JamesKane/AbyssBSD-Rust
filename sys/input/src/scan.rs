// SPDX-License-Identifier: BSD-2-Clause
//! Keys between their three codes: HID usages to keycodes and AT set 1
//! scancodes, as `hkbd` emulates an AT keyboard, and scancodes back to
//! keycodes, as `kbdmux` reads its keyboards' (FreeBSD keycodes are the
//! scancodes, with the `0xE0`-prefixed keys moved to 0x59 and up).
//!
//! Behavior reference: sys/dev/hid/hkbd.c (`hkbd_trtab`,
//! `hkbd_atkeycode`, `hkbd_key2scan`), sys/dev/kbdmux/kbdmux.c
//! (`kbdmux_read_char`).

use alloc::vec::Vec;

use crate::xlate::RELEASE;

/// No keycode (`NN`).
pub const NN: u8 = 0;

/// HID usage to keycode (`hkbd_trtab`).
#[rustfmt::skip]
const TRTAB: [u8; 256] = [
    0, 0, 0, 0, 30, 48, 46, 32,             /* 00 - 07 */
    18, 33, 34, 35, 23, 36, 37, 38,         /* 08 - 0F */
    50, 49, 24, 25, 16, 19, 31, 20,         /* 10 - 17 */
    22, 47, 17, 45, 21, 44, 2, 3,           /* 18 - 1F */
    4, 5, 6, 7, 8, 9, 10, 11,               /* 20 - 27 */
    28, 1, 14, 15, 57, 12, 13, 26,          /* 28 - 2F */
    27, 43, 43, 39, 40, 41, 51, 52,         /* 30 - 37 */
    53, 58, 59, 60, 61, 62, 63, 64,         /* 38 - 3F */
    65, 66, 67, 68, 87, 88, 92, 70,         /* 40 - 47 */
    104, 102, 94, 96, 103, 99, 101, 98,     /* 48 - 4F */
    97, 100, 95, 69, 91, 55, 74, 78,        /* 50 - 57 */
    89, 79, 80, 81, 75, 76, 77, 71,         /* 58 - 5F */
    72, 73, 82, 83, 86, 107, 122, NN,       /* 60 - 67 */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* 68 - 6F */
    NN, NN, NN, NN, 115, 108, 111, 113,     /* 70 - 77 */
    109, 110, 112, 118, 114, 116, 117, 119, /* 78 - 7F */
    121, 120, NN, NN, NN, NN, NN, 123,      /* 80 - 87 */
    124, 125, 126, 127, 128, NN, NN, NN,    /* 88 - 8F */
    129, 130, NN, NN, NN, NN, NN, NN,       /* 90 - 97 */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* 98 - 9F */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* A0 - A7 */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* A8 - AF */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* B0 - B7 */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* B8 - BF */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* C0 - C7 */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* C8 - CF */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* D0 - D7 */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* D8 - DF */
    29, 42, 56, 105, 90, 54, 93, 106,       /* E0 - E7 */
    NN, NN, NN, NN, 254, NN, NN, NN,        /* E8 - EF */
    NN, NN, NN, NN, NN, NN, NN, NN,         /* F0 - F7 */
    NN, NN, NN, NN, NN, NN, NN, 255,        /* F8 - FF */
];

/// Modifier usages (`MOD_MIN` to `MOD_MAX`), which do not repeat.
#[must_use]
pub fn is_modifier(usage: u16) -> bool {
    (0xe0..=0xe7).contains(&usage)
}

/// The usages held down, which the AT emulation consults.
#[derive(Clone, Copy, Debug, Default)]
pub struct Held([u64; 4]);

impl Held {
    /// Whether `usage` is down.
    #[must_use]
    pub fn has(&self, usage: u16) -> bool {
        let u = usize::from(usage & 0xff);
        self.0[u / 64] & (1 << (u % 64)) != 0
    }

    /// Notes `usage` down or up.
    pub fn set(&mut self, usage: u16, down: bool) {
        let u = usize::from(usage & 0xff);
        if down {
            self.0[u / 64] |= 1 << (u % 64);
        } else {
            self.0[u / 64] &= !(1 << (u % 64));
        }
    }

    /// Forgets every key (`hkbd_clear_state`).
    pub fn clear(&mut self) {
        self.0 = [0; 4];
    }

    fn any(&self, usages: [u16; 2]) -> bool {
        usages.iter().any(|&u| self.has(u))
    }
}

const LCTRL: u16 = 0xe0;
const LSHIFT: u16 = 0xe1;
const LALT: u16 = 0xe2;
const RCTRL: u16 = 0xe4;
const RSHIFT: u16 = 0xe5;
const RALT: u16 = 0xe6;

/// `hkbd_atkeycode`: a usage's keycode, Alt-PrintScreen as SysRq.
#[must_use]
pub fn keycode(usage: u16, held: &Held) -> u8 {
    let k = TRTAB[usize::from(usage & 0xff)];
    if (k == 0x5c || k == 0x7e) && held.any([LALT, RALT]) {
        return 0x54;
    }
    k
}

const PREFIX_E0: u32 = 0x100;
const PREFIX_E1: u32 = 0x200;
const PREFIX_CTL: u32 = 0x400;
const PREFIX_SHIFT: u32 = 0x800;
const PREFIX: u32 = PREFIX_E0 | PREFIX_E1 | PREFIX_CTL | PREFIX_SHIFT;

/// The extended keys' scancodes, from keycode 89 (`hkbd_key2scan`).
#[rustfmt::skip]
const EXTENDED: [u32; 42] = [
    0x11c, 0x11d, 0x135, 0x137, 0x138, 0x147, 0x148, 0x149, 0x14b, 0x14d,
    0x14f, 0x150, 0x151, 0x152, 0x153, 0x146, 0x15b, 0x15c, 0x15d, 0x168,
    0x15e, 0x15f, 0x160, 0x161, 0x162, 0x163, 0x164, 0x165, 0x166, 0x167,
    0x125, 0x11f, 0x11e, 0x120, 0x73, 0x70, 0x7d, 0x79, 0x7b, 0x5c,
    0x71, 0x72,
];

/// `hkbd_key2scan`: the AT scancodes keycode `code` sends, pressed or
/// released, in the order `read_char` returns them (prefixes first;
/// PrintScreen and Pause with their fake Shift and Ctrl).
#[must_use]
pub fn scancodes(code: u8, held: &Held, up: bool) -> Vec<u32> {
    let mut code = u32::from(code);
    if let Some(&e) = code
        .checked_sub(89)
        .and_then(|i| EXTENDED.get(usize::try_from(i).ok()?))
    {
        code = e;
    }
    if code == 0x137 && !held.any([LCTRL, RCTRL]) && !held.any([LSHIFT, RSHIFT]) {
        code |= PREFIX_SHIFT;
    }
    if code == 0x146 && !held.any([LCTRL, RCTRL]) {
        code = 0x45 | PREFIX_E1 | PREFIX_CTL;
    }
    if up {
        code |= RELEASE;
    }
    if code & PREFIX == 0 {
        return alloc::vec![code];
    }
    let prefix = |c: u32| if c & PREFIX_E0 != 0 { 0xe0 } else { 0xe1 };
    // The prefix, then the buffered codes, each with its own prefix.
    let buffered = if code & PREFIX_CTL != 0 {
        [0x1d | (code & RELEASE), code & !PREFIX]
    } else if code & PREFIX_SHIFT != 0 {
        [0x2a | (code & RELEASE), code & !PREFIX_SHIFT]
    } else {
        [code & !PREFIX, 0]
    };
    let mut out = alloc::vec![prefix(code)];
    for b in buffered.into_iter().filter(|&b| b != 0) {
        if b & PREFIX != 0 {
            out.push(prefix(b));
        }
        out.push(b & !PREFIX);
    }
    out
}

/// `kbdmux`'s scancode reader: the prefix it has seen.
#[derive(Clone, Copy, Debug, Default)]
pub struct Decoder {
    prefix: u32,
}

impl Decoder {
    /// One scancode: its key's keycode, with [`RELEASE`] if released,
    /// or `None` for a prefix or a key `kbdmux` ignores. (Keycode 0x38
    /// is only ever the left Alt, which composes.)
    pub fn scancode(&mut self, scancode: u32) -> Option<u32> {
        let release = scancode & RELEASE;
        let mut keycode = scancode & 0x7f;
        match self.prefix {
            0 => {
                if scancode == 0xe0 || scancode == 0xe1 {
                    self.prefix = scancode;
                    return None;
                }
            }
            0xe0 => {
                self.prefix = 0;
                keycode = match keycode {
                    0x1c => 0x59, // right Enter
                    0x1d => 0x5a, // right Ctrl
                    0x35 => 0x5b, // keypad divide
                    0x37 => 0x5c, // PrintScreen
                    0x38 => 0x5d, // right Alt (AltGr)
                    0x46 => 0x68, // Ctrl-Pause/Break
                    0x47 => 0x5e, // grey Home
                    0x48 => 0x5f, // grey Up
                    0x49 => 0x60, // grey PageUp
                    0x4b => 0x61, // grey Left
                    0x4d => 0x62, // grey Right
                    0x4f => 0x63, // grey End
                    0x50 => 0x64, // grey Down
                    0x51 => 0x65, // grey PageDown
                    0x52 => 0x66, // grey Insert
                    0x53 => 0x67, // grey Delete
                    0x5b => 0x69, // left Windows
                    0x5c => 0x6a, // right Windows
                    0x5d => 0x6b, // Menu
                    0x5e => 0x6d, // Power
                    0x5f => 0x6e, // Sleep
                    0x63 => 0x6f, // Wake
                    0x64 => 0x73, // JP106 backslash
                    _ => return None,
                };
            }
            0xe1 => {
                // Pause/Break: E1-1D-45 E1-9D-C5.
                self.prefix = if keycode == 0x1d { 0x1d } else { 0 };
                return None;
            }
            _ => {
                self.prefix = 0;
                if keycode != 0x45 {
                    return None;
                }
                keycode = 0x68;
            }
        }
        // `kbdmux` would turn PrintScreen with Alt into SysRq and Pause
        // with Ctrl into Break, but tests its flags, which never hold
        // those bits, so it never does.
        Some(keycode | release)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_their_scancodes() {
        let h = Held::default();
        assert_eq!(keycode(0x04, &h), 0x1e, "a");
        assert_eq!(scancodes(0x1e, &h, false), [0x1e]);
        assert_eq!(scancodes(0x1e, &h, true), [0x9e]);
    }

    #[test]
    fn extended_keys_take_a_prefix() {
        let h = Held::default();
        let up = keycode(0x52, &h);
        assert_eq!(up, 95);
        assert_eq!(scancodes(up, &h, false), [0xe0, 0x48]);
        assert_eq!(scancodes(up, &h, true), [0xe0, 0xc8]);
    }

    #[test]
    fn print_screen_and_pause_as_an_at_keyboard_sends_them() {
        let mut h = Held::default();
        let ps = keycode(0x46, &h);
        assert_eq!(scancodes(ps, &h, false), [0xe0, 0x2a, 0xe0, 0x37]);
        assert_eq!(scancodes(ps, &h, true), [0xe0, 0xaa, 0xe0, 0xb7]);
        let pause = keycode(0x48, &h);
        assert_eq!(scancodes(pause, &h, false), [0xe1, 0x1d, 0x45]);
        assert_eq!(scancodes(pause, &h, true), [0xe1, 0x9d, 0xc5]);
        h.set(LCTRL, true);
        assert_eq!(scancodes(pause, &h, false), [0xe0, 0x46], "Ctrl-Break");
        h.set(LALT, true);
        assert_eq!(keycode(0x46, &h), 0x54, "Alt-PrintScreen is SysRq");
    }

    #[test]
    fn kbdmux_reads_them_back() {
        let h = Held::default();
        let mut d = Decoder::default();
        let mut keys = |usage: u16, up: bool| {
            scancodes(keycode(usage, &h), &h, up)
                .into_iter()
                .filter_map(|s| d.scancode(s))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(0x04, false), [0x1e]);
        assert_eq!(keys(0x52, false), [0x5f], "Up");
        assert_eq!(keys(0x52, true), [0xdf]);
        assert_eq!(keys(0x48, false), [0x68], "Pause");
        assert_eq!(keys(0xe6, false), [0x5d], "AltGr");
        assert_eq!(keys(0x58, false), [0x59], "keypad Enter");
    }
}
