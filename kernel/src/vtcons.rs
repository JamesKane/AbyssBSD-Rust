// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of `vt`: it takes the framebuffer from the early
//! console, and each window gets its terminal, `ttyv0` to `ttyvb`
//! (`terminal_maketty`), sized to the window. Without a serial console,
//! `ttyv0` is the console terminal, as with FreeBSD's `vidconsole`.
//!
//! Behavior reference: sys/dev/vt/vt_core.c (`vt_upgrade`,
//! `vt_allocate_window`), sys/kern/subr_terminal.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::termios::Winsize;
use sync::Once;

use crate::tty::{self, Tty};

/// The windows' terminals, by window.
static TTYS: Once<Vec<Arc<Tty>>> = Once::new();

/// A window as a terminal's device.
struct Window(usize);

impl tty::Driver for Window {
    fn output(&self, bytes: &[u8]) {
        vt::write(self.0, bytes);
    }
}

/// Starts `vt` on the loader's framebuffer, if there is one, and makes
/// the windows' terminals. Before devfs is populated.
pub(crate) fn start() {
    let Some((display, early)) = crate::console::take_framebuffer() else {
        return;
    };
    let (width, height) = (display.width, display.height);
    if let Err(e) = vt::start(display, &early, crate::time::sleep_ns) {
        kprintln!("VT: cannot start: {e:?}");
        return;
    }
    crate::console::vt_started();
    kprintln!("VT(efifb): resolution {width}x{height}");
    let Some((size, (xpixel, ypixel))) = vt::size() else {
        return;
    };
    let ws = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    let winsize = Winsize {
        ws_row: ws(size.row),
        ws_col: ws(size.col),
        ws_xpixel: ws(xpixel),
        ws_ypixel: ws(ypixel),
    };
    let ttys: Vec<_> = (0..vt::WINDOWS)
        .map(|i| {
            let t = tty::create(&alloc::format!("ttyv{i:x}"), Arc::new(Window(i)));
            t.set_winsize(winsize);
            t
        })
        .collect();
    if !crate::console::has_serial() {
        tty::set_console(&ttys[vt::CONSWINDOW]);
    }
    let _ = TTYS.set(ttys);
}

/// The terminal of the visible window, which the keyboard types into.
pub(crate) fn active_tty() -> Option<Arc<Tty>> {
    TTYS.get()?.get(vt::active()).cloned()
}
