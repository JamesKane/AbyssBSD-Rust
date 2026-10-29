// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/klog`, from which `syslogd` reads the kernel's messages, and
//! `log(9)`, which writes to it.
//!
//! - The message buffer is the console's log ring
//!   ([`crate::console::msgbuf`]). `printf` text goes in as it is;
//!   `log(9)` lines start with `<pri>` (`<6>` for `LOG_INFO`), and
//!   `/dev/console` output with `<118>` (`LOG_CONSOLE | LOG_INFO`), as
//!   `syslogd`'s `printsys` expects them.
//! - One open at a time (`EBUSY` otherwise). Reads take what was not read
//!   yet, from where the last reader stopped; a reader left behind by the
//!   ring starts at its oldest byte. A read waits while there is nothing
//!   (or fails with `EAGAIN` without blocking), and readers, pollers,
//!   knotes, and `SIGIO` are woken by a timer `kern.log_wakeups_per_second`
//!   times a second when text came, not by each message.
//! - While it is open, `log(9)` messages go to the buffer only; otherwise
//!   to the consoles too.
//!
//! Behavior reference: sys/kern/subr_log.c, sys/kern/subr_prf.c (`log`,
//! `log_console`, the `kern.*console*` knobs), sys/kern/subr_msgbuf.c.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use abi::errno::Errno;
use abi::event::{EVFILT_READ, Kevent, POLLIN, POLLRDNORM};
use abi::ioctl::{FIOASYNC, FIOGETOWN, FIONBIO, FIONREAD, FIOSETOWN, TIOCGPGRP, TIOCSPGRP};
use abi::syslog::{LOG_CONSOLE, LOG_INFO};
use event::{Filter, Knote, Note, Notify, Wake};
use sched::{Sleepable, WaitQueue};
use sync::Once;
use vfs::{CharDevice, Uio};

use crate::signal::sigio::Sigio;
use crate::softclock::Callout;

/// Someone has `/dev/klog` open (`log_open`).
static OPEN: AtomicBool = AtomicBool::new(false);
/// `FIOASYNC` is on (`LOG_ASYNC`).
static ASYNC: AtomicBool = AtomicBool::new(false);
static SIGIO: Sigio = Sigio::new();
/// Readers (`log_wakeup`).
static WAIT: WaitQueue = WaitQueue::new();
/// Pollers and knotes (`logsoftc.sc_selp`).
static NOTIFY: Notify = Notify::new();
/// The wakeup timer (`logsoftc.sc_callout`).
static TIMER: Once<Pin<Box<Callout>>> = Once::new();

/// `kern.log_wakeups_per_second`.
static WAKEUPS: AtomicI32 = AtomicI32::new(5);
/// `kern.log_console_output`: console output goes to the buffer.
static CONSOLE_OUTPUT: AtomicI32 = AtomicI32::new(1);
/// `kern.log_console_add_linefeed`.
static ADD_LINEFEED: AtomicI32 = AtomicI32::new(0);
/// `kern.always_console_output`: kept for `sysctl`; with no `TIOCCONS`
/// redirection here, console output always reaches the console.
static ALWAYS_CONSOLE: AtomicI32 = AtomicI32::new(0);

/// A kernel message of priority `pri` (`log(9)`): to the buffer, and the
/// consoles too while no one reads `/dev/klog`.
pub(crate) fn log(pri: i32, args: core::fmt::Arguments<'_>) {
    crate::console::log(pri, !OPEN.load(Ordering::Acquire), args);
}

/// Bytes written to `/dev/console`, for the buffer (`log_console`).
pub(crate) fn log_console(bytes: &[u8]) {
    if CONSOLE_OUTPUT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let pri = LOG_INFO | LOG_CONSOLE;
    crate::console::log_bytes(pri, bytes);
    // The old behavior, on request: each write ends a line.
    if bytes.last() != Some(&b'\n') && ADD_LINEFEED.load(Ordering::Relaxed) != 0 {
        crate::console::log_bytes(pri, b"\n");
    }
}

/// A write's [`Uio`] that keeps what the writer takes from it, for
/// [`log_console`] (FreeBSD clones the write's `uio`).
pub(crate) struct Tee<'a> {
    uio: &'a mut dyn Uio,
    seen: alloc::vec::Vec<u8>,
}

impl<'a> Tee<'a> {
    pub(crate) fn new(uio: &'a mut dyn Uio) -> Self {
        Self {
            uio,
            seen: alloc::vec::Vec::new(),
        }
    }

    /// The bytes taken so far.
    pub(crate) fn seen(&self) -> &[u8] {
        &self.seen
    }
}

impl Uio for Tee<'_> {
    fn resid(&self) -> usize {
        self.uio.resid()
    }

    fn copy_out(&mut self, data: &[u8]) -> Result<usize, Errno> {
        self.uio.copy_out(data)
    }

    fn copy_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let n = self.uio.copy_in(buf)?;
        if CONSOLE_OUTPUT.load(Ordering::Relaxed) != 0 {
            self.seen.extend_from_slice(&buf[..n]);
        }
        Ok(n)
    }
}

/// The wakeup period, at least once a second (`logopen` and `logtimeout`
/// raise a setting below 1, saying so).
fn period_ns() -> u64 {
    let mut n = WAKEUPS.load(Ordering::Relaxed);
    if n < 1 {
        kprintln!("syslog wakeup is less than one.  Adjusting to 1.");
        WAKEUPS.store(1, Ordering::Relaxed);
        n = 1;
    }
    1_000_000_000 / u64::from(n.cast_unsigned())
}

fn arm() {
    if let Some(t) = TIMER.get() {
        t.as_ref().reset(period_ns(), tick, 0);
    }
}

/// `logtimeout`: if the buffer gained text, wakes everyone waiting for it.
fn tick(_s: &Sleepable, _: usize) {
    if !OPEN.load(Ordering::Acquire) {
        return;
    }
    if crate::console::take_log_trigger() {
        NOTIFY.notify(0);
        if ASYNC.load(Ordering::Acquire) {
            SIGIO.signal();
        }
        WAIT.wake_all();
    }
    arm();
}

/// `/dev/klog`'s node: each open is a [`KlogOpen`].
struct Klog;

impl CharDevice for Klog {
    /// `logopen`.
    fn instance(&self, _s: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if WAKEUPS.load(Ordering::Relaxed) < 1 {
            let _ = period_ns();
        }
        if OPEN.swap(true, Ordering::AcqRel) {
            return Err(Errno::EBUSY);
        }
        arm();
        // The opener gets `SIGIO`, no group.
        if let Some(p) = crate::proc::Process::current() {
            let _ = SIGIO.set(p.pid());
        }
        Ok(Some(Arc::new(KlogOpen)))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }
}

/// An open of `/dev/klog`; its last close is `logclose`.
struct KlogOpen;

impl Drop for KlogOpen {
    fn drop(&mut self) {
        SIGIO.clear();
        if let Some(t) = TIMER.get() {
            t.stop();
        }
        ASYNC.store(false, Ordering::Release);
        OPEN.store(false, Ordering::Release);
    }
}

fn put_int(arg: &mut [u8], v: i32) {
    if let Some(a) = arg.get_mut(..4) {
        a.copy_from_slice(&v.to_ne_bytes());
    }
}

fn int(arg: &[u8]) -> i32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i32::from_ne_bytes)
}

impl CharDevice for KlogOpen {
    /// `logread`: waits for text, then takes what fits, in pieces.
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        while crate::console::msgbuf_unread() == 0 {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _w = sched::wait_message("klog");
            crate::signal::sleep(s, &WAIT, || crate::console::msgbuf_unread() > 0, None)?;
        }
        let mut buf = [0u8; 128];
        let mut done = 0;
        while uio.resid() > 0 {
            let l = uio.resid().min(buf.len());
            let n = crate::console::msgbuf_read(&mut buf[..l]);
            if n == 0 {
                break;
            }
            done += uio.copy_out(&buf[..n])?;
        }
        Ok(done)
    }

    /// No `d_write` (`enodev`).
    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    /// `logioctl`.
    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            FIONREAD => put_int(
                arg,
                i32::try_from(crate::console::msgbuf_unread()).unwrap_or(i32::MAX),
            ),
            // The file's flag is the non-blocking state reads see.
            FIONBIO => {}
            FIOASYNC => ASYNC.store(int(arg) != 0, Ordering::Release),
            FIOSETOWN => SIGIO.set(int(arg))?,
            FIOGETOWN => put_int(arg, SIGIO.get()),
            // The old names, for a process group.
            TIOCSPGRP => SIGIO.set(int(arg).wrapping_neg())?,
            TIOCGPGRP => put_int(arg, SIGIO.get().wrapping_neg()),
            _ => return Err(Errno::ENOTTY),
        }
        Ok(())
    }

    fn set_async(&self, on: bool) {
        ASYNC.store(on, Ordering::Release);
    }

    /// `logpoll`: readable while there is unread text.
    fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if events & (POLLIN | POLLRDNORM) == 0 {
            return 0;
        }
        if let Some(w) = w {
            NOTIFY.record(w);
        }
        if crate::console::msgbuf_unread() > 0 {
            events & (POLLIN | POLLRDNORM)
        } else {
            0
        }
    }

    /// `logkqfilter`: `EVFILT_READ` only.
    fn kqfilter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        if kev.filter != EVFILT_READ {
            return Err(Errno::EINVAL);
        }
        Ok(Box::new(KlogFilter))
    }
}

/// `log_read_filterops`: active with unread text, its count as data.
struct KlogFilter;

impl Filter for KlogFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        NOTIFY.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        NOTIFY.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let n = crate::console::msgbuf_unread();
        note.kev.data = i64::try_from(n).unwrap_or(i64::MAX);
        n != 0
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self))
    }
}

/// A tunable's value from the kernel environment (`TUNABLE_INT`), else
/// `default`.
fn tunable(name: &str, default: i32) -> i32 {
    crate::boot::value(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Adds `/dev/klog` (FreeBSD's mode and owner) and the `kern.*log*`
/// sysctls.
pub(crate) fn init(s: &Sleepable) {
    let _ = TIMER.set(Callout::new());
    crate::fs::devfs::add(b"klog", 0o600, 0, 0, Arc::new(Klog));
    CONSOLE_OUTPUT.store(tunable("kern.log_console_output", 1), Ordering::Relaxed);
    ADD_LINEFEED.store(
        tunable("kern.log_console_add_linefeed", 0),
        Ordering::Relaxed,
    );
    ALWAYS_CONSOLE.store(tunable("kern.always_console_output", 0), Ordering::Relaxed);
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        let knob =
            |t: &mut sysctl::Tree, name: &str, descr: &str, flags: u32, v: &'static AtomicI32| {
                t.add_writable(
                    kern,
                    sysctl::Number::Auto,
                    name,
                    descr,
                    flags,
                    sysctl::Value::Int(Box::new(|| v.load(Ordering::Relaxed))),
                    sysctl::int_writer(|n| {
                        v.store(n, Ordering::Relaxed);
                        Ok(())
                    }),
                )
                .expect("kern log knob");
            };
        knob(
            t,
            "log_wakeups_per_second",
            "How often (times per second) to check for /dev/log waiters.",
            0,
            &WAKEUPS,
        );
        let tun = abi::sysctl::ctlflag::TUN;
        knob(
            t,
            "log_console_output",
            "Duplicate console output to the syslog",
            tun,
            &CONSOLE_OUTPUT,
        );
        knob(
            t,
            "log_console_add_linefeed",
            "log_console() adds extra newlines",
            tun,
            &ADD_LINEFEED,
        );
        knob(
            t,
            "always_console_output",
            "Always output to console despite TIOCCONS",
            tun,
            &ALWAYS_CONSOLE,
        );
    });
}

#[cfg(feature = "ktest")]
mod ktests {
    use alloc::string::String;

    use abi::event::EVFILT_WRITE;
    use ktest::ktest;
    use vfs::ReadBuf;

    use super::*;
    use crate::devctl::ktests::{NoWake, One, cx, kev, open};

    /// Reads what `/dev/klog` has, without waiting.
    fn drain(k: &Arc<dyn CharDevice>, s: &Sleepable) -> String {
        let mut text = String::new();
        let mut buf = [0u8; 512];
        loop {
            match k.read(s, &mut ReadBuf::new(&mut buf), 0, true) {
                Ok(n) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(Errno::EAGAIN) => return text,
                Err(e) => panic!("klog read: {e:?}"),
            }
        }
    }

    fn unread(k: &Arc<dyn CharDevice>, s: &Sleepable) -> i32 {
        let mut arg = [0u8; 4];
        k.ioctl(s, FIONREAD, &mut arg).unwrap();
        i32::from_ne_bytes(arg)
    }

    ktest! {
        fn klog_reads_the_kernel_messages() {
            let s = cx();
            let k = open(&s, b"klog").unwrap();
            assert_eq!(open(&s, b"klog").err(), Some(Errno::EBUSY));
            // The boot's messages first, from the start of the buffer.
            let boot = drain(&k, &s);
            assert!(boot.contains("ktest: "), "{} bytes", boot.len());
            assert_eq!(unread(&k, &s), 0);
            assert_eq!(k.poll(POLLIN | POLLRDNORM, None), 0);
            let kq = event::Kqueue::new(Box::new(NoWake));
            kq.register(&kev(EVFILT_READ), &One(Arc::clone(&k))).unwrap();
            assert_eq!(kq.register(&kev(EVFILT_WRITE), &One(Arc::clone(&k))), Err(Errno::EINVAL));
            // log(9) lines carry their priority; printf's none; console
            // output is LOG_CONSOLE's, and a change of priority ends an
            // unfinished line.
            log(LOG_INFO, format_args!("klog test: a log line\n"));
            log_console(b"klog test: console");
            kprintln!("klog test: a printf line");
            assert!(unread(&k, &s) > 0);
            assert_eq!(k.poll(POLLIN | POLLRDNORM, None), POLLIN | POLLRDNORM);
            // The wakeup timer tells the knote (5 times a second).
            let start = crate::time::now_ns();
            while kq.pending() == 0 && crate::time::now_ns() - start < 2_000_000_000 {
                crate::time::sleep_ns(&s, 10_000_000);
            }
            assert_eq!(kq.pending(), 1, "no wakeup");
            let text = drain(&k, &s);
            assert!(text.contains("<6>klog test: a log line\n"), "{text}");
            assert!(text.contains("<118>klog test: console\nklog test: a printf line\n"), "{text}");
            // A reader waits for the next wakeup.
            let t = sched::spawn("klog-msg", sched::Priority::KERNEL, |_| {
                log(LOG_INFO, format_args!("klog test: later\n"));
            })
            .unwrap();
            let mut buf = [0u8; 256];
            let n = k.read(&s, &mut ReadBuf::new(&mut buf), 0, false).unwrap();
            t.join(&s);
            let mut text = String::from_utf8_lossy(&buf[..n]).into_owned();
            text.push_str(&drain(&k, &s));
            assert!(text.contains("<6>klog test: later\n"), "{text}");
            // Its requests: no SIGIO owner for a kernel thread.
            let mut arg = [0u8; 4];
            assert_eq!(k.ioctl(&s, FIOGETOWN, &mut arg), Ok(()));
            assert_eq!(i32::from_ne_bytes(arg), 0);
            assert_eq!(k.ioctl(&s, TIOCGPGRP, &mut arg), Ok(()));
            assert_eq!(k.ioctl(&s, abi::ioctl::FIODTYPE, &mut arg), Err(Errno::ENOTTY));
            kq.close();
            drop(k);
            drop(open(&s, b"klog").unwrap());
        }
    }
}
