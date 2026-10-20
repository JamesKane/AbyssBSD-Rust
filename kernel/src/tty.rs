// SPDX-License-Identifier: BSD-2-Clause
//! Terminals: the line discipline ([`tty::Discipline`]) on a device, as
//! a character device, with job control.
//!
//! The console's UART is the one terminal so far (`ttyu0` for `uart0`,
//! and `/dev/console`). Its `uart` driver takes received bytes off the
//! port in its interrupt filter; [`Tty::input`], in the interrupt thread,
//! runs them through the discipline, echoes, and signals the foreground
//! process group. Output goes to the port synchronously.
//!
//! A session leader makes a terminal its session's controlling terminal
//! with `TIOCSCTTY`; it then has a foreground process group. A process of
//! that session outside the foreground that reads is sent `SIGTTIN` (or,
//! with `TOSTOP`, writes and is sent `SIGTTOU`), as is one that changes
//! the terminal: unless it ignores or blocks the signal, or its group is
//! orphaned, which gets `EIO` for a read. When the session leader exits,
//! the foreground group gets `SIGHUP` and the terminal is free again.
//! `/dev/tty` opens the caller's controlling terminal.
//!
//! Behavior reference: sys/kern/tty.c, sys/kern/tty_tty.c.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::ioctl::*;
use abi::signal::{SIGCONT, SIGHUP, SIGINFO, SIGTTIN, SIGTTOU, SIGWINCH};
use abi::termios::{TOSTOP, Termios, VMIN, VTIME, Winsize};
use sched::lock::{LockClass, Mutex};
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};
use tty::{Discipline, Line};
use vfs::{CharDevice, Uio, Vnode};

use crate::proc::Process;
use crate::signal::Woke;

/// Above the driver serializer the interrupt thread holds when it types.
static TTY_LOCK: LockClass = LockClass::new("tty", 450);

/// `NO_PID`: what `TIOCGPGRP` reports with no foreground group.
const NO_PID: i32 = 100_000;

/// The session a terminal controls, and its foreground process group.
#[derive(Clone, Copy, Debug, Default)]
struct Ctty {
    /// 0: none.
    sid: i32,
    pgrp: i32,
}

/// A terminal.
pub(crate) struct Tty {
    name: String,
    disc: Mutex<Discipline>,
    /// Mirrors for wait conditions: a read would return; output is
    /// stopped.
    readable: AtomicBool,
    stopped: AtomicBool,
    ctty: SpinLock<Ctty>,
    /// Bumped when the foreground group changes (background sleepers
    /// look again).
    fg_gen: AtomicU64,
    /// Bytes typed so far.
    received: AtomicU64,
    winsize: SpinLock<Winsize>,
    input_wait: WaitQueue,
    output_wait: WaitQueue,
    bg_wait: WaitQueue,
    /// Writes bytes to the device.
    sink: fn(&[u8]),
    /// The devfs node `/dev/tty` opens.
    vnode: Once<Weak<Vnode>>,
}

/// Every terminal (just the console's, so far).
static TTYS: SpinLock<Vec<Arc<Tty>>> = SpinLock::new(Vec::new());
/// The console's terminal.
static CONSOLE: Once<Arc<Tty>> = Once::new();

/// Makes the console's terminal `ttyu<unit>`, which writes with `sink`.
pub(crate) fn create_console(unit: u32, sink: fn(&[u8])) -> Arc<Tty> {
    let tty = Arc::new(Tty {
        name: alloc::format!("ttyu{unit}"),
        disc: Mutex::new(&TTY_LOCK, Discipline::new()),
        readable: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        ctty: SpinLock::new(Ctty::default()),
        fg_gen: AtomicU64::new(0),
        received: AtomicU64::new(0),
        winsize: SpinLock::new(Winsize::default()),
        input_wait: WaitQueue::new(),
        output_wait: WaitQueue::new(),
        bg_wait: WaitQueue::new(),
        sink,
        vnode: Once::new(),
    });
    TTYS.lock().push(Arc::clone(&tty));
    let _ = CONSOLE.set(Arc::clone(&tty));
    tty
}

/// The console's terminal, once its port can be typed at.
pub(crate) fn console() -> Option<Arc<Tty>> {
    CONSOLE.get().cloned()
}

/// Every terminal, for devfs.
pub(crate) fn all() -> Vec<Arc<Tty>> {
    TTYS.lock().clone()
}

/// The terminal `sid` controls, if any.
fn controlling(sid: i32) -> Option<Arc<Tty>> {
    TTYS.lock()
        .iter()
        .find(|t| t.ctty.lock().sid == sid)
        .cloned()
}

/// The controlling terminal of session `sid`: its device number and its
/// foreground process group (`ki_tdev`, `ki_tpgid`).
pub(crate) fn of_session(sid: i32) -> Option<(u64, i32)> {
    let t = controlling(sid)?;
    let pgrp = t.ctty.lock().pgrp;
    let rdev = t
        .vnode
        .get()
        .and_then(Weak::upgrade)
        // devfs numbers each device by its inode.
        .map_or(abi::kinfo::NODEV, |v| v.ino());
    Some((rdev, pgrp))
}

/// A session's leader exited: its terminal's foreground group gets
/// `SIGHUP` (and `SIGCONT`, to act on it), and the terminal is free.
pub(crate) fn session_ended(sid: i32) {
    let Some(t) = controlling(sid) else {
        return;
    };
    let fg = core::mem::take(&mut *t.ctty.lock()).pgrp;
    t.fg_gen.fetch_add(1, Ordering::Release);
    t.bg_wait.wake_all();
    if fg != 0 {
        crate::signal::post_group(fg, SIGHUP);
        crate::signal::post_group(fg, SIGCONT);
    }
}

/// Makes `t` the controlling terminal of `p`'s session, with `p`'s group
/// in the foreground, as `TIOCSCTTY` by a session leader does (for the
/// kernel's own processes, which start as session leaders).
pub(crate) fn make_controlling(t: &Tty, p: &Process) {
    *t.ctty.lock() = Ctty {
        sid: p.sid(),
        pgrp: p.pgid(),
    };
    t.fg_gen.fetch_add(1, Ordering::Release);
}

impl Tty {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Records the devfs node for this terminal.
    pub(crate) fn set_vnode(&self, v: &Arc<Vnode>) {
        let _ = self.vnode.set(Arc::downgrade(v));
    }

    /// Updates the wait-condition mirrors from the discipline.
    fn mirror(&self, d: &Discipline) {
        // Release: a waiter that sees the flag sees the queue it mirrors.
        self.readable.store(d.readable(), Ordering::Release);
        self.stopped.store(d.stopped(), Ordering::Release);
    }

    /// Sends queued output to the device.
    fn drain(&self, d: &mut Discipline) {
        let mut buf = [0u8; 256];
        loop {
            let n = d.take_output(&mut buf);
            if n == 0 {
                break;
            }
            (self.sink)(&buf[..n]);
        }
    }

    /// Bytes typed so far.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub(crate) fn received(&self) -> u64 {
        self.received.load(Ordering::Acquire)
    }

    /// Throws away typed input not yet read.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub(crate) fn flush_input(&self, s: &Sleepable) {
        let mut d = self.disc.lock(s);
        d.flush(true, false);
        self.mirror(&d);
    }

    /// Typed bytes (the interrupt thread).
    pub(crate) fn input(&self, s: &Sleepable, bytes: &[u8]) {
        let mut signals: Vec<u32> = Vec::new();
        {
            let mut d = self.disc.lock(s);
            for &b in bytes {
                if let Some(sig) = d.input(b) {
                    signals.push(sig);
                }
            }
            self.drain(&mut d);
            self.mirror(&d);
        }
        self.received
            .fetch_add(bytes.len() as u64, Ordering::Release);
        self.input_wait.wake_all();
        self.output_wait.wake_all();
        let fg = self.ctty.lock().pgrp;
        if fg != 0 {
            for sig in signals {
                crate::signal::post_group(fg, sig);
            }
        }
    }

    /// Whether `p` may go ahead with something that, from the background,
    /// sends `sig` (FreeBSD's `tty_wait_background`). Sends it and waits
    /// until the process is in the foreground, or fails.
    fn background(&self, s: &Sleepable, sig: u32) -> Result<(), Errno> {
        let Some(p) = Process::current() else {
            return Ok(());
        };
        loop {
            let generation = self.fg_gen.load(Ordering::Acquire);
            let c = *self.ctty.lock();
            if c.sid != p.sid() || c.pgrp == p.pgid() {
                return Ok(());
            }
            if crate::signal::ignores_or_blocks(&p, sig) {
                // Writes and changes go ahead; reads fail.
                return if sig == SIGTTOU {
                    Ok(())
                } else {
                    Err(Errno::EIO)
                };
            }
            if p.vforked() || crate::proc::pgrp_orphaned(p.pgid()) {
                return Err(Errno::EIO);
            }
            crate::signal::post_group(p.pgid(), sig);
            crate::signal::sleep_after_stop_signal(s, &self.bg_wait, || {
                self.fg_gen.load(Ordering::Acquire) != generation
            })?;
        }
    }

    fn read_canonical(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let mut buf = alloc::vec![0u8; uio.resid().min(tty::INPUT_MAX)];
        loop {
            self.background(s, SIGTTIN)?;
            let got = {
                let mut d = self.disc.lock(s);
                let r = d.read_line(&mut buf);
                self.mirror(&d);
                r
            };
            if let Line::Read(n) = got {
                return uio.copy_out(&buf[..n]);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            crate::signal::sleep(
                s,
                &self.input_wait,
                || self.readable.load(Ordering::Acquire),
                None,
            )?;
        }
    }

    /// Non-canonical reads: `VMIN` bytes, or what came within `VTIME`
    /// tenths of a second (POSIX's four cases).
    fn read_raw(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
        t: &Termios,
    ) -> Result<usize, Errno> {
        let (vmin, vtime) = (usize::from(t.c_cc[VMIN]), u64::from(t.c_cc[VTIME]));
        let mut buf = [0u8; 256];
        let mut total = 0;
        loop {
            self.background(s, SIGTTIN)?;
            loop {
                let n = {
                    let mut d = self.disc.lock(s);
                    let n = d.read_raw(&mut buf[..uio.resid().min(256)]);
                    self.mirror(&d);
                    n
                };
                if n == 0 {
                    break;
                }
                total += uio.copy_out(&buf[..n])?;
                if uio.resid() == 0 {
                    return Ok(total);
                }
            }
            if total >= vmin.max(1) || (vmin == 0 && vtime == 0) {
                return Ok(total);
            }
            if nonblock {
                return if total > 0 {
                    Ok(total)
                } else {
                    Err(Errno::EAGAIN)
                };
            }
            // The timer runs from the call (VMIN 0) or between bytes.
            let timeout = (vtime > 0 && (vmin == 0 || total > 0)).then(|| vtime * 100_000_000);
            match crate::signal::sleep(
                s,
                &self.input_wait,
                || self.readable.load(Ordering::Acquire),
                timeout,
            ) {
                Ok(Woke::Done) => {}
                Ok(Woke::TimedOut) => return Ok(total),
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            }
        }
    }

    /// Changes the foreground process group (`TIOCSPGRP`).
    fn set_pgrp(&self, s: &Sleepable, pgrp: i32) -> Result<(), Errno> {
        self.background(s, SIGTTOU)?;
        let p = Process::current().ok_or(Errno::ENOTTY)?;
        let in_session = crate::proc::all()
            .iter()
            .any(|q| q.pgid() == pgrp && q.sid() == p.sid());
        if pgrp <= 0 || !in_session {
            return Err(Errno::EPERM);
        }
        {
            let mut c = self.ctty.lock();
            if c.sid != p.sid() {
                return Err(Errno::ENOTTY);
            }
            c.pgrp = pgrp;
        }
        self.fg_gen.fetch_add(1, Ordering::Release);
        self.bg_wait.wake_all();
        Ok(())
    }

    /// `TIOCSCTTY`: only a session leader, for a session without one, on
    /// a terminal no other session holds.
    fn set_ctty(&self, s: &Sleepable) -> Result<(), Errno> {
        self.background(s, SIGTTOU)?;
        let p = Process::current().ok_or(Errno::EPERM)?;
        if p.pid() != p.sid() {
            return Err(Errno::EPERM);
        }
        if self.ctty.lock().sid == p.sid() {
            return Ok(());
        }
        let others = TTYS.lock().iter().any(|t| t.ctty.lock().sid == p.sid());
        let mut c = self.ctty.lock();
        if others || c.sid != 0 {
            return Err(Errno::EPERM);
        }
        *c = Ctty {
            sid: p.sid(),
            pgrp: p.pgid(),
        };
        drop(c);
        self.fg_gen.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// The caller's session, if this is its controlling terminal.
    fn caller_ctty(&self) -> Result<Ctty, Errno> {
        let p = Process::current().ok_or(Errno::ENOTTY)?;
        let c = *self.ctty.lock();
        if c.sid == 0 || c.sid != p.sid() {
            return Err(Errno::ENOTTY);
        }
        Ok(c)
    }

    fn signal_foreground(&self, sig: u32) {
        let fg = self.ctty.lock().pgrp;
        if fg != 0 {
            crate::signal::post_group(fg, sig);
        }
    }
}

fn get_int(arg: &[u8]) -> i32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i32::from_ne_bytes)
}

fn put_int(arg: &mut [u8], v: i32) {
    if let Some(b) = arg.get_mut(..4) {
        b.copy_from_slice(&v.to_ne_bytes());
    }
}

/// `termios` from an `ioctl` argument (laid out as C's).
fn termios_from(arg: &[u8]) -> Termios {
    let word = |o: usize| u32::from_ne_bytes([arg[o], arg[o + 1], arg[o + 2], arg[o + 3]]);
    let mut t = Termios {
        c_iflag: word(0),
        c_oflag: word(4),
        c_cflag: word(8),
        c_lflag: word(12),
        c_ispeed: word(36),
        c_ospeed: word(40),
        ..Termios::default()
    };
    t.c_cc.copy_from_slice(&arg[16..36]);
    t
}

fn termios_to(t: &Termios, arg: &mut [u8]) {
    for (o, v) in [
        (0, t.c_iflag),
        (4, t.c_oflag),
        (8, t.c_cflag),
        (12, t.c_lflag),
        (36, t.c_ispeed),
        (40, t.c_ospeed),
    ] {
        arg[o..o + 4].copy_from_slice(&v.to_ne_bytes());
    }
    arg[16..36].copy_from_slice(&t.c_cc);
}

fn winsize_from(arg: &[u8]) -> Winsize {
    let half = |o: usize| u16::from_ne_bytes([arg[o], arg[o + 1]]);
    Winsize {
        ws_row: half(0),
        ws_col: half(2),
        ws_xpixel: half(4),
        ws_ypixel: half(6),
    }
}

fn winsize_to(w: &Winsize, arg: &mut [u8]) {
    for (o, v) in [
        (0, w.ws_row),
        (2, w.ws_col),
        (4, w.ws_xpixel),
        (6, w.ws_ypixel),
    ] {
        arg[o..o + 2].copy_from_slice(&v.to_ne_bytes());
    }
}

/// A terminal as a device (`/dev/ttyu0`, `/dev/console`).
pub(crate) struct TtyDevice(pub(crate) Arc<Tty>);

impl CharDevice for TtyDevice {
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        if uio.resid() == 0 {
            return Ok(0);
        }
        let t = *self.0.disc.lock(s).termios();
        if t.c_lflag & abi::termios::ICANON != 0 {
            self.0.read_canonical(s, uio, nonblock)
        } else {
            self.0.read_raw(s, uio, nonblock, &t)
        }
    }

    fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let tty = &self.0;
        if tty.disc.lock(s).termios().c_lflag & TOSTOP != 0 {
            tty.background(s, SIGTTOU)?;
        }
        let mut buf = [0u8; 256];
        let mut done = 0;
        while uio.resid() > 0 {
            let n = uio.copy_in(&mut buf)?;
            let mut taken = 0;
            loop {
                {
                    let mut d = tty.disc.lock(s);
                    taken += d.write(&buf[taken..n]);
                    tty.drain(&mut d);
                    tty.mirror(&d);
                }
                if taken == n {
                    break;
                }
                // Output is stopped and the queue full.
                if nonblock {
                    return Ok(done + taken);
                }
                if let Err(e) = crate::signal::sleep(
                    s,
                    &tty.output_wait,
                    || !tty.stopped.load(Ordering::Acquire),
                    None,
                ) {
                    return if done + taken > 0 {
                        Ok(done + taken)
                    } else {
                        Err(e)
                    };
                }
            }
            done += n;
        }
        Ok(done)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let tty = &self.0;
        match req {
            TIOCGETA => termios_to(tty.disc.lock(s).termios(), arg),
            TIOCSETA | TIOCSETAW | TIOCSETAF => {
                tty.background(s, SIGTTOU)?;
                let t = termios_from(arg);
                {
                    let mut d = tty.disc.lock(s);
                    // Output already went out (writes are synchronous).
                    if req == TIOCSETAF {
                        d.flush(true, false);
                    }
                    d.set_termios(t);
                    tty.mirror(&d);
                }
                tty.input_wait.wake_all();
            }
            TIOCGWINSZ => winsize_to(&tty.winsize.lock(), arg),
            TIOCSWINSZ => {
                tty.background(s, SIGTTOU)?;
                let w = winsize_from(arg);
                let changed = core::mem::replace(&mut *tty.winsize.lock(), w) != w;
                if changed {
                    tty.signal_foreground(SIGWINCH);
                }
            }
            TIOCGPGRP => {
                let c = tty.caller_ctty()?;
                put_int(arg, if c.pgrp == 0 { NO_PID } else { c.pgrp });
            }
            TIOCSPGRP => tty.set_pgrp(s, get_int(arg))?,
            TIOCGSID => put_int(arg, tty.caller_ctty()?.sid),
            TIOCSCTTY => tty.set_ctty(s)?,
            TIOCNOTTY => {
                let c = tty.caller_ctty()?;
                let p = Process::current().ok_or(Errno::ENOTTY)?;
                if p.pid() == c.sid {
                    session_ended(c.sid);
                }
            }
            FIONREAD => put_int(
                arg,
                i32::try_from(tty.disc.lock(s).available()).unwrap_or(i32::MAX),
            ),
            TIOCOUTQ => put_int(
                arg,
                i32::try_from(tty.disc.lock(s).output_len()).unwrap_or(i32::MAX),
            ),
            TIOCFLUSH => {
                tty.background(s, SIGTTOU)?;
                let what = get_int(arg).cast_unsigned();
                let what = if what == 0 {
                    abi::fcntl::FREAD | abi::fcntl::FWRITE
                } else {
                    what
                };
                let mut d = tty.disc.lock(s);
                d.flush(
                    what & abi::fcntl::FREAD != 0,
                    what & abi::fcntl::FWRITE != 0,
                );
                tty.mirror(&d);
            }
            TIOCSTART | TIOCSTOP => {
                tty.background(s, SIGTTOU)?;
                {
                    let mut d = tty.disc.lock(s);
                    d.set_stopped(req == TIOCSTOP);
                    tty.drain(&mut d);
                    tty.mirror(&d);
                }
                tty.output_wait.wake_all();
            }
            TIOCSTI => {
                tty.background(s, SIGTTOU)?;
                let c = arg.first().copied().unwrap_or(0);
                tty.input(s, &[c]);
            }
            TIOCSTAT => {
                tty.background(s, SIGTTOU)?;
                tty.signal_foreground(SIGINFO);
            }
            TIOCGETD => put_int(arg, TTYDISC.cast_signed()),
            TIOCSETD if get_int(arg) == TTYDISC.cast_signed() => {}
            TIOCSETD => return Err(Errno::ENOTTY),
            // The console UART has its modem lines tied up.
            TIOCMGET => put_int(
                arg,
                (TIOCM_LE | TIOCM_DTR | TIOCM_RTS | TIOCM_CTS | TIOCM_DCD | TIOCM_DSR)
                    .cast_signed(),
            ),
            // Changes, from the background, wait like the rest.
            TIOCSBRK | TIOCCBRK | TIOCEXCL | TIOCNXCL | TIOCDRAIN => tty.background(s, SIGTTOU)?,
            TIOCMSET | TIOCMBIS | TIOCMBIC | TIOCSDTR | TIOCCDTR => {}
            _ => return Err(Errno::ENOTTY),
        }
        Ok(())
    }
}

/// `/dev/tty`: opens the caller's controlling terminal.
pub(crate) struct ControllingTty;

impl CharDevice for ControllingTty {
    fn open(&self) -> Result<Option<Arc<Vnode>>, Errno> {
        let p = Process::current().ok_or(Errno::ENXIO)?;
        let t = controlling(p.sid()).ok_or(Errno::ENXIO)?;
        t.vnode
            .get()
            .and_then(Weak::upgrade)
            .map(Some)
            .ok_or(Errno::ENXIO)
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }
}
