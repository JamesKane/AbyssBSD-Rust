// SPDX-License-Identifier: BSD-2-Clause
//! Virtual terminals (`vt`): twelve windows on the framebuffer, each a
//! [`vtterm::Terminal`], one of them visible. A window's terminal
//! (`ttyv0` to `ttyvb`, which the kernel makes) writes into its
//! emulator; kernel messages go to window 0, the console window, in the
//! kernel rendition. Drawing is deferred to the `vtflush` thread, at
//! most 25 frames a second (`VT_TIMERFREQ`), and draws only what
//! changed. Switching follows `vt`'s protocol: only to a window that is
//! open (or the console's), and with the consent of a process that
//! controls the visible one (`VT_PROCESS`). Each window keeps its own
//! keyboard mode and lock state, given to `kbdmux0` while it is
//! visible.
//!
//! Behavior reference: sys/dev/vt/vt_core.c, sys/kern/subr_terminal.c,
//! sys/kern/kern_cons.c (`cnputc`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod ioctl;
mod keys;
pub mod render;

#[cfg(test)]
mod tests;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use abi::consio::{VT_AUTO, VT_PROCESS, VtMode};
use abi::errno::Errno;
use abi::kbio::{K_XLATE, KDSKBMODE, KDSKBSTATE, LOCK_MASK};
use sched::lock::{LockClass, Mutex};
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};
use vtterm::{Cell, DEFAULT_HISTORY, Pos, Terminal};

pub use ioctl::ioctl;
pub use keys::process_keys;
pub use render::{Display, Renderer};

/// `VT_MAXWINDOWS`.
pub const WINDOWS: usize = 12;
const NWIN: u32 = 12;
const _: () = assert!(WINDOWS == NWIN as usize);
/// `kern.vt.deadtimer`: seconds a controlling process has to release
/// the screen before the switch goes ahead without it.
const DEADTIMER_NS: u64 = 15_000_000_000;

/// What `vt` needs of the kernel.
pub trait Host: Send + Sync {
    /// The calling process's ID.
    fn caller(&self) -> i32;
    /// Whether process `pid` exists.
    fn alive(&self, pid: i32) -> bool;
    /// Sends signal `sig` to process `pid`.
    fn signal(&self, pid: i32, sig: i32);
    /// Nanoseconds since boot.
    fn now_ns(&self) -> u64;
    /// Sleeps `ns` nanoseconds.
    fn sleep_ns(&self, s: &Sleepable, ns: u64);
    /// Waits on `q` until `done`, or a signal (`EINTR`, `ERESTART`).
    ///
    /// # Errors
    ///
    /// On a signal.
    fn wait(&self, s: &Sleepable, q: &WaitQueue, done: &dyn Fn() -> bool) -> Result<(), Errno>;
}

/// A window's state beyond its terminal (`struct vt_window`).
#[derive(Clone, Copy, Debug)]
struct Win {
    /// Its terminal is open (`VWF_OPENED`).
    opened: bool,
    /// `vw_kbdmode`, and the lock and LED bits of `vw_kbdstate`.
    kbdmode: i32,
    kbdstate: i32,
    /// `KD_GRAPHICS` (`VWF_GRAPHICS`).
    graphics: bool,
    /// `VT_LOCKSWITCH` (`VWF_VTYLOCK`).
    vtylock: bool,
    /// Scroll Lock's scroll mode (`VWF_SCROLL`).
    scroll: bool,
    /// `vw_smode`, and the controlling process's ID.
    smode: VtMode,
    pid: i32,
    /// Waiting for the process to release (`VWF_SWWAIT_REL`) or
    /// acknowledge (`VWF_SWWAIT_ACQ`) the screen.
    swwait_rel: bool,
    swwait_acq: bool,
    /// The window a pending release switches to.
    switch_to: usize,
}

impl Win {
    const NEW: Self = Self {
        opened: false,
        kbdmode: K_XLATE,
        kbdstate: 0,
        graphics: false,
        vtylock: false,
        scroll: false,
        smode: VtMode {
            mode: VT_AUTO,
            waitv: 0,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
        },
        pid: 0,
        swwait_rel: false,
        swwait_acq: false,
        switch_to: 0,
    };
}

/// The device's state beyond the windows' terminals.
struct State {
    win: [Win; WINDOWS],
    /// An Alt key is down (`vd_kbstate & ALKED`).
    alt: bool,
    /// `MOUSE_SHOW` (`VDF_MOUSECURSOR`).
    mouse_cursor: bool,
    /// `CONS_QUIET_BELL` (`VDF_QUIET_BELL`).
    quiet_bell: bool,
    /// A pending release's deadline, and the window it switches to
    /// (`vw_timeout_task_dead`).
    dead: Option<(u64, usize)>,
}

/// A signal to send once the state is unlocked.
type Signal = Option<(i32, i32)>;
/// `VT_CONSWINDOW`: the window kernel messages go to.
pub const CONSWINDOW: usize = 0;
/// `VT_TIMERFREQ`: frames a second at most.
const TIMERFREQ: u64 = 25;

/// The virtual terminals (`struct vt_device`).
struct Vt {
    windows: Vec<SpinLock<Terminal>>,
    /// The visible window (`vd_curwindow`).
    active: AtomicUsize,
    /// The visible window changed since the last frame.
    dirty: AtomicBool,
    /// The next frame draws every cell (`VDF_INVALID`).
    invalid: AtomicBool,
    /// Drawing is held while nonzero.
    holds: AtomicU32,
    wake: WaitQueue,
    /// The flush thread's, and the panic path's: drawing sleeps no
    /// one and stays preemptible.
    renderer: Mutex<Frame>,
    size: Pos,
    pixels: (u32, u32),
    host: &'static dyn Host,
    state: SpinLock<State>,
    /// Woken on every switch (`vd_winswitch`).
    switched: WaitQueue,
}

/// The flush thread's: the renderer and the cells it last took.
struct Frame {
    renderer: Renderer,
    view: Vec<Cell>,
}

static VT: Once<Vt> = Once::new();

/// The renderer's lock: taken with nothing else held.
static RENDERER: LockClass = LockClass::new("vt renderer", 960);

/// Why [`start`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Already started.
    Started,
    /// The framebuffer cannot hold one cell.
    Display,
    /// The flush thread cannot start.
    Spawn,
}

/// Takes the framebuffer over (`vt_upgrade`): makes the windows, puts
/// `early` (what the early console showed) on the console window as
/// kernel output, draws it, and starts the flush thread.
///
/// # Errors
///
/// See [`Error`].
pub fn start(display: Display, early: &[u8], host: &'static dyn Host) -> Result<(), Error> {
    if VT.get().is_some() {
        return Err(Error::Started);
    }
    let renderer = Renderer::new(display).ok_or(Error::Display)?;
    let size = renderer.size();
    let pixels = renderer.pixel_size();
    let windows = (0..WINDOWS)
        .map(|_| SpinLock::new(Terminal::new(size, DEFAULT_HISTORY)))
        .collect();
    let vt = Vt {
        windows,
        active: AtomicUsize::new(CONSWINDOW),
        dirty: AtomicBool::new(true),
        invalid: AtomicBool::new(true),
        holds: AtomicU32::new(0),
        wake: WaitQueue::new(),
        renderer: Mutex::new(
            &RENDERER,
            Frame {
                renderer,
                view: Vec::new(),
            },
        ),
        size,
        pixels,
        host,
        state: SpinLock::new(State {
            win: [Win::NEW; WINDOWS],
            alt: false,
            mouse_cursor: false,
            quiet_bell: false,
            dead: None,
        }),
        switched: WaitQueue::new(),
    };
    if VT.set(vt).is_err() {
        return Err(Error::Started);
    }
    write_kernel(early);
    if let Some(vt) = VT.get() {
        vt.flush(None);
    }
    sched::Builder::new("vtflush")
        .priority(sched::Priority::KERNEL)
        .spawn(thread)
        .map_err(|_| Error::Spawn)?;
    Ok(())
}

/// Whether `vt` has the framebuffer.
#[must_use]
pub fn running() -> bool {
    VT.get().is_some()
}

/// A window's size in cells and in pixels (`vt_winsize`).
#[must_use]
pub fn size() -> Option<(Pos, (u32, u32))> {
    VT.get().map(|vt| (vt.size, vt.pixels))
}

/// The visible window.
#[must_use]
pub fn active() -> usize {
    VT.get()
        .map_or(CONSWINDOW, |vt| vt.active.load(Ordering::Acquire))
}

/// Writes a window terminal's output (`termtty_outwakeup`).
pub fn write(window: usize, bytes: &[u8]) {
    let Some(vt) = VT.get() else { return };
    let Some(w) = vt.windows.get(window) else {
        return;
    };
    w.lock().write(bytes);
    vt.changed(window);
}

/// Writes kernel output to the console window in the kernel rendition,
/// each NL as CR NL (`cnputc`, `termcn_cnputc`).
pub fn write_kernel(bytes: &[u8]) {
    let Some(vt) = VT.get() else { return };
    {
        let mut t = vt.windows[CONSWINDOW].lock();
        for line in bytes.split_inclusive(|&b| b == b'\n') {
            match line.split_last() {
                Some((b'\n', text)) => {
                    t.write_kernel(text);
                    t.write_kernel(b"\r\n");
                }
                _ => t.write_kernel(line),
            }
        }
    }
    vt.changed(CONSWINDOW);
}

/// Window `window`'s terminal was opened (first open) or closed (last
/// close) (`vtterm_opened`).
pub fn opened(window: usize, yes: bool) {
    if let Some(vt) = VT.get()
        && let Some(w) = vt.state.lock().win.get_mut(window)
    {
        w.opened = yes;
    }
}

/// Requests a switch to `window` as the keyboard's screen keys do
/// (`vt_proc_window_switch`).
///
/// # Errors
///
/// `EBUSY` if switching is locked or another switch is pending;
/// `EINVAL` if the window isn't open.
pub fn switch(window: usize) -> Result<(), Errno> {
    let Some(vt) = VT.get() else {
        return Err(Errno::ENXIO);
    };
    if window >= WINDOWS {
        return Err(Errno::EINVAL);
    }
    vt.proc_window_switch(window)
}

/// Holds (`true`) or releases (`false`) drawing; calls nest. Output
/// still goes to the windows, and reaches the screen on release.
pub fn hold(on: bool) {
    let Some(vt) = VT.get() else { return };
    if on {
        vt.holds.fetch_add(1, Ordering::AcqRel);
    } else if vt
        .holds
        .try_update(Ordering::AcqRel, Ordering::Acquire, |h| h.checked_sub(1))
        == Ok(1)
    {
        vt.invalid.store(true, Ordering::Release);
        vt.changed(vt.active.load(Ordering::Acquire));
    }
}

/// Writes kernel output on the panic path and draws it at once. The
/// windows' and the renderer's locks are only tried: if the code that
/// panicked holds them, the screen keeps what it had.
pub fn panic_write(bytes: &[u8]) {
    let Some(vt) = VT.get() else { return };
    vt.active.store(CONSWINDOW, Ordering::Release);
    if let Some(mut t) = vt.windows[CONSWINDOW].try_lock() {
        for line in bytes.split_inclusive(|&b| b == b'\n') {
            match line.split_last() {
                Some((b'\n', text)) => {
                    t.write_kernel(text);
                    t.write_kernel(b"\r\n");
                }
                _ => t.write_kernel(line),
            }
        }
    }
    vt.flush(None);
}

impl Vt {
    /// `vt_proc_window_switch`.
    fn proc_window_switch(&self, to: usize) -> Result<(), Errno> {
        let cur = self.active.load(Ordering::Acquire);
        let sig;
        {
            let mut st = self.state.lock();
            let c = st.win[cur];
            if c.vtylock {
                return Err(Errno::EBUSY);
            }
            if c.swwait_rel {
                return if c.switch_to == to {
                    Ok(())
                } else {
                    Err(Errno::EBUSY)
                };
            }
            if to == cur {
                return Ok(());
            }
            if !st.win[to].opened && to != CONSWINDOW {
                return Err(Errno::EINVAL);
            }
            if !(c.smode.mode == VT_PROCESS && self.proc_alive(&mut st.win[cur])) {
                drop(st);
                return self.late_window_switch(to);
            }
            // Ask the controlling process to release the screen, with a
            // deadline (`vt_window_preswitch`).
            st.win[cur].switch_to = to;
            st.dead = Some((self.host.now_ns() + DEADTIMER_NS, to));
            sig = self.signal_rel(&mut st.win[cur]);
        }
        self.send(sig);
        self.wake.wake_one();
        Ok(())
    }

    /// `vt_late_window_switch`: the switch, then the new window's
    /// controlling process is told it has the screen, or, if the switch
    /// failed, the old one's that it keeps it.
    fn late_window_switch(&self, to: usize) -> Result<(), Errno> {
        {
            let mut st = self.state.lock();
            if st.dead.is_some_and(|(_, t)| t == to) {
                st.dead = None;
            }
        }
        let r = self.window_switch(to);
        let w = if r.is_ok() {
            to
        } else {
            self.active.load(Ordering::Acquire)
        };
        let sig = {
            let mut st = self.state.lock();
            self.signal_acq(&mut st.win[w])
        };
        self.send(sig);
        r
    }

    /// `vt_window_switch`: makes `to` visible, and gives the keyboard
    /// its mode and lock state, keeping the old window's.
    fn window_switch(&self, to: usize) -> Result<(), Errno> {
        let cur = self.active.load(Ordering::Acquire);
        if cur == to {
            return Ok(());
        }
        if !self.state.lock().win[to].opened && to != CONSWINDOW {
            return Err(Errno::EINVAL);
        }
        self.active.store(to, Ordering::Release);
        self.invalidate(to);
        self.switched.wake_all();
        if self.state.lock().win[cur].kbdmode == K_XLATE {
            self.save_kbd_state(cur);
        }
        let (mode, locks) = {
            let st = self.state.lock();
            (st.win[to].kbdmode, st.win[to].kbdstate & LOCK_MASK)
        };
        let _ = input::console_ioctl(KDSKBMODE, &mut mode.to_ne_bytes());
        let _ = input::console_ioctl(KDSKBSTATE, &mut locks.to_ne_bytes());
        Ok(())
    }

    /// Whether `w`'s controlling process lives (`vt_proc_alive`); if it
    /// died, the window goes back to `VT_AUTO`.
    fn proc_alive(&self, w: &mut Win) -> bool {
        if w.smode.mode != VT_PROCESS {
            return false;
        }
        if w.pid != 0 {
            if self.host.alive(w.pid) {
                return true;
            }
            w.pid = 0;
            w.smode.mode = VT_AUTO;
        }
        false
    }

    /// `signal_vt_rel`: the release signal to send, if a process
    /// controls `w`.
    fn signal_rel(&self, w: &mut Win) -> Signal {
        if w.smode.mode != VT_PROCESS || !self.proc_alive(w) {
            w.pid = 0;
            return None;
        }
        w.swwait_rel = true;
        Some((w.pid, i32::from(w.smode.relsig)))
    }

    /// `signal_vt_acq`: the acquire signal to send, if a process
    /// controls `w`.
    fn signal_acq(&self, w: &mut Win) -> Signal {
        if w.smode.mode != VT_PROCESS || !self.proc_alive(w) {
            w.pid = 0;
            return None;
        }
        w.swwait_acq = true;
        Some((w.pid, i32::from(w.smode.acqsig)))
    }

    fn send(&self, sig: Signal) {
        if let Some((pid, sig)) = sig {
            self.host.signal(pid, sig);
        }
    }

    /// `finish_vt_rel`: the process answered a release request; with
    /// `release`, the switch goes ahead.
    fn finish_rel(&self, w: usize, release: bool) -> Result<(), Errno> {
        let to = {
            let mut st = self.state.lock();
            if !st.win[w].swwait_rel {
                return Err(Errno::EINVAL);
            }
            st.win[w].swwait_rel = false;
            if !release {
                return Ok(());
            }
            st.dead = None;
            st.win[w].switch_to
        };
        let _ = self.late_window_switch(to);
        Ok(())
    }

    /// `finish_vt_acq`: the process acknowledged the screen.
    fn finish_acq(&self, w: usize) -> Result<(), Errno> {
        let mut st = self.state.lock();
        if !st.win[w].swwait_acq {
            return Err(Errno::EINVAL);
        }
        st.win[w].swwait_acq = false;
        Ok(())
    }

    /// Redraws window `w` in full, if it is visible.
    fn invalidate(&self, w: usize) {
        if w == self.active.load(Ordering::Acquire) {
            self.invalid.store(true, Ordering::Release);
            self.changed(w);
        }
    }

    /// The release deadline passed: the switch goes ahead
    /// (`vt_switch_timer`).
    fn check_dead(&self) {
        let due = {
            let mut st = self.state.lock();
            match st.dead {
                Some((at, to)) if self.host.now_ns() >= at => {
                    st.dead = None;
                    Some(to)
                }
                _ => None,
            }
        };
        if let Some(to) = due {
            let _ = self.late_window_switch(to);
        }
    }

    /// Window `window` has new output: a frame is due if it is visible
    /// (`vtterm_done`).
    fn changed(&self, window: usize) {
        if window == self.active.load(Ordering::Acquire) && !self.dirty.swap(true, Ordering::AcqRel)
        {
            self.wake.wake_one();
        }
    }

    /// Draws the visible window's changes (`vt_flush`). Without `s`
    /// (at start, and on the panic path), only if the locks are free.
    fn flush(&self, s: Option<&Sleepable>) {
        if self.holds.load(Ordering::Acquire) != 0 {
            return;
        }
        let mut f = match s {
            Some(s) => self.renderer.lock(s),
            None => {
                let Some(f) = self.renderer.try_lock() else {
                    return;
                };
                f
            }
        };
        let Frame { renderer, view } = &mut *f;
        let invalid = self.invalid.swap(false, Ordering::AcqRel);
        let cursor;
        {
            let w = &self.windows[self.active.load(Ordering::Acquire)];
            let mut t = if s.is_some() {
                w.lock()
            } else {
                let Some(t) = w.try_lock() else {
                    self.invalid.fetch_or(invalid, Ordering::AcqRel);
                    return;
                };
                t
            };
            if !invalid && !t.buf.damaged() {
                return;
            }
            t.buf.take_damage(|_, _, _| {});
            view.clear();
            for r in 0..self.size.row {
                view.extend_from_slice(t.buf.view_row(r));
            }
            let c = t.buf.cursor();
            cursor = t.buf.is_cursor(c).then_some(c);
        }
        if invalid {
            renderer.invalidate();
        }
        renderer.draw(view, cursor);
    }
}

/// The flush thread (`vt_timer`): a frame when something changed, then
/// a frame's time before the next; it also runs out release deadlines.
fn thread(s: &Sleepable) {
    let Some(vt) = VT.get() else { return };
    loop {
        vt.wake.wait_until(s, || {
            vt.dirty.load(Ordering::Acquire) || vt.state.lock().dead.is_some()
        });
        vt.dirty.store(false, Ordering::Release);
        vt.flush(Some(s));
        vt.check_dead();
        vt.host.sleep_ns(s, 1_000_000_000 / TIMERFREQ);
    }
}
