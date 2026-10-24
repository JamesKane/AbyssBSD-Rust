// SPDX-License-Identifier: BSD-2-Clause
//! Keycodes to key actions (`K_XLATE`), as `genkbd_keyaction` does it:
//! the shift state, the lock keys, dead keys through the accent map,
//! and Alt plus keypad digits composing a character, as `hkbd` and
//! `kbdmux` do it.
//!
//! Behavior reference: sys/dev/kbd/kbd.c (`genkbd_keyaction`,
//! `save_accent_key`, `make_accent_char`), sys/dev/kbdmux/kbdmux.c and
//! sys/dev/hid/hkbd.c (`read_char`'s compose handling).

use abi::kbio::spcl::*;
use abi::kbio::{
    ALKED, ALTGR_OFFSET, BKEY, CLKED, ERRKEY, FKEY, FLAG_LOCK_C, FLAG_LOCK_N, LOCK_MASK, MKEY,
    NLKED, NOKEY, NUM_ACCENTCHARS, NUM_KEYS, RELKEY, SLKED, SPCLKEY,
};

use crate::keymap::Maps;

/// The shift state's bits above the lock bits (sys/dev/kbd/kbdreg.h).
pub const SHIFTS1: u32 = 1 << 16;
pub const SHIFTS2: u32 = 1 << 17;
pub const SHIFTS: u32 = SHIFTS1 | SHIFTS2;
pub const CTLS1: u32 = 1 << 18;
pub const CTLS2: u32 = 1 << 19;
pub const CTLS: u32 = CTLS1 | CTLS2;
pub const ALTS1: u32 = 1 << 20;
pub const ALTS2: u32 = 1 << 21;
pub const ALTS: u32 = ALTS1 | ALTS2;
pub const AGRS1: u32 = 1 << 22;
pub const AGRS2: u32 = 1 << 23;
pub const AGRS: u32 = AGRS1 | AGRS2;
pub const METAS1: u32 = 1 << 24;
pub const METAS2: u32 = 1 << 25;
pub const METAS: u32 = METAS1 | METAS2;
pub const NLKDOWN: u32 = 1 << 26;
pub const SLKDOWN: u32 = 1 << 27;
pub const CLKDOWN: u32 = 1 << 28;
pub const ALKDOWN: u32 = 1 << 29;
pub const SHIFTAON: u32 = 1 << 30;

const LOCKS: u32 = LOCK_MASK.cast_unsigned();

/// A scancode or keycode's release bit.
pub const RELEASE: u32 = 0x80;

/// One keyboard's translation state.
#[derive(Clone, Debug)]
pub struct Xlate {
    /// The shift state and lock keys (`ks_state`, `sc_state`).
    pub state: u32,
    /// The pending dead key, 1-based; 0 for none.
    accents: u32,
    /// Left Alt is down: keypad digits compose a character.
    compose: bool,
    composed: u32,
    /// What each key's press did, which its release undoes
    /// (`kb_lastact`).
    lastact: [u32; NUM_KEYS],
}

impl Default for Xlate {
    fn default() -> Self {
        Self {
            state: 0,
            accents: 0,
            compose: false,
            composed: 0,
            lastact: [NOP; NUM_KEYS],
        }
    }
}

/// What one key did: its action, and the lock state to show if a lock
/// key changed it (FreeBSD's `KDSETLED` from `set_lockkey_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    pub action: u32,
    pub leds: Option<i32>,
}

impl Xlate {
    /// The lock key bits (`state & LOCK_MASK`).
    #[must_use]
    pub fn locks(&self) -> i32 {
        (self.state & LOCKS).cast_signed()
    }

    /// Sets the lock key bits.
    pub fn set_locks(&mut self, locks: i32) {
        self.state = (self.state & !LOCKS) | (locks.cast_unsigned() & LOCKS);
    }

    /// `clear_state`: forgets everything but the locks.
    pub fn clear(&mut self) {
        self.compose = false;
        self.state &= LOCKS;
        self.accents = 0;
        self.composed = 0;
    }

    /// Forgets a pending dead key (a new keymap or accent map).
    pub fn clear_accents(&mut self) {
        self.accents = 0;
    }

    /// Left Alt pressed (`down`) or released: starts or ends composing.
    pub fn compose_key(&mut self, down: bool) {
        if down {
            if !self.compose {
                self.compose = true;
                self.composed = 0;
            }
        } else if self.compose {
            self.compose = false;
            if self.composed > 0xff {
                self.composed = 0;
            }
        }
    }

    /// The character Alt and the keypad composed, once Alt is up.
    pub fn take_composed(&mut self) -> Option<u32> {
        if self.compose || self.composed == 0 {
            return None;
        }
        let c = core::mem::take(&mut self.composed);
        Some(if c > 0xff { ERRKEY } else { c })
    }

    /// Keycode `code` (with [`RELEASE`] for a release) through the
    /// compose state, then the keymap. [`NOKEY`] if it does nothing
    /// yet.
    pub fn code(&mut self, maps: &Maps, code: u32) -> Action {
        let none = |action| Action { action, leds: None };
        if self.compose {
            let digit = match code {
                0x47..=0x49 => Some(code - 0x40),
                0x4b..=0x4d => Some(code - 0x47),
                0x4f..=0x51 => Some(code - 0x4e),
                0x52 => Some(0),
                _ => None,
            };
            if let Some(d) = digit {
                self.composed = self.composed.saturating_mul(10).saturating_add(d);
                return none(if self.composed > 0xff { ERRKEY } else { NOKEY });
            }
            match code {
                0xc7..=0xc9 | 0xcb..=0xcd | 0xcf..=0xd2 => return none(NOKEY),
                0x38 => {}
                _ if self.composed > 0 => {
                    self.compose = false;
                    self.composed = 0;
                    return none(ERRKEY);
                }
                _ => {}
            }
        }
        self.keyaction(maps, code & 0x7f, code & RELEASE != 0)
    }

    /// `genkbd_keyaction`.
    fn keyaction(&mut self, maps: &Maps, keycode: u32, up: bool) -> Action {
        let kc = usize::try_from(keycode).unwrap_or(0) & 0x7f;
        let mut state = self.state;
        let mut leds = None;
        let mut i = kc;
        let f = state & (AGRS | ALKED.cast_unsigned());
        if f == AGRS1 || f == AGRS2 || f == ALKED.cast_unsigned() {
            i += ALTGR_OFFSET;
        }
        let key = maps.keys[i];
        let mut col = usize::from(state & SHIFTS != 0)
            | usize::from(state & CTLS != 0) << 1
            | usize::from(state & ALTS != 0) << 2;
        if (key.flgs & FLAG_LOCK_C != 0 && state & CLKED.cast_unsigned() != 0)
            || (key.flgs & FLAG_LOCK_N != 0 && state & NLKED.cast_unsigned() != 0)
        {
            col ^= 1;
        }
        // `set_lockkey_state`: toggles a lock once per press.
        let mut lock = |state: &mut u32, down: u32, ed: i32| {
            if *state & down == 0 {
                *state |= down;
                *state ^= ed.cast_unsigned();
                leds = Some((*state & LOCKS).cast_signed());
            }
        };

        if up {
            let mut action = core::mem::replace(&mut self.lastact[kc], NOP);
            // The alt-lock variants release as their plain key, toggling
            // Alt lock if nothing was pressed in between.
            let plain = match action {
                LSHA => Some(LSH),
                RSHA => Some(RSH),
                LCTRA => Some(LCTR),
                RCTRA => Some(RCTR),
                LALTA => Some(LALT),
                RALTA => Some(RALT),
                _ => None,
            };
            if let Some(p) = plain {
                if state & SHIFTAON != 0 {
                    lock(&mut state, ALKDOWN, ALKED);
                    state &= !ALKDOWN;
                }
                action = p;
            }
            match action {
                LSH => state &= !SHIFTS1,
                RSH => state &= !SHIFTS2,
                LCTR => state &= !CTLS1,
                RCTR => state &= !CTLS2,
                LALT => state &= !ALTS1,
                RALT => state &= !ALTS2,
                ASH => state &= !AGRS1,
                META => state &= !METAS1,
                NLK => state &= !NLKDOWN,
                CLK => state &= !CLKDOWN,
                SLK => state &= !SLKDOWN,
                ALK => state &= !ALKDOWN,
                NOP => {
                    // Releases of regular keys are not reported.
                    self.state &= !SHIFTAON;
                    return Action {
                        action: NOKEY,
                        leds,
                    };
                }
                _ => {}
            }
            self.state = state & !SHIFTAON;
            return Action {
                action: SPCLKEY | RELKEY | action,
                leds,
            };
        }

        let mut action = key.map[col];
        state &= !SHIFTAON;
        if key.spcl & (0x80 >> col) == 0 {
            // A regular key.
            self.lastact[kc] = NOP;
            self.state = state;
            if self.accents > 0 {
                action = self.accent_char(maps, action);
                if action == ERRKEY {
                    return Action { action, leds };
                }
            }
            if state & METAS != 0 {
                action |= MKEY;
            }
            return Action { action, leds };
        }

        if self.lastact[kc] == NOP {
            self.lastact[kc] = action;
        }
        if self.lastact[kc] != action {
            action = NOP;
        }
        match action {
            NLK => lock(&mut state, NLKDOWN, NLKED),
            CLK => lock(&mut state, CLKDOWN, CLKED),
            SLK => lock(&mut state, SLKDOWN, SLKED),
            ALK => lock(&mut state, ALKDOWN, ALKED),
            SPSC | RBT | SUSP | STBY | DBG | NEXT | PREV | PNC | HALT | PDWN => self.accents = 0,
            BTAB => {
                self.accents = 0;
                action |= BKEY;
            }
            LSHA | RSHA | LCTRA | RCTRA | LALTA | RALTA | LSH | RSH | LCTR | RCTR | LALT | RALT => {
                let (plain, bit) = match action {
                    LSHA | LSH => (LSH, SHIFTS1),
                    RSHA | RSH => (RSH, SHIFTS2),
                    LCTRA | LCTR => (LCTR, CTLS1),
                    RCTRA | RCTR => (RCTR, CTLS2),
                    LALTA | LALT => (LALT, ALTS1),
                    _ => (RALT, ALTS2),
                };
                if plain != action {
                    state |= SHIFTAON;
                }
                action = plain;
                state |= bit;
            }
            ASH => state |= AGRS1,
            META => state |= METAS1,
            NOP => {
                self.state = state;
                return Action {
                    action: NOKEY,
                    leds,
                };
            }
            _ => {
                self.state = state;
                if (F_ACC..=L_ACC).contains(&action) {
                    // A dead key.
                    let a = self.save_accent(maps, action);
                    let a = if a == NOKEY || a == ERRKEY || state & METAS == 0 {
                        a
                    } else {
                        a | MKEY
                    };
                    return Action { action: a, leds };
                }
                if self.accents > 0 {
                    self.accents = 0;
                    return Action {
                        action: ERRKEY,
                        leds,
                    };
                }
                if (F_FN..=L_FN).contains(&action) {
                    action |= FKEY;
                }
                return Action {
                    action: SPCLKEY | action,
                    leds,
                };
            }
        }
        self.state = state;
        Action {
            action: SPCLKEY | action,
            leds,
        }
    }

    /// `save_accent_key`: a dead key waits for the next key; pressed
    /// twice, it sends its own character.
    fn save_accent(&mut self, maps: &Maps, key: u32) -> u32 {
        let i = key - F_ACC + 1;
        let entry = usize::try_from(i - 1)
            .ok()
            .and_then(|n| maps.accents.get(n));
        let Some(acc) = entry.filter(|a| i <= u32::from(maps.n_accs) && a.accchar != 0) else {
            self.accents = 0;
            return ERRKEY;
        };
        if i == self.accents {
            self.accents = 0;
            return acc.accchar;
        }
        self.accents = i;
        NOKEY
    }

    /// `make_accent_char`: the pending dead key applied to `ch`.
    fn accent_char(&mut self, maps: &Maps, ch: u32) -> u32 {
        let i = core::mem::take(&mut self.accents);
        let Some(acc) = usize::try_from(i - 1)
            .ok()
            .and_then(|n| maps.accents.get(n))
        else {
            return ERRKEY;
        };
        if ch == u32::from(b' ') {
            return acc.accchar;
        }
        acc.map
            .iter()
            .take(NUM_ACCENTCHARS)
            .take_while(|p| p[0] != 0)
            .find(|p| p[0] == ch)
            .map_or(ERRKEY, |p| p[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(x: &mut Xlate, m: &Maps, code: u32) -> u32 {
        x.code(m, code).action
    }

    fn release(x: &mut Xlate, m: &Maps, code: u32) -> u32 {
        x.code(m, code | RELEASE).action
    }

    #[test]
    fn letters_shift_and_caps_lock() {
        let m = Maps::default_maps();
        let mut x = Xlate::default();
        assert_eq!(press(&mut x, &m, 0x1e), u32::from(b'a'));
        assert_eq!(release(&mut x, &m, 0x1e), NOKEY);
        assert_eq!(press(&mut x, &m, 0x2a), SPCLKEY | LSH);
        assert_eq!(press(&mut x, &m, 0x1e), u32::from(b'A'));
        assert_eq!(release(&mut x, &m, 0x2a), SPCLKEY | RELKEY | LSH);
        // Caps Lock toggles once per press, and shows.
        let a = x.code(&m, 0x3a);
        assert_eq!(
            a,
            Action {
                action: SPCLKEY | CLK,
                leds: Some(CLKED)
            }
        );
        assert_eq!(x.code(&m, 0x3a).leds, None, "a held lock repeats nothing");
        release(&mut x, &m, 0x3a);
        assert_eq!(press(&mut x, &m, 0x1e), u32::from(b'A'));
        assert_eq!(
            press(&mut x, &m, 0x02),
            u32::from(b'1'),
            "digits ignore Caps Lock"
        );
    }

    #[test]
    fn control_function_keys_and_screens() {
        let m = Maps::default_maps();
        let mut x = Xlate::default();
        press(&mut x, &m, 0x1d);
        assert_eq!(press(&mut x, &m, 0x2e), 0x03);
        release(&mut x, &m, 0x1d);
        assert_eq!(press(&mut x, &m, 0x3b), SPCLKEY | FKEY | f(1));
        press(&mut x, &m, 0x38);
        assert_eq!(press(&mut x, &m, 0x3c), SPCLKEY | s(2), "Alt-F2");
        release(&mut x, &m, 0x38);
        press(&mut x, &m, 0x2a);
        assert_eq!(press(&mut x, &m, 0x0f), SPCLKEY | BKEY | BTAB);
    }

    #[test]
    fn alt_and_the_keypad_compose() {
        let m = Maps::default_maps();
        let mut x = Xlate::default();
        x.compose_key(true);
        press(&mut x, &m, 0x38);
        for code in [0x4f, 0x4c, 0x4b] {
            // 1, 5, 4 on the keypad: 154.
            assert_eq!(press(&mut x, &m, code), NOKEY);
            assert_eq!(release(&mut x, &m, code), NOKEY);
        }
        assert_eq!(x.take_composed(), None, "Alt is still down");
        x.compose_key(false);
        release(&mut x, &m, 0x38);
        assert_eq!(x.take_composed(), Some(154));
    }

    #[test]
    fn dead_keys_accent_the_next_key() {
        let mut m = Maps::default_maps();
        // `kbdcontrol -l` with a dead grave on F1's key.
        m.keys[0x3b].map[0] = DGRA;
        let mut x = Xlate::default();
        assert_eq!(press(&mut x, &m, 0x3b), NOKEY);
        assert_eq!(press(&mut x, &m, 0x1e), 0xe0, "à");
        assert_eq!(press(&mut x, &m, 0x3b), NOKEY);
        assert_eq!(
            press(&mut x, &m, 0x3b),
            u32::from(b'`'),
            "twice: the accent"
        );
        assert_eq!(press(&mut x, &m, 0x3b), NOKEY);
        assert_eq!(press(&mut x, &m, 0x2c), ERRKEY, "z takes no grave");
    }
}
