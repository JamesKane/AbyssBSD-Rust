// SPDX-License-Identifier: BSD-2-Clause
//! Terminals (docs/MULTIUSER.md §9): the line discipline
//! ([`tty::Discipline`]) on a device below it ([`Driver`], FreeBSD's
//! `ttydevsw`), as character devices, with job control.
//!
//! A terminal's devices are its call-in node (`ttyu0`, `ttyv0`, `pts/0`),
//! with a serial port's call-out node (`cuau0`) and the init and lock
//! state nodes (`ttyu0.init`, `ttyu0.lock`, `cuau0.init`, `cuau0.lock`)
//! beside it; `/dev/console` is the console's terminal as a device of its
//! own. The first open of any of them gives the terminal its init state
//! (with `CLOCAL` through `cuau0` and the console) and sizes its queues;
//! the last close drains output for up to a second without progress,
//! empties the queues, and tells the device. Call-in and call-out exclude
//! each other (`EBUSY`), as does `TIOCEXCL` for all but the superuser.
//!
//! Output is queued and the device drains it ([`Driver::outwakeup`]): the
//! console's UART and `vt` at once, a pseudo-terminal's master as it
//! reads ([`pts`]). Writers wait for room when the queue fills and are
//! woken at its low watermark. Typed bytes come from the device
//! ([`Tty::input`]), which signals the foreground process group as the
//! discipline asks.
//!
//! A session leader makes a terminal its session's controlling terminal
//! with `TIOCSCTTY`; it then has a foreground process group. A process of
//! that session outside the foreground that reads is sent `SIGTTIN` (or,
//! with `TOSTOP`, writes and is sent `SIGTTOU`), as is one that changes
//! the terminal: unless it ignores or blocks the signal, or its group is
//! orphaned, which gets `EIO` for a read. When the session leader exits,
//! the foreground group gets `SIGHUP`, and the node it took the terminal
//! through is revoked: every file open on it is dead, and the terminal is
//! free for another session. `/dev/tty` opens the caller's controlling
//! terminal.
//!
//! A terminal whose device went (a pseudo-terminal whose master closed) is
//! gone: reads of it return end of file, everything else `ENXIO`, and it
//! is freed, with its nodes, once nothing holds it.
//!
//! Behavior reference: sys/kern/tty.c, sys/kern/tty_tty.c,
//! sys/kern/kern_proc.c (`killjobc`).

mod info;
pub(crate) mod pts;
pub(crate) mod serial;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::fcntl::{FREAD, FWRITE};
use abi::ioctl::*;
use abi::signal::{SIGHUP, SIGTTIN, SIGTTOU, SIGWINCH};
use abi::termios::{
    CLOCAL, CNO_RTSDTR, ICANON, IXANY, IXOFF, IXON, NCCS, TOSTOP, Termios, VMIN, VSTART, VSTOP,
    VTIME, Winsize,
};
use sched::lock::{LockClass, Mutex, MutexGuard};
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};
use tty::{Discipline, Input, Line};
use vfs::{CharDevice, Uio, Vnode};

use crate::cred::Priv;
use crate::proc::Process;
use crate::signal::Woke;

/// Above the driver serializer the interrupt thread holds when it types.
static TTY_LOCK: LockClass = LockClass::new("tty", 450);

/// Keyboards' bytes, into the console terminal.
pub(crate) struct ConsoleInput;

impl input::Consumer for ConsoleInput {
    /// Without `vt`, into the console's terminal.
    fn input(&self, s: &Sleepable, bytes: &[u8]) {
        if let Some(t) = console() {
            t.input(s, bytes);
        }
    }

    /// With `vt`, it acts on the keys itself.
    fn keys(&self, s: &Sleepable, _mode: i32, codes: &[u32]) -> bool {
        crate::vtcons::keys(s, codes, &mut |e| self.event(s, e))
    }

    /// The reboot, halt, and power-off keys, as FreeBSD's `shutdown_nice`
    /// (which signals init).
    fn event(&self, s: &Sleepable, e: input::term::Event) {
        use crate::shutdown::{RB_HALT, RB_POWEROFF, shutdown_nice};
        use input::term::Event;
        match e {
            Event::Reboot => shutdown_nice(s, 0),
            Event::Halt => shutdown_nice(s, RB_HALT),
            Event::PowerOff => shutdown_nice(s, RB_HALT | RB_POWEROFF),
            _ => {}
        }
    }
}

/// Bytes moved at a time: from a writer into the discipline (FreeBSD's
/// `TTY_STACKBUF`), and from the discipline to a device.
const CHUNK: usize = tty::STACKBUF;

/// `NO_PID`: what `TIOCGPGRP` reports with no foreground group.
const NO_PID: i32 = 100_000;

/// `tty_drainwait`: `tcdrain`'s default timeout, in seconds.
const DRAINWAIT: u32 = 5 * 60;

/// The modem lines (`SER_*`), as [`Driver::modem`] sets and reports them;
/// `TIOCM_*` are these shifted left by one.
pub(crate) const SER_DTR: i32 = 0x01;
pub(crate) const SER_RTS: i32 = 0x02;
pub(crate) const SER_CTS: i32 = 0x10;
pub(crate) const SER_DCD: i32 = 0x20;
pub(crate) const SER_DSR: i32 = 0x80;

/// FreeBSD's `GID_TTY`, which owns users' terminals.
const GID_TTY: u32 = 4;

/// Which of a terminal's devices a file opened (FreeBSD's `TTY_CALLOUT`
/// and `dev_console`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unit {
    /// The call-in device (`ttyu0`, `ttyv0`, `pts/0`).
    In,
    /// The call-out device (`cuau0`).
    Out,
    /// `/dev/console`.
    Console,
}

impl Unit {
    const fn index(self) -> usize {
        match self {
            Self::In => 0,
            Self::Out => 1,
            Self::Console => 2,
        }
    }

    /// Its init and lock states: the call-out ones for `cuau0`, the
    /// call-in ones otherwise.
    const fn side(self) -> usize {
        match self {
            Self::Out => 1,
            _ => 0,
        }
    }
}

/// A terminal's device (FreeBSD's `ttydevsw`). Every hook but
/// [`Driver::outwakeup`] has the default FreeBSD patches in.
pub(crate) trait Driver: Send + Sync {
    /// Output was queued (`tsw_outwakeup`): the device takes what it can
    /// with [`Output::getc`], now or as it drains. Called with the
    /// terminal locked.
    fn outwakeup(&self, out: &mut Output<'_>);
    /// The input queue has room again (`tsw_inwakeup`).
    fn inwakeup(&self) {}
    /// The terminal's first open (`tsw_open`).
    fn open(&self) -> Result<(), Errno> {
        Ok(())
    }
    /// Its last close (`tsw_close`).
    fn close(&self) {}
    /// Its line settings changed, or it is opening (`tsw_param`): the
    /// device may adjust them, or refuse. The default is
    /// `ttydevsw_defparam`'s, for devices without a line: speeds held to
    /// 50 to 115200, and `CREAD`.
    fn param(&self, t: &mut Termios) -> Result<(), Errno> {
        t.c_ispeed = t.c_ispeed.clamp(50, 115_200);
        t.c_ospeed = t.c_ospeed.clamp(50, 115_200);
        t.c_cflag |= abi::termios::CREAD;
        Ok(())
    }
    /// Raises the lines in `on` and drops those in `off`; returns their
    /// state (`tsw_modem`). The default simulates a carrier.
    fn modem(&self, _on: i32, _off: i32) -> i32 {
        SER_DCD
    }
    /// A packet-mode event (`tsw_pktnotify`): only a pseudo-terminal's
    /// master hears of them.
    fn pktnotify(&self, _event: u8) {}
    /// A request the terminal layer doesn't know (`tsw_ioctl`).
    fn ioctl(&self, _s: &Sleepable, _req: u32, _arg: &mut [u8]) -> Result<(), Errno> {
        Err(Errno::ENOTTY)
    }
    /// Its memory, for `mmap` (`tsw_mmap`); see [`CharDevice::mmap`].
    fn mmap(&self, _page: u64, _pages: u64) -> Result<Arc<vm::object::Object>, Errno> {
        Err(Errno::ENODEV)
    }
    /// The terminal is freed (`tsw_free`).
    fn free(&self) {}
}

/// A device's view of the output queue, in [`Driver::outwakeup`].
pub(crate) struct Output<'a> {
    tty: &'a Tty,
    d: &'a mut Discipline,
}

impl Output<'_> {
    /// Takes queued output (`ttydisc_getc`), waking writers once there is
    /// room enough.
    pub(crate) fn getc(&mut self, buf: &mut [u8]) -> usize {
        let (n, wake) = self.d.getc(buf);
        if wake {
            // The writers' condition first, then the writers.
            self.tty.mirror_disc(self.d);
            self.tty.wakeup(false, true);
        }
        n
    }
}

/// Writes all queued output with `f`, for devices that write at once.
fn drain_with(out: &mut Output<'_>, f: &mut dyn FnMut(&[u8])) {
    let mut buf = [0u8; 1024];
    loop {
        let n = out.getc(&mut buf);
        if n == 0 {
            break;
        }
        f(&buf[..n]);
    }
}

/// The session a terminal controls, and its foreground process group.
#[derive(Clone, Copy, Debug, Default)]
struct Ctty {
    /// 0: none.
    sid: i32,
    pgrp: i32,
    /// The node the leader took it through (`s_ttyvp`), revoked when the
    /// leader exits; `None` when the kernel gave it.
    unit: Option<Unit>,
    /// That node's revocation count then: once it is revoked, another
    /// session may take the terminal (`s_ttyvp` is `VBAD`).
    epoch: u64,
}

/// What the terminal's lock guards: the discipline and FreeBSD's `t_flags`.
struct State {
    d: Discipline,
    /// Files open on each unit, and how often each was revoked: a file
    /// opened before the last revocation no longer counts.
    opens: [u64; 3],
    epoch: [u64; 3],
    /// The device went (`TF_GONE`).
    gone: bool,
    /// The carrier went (`TF_ZOMBIE`): reads return end of file, writes
    /// fail.
    zombie: bool,
    /// Only the superuser may open it (`TIOCEXCL`, `TF_EXCLUDE`).
    exclude: bool,
    /// The init and lock states, call-in and call-out
    /// (`t_termios_init_in`, ...).
    init: [Termios; 2],
    lock: [Termios; 2],
    /// `tcdrain`'s timeout, in seconds (`t_drainwait`).
    drainwait: u32,
}

impl State {
    /// Open through any unit (`tty_opened`).
    fn opened(&self) -> bool {
        self.opens.iter().any(|&n| n > 0)
    }
}

/// How a terminal is made.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Make {
    /// It has init and lock state nodes (`TF_INITLOCK`).
    pub(crate) initlock: bool,
    /// It has a call-out node (`TF_CALLOUT`).
    pub(crate) callout: bool,
    /// A user's terminal (`tty_makedev` with a credential): this user owns
    /// it, group `tty`, mode 0620; otherwise `root:wheel`, 0600.
    pub(crate) user: Option<u32>,
}

/// A terminal.
pub(crate) struct Tty {
    /// Its call-in node's name under `/dev`.
    name: String,
    /// Its other nodes' names (`cuau0`, `ttyu0.init`, ...).
    others: Vec<String>,
    st: Mutex<State>,
    /// Mirrors for wait conditions and polls, set under the lock: a read
    /// would return; how much it could take; room for output, output
    /// waiting, and room for input; a writer waits for room; output is
    /// stopped; the carrier or the device went.
    readable: AtomicBool,
    ready: AtomicUsize,
    out_room: AtomicUsize,
    out_used: AtomicUsize,
    in_room: AtomicUsize,
    hiwat_out: AtomicBool,
    stopped: AtomicBool,
    zombie: AtomicBool,
    gone: AtomicBool,
    /// Bumped by every last close (`t_revokecnt`): sleepers give up.
    revokes: AtomicU64,
    /// Pollers and knotes (`t_inpoll`, `t_outpoll`), told of every change.
    notify: event::Notify,
    ctty: SpinLock<Ctty>,
    /// Bumped when the foreground group changes (background sleepers
    /// look again).
    fg_gen: AtomicU64,
    /// Bytes typed so far.
    received: AtomicU64,
    winsize: SpinLock<Winsize>,
    input_wait: WaitQueue,
    output_wait: WaitQueue,
    /// A writer is in (`TF_BUSY_OUT`): writes take turns, waiting here
    /// (`t_outserwait`).
    busy_out: AtomicBool,
    serial_wait: WaitQueue,
    bg_wait: WaitQueue,
    /// Opens waiting for the carrier (`t_dcdwait`).
    dcd_wait: WaitQueue,
    /// The device under the discipline.
    driver: Arc<dyn Driver>,
}

/// Every terminal (`tty_list`).
static TTYS: SpinLock<Vec<Arc<Tty>>> = SpinLock::new(Vec::new());
/// The console's terminal.
static CONSOLE: Once<Arc<Tty>> = Once::new();

/// A device that writes with a function, at once: the console's UART.
struct Sink(fn(&[u8]));

impl Driver for Sink {
    fn outwakeup(&self, out: &mut Output<'_>) {
        drain_with(out, &mut |b| (self.0)(b));
    }

    /// The firmware's line stays (the "Console line" deviation).
    fn param(&self, t: &mut Termios) -> Result<(), Errno> {
        if t.c_ispeed != t.c_ospeed && t.c_ospeed != 0 {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    /// The console UART has its modem lines tied up.
    fn modem(&self, _on: i32, _off: i32) -> i32 {
        SER_DTR | SER_RTS | SER_CTS | SER_DCD | SER_DSR
    }

    /// Breaks are the port's (`uart_tty_ioctl`); this one sends none.
    fn ioctl(&self, _s: &Sleepable, req: u32, _arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            TIOCSBRK | TIOCCBRK => Ok(()),
            _ => Err(Errno::ENOTTY),
        }
    }
}

/// The console port's speed: the loader's (`comconsole_speed`).
const CONSOLE_SPEED: u32 = 115_200;

/// Makes the serial console's terminal `ttyu<unit>` (with `cuau<unit>`
/// and the init and lock nodes), which writes with `sink`: the console's
/// terminal too, if the serial port is primary.
pub(crate) fn create_console(unit: u32, sink: fn(&[u8])) -> Arc<Tty> {
    let tty = create(
        &alloc::format!("ttyu{unit}"),
        Arc::new(Sink(sink)),
        Make {
            initlock: true,
            callout: true,
            user: None,
        },
    );
    tty.init_console(CONSOLE_SPEED);
    if crate::console::serial_primary() {
        set_console(&tty);
    }
    tty
}

/// Makes `tty` the console's terminal (`/dev/console`), unless one is.
pub(crate) fn set_console(tty: &Arc<Tty>) {
    let _ = CONSOLE.set(Arc::clone(tty));
}

/// The names of a terminal's other nodes, for its call-in node `name`.
fn other_names(name: &str, make: Make) -> Vec<String> {
    let mut out = Vec::new();
    let cua = name
        .strip_prefix("tty")
        .map(|rest| alloc::format!("cua{rest}"));
    if make.initlock {
        out.push(alloc::format!("{name}.init"));
        out.push(alloc::format!("{name}.lock"));
    }
    if make.callout
        && let Some(cua) = cua
    {
        if make.initlock {
            out.push(alloc::format!("{cua}.init"));
            out.push(alloc::format!("{cua}.lock"));
        }
        out.push(cua);
    }
    out
}

/// Makes a terminal named `name` (its call-in node under `/dev`) on
/// `driver`, with its nodes (`tty_alloc`, `tty_makedev`).
pub(crate) fn create(name: &str, driver: Arc<dyn Driver>, make: Make) -> Arc<Tty> {
    let mut d = Discipline::new();
    // Its queues are sized on opening.
    d.close();
    let init = Termios::defaults();
    let tty = Arc::new(Tty {
        name: String::from(name),
        others: other_names(name, make),
        st: Mutex::new(
            &TTY_LOCK,
            State {
                d,
                opens: [0; 3],
                epoch: [0; 3],
                gone: false,
                zombie: false,
                exclude: false,
                init: [init; 2],
                lock: [Termios::default(); 2],
                drainwait: DRAINWAIT,
            },
        ),
        readable: AtomicBool::new(false),
        ready: AtomicUsize::new(0),
        out_room: AtomicUsize::new(0),
        out_used: AtomicUsize::new(0),
        in_room: AtomicUsize::new(0),
        hiwat_out: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        zombie: AtomicBool::new(false),
        gone: AtomicBool::new(false),
        revokes: AtomicU64::new(0),
        notify: event::Notify::new(),
        ctty: SpinLock::new(Ctty::default()),
        fg_gen: AtomicU64::new(0),
        received: AtomicU64::new(0),
        winsize: SpinLock::new(Winsize::default()),
        input_wait: WaitQueue::new(),
        output_wait: WaitQueue::new(),
        busy_out: AtomicBool::new(false),
        serial_wait: WaitQueue::new(),
        bg_wait: WaitQueue::new(),
        dcd_wait: WaitQueue::new(),
        driver,
    });
    TTYS.lock().push(Arc::clone(&tty));
    let (uid, gid, mode) = match make.user {
        Some(uid) => (uid, GID_TTY, 0o620),
        None => (0, 0, 0o600),
    };
    let _ = vfs::devices::make_dev(
        name.as_bytes(),
        mode,
        uid,
        gid,
        Arc::new(TtyDevice::new(&tty, Unit::In)),
    );
    for other in &tty.others {
        let ops: Arc<dyn CharDevice> = if let Some(base) = other.strip_suffix(".init") {
            Arc::new(InitLock::new(&tty, base != tty.name, false))
        } else if let Some(base) = other.strip_suffix(".lock") {
            Arc::new(InitLock::new(&tty, base != tty.name, true))
        } else {
            Arc::new(TtyDevice::new(&tty, Unit::Out))
        };
        let _ = vfs::devices::make_dev(other.as_bytes(), mode, uid, gid, ops);
    }
    tty
}

/// The console's terminal, once its port can be typed at.
pub(crate) fn console() -> Option<Arc<Tty>> {
    CONSOLE.get().cloned()
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
    let rdev = crate::fs::devfs::rdev_of(t.name.as_bytes()).unwrap_or(abi::kinfo::NODEV);
    Some((rdev, pgrp))
}

/// The node session `sid` took its controlling terminal through, under
/// `/dev` (`s_ttyvp`'s name: `pts/0`, `ttyu0`, `console`).
pub(crate) fn controlling_node(sid: i32) -> Option<String> {
    let t = controlling(sid)?;
    let unit = t.ctty.lock().unit;
    Some(unit.map_or_else(|| t.name.clone(), |u| t.node_name(u)))
}

/// A session's leader exited (`killjobc`): its terminal's foreground group
/// gets `SIGHUP`, the node the leader took it through is revoked, and the
/// terminal is free for another session.
pub(crate) fn session_ended(s: &Sleepable, sid: i32) {
    let Some(t) = controlling(sid) else {
        return;
    };
    let c = core::mem::take(&mut *t.ctty.lock());
    t.fg_gen.fetch_add(1, Ordering::Release);
    t.bg_wait.wake_all();
    if c.pgrp != 0 {
        let mut st = t.st.lock(s);
        st.d.signaled();
        t.mirror(&st);
        drop(st);
        crate::signal::post_group(c.pgrp, SIGHUP);
    }
    if let Some(unit) = c.unit {
        crate::fs::devfs::revoke_path(s, t.node_name(unit).as_bytes());
    }
    t.rel_free(s);
}

/// Makes `t` the controlling terminal of `p`'s session, with `p`'s group
/// in the foreground, as `TIOCSCTTY` by a session leader does (for the
/// kernel's own processes, which start as session leaders).
pub(crate) fn make_controlling(t: &Tty, p: &Process) {
    *t.ctty.lock() = Ctty {
        sid: p.sid(),
        pgrp: p.pgid(),
        unit: None,
        epoch: 0,
    };
    t.fg_gen.fetch_add(1, Ordering::Release);
}

impl Tty {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The node `unit` opens.
    fn node_name(&self, unit: Unit) -> String {
        match unit {
            Unit::In => self.name.clone(),
            Unit::Console => String::from("console"),
            Unit::Out => self
                .others
                .iter()
                .find(|n| n.starts_with("cua") && !n.contains('.'))
                .cloned()
                .unwrap_or_else(|| self.name.clone()),
        }
    }

    /// The console's line (`tty_init_console`): its speed, and `CLOCAL`.
    fn init_console(&self, speed: u32) {
        // SAFETY: drivers attach in thread context.
        let s = unsafe { Sleepable::assume() };
        let mut st = self.st.lock(&s);
        for t in &mut st.init {
            if speed != 0 {
                t.c_ispeed = speed;
                t.c_ospeed = speed;
            }
            t.c_cflag |= CLOCAL;
        }
    }

    /// Sets the window size, as the driver knows it.
    pub(crate) fn set_winsize(&self, w: Winsize) {
        *self.winsize.lock() = w;
    }

    /// The device changed size (`tty_set_winsize`): the foreground
    /// process group gets `SIGWINCH` if the size differs.
    pub(crate) fn resize(&self, w: Winsize) {
        if core::mem::replace(&mut *self.winsize.lock(), w) != w {
            self.signal_foreground(SIGWINCH);
        }
    }

    /// Updates the wait-condition mirrors from the state, and tells
    /// pollers.
    fn mirror(&self, st: &State) {
        self.zombie.store(st.zombie, Ordering::Release);
        self.gone.store(st.gone, Ordering::Release);
        self.mirror_disc(&st.d);
    }

    /// The mirrors of the discipline's state. Sleepers' conditions read
    /// them, so they are set before any wakeup.
    fn mirror_disc(&self, d: &Discipline) {
        // Release: a waiter that sees a mirror sees the state it mirrors.
        self.readable.store(d.readable(), Ordering::Release);
        self.ready.store(d.read_poll(), Ordering::Release);
        self.out_room.store(d.output_room(), Ordering::Release);
        let used = if d.stopped() { 0 } else { d.output_len() };
        self.out_used.store(used, Ordering::Release);
        self.in_room.store(d.input_room(), Ordering::Release);
        self.hiwat_out.store(d.hiwat_out(), Ordering::Release);
        self.stopped.store(d.stopped(), Ordering::Release);
        self.notify.notify(0);
    }

    /// Wakes readers, writers, or both (`tty_wakeup`).
    fn wakeup(&self, read: bool, write: bool) {
        if read {
            self.input_wait.wake_all();
        }
        if write {
            self.output_wait.wake_all();
        }
        self.notify.notify(0);
    }

    /// Wakes every sleeper: the terminal went or was closed.
    fn wake_everyone(&self) {
        self.wakeup(true, true);
        self.serial_wait.wake_all();
        self.bg_wait.wake_all();
    }

    /// Tells the device output is queued (`ttydevsw_outwakeup`), once the
    /// mirrors show it: the device's sleepers and pollers look at them.
    fn outwakeup(&self, st: &mut State) {
        self.mirror(st);
        if !st.gone {
            self.driver.outwakeup(&mut Output {
                tty: self,
                d: &mut st.d,
            });
        }
    }

    /// Takes queued output for a device outside [`Driver::outwakeup`] (a
    /// pseudo-terminal's master read), waking writers at the watermark.
    fn getc(&self, st: &mut State, buf: &mut [u8]) -> usize {
        Output {
            tty: self,
            d: &mut st.d,
        }
        .getc(buf)
    }

    /// Typed bytes are in (`ttydisc_rint_done`): readers wake, and the
    /// device takes the echo.
    fn rint_done(&self, st: &mut State) {
        self.mirror(st);
        self.wakeup(true, false);
        self.outwakeup(st);
    }

    /// Discards queued input, output, or both (`tty_flush`).
    fn flush(&self, st: &mut State, read: bool, write: bool) {
        st.d.flush(read, write);
        self.mirror(st);
        if write {
            self.wakeup(false, true);
            if !st.gone {
                self.outwakeup(st);
                self.driver.pktnotify(TIOCPKT_FLUSHWRITE);
            }
        }
        if read {
            self.wakeup(true, false);
            if !st.gone {
                self.driver.inwakeup();
                self.driver.pktnotify(TIOCPKT_FLUSHREAD);
            }
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
        let mut st = self.st.lock(s);
        st.d.flush(true, false);
        self.mirror(&st);
    }

    /// Bytes from the device (`ttydisc_rint`, then `ttydisc_rint_done`).
    /// Returns how many were taken: fewer when the input queue is full
    /// and lines wait to be read.
    pub(crate) fn input(&self, s: &Sleepable, bytes: &[u8]) -> usize {
        let mut signals: Vec<u32> = Vec::new();
        let taken = {
            let mut st = self.st.lock(s);
            let n = st.d.input_bulk(bytes, &mut |sig| signals.push(sig));
            self.rint_done(&mut st);
            self.mirror(&st);
            n
        };
        self.received.fetch_add(taken as u64, Ordering::Release);
        for sig in signals {
            self.signal_foreground(sig);
        }
        taken
    }

    /// The device has room to send (`uart_tty_intr`'s `SER_INT_TXIDLE`).
    pub(crate) fn transmit(&self, s: &Sleepable) {
        let mut st = self.st.lock(s);
        self.rint_done(&mut st);
        self.mirror(&st);
    }

    /// The carrier came or went (`ttydisc_modem`).
    pub(crate) fn carrier(&self, s: &Sleepable, on: bool) {
        if on {
            self.dcd_wait.wake_all();
            return;
        }
        {
            let mut st = self.st.lock(s);
            self.modem_lost(&mut st);
            self.mirror(&st);
        }
        self.wake_everyone();
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
            let revokes = self.revokes.load(Ordering::Acquire);
            let _w = sched::wait_message("ttybg");
            crate::signal::sleep_after_stop_signal(s, &self.bg_wait, || {
                self.fg_gen.load(Ordering::Acquire) != generation
                    || self.revokes.load(Ordering::Acquire) != revokes
            })?;
            self.after_wait(revokes)?;
        }
    }

    /// After a sleep (`tty_wait`): `ENXIO` if the device went, or if the
    /// terminal was closed meanwhile (FreeBSD restarts the call, which
    /// then finds its file dead: end of file for a read, `ENXIO` for the
    /// rest).
    fn after_wait(&self, revokes: u64) -> Result<(), Errno> {
        if self.gone.load(Ordering::Acquire) || self.revokes.load(Ordering::Acquire) != revokes {
            return Err(Errno::ENXIO);
        }
        Ok(())
    }

    /// Sleeps on `q` until `cond`, the device goes, or the terminal is
    /// closed (`tty_wait`, `tty_timedwait`).
    fn wait(
        &self,
        s: &Sleepable,
        q: &WaitQueue,
        cond: impl Fn() -> bool,
        ns: Option<u64>,
    ) -> Result<Woke, Errno> {
        let revokes = self.revokes.load(Ordering::Acquire);
        // `t_inwait`, `t_outwait`, and the rest are named.
        let name = if core::ptr::eq(q, &self.input_wait) {
            "ttyin"
        } else if core::ptr::eq(q, &self.output_wait) {
            "ttyout"
        } else if core::ptr::eq(q, &self.serial_wait) {
            "ttyosr"
        } else if core::ptr::eq(q, &self.dcd_wait) {
            "ttydcd"
        } else {
            "ttybg"
        };
        let _w = sched::wait_message(name);
        let woke = crate::signal::sleep(
            s,
            q,
            || {
                cond()
                    || self.gone.load(Ordering::Acquire)
                    || self.revokes.load(Ordering::Acquire) != revokes
            },
            ns,
        )?;
        self.after_wait(revokes)?;
        Ok(woke)
    }

    /// `ttydev_enter`: the terminal is locked, unless it is gone or not
    /// open (`ENXIO`).
    fn enter(&self, s: &Sleepable) -> Result<MutexGuard<'_, State>, Errno> {
        let st = self.st.lock(s);
        if st.gone || !st.opened() {
            return Err(Errno::ENXIO);
        }
        Ok(st)
    }

    /// The first open of `unit`, or another (`ttydev_open`): without
    /// `nonblock` or `CLOCAL`, once the carrier is up.
    fn open(self: &Arc<Self>, s: &Sleepable, unit: Unit, nonblock: bool) -> Result<TtyOpen, Errno> {
        let mut st = self.st.lock(s);
        if st.gone {
            return Err(Errno::ENXIO);
        }
        // Call-in and call-out exclude each other; the console is a
        // call-in device.
        let busy = match unit {
            Unit::Out => st.opens[Unit::In.index()] + st.opens[Unit::Console.index()] > 0,
            _ => st.opens[Unit::Out.index()] > 0,
        };
        if busy || (st.exclude && crate::fs::priv_check(Priv::TtyExclusive).is_err()) {
            return Err(Errno::EBUSY);
        }
        if !st.opened() {
            let mut t = st.init[unit.side()];
            // FreeBSD ignores what `param` says here.
            let _ = self.driver.param(&mut t);
            // No modem control on call-out devices and the console.
            if unit != Unit::In {
                t.c_cflag |= CLOCAL;
            }
            if t.c_cflag & CNO_RTSDTR == 0 {
                self.driver.modem(SER_DTR | SER_RTS, 0);
            }
            self.driver.open()?;
            st.d.set_termios(t);
            st.d.set_watermarks();
        }
        // Wait for the carrier (polled: devices report it as it changes,
        // and the wait gives up when a signal comes).
        while !nonblock
            && st.d.termios().c_cflag & CLOCAL == 0
            && self.driver.modem(0, 0) & SER_DCD == 0
        {
            drop(st);
            let waited = self.wait(s, &self.dcd_wait, || false, Some(100_000_000));
            st = self.st.lock(s);
            if waited.is_err() || st.gone {
                // Given up: a first open is undone (`ttydev_leave`).
                drop(st);
                self.leave(s);
                return Err(waited.err().unwrap_or(Errno::ENXIO));
            }
        }
        st.opens[unit.index()] += 1;
        let epoch = st.epoch[unit.index()];
        self.mirror(&st);
        Ok(TtyOpen {
            tty: Arc::clone(self),
            unit,
            epoch,
        })
    }

    /// The last close of `unit` (`ttydev_close`), or its revocation.
    fn close(&self, s: &Sleepable, unit: Unit, revoke: bool) {
        {
            let mut st = self.st.lock(s);
            // A last close was open; a revocation may find nothing open.
            let was_open = !revoke || st.opened();
            st.opens[unit.index()] = 0;
            if revoke {
                st.epoch[unit.index()] += 1;
            }
            // Revoking a node nothing has open closes nothing.
            if st.opened() || !was_open {
                return;
            }
            // Revoking: output goes now rather than being drained.
            if revoke {
                self.flush(&mut st, false, true);
            }
            st.exclude = false;
            self.revokes.fetch_add(1, Ordering::AcqRel);
            self.mirror(&st);
        }
        self.wake_everyone();
        self.leave(s);
    }

    /// Nothing has it open (`ttydev_leave`): output drains, the queues
    /// empty, the device hears, and the terminal may be freed.
    fn leave(&self, s: &Sleepable) {
        if !self.gone.load(Ordering::Acquire) {
            let _ = self.drain(s, true);
        }
        {
            let mut st = self.st.lock(s);
            if st.opened() {
                return;
            }
            st.d.close();
            st.zombie = false;
            if !st.gone {
                self.driver.close();
            }
            self.mirror(&st);
        }
        self.rel_free(s);
    }

    /// Frees the terminal once its device went, nothing has it open, and
    /// no session holds it (`tty_rel_free`): its nodes go.
    fn rel_free(&self, s: &Sleepable) {
        {
            let st = self.st.lock(s);
            if !st.gone || st.opened() || self.ctty.lock().sid != 0 {
                return;
            }
        }
        let me = {
            let mut all = TTYS.lock();
            let Some(i) = all.iter().position(|t| core::ptr::eq(t.as_ref(), self)) else {
                return;
            };
            all.remove(i)
        };
        crate::fs::devfs::remove(s, self.name.as_bytes());
        for other in &self.others {
            crate::fs::devfs::remove(s, other.as_bytes());
        }
        self.driver.free();
        drop(me);
    }

    /// The device went (`tty_rel_gone`): the carrier drops, sleepers
    /// wake, and the terminal goes once nothing holds it.
    fn rel_gone(&self, s: &Sleepable) {
        {
            let mut st = self.st.lock(s);
            self.modem_lost(&mut st);
            st.gone = true;
            self.mirror(&st);
        }
        self.wake_everyone();
        self.rel_free(s);
    }

    /// The carrier dropped (`ttydisc_modem` with 0): unless `CLOCAL` (or
    /// closed), the terminal is a zombie, its session leader gets
    /// `SIGHUP`, and its queues empty.
    fn modem_lost(&self, st: &mut State) {
        if !st.opened() || st.d.termios().c_cflag & CLOCAL != 0 {
            return;
        }
        st.zombie = true;
        st.d.signaled();
        let leader = self.ctty.lock().sid;
        if leader != 0
            && let Some(p) = crate::proc::all().into_iter().find(|p| p.pid() == leader)
        {
            let info = crate::signal::user_info(SIGHUP, 0, abi::signal::code::SI_KERNEL);
            let _ = crate::signal::post(&p, info);
        }
        self.flush(st, true, true);
    }

    /// Waits for queued output to go (`tty_drain`): on a last close, up to
    /// a second without progress; for `tcdrain`, up to the drain wait.
    fn drain(&self, s: &Sleepable, leaving: bool) -> Result<(), Errno> {
        const SECOND: u64 = 1_000_000_000;
        let wait = u64::from(self.st.lock(s).drainwait);
        let mut deadline = if leaving {
            Some(crate::time::now_ns() + SECOND)
        } else {
            (wait != 0).then(|| crate::time::now_ns() + wait * SECOND)
        };
        let mut bytes = self.st.lock(s).d.output_len();
        loop {
            {
                let mut st = self.st.lock(s);
                if st.d.output_len() == 0 {
                    return Ok(());
                }
                self.outwakeup(&mut st);
                self.mirror(&st);
                if st.d.output_len() == 0 {
                    return Ok(());
                }
            }
            // Polled at 10 Hz, as FreeBSD's.
            match self.wait(s, &self.output_wait, || false, Some(SECOND / 10)) {
                Ok(_) => {}
                Err(e) if !leaving => return Err(e),
                Err(_) => return Ok(()),
            }
            let now = crate::time::now_ns();
            let used = self.st.lock(s).d.output_len();
            match deadline {
                Some(at) if now >= at => {
                    if leaving && used < bytes {
                        // Making progress: another second.
                        deadline = Some(at + SECOND);
                        bytes = used;
                    } else {
                        return if leaving { Ok(()) } else { Err(Errno::EAGAIN) };
                    }
                }
                _ => {}
            }
        }
    }

    /// `ttydisc_read`, past `ttydev_enter`.
    fn read(&self, s: &Sleepable, uio: &mut dyn Uio, nonblock: bool) -> Result<usize, Errno> {
        if uio.resid() == 0 {
            return Ok(0);
        }
        let t = *self.enter(s)?.d.termios();
        let r = if t.c_lflag & ICANON != 0 {
            self.read_canonical(s, uio, nonblock)
        } else {
            self.read_raw(s, uio, nonblock, &t)
        };
        let mut st = self.st.lock(s);
        let room = st.d.read_done();
        self.mirror(&st);
        if room && !st.gone {
            self.driver.inwakeup();
        }
        r
    }

    /// The input condition: a read would return, or the carrier went.
    fn input_ready(&self) -> bool {
        self.readable.load(Ordering::Acquire) || self.zombie.load(Ordering::Acquire)
    }

    fn read_canonical(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let mut buf = [0u8; CHUNK];
        let mut total = 0;
        loop {
            self.background(s, SIGTTIN)?;
            let got = {
                let mut st = self.st.lock(s);
                let r = st.d.read_line(&mut buf[..uio.resid().min(CHUNK)]);
                if matches!(r, Line::NotYet) && st.zombie {
                    return Ok(total);
                }
                self.mirror(&st);
                r
            };
            match got {
                Line::Read(n) => return Ok(total + uio.copy_out(&buf[..n])?),
                Line::Part(n) => {
                    total += uio.copy_out(&buf[..n])?;
                    if uio.resid() == 0 {
                        return Ok(total);
                    }
                }
                Line::NotYet if total > 0 => return Ok(total),
                Line::NotYet if nonblock => return Err(Errno::EAGAIN),
                Line::NotYet => {
                    self.wait(s, &self.input_wait, || self.input_ready(), None)?;
                }
            }
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
        let mut buf = [0u8; CHUNK];
        let mut total = 0;
        // With a read timer (`VMIN` 0), it runs from the call.
        let start = crate::time::now_ns();
        loop {
            self.background(s, SIGTTIN)?;
            loop {
                let n = {
                    let mut st = self.st.lock(s);
                    let n = st.d.read_raw(&mut buf[..uio.resid().min(CHUNK)]);
                    self.mirror(&st);
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
            if self.zombie.load(Ordering::Acquire) {
                return Ok(total);
            }
            if nonblock {
                return if total > 0 {
                    Ok(total)
                } else {
                    Err(Errno::EAGAIN)
                };
            }
            // The interbyte timer runs once a byte came.
            let timeout = if vtime == 0 || (vmin > 0 && total == 0) {
                None
            } else if vmin == 0 {
                let left = (vtime * 100_000_000).saturating_sub(crate::time::now_ns() - start);
                if left == 0 {
                    return Ok(total);
                }
                Some(left)
            } else {
                Some(vtime * 100_000_000)
            };
            match self.wait(s, &self.input_wait, || self.input_ready(), timeout) {
                Ok(Woke::Done) => {}
                Ok(Woke::TimedOut) => return Ok(total),
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            }
        }
    }

    /// `ttydev_write`: writes take turns, and wait for room.
    fn write(&self, s: &Sleepable, uio: &mut dyn Uio, nonblock: bool) -> Result<usize, Errno> {
        let tostop = self.enter(s)?.d.termios().c_lflag & TOSTOP != 0;
        if tostop {
            self.background(s, SIGTTOU)?;
        }
        let take = || {
            self.busy_out
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        };
        // Non-blocking writes need not wait their turn.
        let turn = if nonblock && self.busy_out.load(Ordering::Acquire) {
            false
        } else {
            while !take() {
                self.wait(
                    s,
                    &self.serial_wait,
                    || !self.busy_out.load(Ordering::Acquire),
                    None,
                )?;
                drop(self.enter(s)?);
            }
            true
        };
        let r = self.write_disc(s, uio, nonblock);
        if turn {
            self.busy_out.store(false, Ordering::Release);
            self.serial_wait.wake_all();
        }
        r
    }

    /// `ttydisc_write`: output processing into the queue, waiting while it
    /// is full. Returns what was queued; an error only if nothing was.
    fn write_disc(&self, s: &Sleepable, uio: &mut dyn Uio, nonblock: bool) -> Result<usize, Errno> {
        if self.zombie.load(Ordering::Acquire) {
            return Err(Errno::EIO);
        }
        let mut buf = [0u8; CHUNK];
        let mut done = 0;
        let mut result = Ok(());
        'chunks: while uio.resid() > 0 {
            let n = match uio.copy_in(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            if crate::prof::on() {
                crate::prof::console_output(&buf[..n]);
            }
            let mut taken = 0;
            loop {
                {
                    let mut st = self.st.lock(s);
                    if st.gone {
                        result = Err(Errno::ENXIO);
                        break 'chunks;
                    }
                    taken += st.d.write(&buf[taken..n]);
                    if taken < n {
                        // The high watermark: the device may drain at once.
                        st.d.set_hiwat_out();
                    }
                    self.outwakeup(&mut st);
                    self.mirror(&st);
                    if taken == n {
                        break;
                    }
                    if nonblock {
                        done += taken;
                        result = Err(Errno::EAGAIN);
                        break 'chunks;
                    }
                    if !st.d.hiwat_out() {
                        continue;
                    }
                }
                if let Err(e) = self.wait(
                    s,
                    &self.output_wait,
                    || !self.hiwat_out.load(Ordering::Acquire),
                    None,
                ) {
                    done += taken;
                    result = Err(e);
                    break 'chunks;
                }
                if self.zombie.load(Ordering::Acquire) {
                    done += taken;
                    result = Err(Errno::EIO);
                    break 'chunks;
                }
            }
            done += n;
        }
        match result {
            Err(e) if done == 0 => Err(e),
            _ => Ok(done),
        }
    }

    /// `ttydev_poll`.
    fn poll(&self, events: i16, w: Option<&Arc<dyn event::Wake>>) -> i16 {
        use abi::event::{POLLHUP, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM};
        if self.gone.load(Ordering::Acquire) {
            return (events & (POLLIN | POLLRDNORM)) | POLLHUP;
        }
        if let Some(w) = w {
            self.notify.record(w);
        }
        let mut revents = 0;
        if events & (POLLIN | POLLRDNORM) != 0 && self.ready.load(Ordering::Acquire) > 0 {
            revents |= events & (POLLIN | POLLRDNORM);
        }
        if self.zombie.load(Ordering::Acquire) {
            revents |= POLLHUP;
        } else if events & (POLLOUT | POLLWRNORM) != 0 && self.out_room.load(Ordering::Acquire) > 0
        {
            revents |= events & (POLLOUT | POLLWRNORM);
        }
        revents
    }

    /// `ttydev_kqfilter`.
    fn kqfilter(
        self: &Arc<Self>,
        kev: &abi::event::Kevent,
    ) -> Result<Box<dyn event::Filter>, Errno> {
        if self.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        match kev.filter {
            abi::event::EVFILT_READ => Ok(Box::new(TtyFilter(Arc::clone(self), false))),
            abi::event::EVFILT_WRITE => Ok(Box::new(TtyFilter(Arc::clone(self), true))),
            _ => Err(Errno::EINVAL),
        }
    }

    /// Changes the foreground process group (`TIOCSPGRP`).
    fn set_pgrp(&self, s: &Sleepable, pgrp: i32) -> Result<(), Errno> {
        drop(self.enter(s)?);
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

    /// `TIOCSCTTY` through `unit` (`None`: the master): only a session
    /// leader, for a session without one, on a terminal no other session
    /// holds, or whose holder's node was revoked since it took it (init's
    /// `revoke` before it gives the console to `rc.shutdown`).
    fn set_ctty(&self, s: &Sleepable, unit: Option<Unit>) -> Result<(), Errno> {
        let epochs = self.enter(s)?.epoch;
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
        let revoked = c.unit.is_some_and(|u| epochs[u.index()] != c.epoch);
        if others || (c.sid != 0 && !revoked) {
            return Err(Errno::EPERM);
        }
        *c = Ctty {
            sid: p.sid(),
            pgrp: p.pgid(),
            unit,
            epoch: unit.map_or(0, |u| epochs[u.index()]),
        };
        drop(c);
        self.fg_gen.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// `TIOCNOTTY` (`tty_ioctl_cnotty`): the session leader lets its
    /// controlling terminal go.
    fn notty(&self, s: &Sleepable) -> Result<(), Errno> {
        drop(self.enter(s)?);
        let p = Process::current().ok_or(Errno::ENOTTY)?;
        let mut c = self.ctty.lock();
        if c.sid == 0 || c.sid != p.sid() {
            return Err(Errno::ENOTTY);
        }
        if p.pid() != p.sid() {
            return Err(Errno::EPERM);
        }
        *c = Ctty::default();
        drop(c);
        self.fg_gen.fetch_add(1, Ordering::Release);
        self.bg_wait.wake_all();
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

    /// Whether this is the caller's controlling terminal (`tty_is_ctty`).
    fn is_ctty(&self) -> bool {
        self.caller_ctty().is_ok()
    }

    /// Signals the foreground process group (`tty_signal_pgrp`).
    fn signal_foreground(&self, sig: u32) {
        if sig == abi::signal::SIGINFO {
            self.info();
        }
        let fg = self.ctty.lock().pgrp;
        if fg != 0 {
            crate::signal::post_group(fg, sig);
        }
    }

    /// Prints the status line (`tty_info`), unless `NOKERNINFO` is set.
    fn info(&self) {
        let s = crate::kinfo::sleepable();
        let (wanted, column0) = self.st.lock(&s).d.status_wanted();
        if !wanted {
            return;
        }
        let c = *self.ctty.lock();
        let sid = (c.sid != 0).then_some(c.sid);
        let line = info::status_line(sid, c.pgrp, column0);
        let mut st = self.st.lock(&s);
        st.d.status(&line);
        self.outwakeup(&mut st);
    }

    /// The proctree requests, on either side.
    fn ioctl_proctree(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &[u8],
        unit: Option<Unit>,
    ) -> Result<(), Errno> {
        match req {
            TIOCNOTTY => self.notty(s),
            TIOCSCTTY => self.set_ctty(s, unit),
            _ => self.set_pgrp(s, get_int(arg)),
        }
    }

    /// `ttydev_ioctl` through `unit`, by a file with flags `fflag`.
    fn ioctl(
        &self,
        s: &Sleepable,
        unit: Unit,
        req: u32,
        arg: &mut [u8],
        fflag: u32,
    ) -> Result<(), Errno> {
        if matches!(req, TIOCNOTTY | TIOCSCTTY | TIOCSPGRP) {
            return self.ioctl_proctree(s, req, arg, Some(unit));
        }
        drop(self.enter(s)?);
        // Requests that change the terminal wait in the background.
        if matches!(
            req,
            TIOCCBRK
                | TIOCCONS
                | TIOCDRAIN
                | TIOCEXCL
                | TIOCFLUSH
                | TIOCNXCL
                | TIOCSBRK
                | TIOCSETA
                | TIOCSETAF
                | TIOCSETAW
                | TIOCSTART
                | TIOCSTAT
                | TIOCSTI
                | TIOCSTOP
                | TIOCSWINSZ
        ) {
            self.background(s, SIGTTOU)?;
        }
        if matches!(req, TIOCSETA | TIOCSETAW | TIOCSETAF) {
            // The lock state keeps what it locks.
            let st = self.st.lock(s);
            let old = *st.d.termios();
            let lock = st.lock[unit.side()];
            drop(st);
            let mut new = termios_from(arg);
            new.c_iflag = (old.c_iflag & lock.c_iflag) | (new.c_iflag & !lock.c_iflag);
            new.c_oflag = (old.c_oflag & lock.c_oflag) | (new.c_oflag & !lock.c_oflag);
            new.c_cflag = (old.c_cflag & lock.c_cflag) | (new.c_cflag & !lock.c_cflag);
            new.c_lflag = (old.c_lflag & lock.c_lflag) | (new.c_lflag & !lock.c_lflag);
            for cc in 0..NCCS {
                if lock.c_cc[cc] != 0 {
                    new.c_cc[cc] = old.c_cc[cc];
                }
            }
            if lock.c_ispeed != 0 {
                new.c_ispeed = old.c_ispeed;
            }
            if lock.c_ospeed != 0 {
                new.c_ospeed = old.c_ospeed;
            }
            termios_to(&new, arg);
        }
        self.tty_ioctl(s, req, arg, fflag)
    }

    /// `tty_ioctl`: the generic requests, then the device's.
    fn tty_ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8], fflag: u32) -> Result<(), Errno> {
        if self.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        match req {
            TIOCSDTR => {
                let _ = self.driver.modem(SER_DTR, 0);
            }
            TIOCCDTR => {
                let _ = self.driver.modem(0, SER_DTR);
            }
            TIOCMSET | TIOCMBIS | TIOCMBIC => {
                let bits = get_int(arg);
                let lines = (bits & (TIOCM_DTR | TIOCM_RTS).cast_signed()) >> 1;
                let _ = match req {
                    TIOCMSET => self
                        .driver
                        .modem(lines, (!bits & (TIOCM_DTR | TIOCM_RTS).cast_signed()) >> 1),
                    TIOCMBIS => self.driver.modem(lines, 0),
                    _ => self.driver.modem(0, lines),
                };
            }
            TIOCMGET => put_int(arg, TIOCM_LE.cast_signed() + (self.driver.modem(0, 0) << 1)),
            FIONREAD => put_int(
                arg,
                i32::try_from(self.st.lock(s).d.available()).unwrap_or(i32::MAX),
            ),
            FIONWRITE | TIOCOUTQ => put_int(
                arg,
                i32::try_from(self.st.lock(s).d.output_len()).unwrap_or(i32::MAX),
            ),
            TIOCGETA => termios_to(self.st.lock(s).d.termios(), arg),
            TIOCSETA | TIOCSETAW | TIOCSETAF => self.set_termios(s, req, termios_from(arg))?,
            TIOCGETD => put_int(arg, TTYDISC.cast_signed()),
            TIOCSETD if get_int(arg) == TTYDISC.cast_signed() => {}
            TIOCGPGRP => {
                let c = self.caller_ctty()?;
                put_int(arg, if c.pgrp == 0 { NO_PID } else { c.pgrp });
            }
            TIOCGSID => put_int(arg, self.caller_ctty()?.sid),
            TIOCFLUSH => {
                let what = get_int(arg).cast_unsigned();
                let what = if what == 0 { FREAD | FWRITE } else { what };
                let mut st = self.st.lock(s);
                self.flush(&mut st, what & FREAD != 0, what & FWRITE != 0);
                self.mirror(&st);
            }
            TIOCDRAIN => self.drain(s, false)?,
            TIOCGDRAINWAIT => put_int(arg, self.st.lock(s).drainwait.cast_signed()),
            TIOCSDRAINWAIT => {
                crate::fs::priv_check(Priv::TtyDrainwait)?;
                self.st.lock(s).drainwait = get_int(arg).cast_unsigned();
            }
            TIOCGWINSZ => winsize_to(&self.winsize.lock(), arg),
            TIOCSWINSZ => self.resize(winsize_from(arg)),
            TIOCEXCL => self.st.lock(s).exclude = true,
            TIOCNXCL => self.st.lock(s).exclude = false,
            TIOCSTOP => {
                let mut st = self.st.lock(s);
                st.d.set_stopped(true);
                self.driver.pktnotify(TIOCPKT_STOP);
                self.mirror(&st);
            }
            TIOCSTART => {
                let mut st = self.st.lock(s);
                st.d.signaled();
                self.outwakeup(&mut st);
                self.driver.pktnotify(TIOCPKT_START);
                self.mirror(&st);
                drop(st);
                self.wakeup(false, true);
            }
            // The status line alone (`tty_info`), no signal.
            TIOCSTAT => self.info(),
            TIOCSTI => {
                self.sti_check(fflag)?;
                let c = arg.first().copied().unwrap_or(0);
                let mut signals = Vec::new();
                {
                    let mut st = self.st.lock(s);
                    match st.d.input(c) {
                        Input::Signal(sig) => signals.push(sig),
                        Input::Taken | Input::Full => {}
                    }
                    self.rint_done(&mut st);
                    self.mirror(&st);
                }
                for sig in signals {
                    self.signal_foreground(sig);
                }
            }
            _ => return self.driver.ioctl(s, req, arg),
        }
        Ok(())
    }

    /// `tty_sti_check`: with `PRIV_TTY_STI` one may type into any
    /// terminal; without it only into one's controlling terminal, opened
    /// for reading.
    fn sti_check(&self, fflag: u32) -> Result<(), Errno> {
        if crate::fs::priv_check(Priv::TtySti).is_ok() {
            return Ok(());
        }
        if fflag & FREAD == 0 {
            return Err(Errno::EPERM);
        }
        if !self.is_ctty() {
            return Err(Errno::EACCES);
        }
        Ok(())
    }

    /// `TIOCSETA` and kin (`tty_generic_ioctl`).
    fn set_termios(&self, s: &Sleepable, req: u32, mut t: Termios) -> Result<(), Errno> {
        use abi::termios::{CIGNORE, TTYSUP_CFLAG, TTYSUP_IFLAG, TTYSUP_LFLAG, TTYSUP_OFLAG};
        // POSIX: an input speed of 0 is the output speed.
        if t.c_ispeed == 0 {
            t.c_ispeed = t.c_ospeed;
        }
        t.c_iflag &= TTYSUP_IFLAG;
        t.c_oflag &= TTYSUP_OFLAG;
        t.c_lflag &= TTYSUP_LFLAG;
        t.c_cflag &= TTYSUP_CFLAG | CIGNORE;
        if matches!(req, TIOCSETAW | TIOCSETAF) {
            self.drain(s, false)?;
            if req == TIOCSETAF {
                let mut st = self.st.lock(s);
                self.flush(&mut st, true, false);
                self.mirror(&st);
            }
        }
        let mut st = self.st.lock(s);
        let old = *st.d.termios();
        let mut new = t;
        // The device hears only of changes to its line.
        if t.c_cflag & CIGNORE == 0
            && (old.c_cflag != t.c_cflag
                || (old.c_iflag ^ t.c_iflag) & (IXON | IXOFF | IXANY) != 0
                || old.c_ispeed != t.c_ispeed
                || old.c_ospeed != t.c_ospeed)
        {
            self.driver.param(&mut new)?;
            new.c_cflag &= !CIGNORE;
        } else {
            new.c_cflag = old.c_cflag;
            new.c_ispeed = old.c_ispeed;
            new.c_ospeed = old.c_ospeed;
        }
        let speeds =
            (new.c_ispeed, new.c_ospeed, new.c_cflag) != (old.c_ispeed, old.c_ospeed, old.c_cflag);
        st.d.set_termios(new);
        if speeds {
            st.d.set_watermarks();
        }
        self.mirror(&st);
        if new.c_lflag & ICANON == 0 {
            // `VMIN` and `VTIME` may have changed too.
            self.wakeup(true, false);
        }
        // For packet mode: whether `^S` and `^Q` stop output.
        let ctrl = |c: u8| c & 0x1f;
        if new.c_iflag & IXON != 0
            && new.c_cc[VSTOP] == ctrl(b'S')
            && new.c_cc[VSTART] == ctrl(b'Q')
        {
            self.driver.pktnotify(TIOCPKT_DOSTOP);
        } else {
            self.driver.pktnotify(TIOCPKT_NOSTOP);
        }
        self.mirror(&st);
        Ok(())
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
    if arg.len() < 44 {
        return Termios::default();
    }
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

pub(crate) fn termios_to(t: &Termios, arg: &mut [u8]) {
    if arg.len() < 44 {
        return;
    }
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
    let half = |o: usize| {
        arg.get(o..o + 2)
            .map_or(0, |b| u16::from_ne_bytes([b[0], b[1]]))
    };
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
        if let Some(b) = arg.get_mut(o..o + 2) {
            b.copy_from_slice(&v.to_ne_bytes());
        }
    }
}

/// A terminal's node: the call-in, call-out, or console device
/// (`ttydev_cdevsw`).
pub(crate) struct TtyDevice {
    tty: Arc<Tty>,
    unit: Unit,
}

impl TtyDevice {
    pub(crate) fn new(tty: &Arc<Tty>, unit: Unit) -> Self {
        Self {
            tty: Arc::clone(tty),
            unit,
        }
    }
}

/// A terminal's `EVFILT_READ` (`false`: `tty_kqops_read`) and
/// `EVFILT_WRITE` (`tty_kqops_write`).
struct TtyFilter(Arc<Tty>, bool);

impl event::Filter for TtyFilter {
    fn attach(&self, kn: &Arc<event::Knote>) -> Result<(), Errno> {
        self.0.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &event::Knote) {
        self.0.notify.remove(kn);
    }

    fn event(&self, _kn: &event::Knote, note: &mut event::Note, _hint: u64) -> bool {
        let t = &self.0;
        let gone = t.gone.load(Ordering::Acquire);
        if gone || (!self.1 && t.zombie.load(Ordering::Acquire)) {
            note.kev.flags |= abi::event::EV_EOF;
            return true;
        }
        let n = if self.1 {
            t.out_room.load(Ordering::Acquire)
        } else {
            t.ready.load(Ordering::Acquire)
        };
        note.kev.data = i64::try_from(n).unwrap_or(0);
        n > 0
    }

    fn fork(&self, _kn: &event::Knote) -> Option<Box<dyn event::Filter>> {
        Some(Box::new(Self(Arc::clone(&self.0), self.1)))
    }
}

/// An open file of a terminal's node: its last close, of the files
/// opened since the node was last revoked, closes the node.
pub(crate) struct TtyOpen {
    tty: Arc<Tty>,
    unit: Unit,
    epoch: u64,
}

impl Drop for TtyOpen {
    fn drop(&mut self) {
        // SAFETY: files close in thread context (`close`, `exit`, the
        // socket collector).
        let s = unsafe { Sleepable::assume() };
        let last = {
            let mut st = self.tty.st.lock(&s);
            let i = self.unit.index();
            if st.epoch[i] != self.epoch || st.opens[i] == 0 {
                false
            } else {
                st.opens[i] -= 1;
                st.opens[i] == 0
            }
        };
        if last {
            self.tty.close(&s, self.unit, false);
        }
    }
}

impl CharDevice for TtyOpen {
    fn dtype(&self) -> u32 {
        vfs::D_TTY
    }

    /// The device being destroyed reads as end of file.
    fn read(&self, s: &Sleepable, uio: &mut dyn Uio, _: u64, nb: bool) -> Result<usize, Errno> {
        match self.tty.read(s, uio, nb) {
            Err(Errno::ENXIO) => Ok(0),
            r => r,
        }
    }

    /// `/dev/console`'s output goes to the message buffer too
    /// (`ttyconsdev_write`'s `log_console`).
    fn write(&self, s: &Sleepable, uio: &mut dyn Uio, _: u64, nb: bool) -> Result<usize, Errno> {
        if self.unit != Unit::Console {
            return self.tty.write(s, uio, nb);
        }
        let mut tee = crate::klog::Tee::new(uio);
        let r = self.tty.write(s, &mut tee, nb);
        crate::klog::log_console(tee.seen());
        r
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        self.tty.ioctl(s, self.unit, req, arg, FREAD | FWRITE)
    }

    fn ioctl_file(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &mut [u8],
        fflag: u32,
    ) -> Result<usize, Errno> {
        self.tty.ioctl(s, self.unit, req, arg, fflag).map(|()| 0)
    }

    /// `ttydev_mmap`: through the driver.
    fn mmap(&self, page: u64, pages: u64) -> Result<Arc<vm::object::Object>, Errno> {
        if self.tty.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        self.tty.driver.mmap(page, pages)
    }

    fn poll(&self, events: i16, w: Option<&Arc<dyn event::Wake>>) -> i16 {
        self.tty.poll(events, w)
    }

    fn kqfilter(&self, kev: &abi::event::Kevent) -> Result<Box<dyn event::Filter>, Errno> {
        self.tty.kqfilter(kev)
    }
}

impl CharDevice for TtyDevice {
    fn dtype(&self) -> u32 {
        vfs::D_TTY
    }

    fn instance(&self, s: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        self.instance_nonblock(s, false)
    }

    fn instance_nonblock(
        &self,
        s: &Sleepable,
        nonblock: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        Ok(Some(Arc::new(self.tty.open(s, self.unit, nonblock)?)))
    }

    /// Every open of this node is over (`devfs_revoke`, then
    /// `ttydev_close` with `FREVOKE`).
    fn revoke(&self, s: &Sleepable) {
        self.tty.close(s, self.unit, true);
    }

    /// Opened files read and write through their instance; this is not
    /// reached.
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }
}

/// A terminal's init or lock state node (`ttyil_cdevsw`): `TIOCGETA` and
/// `TIOCSETA` (the superuser's) read and set the state the next open
/// starts with, or what `TIOCSETA` may not change.
struct InitLock {
    tty: Arc<Tty>,
    /// The call-out side's.
    out: bool,
    lock: bool,
}

impl InitLock {
    fn new(tty: &Arc<Tty>, out: bool, lock: bool) -> Self {
        Self {
            tty: Arc::clone(tty),
            out,
            lock,
        }
    }
}

impl CharDevice for InitLock {
    fn dtype(&self) -> u32 {
        vfs::D_TTY
    }

    fn instance(&self, _s: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if self.tty.gone.load(Ordering::Acquire) {
            return Err(Errno::ENODEV);
        }
        Ok(None)
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let mut st = self.tty.st.lock(s);
        if st.gone {
            return Err(Errno::ENODEV);
        }
        let side = usize::from(self.out);
        let state = if self.lock {
            &mut st.lock[side]
        } else {
            &mut st.init[side]
        };
        match req {
            TIOCGETA => termios_to(state, arg),
            TIOCSETA => {
                crate::fs::priv_check(Priv::TtySeta)?;
                *state = termios_from(arg);
            }
            TIOCGETD => put_int(arg, TTYDISC.cast_signed()),
            TIOCGWINSZ => winsize_to(&Winsize::default(), arg),
            _ => return Err(Errno::ENOTTY),
        }
        Ok(())
    }
}

/// `/dev/tty`: opens the caller's controlling terminal (`cttyopen`).
pub(crate) struct ControllingTty;

impl CharDevice for ControllingTty {
    fn open(&self) -> Result<Option<Arc<Vnode>>, Errno> {
        // SAFETY: opens run in thread context.
        let s = unsafe { Sleepable::assume() };
        let p = Process::current().ok_or(Errno::ENXIO)?;
        let t = controlling(p.sid()).ok_or(Errno::ENXIO)?;
        let unit = t.ctty.lock().unit.unwrap_or(Unit::In);
        crate::fs::devfs::vnode_of(&s, t.node_name(unit).as_bytes())
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
