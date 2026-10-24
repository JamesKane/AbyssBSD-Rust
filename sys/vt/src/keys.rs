// SPDX-License-Identifier: BSD-2-Clause
//! What the console makes of `kbdmux0`'s values (`vt_processkey`):
//! the machine keys, screen switches, Scroll Lock's scroll mode and its
//! keys, the strings the visible window's emulator gives the special
//! keys (`terminal_input_special`), and characters as UTF-8 with ESC
//! before them while Alt is down (`VT_ALT_TO_ESC_HACK`); in `K_RAW`
//! and `K_CODE`, the values as bytes.
//!
//! Behavior reference: sys/dev/vt/vt_core.c (`vt_processkey`,
//! `vt_machine_kbdevent`, `vt_scrollmode_kbdevent`),
//! sys/kern/subr_terminal.c (`terminal_input_char`).

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::kbio::spcl::*;
use abi::kbio::{
    BKEY, FKEY, K_XLATE, KDGKBSTATE, LOCK_MASK, RELKEY, SLKED, SPCLKEY, keychar, keyflags,
};
use input::term::Event;
use vtterm::{Key, Seek};

use crate::{VT, Vt, WINDOWS};

impl Vt {
    /// One value from the keyboard (`vt_processkey`): typed bytes go
    /// to `typed` with the window they are for, machine keys to
    /// `machine`.
    fn process_key(
        &self,
        c: u32,
        typed: &mut dyn FnMut(usize, &[u8]),
        machine: &mut dyn FnMut(Event),
    ) {
        if c & RELKEY != 0 {
            // Right Alt is AltGr (`kern.vt.enable_altgr`).
            if c & !RELKEY == SPCLKEY | LALT {
                self.state.lock().alt = false;
            }
            return;
        }
        if c == SPCLKEY | LALT {
            self.state.lock().alt = true;
        }
        // `vt_machine_kbdevent`, with `kern.vt.kbd_*` all on but the
        // panic key.
        let event = match c {
            x if x == SPCLKEY | DBG => Some(Event::Debug),
            x if x == SPCLKEY | HALT => Some(Event::Halt),
            x if x == SPCLKEY | PDWN => Some(Event::PowerOff),
            x if x == SPCLKEY | RBT => Some(Event::Reboot),
            x if x == SPCLKEY | SUSP => Some(Event::Suspend),
            x if x == SPCLKEY | STBY => Some(Event::Standby),
            x if x == SPCLKEY | PNC || x == SPCLKEY | SPSC => return,
            _ => None,
        };
        if let Some(e) = event {
            if e == Event::Debug {
                self.state.lock().alt = false;
            }
            machine(e);
            return;
        }
        let w = self.active.load(Ordering::Acquire);
        if self.state.lock().win[w].scroll {
            self.scroll_key(w, c);
            return;
        }
        if c & SPCLKEY != 0 {
            let k = c & !SPCLKEY;
            if (F_SCR..=L_SCR.min(F_SCR + crate::NWIN - 1)).contains(&k) {
                let _ = self.proc_window_switch(usize::try_from(k - F_SCR).unwrap_or(0));
                return;
            }
            let key = match k {
                NEXT => {
                    let _ = self.proc_window_switch((w + 1) % WINDOWS);
                    return;
                }
                PREV => {
                    let _ = self.proc_window_switch((w + WINDOWS - 1) % WINDOWS);
                    return;
                }
                SLK => {
                    self.save_kbd_state(w);
                    let on = self.state.lock().win[w].kbdstate & SLKED != 0;
                    self.state.lock().win[w].scroll = on;
                    let mut t = self.windows[w].lock();
                    t.buf.set_scroll_mode(on);
                    if !on {
                        drop(t);
                        self.scroll(w, 0, Seek::End);
                    }
                    return;
                }
                x if x == BKEY | BTAB => Key::BackTab,
                x if x & FKEY != 0 => match x & !FKEY {
                    n if (f(1)..=f(12)).contains(&n) => Key::F(u8::try_from(n - f(1)).unwrap_or(0)),
                    n if n == f(49) => Key::Home,
                    n if n == f(50) => Key::Up,
                    n if n == f(51) => Key::PageUp,
                    n if n == f(53) => Key::Left,
                    n if n == f(55) => Key::Right,
                    n if n == f(57) => Key::End,
                    n if n == f(58) => Key::Down,
                    n if n == f(59) => Key::PageDown,
                    n if n == f(60) => Key::Insert,
                    n if n == f(61) => Key::Delete,
                    _ => return,
                },
                _ => return,
            };
            let seq = self.windows[w].lock().emu.sequence(key);
            if let Some(seq) = seq {
                typed(w, seq);
            }
        } else if keyflags(c) == 0 {
            let (mode, alt) = {
                let st = self.state.lock();
                (st.win[w].kbdmode, st.alt)
            };
            if mode == K_XLATE {
                let mut out = Vec::with_capacity(5);
                if alt {
                    out.push(0x1b);
                }
                utf8(keychar(c), &mut out);
                typed(w, &out);
            } else {
                typed(w, &[c.to_le_bytes()[0]]);
            }
        }
    }

    /// A key in scroll mode (`vt_scrollmode_kbdevent`): only special
    /// keys count.
    fn scroll_key(&self, w: usize, c: u32) {
        if c & SPCLKEY == 0 {
            return;
        }
        let k = c & !SPCLKEY;
        if (F_SCR..=L_SCR.min(F_SCR + crate::NWIN - 1)).contains(&k) {
            let _ = self.proc_window_switch(usize::try_from(k - F_SCR).unwrap_or(0));
            return;
        }
        let rows = self.windows[w].lock().emu.winsize().row;
        let rows = i32::try_from(rows).unwrap_or(i32::MAX);
        match k {
            SLK => {
                self.scroll(w, 0, Seek::End);
                self.windows[w].lock().buf.set_scroll_mode(false);
                self.state.lock().win[w].scroll = false;
            }
            x if x == FKEY | f(49) => self.scroll(w, 0, Seek::Set),
            x if x == FKEY | f(50) => self.scroll(w, -1, Seek::Cur),
            x if x == FKEY | f(51) => self.scroll(w, -rows, Seek::Cur),
            x if x == FKEY | f(57) => self.scroll(w, 0, Seek::End),
            x if x == FKEY | f(58) => self.scroll(w, 1, Seek::Cur),
            x if x == FKEY | f(59) => self.scroll(w, rows, Seek::Cur),
            _ => {}
        }
    }

    /// Moves window `w`'s view (`vt_scroll`), redrawing it if visible.
    pub(crate) fn scroll(&self, w: usize, offset: i32, whence: Seek) {
        if self.windows[w].lock().buf.seek(offset, whence) {
            self.invalidate(w);
        }
    }

    /// Takes the keyboard's lock state into window `w`'s
    /// (`vt_save_kbd_state`).
    pub(crate) fn save_kbd_state(&self, w: usize) {
        let mut v = [0u8; 4];
        if let Some(Ok(())) = input::console_ioctl(KDGKBSTATE, &mut v) {
            let state = i32::from_ne_bytes(v);
            let mut st = self.state.lock();
            st.win[w].kbdstate = (st.win[w].kbdstate & !LOCK_MASK) | (state & LOCK_MASK);
        }
    }
}

/// Values read from the keyboard: what they type goes to `typed` with
/// its window, machine keys to `machine`. `false` without `vt`.
pub fn process_keys(
    codes: &[u32],
    typed: &mut dyn FnMut(usize, &[u8]),
    machine: &mut dyn FnMut(Event),
) -> bool {
    let Some(vt) = VT.get() else { return false };
    for &c in codes {
        vt.process_key(c, typed, machine);
    }
    true
}

/// Appends `c` as UTF-8 (`terminal_input_char`, which encodes
/// surrogates as three bytes too).
fn utf8(c: u32, out: &mut Vec<u8>) {
    let mut buf = [0u8; 4];
    match char::from_u32(c) {
        Some(ch) => out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()),
        None if c < 0x10000 => out.extend_from_slice(&[
            0xe0 | (c >> 12).to_le_bytes()[0],
            0x80 | ((c >> 6) & 0x3f).to_le_bytes()[0],
            0x80 | (c & 0x3f).to_le_bytes()[0],
        ]),
        None => {}
    }
}
