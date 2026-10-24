// SPDX-License-Identifier: BSD-2-Clause
//! The requests on a window's terminal that the terminal layer leaves
//! to `vt` (`termtty_ioctl`, `vtterm_ioctl`): the keyboard's, which go
//! to `kbdmux0`, with the mode and lock state kept per window; the
//! display mode; the history; the console's information; and the
//! virtual terminals' own (`VT_*`), switching among them.
//!
//! Behavior reference: sys/dev/vt/vt_core.c (`vtterm_ioctl`),
//! sys/kern/subr_terminal.c (`termtty_ioctl`).

use core::sync::atomic::Ordering;

use abi::consio::*;
use abi::errno::Errno;
use abi::kbio::{
    GETFKEY, GIO_DEADKEYMAP, GIO_KEYMAP, K_CODE, K_RAW, K_XLATE, KBADDKBD, KBRELKBD, KDDISABIO,
    KDGETLED, KDGETMODE, KDGETREPEAT, KDGKBINFO, KDGKBMODE, KDGKBSTATE, KDGKBTYPE, KDMKTONE,
    KDSETLED, KDSETMODE, KDSETRAD, KDSETREPEAT, KDSKBMODE, KDSKBSTATE, KIOCSOUND, LED_MASK,
    LOCK_MASK, PIO_DEADKEYMAP, PIO_KEYMAP, SETFKEY,
};
use sched::Sleepable;
use vtterm::Pos;

use crate::{VT, Vt, WINDOWS};

/// `_SIG_MAXSIG`.
const SIG_MAXSIG: i16 = 128;

fn int(arg: &[u8]) -> i32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i32::from_ne_bytes)
}

fn put_int(arg: &mut [u8], v: i32) {
    if let Some(b) = arg.get_mut(..4) {
        b.copy_from_slice(&v.to_ne_bytes());
    }
}

fn put_u16(arg: &mut [u8], at: usize, v: u32) {
    if let Some(b) = arg.get_mut(at..at + 2) {
        b.copy_from_slice(&u16::try_from(v).unwrap_or(u16::MAX).to_ne_bytes());
    }
}

/// A request on window `window`'s terminal.
///
/// # Errors
///
/// As FreeBSD's `vt` returns them; `ENOTTY` for a request it doesn't
/// know.
pub fn ioctl(window: usize, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
    let Some(vt) = VT.get() else {
        return Err(Errno::ENOTTY);
    };
    if window >= WINDOWS {
        return Err(Errno::ENXIO);
    }
    vt.ioctl(window, s, req, arg)
}

impl Vt {
    fn is_active(&self, w: usize) -> bool {
        self.active.load(Ordering::Acquire) == w
    }

    /// A keyboard request for `kbdmux0` with an `int` argument.
    fn kbd_value(req: u32, v: i32) -> Result<(), Errno> {
        input::console_ioctl(req, &mut v.to_ne_bytes()).unwrap_or(Err(Errno::ENODEV))
    }

    #[allow(clippy::too_many_lines)] // one arm per request, as `vtterm_ioctl`
    fn ioctl(&self, w: usize, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            KDSETRAD | GIO_KEYMAP | PIO_KEYMAP | GIO_DEADKEYMAP | PIO_DEADKEYMAP | GETFKEY
            | SETFKEY | KDGKBINFO | KDGKBTYPE | KDGETREPEAT | KDSETREPEAT | KBADDKBD | KBRELKBD => {
                if req == KDSETRAD && int(arg) & !0x7f != 0 {
                    return Err(Errno::EINVAL);
                }
                match input::console_ioctl(req, arg) {
                    Some(r) => r,
                    // "always return something?"
                    None if req == KDGKBTYPE => {
                        put_int(arg, 0);
                        Ok(())
                    }
                    None => Err(Errno::ENODEV),
                }
            }
            KDGKBSTATE => {
                if self.is_active(w) {
                    self.save_kbd_state(w);
                }
                put_int(arg, self.state.lock().win[w].kbdstate & LOCK_MASK);
                Ok(())
            }
            KDSKBSTATE => {
                let state = int(arg);
                if state & !LOCK_MASK != 0 {
                    return Err(Errno::EINVAL);
                }
                {
                    let mut st = self.state.lock();
                    st.win[w].kbdstate = (st.win[w].kbdstate & !LOCK_MASK) | state;
                }
                if self.is_active(w) {
                    Self::kbd_value(KDSKBSTATE, state)
                } else {
                    Ok(())
                }
            }
            KDGETLED => {
                if self.is_active(w) {
                    let mut v = [0u8; 4];
                    input::console_ioctl(KDGETLED, &mut v).unwrap_or(Err(Errno::ENODEV))?;
                    let mut st = self.state.lock();
                    st.win[w].kbdstate =
                        (st.win[w].kbdstate & !LED_MASK) | (i32::from_ne_bytes(v) & LED_MASK);
                }
                put_int(arg, self.state.lock().win[w].kbdstate & LED_MASK);
                Ok(())
            }
            KDSETLED => {
                let leds = int(arg);
                if leds & !LED_MASK != 0 {
                    return Err(Errno::EINVAL);
                }
                {
                    let mut st = self.state.lock();
                    st.win[w].kbdstate = (st.win[w].kbdstate & !LED_MASK) | leds;
                }
                if self.is_active(w) {
                    Self::kbd_value(KDSETLED, leds)
                } else {
                    Ok(())
                }
            }
            KDGETMODE => {
                let graphics = self.state.lock().win[w].graphics;
                put_int(arg, if graphics { KD_GRAPHICS } else { KD_TEXT });
                Ok(())
            }
            KDGKBMODE => {
                if self.is_active(w) {
                    let mut v = [0u8; 4];
                    input::console_ioctl(KDGKBMODE, &mut v).unwrap_or(Err(Errno::ENODEV))?;
                    self.state.lock().win[w].kbdmode = i32::from_ne_bytes(v);
                }
                put_int(arg, self.state.lock().win[w].kbdmode);
                Ok(())
            }
            KDSKBMODE => {
                let mode = int(arg);
                if ![K_XLATE, K_RAW, K_CODE].contains(&mode) {
                    return Err(Errno::EINVAL);
                }
                self.state.lock().win[w].kbdmode = mode;
                if self.is_active(w) {
                    Self::kbd_value(KDSKBMODE, mode)
                } else {
                    Ok(())
                }
            }
            CONS_BLANKTIME | KDMKTONE | KIOCSOUND | PIO_VFONT_DEFAULT | KDDISABIO => Ok(()),
            CONS_HISTORY => {
                let size = int(arg);
                let max = u32::MAX / u32::from(u16::MAX) / 4;
                let Ok(size) = u32::try_from(size) else {
                    return Err(Errno::EINVAL);
                };
                if size > max {
                    return Err(Errno::EINVAL);
                }
                let mut t = self.windows[w].lock();
                if size != t.buf.history_size() {
                    let a = *t.emu.curattr();
                    let screen = t.buf.size();
                    t.buf.grow(screen, size, &a);
                }
                Ok(())
            }
            CONS_CLRHIST => {
                {
                    let mut t = self.windows[w].lock();
                    let a = *t.emu.curattr();
                    t.buf.clear_history(&a);
                    // `termtty_ioctl`: the cursor goes to the top left.
                    t.emu.set_cursor(Pos::new(0, 0));
                    t.buf.cursor_position(Pos::new(0, 0));
                }
                self.invalidate(w);
                Ok(())
            }
            CONS_GET => {
                put_int(arg, M_CG640X480);
                Ok(())
            }
            CONS_BELLTYPE => {
                self.state.lock().quiet_bell = int(arg) & CONS_QUIET_BELL != 0;
                Ok(())
            }
            CONS_GETINFO => self.getinfo(w, arg),
            CONS_GETVERS => {
                put_int(arg, 0x200);
                Ok(())
            }
            CONS_MOUSECTL => {
                // Only showing and hiding is a window's; the rest is
                // `/dev/consolectl`'s.
                let op = int(arg);
                let mut st = self.state.lock();
                match op {
                    MOUSE_HIDE => st.mouse_cursor = false,
                    MOUSE_SHOW => st.mouse_cursor = true,
                    _ => return Err(Errno::EINVAL),
                }
                Ok(())
            }
            GIO_SCRNMAP => {
                // No screen maps: the identity.
                for (i, b) in (0u8..=255).zip(arg.iter_mut()) {
                    *b = i;
                }
                Ok(())
            }
            KDSETMODE => {
                match int(arg) {
                    KD_TEXT | KD_TEXT1 | KD_PIXEL => {
                        let was = core::mem::replace(&mut self.state.lock().win[w].graphics, false);
                        if was {
                            self.invalidate(w);
                        }
                    }
                    KD_GRAPHICS => self.state.lock().win[w].graphics = true,
                    _ => {}
                }
                Ok(())
            }
            VT_ACTIVATE => {
                let to = usize::try_from(int(arg) - 1).map_err(|_| Errno::EINVAL)?;
                if to >= WINDOWS {
                    return Err(Errno::EINVAL);
                }
                self.proc_window_switch(to)
            }
            VT_GETACTIVE => {
                let a = self.active.load(Ordering::Acquire);
                put_int(arg, i32::try_from(a + 1).unwrap_or(0));
                Ok(())
            }
            VT_GETINDEX => {
                put_int(arg, i32::try_from(w + 1).unwrap_or(0));
                Ok(())
            }
            VT_LOCKSWITCH => {
                let mut st = self.state.lock();
                match int(arg) {
                    1 => st.win[w].vtylock = true,
                    2 => st.win[w].vtylock = false,
                    _ => return Err(Errno::EINVAL),
                }
                Ok(())
            }
            VT_OPENQRY => {
                let st = self.state.lock();
                let free = st.win.iter().position(|x| !x.opened).ok_or(Errno::EINVAL)?;
                put_int(arg, i32::try_from(free + 1).unwrap_or(0));
                Ok(())
            }
            VT_WAITACTIVE => {
                let idx = int(arg).cast_unsigned();
                if idx > crate::NWIN {
                    return Err(Errno::EINVAL);
                }
                let target = if idx > 0 {
                    usize::try_from(idx - 1).unwrap_or(0)
                } else {
                    w
                };
                self.host
                    .wait(s, &self.switched, &|| self.is_active(target))
            }
            VT_SETMODE => self.setmode(w, arg),
            VT_GETMODE => {
                let m = self.state.lock().win[w].smode.to_bytes();
                arg.get_mut(..VtMode::SIZE)
                    .ok_or(Errno::EINVAL)?
                    .copy_from_slice(&m);
                Ok(())
            }
            VT_RELDISP => {
                {
                    let st = self.state.lock();
                    if !self.is_active(w) || st.win[w].smode.mode != VT_PROCESS {
                        return Err(Errno::EINVAL);
                    }
                    if st.win[w].pid != self.host.caller() {
                        return Err(Errno::EPERM);
                    }
                }
                match int(arg) {
                    VT_FALSE => self.finish_rel(w, false),
                    VT_TRUE => self.finish_rel(w, true),
                    VT_ACKACQ => self.finish_acq(w),
                    _ => Err(Errno::EINVAL),
                }
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    /// `CONS_GETINFO`: the terminal layer fills in the cursor, the
    /// size, and the colors (`termtty_ioctl`), `vt` the visible window
    /// and the lock keys.
    fn getinfo(&self, w: usize, arg: &mut [u8]) -> Result<(), Errno> {
        let size = arg
            .get(..2)
            .and_then(|b| b.try_into().ok())
            .map(i16::from_ne_bytes);
        if size != i16::try_from(vid_info::SIZE).ok() || arg.len() < vid_info::SIZE {
            return Err(Errno::EINVAL);
        }
        {
            let t = self.windows[w].lock();
            let c = t.emu.cursor();
            let ws = t.emu.winsize();
            put_u16(arg, vid_info::MV_ROW, c.row);
            put_u16(arg, vid_info::MV_COL, c.col);
            put_u16(arg, vid_info::MV_RSZ, ws.row);
            put_u16(arg, vid_info::MV_CSZ, ws.col);
            let (fg, bg) = t.emu.defattr_cons25();
            arg[vid_info::MV_NORM] = fg;
            arg[vid_info::MV_NORM + 1] = bg;
            // "keep vidcontrol happy; bold backgrounds."
            arg[vid_info::MV_REV] = bg;
            arg[vid_info::MV_REV + 1] = fg & 7;
        }
        if self.is_active(w) {
            self.save_kbd_state(w);
        }
        let a = self.active.load(Ordering::Acquire);
        if let Some(b) = arg.get_mut(vid_info::M_NUM..vid_info::M_NUM + 2) {
            b.copy_from_slice(&i16::try_from(a + 1).unwrap_or(0).to_ne_bytes());
        }
        let locks = self.state.lock().win[w].kbdstate & LOCK_MASK;
        arg[vid_info::MK_KEYLOCK] = locks.to_le_bytes()[0];
        Ok(())
    }

    /// `VT_SETMODE`.
    fn setmode(&self, w: usize, arg: &[u8]) -> Result<(), Errno> {
        let m = VtMode::from_bytes(arg).ok_or(Errno::EINVAL)?;
        let caller = self.host.caller();
        {
            let mut st = self.state.lock();
            let win = &mut st.win[w];
            if win.smode.mode == VT_PROCESS
                && win.pid != caller
                && win.pid != 0
                && self.host.alive(win.pid)
            {
                return Err(Errno::EPERM);
            }
            match m.mode {
                VT_AUTO => {
                    win.smode.mode = VT_AUTO;
                    win.pid = 0;
                }
                VT_PROCESS => {
                    let valid = |s: i16| s > 0 && s <= SIG_MAXSIG;
                    if !(valid(m.relsig) && valid(m.acqsig) && (m.frsig == 0 || valid(m.frsig))) {
                        return Err(Errno::EINVAL);
                    }
                    win.smode = m;
                    win.pid = caller;
                    return Ok(());
                }
                _ => return Err(Errno::EINVAL),
            }
        }
        // Back to `VT_AUTO`, mid-switch: the switch completes.
        let _ = self.finish_rel(w, true);
        let _ = self.finish_acq(w);
        Ok(())
    }
}
