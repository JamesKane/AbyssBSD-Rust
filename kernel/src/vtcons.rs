// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of `vt`: it takes the framebuffer from the early
//! console, and each window gets its terminal, `ttyv0` to `ttyvb`
//! (`terminal_maketty`), sized to the window, whose requests the
//! terminal layer doesn't know go to `vt`. The keyboard's values go to
//! `vt` too, which types into the visible window's terminal. Without a
//! serial console, `ttyv0` is the console terminal, as with FreeBSD's
//! `vidconsole`.
//!
//! Behavior reference: sys/dev/vt/vt_core.c (`vt_upgrade`,
//! `vt_allocate_window`), sys/kern/subr_terminal.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::termios::Winsize;
use sched::{Sleepable, WaitQueue};
use sync::Once;
use vt::Pos;

use crate::proc::Process;
use crate::tty::{self, Tty};

/// The windows' terminals, by window.
static TTYS: Once<Vec<Arc<Tty>>> = Once::new();

/// A window as a terminal's device.
struct Window(usize);

impl tty::Driver for Window {
    fn output(&self, bytes: &[u8]) {
        vt::write(self.0, bytes);
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        vt::ioctl(self.0, s, req, arg)
    }

    fn opened(&self, yes: bool) {
        vt::opened(self.0, yes);
    }

    /// The framebuffer, write-combining (`vtterm_mmap`, `vt_fb_mmap`):
    /// every page asked must start inside it.
    fn mmap(&self, page: u64, pages: u64) -> Result<Arc<vm::object::Object>, Errno> {
        let object = FRAMEBUFFER.get().ok_or(Errno::ENXIO)?;
        let (_, len) = FB_RANGE.get().copied().ok_or(Errno::ENXIO)?;
        let last = page
            .checked_add(pages)
            .and_then(|end| end.checked_sub(1))
            .ok_or(Errno::EINVAL)?;
        if last.checked_mul(PAGE_SIZE).is_none_or(|off| off >= len) {
            return Err(Errno::EINVAL);
        }
        Ok(Arc::clone(object))
    }
}

const PAGE_SIZE: u64 = vm::PAGE_SIZE;

/// The framebuffer's physical base and size in bytes (`fb_pbase`,
/// `fb_size`: its height in scan lines).
static FB_RANGE: Once<(u64, u64)> = Once::new();
/// The framebuffer as the device pager's object, for `mmap`.
static FRAMEBUFFER: Once<Arc<vm::object::Object>> = Once::new();

/// The kernel, as `vt` sees it.
struct Host;

impl vt::Host for Host {
    fn caller(&self) -> i32 {
        Process::current().map_or(0, |p| p.pid())
    }

    fn alive(&self, pid: i32) -> bool {
        crate::proc::all().iter().any(|p| p.pid() == pid)
    }

    fn signal(&self, pid: i32, sig: i32) {
        let Ok(sig) = u32::try_from(sig) else { return };
        if let Some(p) = crate::proc::all().into_iter().find(|p| p.pid() == pid) {
            let info = crate::signal::user_info(sig, 0, abi::signal::code::SI_KERNEL);
            let _ = crate::signal::post(&p, info);
        }
    }

    fn now_ns(&self) -> u64 {
        crate::time::now_ns()
    }

    fn sleep_ns(&self, s: &Sleepable, ns: u64) {
        crate::time::sleep_ns(s, ns);
    }

    fn wait(&self, s: &Sleepable, q: &WaitQueue, done: &dyn Fn() -> bool) -> Result<(), Errno> {
        crate::signal::sleep(s, q, done, None).map(|_| ())
    }

    fn copy_in(&self, dst: &mut [u8], addr: u64) -> Result<(), Errno> {
        crate::uaccess::copy_in(dst, addr).map_err(|_| Errno::EFAULT)
    }

    fn resized(&self, window: usize, size: Pos, pixels: (u32, u32)) {
        if let Some(t) = TTYS.get().and_then(|t| t.get(window)) {
            t.resize(winsize(size, pixels));
        }
    }

    fn io_access(&self, on: bool) -> Result<(), Errno> {
        crate::signal::io_access(on)
    }

    fn enable_bell(&self) -> bool {
        enable_bell()
    }

    fn notify(&self, system: &str, subsystem: &str, kind: &str, data: &str) {
        bus::manager().notify(system, subsystem, kind, Some(data));
    }

    fn beep(&self, hz: u32, ms: u32) {
        let _ = crate::beep::sysbeep(hz, u64::from(ms) * 1_000_000);
    }
}

/// `kern.vt.enable_bell`, 0 unless the command line sets it (a tunable
/// in FreeBSD).
pub(crate) fn enable_bell() -> bool {
    crate::boot::value("kern.vt.enable_bell")
        .and_then(|v| v.parse::<i32>().ok())
        .is_some_and(|v| v != 0)
}

/// A window's terminal size (`vt_winsize`).
fn winsize(size: Pos, (xpixel, ypixel): (u32, u32)) -> Winsize {
    let ws = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    Winsize {
        ws_row: ws(size.row),
        ws_col: ws(size.col),
        ws_xpixel: ws(xpixel),
        ws_ypixel: ws(ypixel),
    }
}

/// Values read from the keyboard: `vt` acts on them (`vt_processkey`),
/// typing into the windows' terminals; machine keys go to `machine`.
/// `false` without `vt`.
pub(crate) fn keys(
    s: &Sleepable,
    codes: &[u32],
    machine: &mut dyn FnMut(input::term::Event),
) -> bool {
    let Some(ttys) = TTYS.get() else {
        return false;
    };
    vt::process_keys(
        codes,
        &mut |w, bytes| {
            if let Some(t) = ttys.get(w) {
                t.input(s, bytes);
            }
        },
        machine,
    )
}

/// Starts `vt` on the loader's framebuffer, if there is one, and makes
/// the windows' terminals. Before devfs is populated.
pub(crate) fn start() {
    let Some((display, early, phys)) = crate::console::take_framebuffer() else {
        return;
    };
    let (width, height) = (display.width, display.height);
    let size = u64::from(display.height) * u64::from(display.stride) * 4;
    if let Err(e) = vt::start(display, &early, &Host) {
        kprintln!("VT: cannot start: {e:?}");
        return;
    }
    crate::console::vt_started();
    let _ = FB_RANGE.set((phys, size));
    let _ = FRAMEBUFFER.set(vm::object::Object::device(
        crate::mm::space::page_ops(),
        vm::Pfn::containing(vm::PhysAddr::new(phys)),
        size.div_ceil(PAGE_SIZE),
        vm::object::MemAttr::WriteCombining,
    ));
    kprintln!("VT(efifb): resolution {width}x{height}");
    let ttys: Vec<_> = (0..vt::WINDOWS)
        .map(|i| {
            let t = tty::create(&alloc::format!("ttyv{i:x}"), Arc::new(Window(i)));
            if let Some((size, pixels)) = vt::size(i) {
                t.set_winsize(winsize(size, pixels));
            }
            t
        })
        .collect();
    if !crate::console::serial_primary() {
        tty::set_console(&ttys[vt::CONSWINDOW]);
    }
    let _ = TTYS.set(ttys);
}
