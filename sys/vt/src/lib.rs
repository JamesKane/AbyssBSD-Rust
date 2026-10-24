// SPDX-License-Identifier: BSD-2-Clause
//! Virtual terminals (`vt`): twelve windows on the framebuffer, each a
//! [`vtterm::Terminal`], one of them visible. A window's terminal
//! (`ttyv0` to `ttyvb`, which the kernel makes) writes into its
//! emulator; kernel messages go to window 0, the console window, in the
//! kernel rendition. Drawing is deferred to the `vtflush` thread, at
//! most 25 frames a second (`VT_TIMERFREQ`), and draws only what
//! changed.
//!
//! Behavior reference: sys/dev/vt/vt_core.c, sys/kern/subr_terminal.c,
//! sys/kern/kern_cons.c (`cnputc`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod render;

#[cfg(test)]
mod tests;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use sched::{Sleepable, WaitQueue};
use sync::{Once, PreemptLock, SpinLock};
use vtterm::{Cell, DEFAULT_HISTORY, Pos, Terminal};

pub use render::{Display, Renderer};

/// `VT_MAXWINDOWS`.
pub const WINDOWS: usize = 12;
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
    renderer: PreemptLock<Frame>,
    size: Pos,
    pixels: (u32, u32),
    sleep_ns: fn(&Sleepable, u64),
}

/// The flush thread's: the renderer and the cells it last took.
struct Frame {
    renderer: Renderer,
    view: Vec<Cell>,
}

static VT: Once<Vt> = Once::new();

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
/// kernel output, draws it, and starts the flush thread, which waits
/// between frames with `sleep_ns`.
///
/// # Errors
///
/// See [`Error`].
pub fn start(display: Display, early: &[u8], sleep_ns: fn(&Sleepable, u64)) -> Result<(), Error> {
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
        renderer: PreemptLock::new(Frame {
            renderer,
            view: Vec::new(),
        }),
        size,
        pixels,
        sleep_ns,
    };
    if VT.set(vt).is_err() {
        return Err(Error::Started);
    }
    write_kernel(early);
    if let Some(vt) = VT.get() {
        vt.flush(true);
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

/// Makes `window` the visible one (`vt_window_switch`); whether it is.
pub fn switch(window: usize) -> bool {
    let Some(vt) = VT.get() else { return false };
    if window >= WINDOWS {
        return false;
    }
    if vt.active.swap(window, Ordering::AcqRel) != window {
        vt.invalid.store(true, Ordering::Release);
        vt.changed(window);
    }
    true
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
    vt.flush(false);
}

impl Vt {
    /// Window `window` has new output: a frame is due if it is visible
    /// (`vtterm_done`).
    fn changed(&self, window: usize) {
        if window == self.active.load(Ordering::Acquire) && !self.dirty.swap(true, Ordering::AcqRel)
        {
            self.wake.wake_one();
        }
    }

    /// Draws the visible window's changes (`vt_flush`); with `wait`
    /// false (the panic path), only if the locks are free.
    fn flush(&self, wait: bool) {
        if self.holds.load(Ordering::Acquire) != 0 {
            return;
        }
        let mut f = if wait {
            self.renderer.lock()
        } else {
            let Some(f) = self.renderer.try_lock() else {
                return;
            };
            f
        };
        let Frame { renderer, view } = &mut *f;
        let invalid = self.invalid.swap(false, Ordering::AcqRel);
        let cursor;
        {
            let w = &self.windows[self.active.load(Ordering::Acquire)];
            let mut t = if wait {
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
/// a frame's time before the next.
fn thread(s: &Sleepable) {
    let Some(vt) = VT.get() else { return };
    loop {
        vt.wake.wait_until(s, || vt.dirty.load(Ordering::Acquire));
        vt.dirty.store(false, Ordering::Release);
        vt.flush(true);
        (vt.sleep_ns)(s, 1_000_000_000 / TIMERFREQ);
    }
}
