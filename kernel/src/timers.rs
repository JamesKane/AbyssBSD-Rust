// SPDX-License-Identifier: BSD-2-Clause
//! User timers (docs/MULTIUSER.md §7.5): kqueue's `EVFILT_TIMER`,
//! `timerfd`, the interval timers (`setitimer`), and POSIX timers
//! (`ktimer_*`). Each runs on a [`Callout`], so its expiry is handled on
//! the softclock thread.
//!
//! Deadlines are kept in nanoseconds of uptime ([`crate::time::now_ns`]);
//! a deadline on another clock is converted by that clock's offset from
//! uptime when it is set, as FreeBSD converts `CLOCK_REALTIME` deadlines
//! by the boot time.
//!
//! Behavior reference: sys/kern/kern_event.c (`filt_timer*`),
//! sys/kern/sys_timerfd.c, sys/kern/kern_time.c.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::pin::Pin;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::event::{
    EV_CLEAR, EV_ERROR, EV_ONESHOT, EVFILT_READ, Itimerspec, Kevent, NOTE_ABSTIME, NOTE_MSECONDS,
    NOTE_NSECONDS, NOTE_SECONDS, NOTE_USECONDS, POLLIN, POLLRDNORM, SIGEV_NONE, SIGEV_SIGNAL,
    SIGEV_THREAD_ID, TFD_TIMER_ABSTIME, TFD_TIMER_CANCEL_ON_SET,
};
use abi::stat::Stat;
use abi::time::{Timespec, clock};
use event::{Filter, Knote, Note, Notify, Wake};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vfs::Uio;

use crate::proc::Process;
use crate::softclock::Callout;

pub(crate) const NANOS: u64 = 1_000_000_000;

/// Nanoseconds of uptime.
pub(crate) fn now() -> u64 {
    crate::time::now_ns()
}

/// `ts` in nanoseconds, if it is a valid interval (`timespecvalid_interval`).
pub(crate) fn ts_ns(ts: &Timespec) -> Result<u64, Errno> {
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    Ok(ts
        .tv_sec
        .cast_unsigned()
        .saturating_mul(NANOS)
        .saturating_add(ts.tv_nsec.cast_unsigned()))
}

pub(crate) fn ns_ts(ns: u64) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(ns / NANOS).unwrap_or(i64::MAX),
        tv_nsec: (ns % NANOS).cast_signed(),
    }
}

/// How far clock `id` reads ahead of uptime, in nanoseconds (the boot
/// time, for the real-time clocks).
pub(crate) fn clock_offset(id: u32) -> Result<i128, Errno> {
    let c = i128::from(crate::syscall::clock_ns(id)?);
    Ok(c - i128::from(now()))
}

/// The uptime at which clock `id` reads `at`.
fn uptime_of(id: u32, at: u64) -> Result<u64, Errno> {
    let up = i128::from(at) - clock_offset(id)?;
    Ok(u64::try_from(up.max(0)).unwrap_or(u64::MAX))
}

/// The boot time, in nanoseconds of the real-time clock.
fn boottime() -> u64 {
    u64::try_from(clock_offset(clock::REALTIME).unwrap_or(0).max(0)).unwrap_or(0)
}

// --- EVFILT_TIMER ---

/// `NOTE_TIMER_PRECMASK`.
const PRECMASK: u32 = NOTE_SECONDS | NOTE_MSECONDS | NOTE_USECONDS | NOTE_NSECONDS;

/// `timer2sbintime`, in nanoseconds: `data` in the unit `fflags` names
/// (milliseconds by default), saturating.
fn timer_ns(data: i64, fflags: u32) -> Option<u64> {
    let per = match fflags & PRECMASK {
        NOTE_SECONDS => NANOS,
        NOTE_MSECONDS | 0 => 1_000_000,
        NOTE_USECONDS => 1_000,
        NOTE_NSECONDS => 1,
        _ => return None,
    };
    Some(data.cast_unsigned().saturating_mul(per))
}

struct KqTimerState {
    /// Armed: `next` is when it fires.
    armed: bool,
    /// Bumped by each start, so an expiry of an earlier arming is not
    /// counted.
    arming: u64,
    next: u64,
    /// The period, 0 for an absolute timer.
    to: u64,
}

/// `EVFILT_TIMER`: `data` counts expiries since last reported; periodic
/// unless `EV_ONESHOT` or `NOTE_ABSTIME`.
struct KqTimer {
    callout: Pin<Box<Callout>>,
    kn: AtomicPtr<Knote>,
    st: SpinLock<KqTimerState>,
    /// A forked copy's `next` and period, which it resumes with.
    copied: Option<(u64, u64)>,
}

/// The filter for a new `EVFILT_TIMER` knote.
pub(crate) fn kevent_timer(_kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
    Ok(Box::new(KqTimer::new(None)))
}

impl KqTimer {
    fn new(copied: Option<(u64, u64)>) -> Self {
        Self {
            callout: Callout::new(),
            kn: AtomicPtr::new(core::ptr::null_mut()),
            st: SpinLock::new(KqTimerState {
                armed: false,
                arming: 0,
                next: 0,
                to: 0,
            }),
            copied,
        }
    }

    /// `filt_timervalidate`: the period, or the absolute deadline in
    /// uptime with `NOTE_ABSTIME`.
    fn validate(note: &mut Note) -> Result<u64, Errno> {
        if note.sdata < 0 {
            return Err(Errno::EINVAL);
        }
        if note.sdata == 0 && note.kev.flags & EV_ONESHOT == 0 {
            note.sdata = 1;
        }
        if note.sfflags & !(PRECMASK | NOTE_ABSTIME) != 0 {
            return Err(Errno::EINVAL);
        }
        let to = timer_ns(note.sdata, note.sfflags).ok_or(Errno::EINVAL)?;
        if note.sfflags & NOTE_ABSTIME != 0 {
            return Ok(to.saturating_sub(boottime()));
        }
        Ok(to)
    }

    /// `filt_timerstart`.
    fn start(&self, note: &Note, to: u64) {
        let mut st = self.st.lock();
        if note.sfflags & NOTE_ABSTIME != 0 {
            st.next = to;
            st.to = 0;
        } else {
            st.next = to.saturating_add(now());
            st.to = to;
        }
        st.armed = true;
        st.arming += 1;
        self.arm(st.next);
    }

    fn arm(&self, at: u64) {
        let me = core::ptr::from_ref(self).expose_provenance();
        self.callout.as_ref().reset_at(at, Self::expire, me);
    }

    /// `filt_timerexpire`, on the softclock thread.
    fn expire(_s: &Sleepable, arg: usize) {
        // SAFETY: the timer's callout is drained before it goes.
        let me = unsafe { &*core::ptr::with_exposed_provenance::<Self>(arg) };
        let kn = me.kn.load(Ordering::Acquire);
        if kn.is_null() {
            return;
        }
        // SAFETY: the knote holds the filter, which drains its callout
        // before the knote's other fields go.
        let kn = unsafe { &*kn };
        let oneshot = kn.note().kev.flags & EV_ONESHOT != 0;
        let (delta, arming) = {
            let mut st = me.st.lock();
            if !st.armed {
                return;
            }
            let t = now();
            if t < st.next {
                // An arming that a change replaced.
                me.arm(st.next);
                return;
            }
            if oneshot || st.to == 0 {
                st.armed = false;
                (1, st.arming)
            } else {
                let delta = ((t - st.next) / st.to).max(1);
                st.next = st.next.saturating_add(delta.saturating_mul(st.to));
                if t >= st.next {
                    st.next = t + st.to;
                }
                me.arm(st.next);
                (delta, st.arming)
            }
        };
        // Under the note's lock (as a re-add holds it), the arming is
        // still this one.
        kn.trigger(|n| {
            if me.st.lock().arming != arming {
                return false;
            }
            n.kev.data = n
                .kev
                .data
                .saturating_add(i64::try_from(delta).unwrap_or(i64::MAX));
            true
        });
    }

    fn stop(&self) {
        self.st.lock().armed = false;
        self.callout.stop();
    }
}

impl Filter for KqTimer {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.kn.store(Arc::as_ptr(kn).cast_mut(), Ordering::Release);
        if let Some((next, to)) = self.copied {
            {
                let mut st = self.st.lock();
                st.next = next;
                st.to = to;
                st.armed = true;
            }
            self.arm(next);
            return Ok(());
        }
        let mut r = Ok(());
        kn.trigger(|n| {
            match Self::validate(n) {
                Ok(to) => {
                    if n.sfflags & NOTE_ABSTIME == 0 {
                        n.kev.flags |= EV_CLEAR;
                    }
                    self.start(n, to);
                }
                Err(e) => r = Err(e),
            }
            false
        });
        r
    }

    fn detach(&self, _kn: &Knote) {
        self.st.lock().armed = false;
        self.callout.drain();
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        note.kev.data != 0
    }

    fn touches(&self) -> bool {
        true
    }

    /// A re-add with new parameters (`filt_timertouch`): what was
    /// pending is forgotten, and the timer starts over.
    fn register(&self, kn: &Knote, note: &mut Note, kev: &Kevent) {
        if kev.flags & abi::event::EV_ADD == 0 {
            return;
        }
        self.stop();
        kn.deactivate();
        note.kev.data = 0;
        note.sfflags = kev.fflags;
        note.sdata = kev.data;
        match Self::validate(note) {
            Ok(to) => self.start(note, to),
            Err(e) => {
                note.kev.flags |= EV_ERROR;
                note.kev.data = i64::from(e.get());
            }
        }
    }

    fn process(&self, _kn: &Knote, note: &mut Note) -> Kevent {
        let kev = note.kev;
        if note.kev.flags & EV_CLEAR != 0 {
            note.kev.data = 0;
            note.kev.fflags = 0;
        }
        kev
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        let st = self.st.lock();
        st.armed
            .then(|| Box::new(Self::new(Some((st.next, st.to)))) as Box<dyn Filter>)
    }
}

// --- timerfd ---

/// `tfd_jumped`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jump {
    /// `TFD_NOJUMP`.
    None,
    /// `TFD_READ`: jumped, and read since.
    Read,
    /// `TFD_ZREAD`: jumped backwards; the next read gets 0.
    ZeroRead,
    /// `TFD_CANCELED`: jumped with `TFD_TIMER_CANCEL_ON_SET`.
    Canceled,
}

struct TfdState {
    /// When it fires next, in uptime (which a real-time deadline before
    /// boot is before: negative, as FreeBSD's `sbintime_t`), if armed;
    /// and its period.
    value: Option<i64>,
    interval: u64,
    timflags: u32,
    count: u64,
    expired: bool,
    /// The boot time it last saw (`tfd_boottim`).
    boottime: u64,
    jumped: Jump,
    atime: Timespec,
    mtime: Timespec,
}

/// A `timerfd`.
pub(crate) struct Timerfd {
    callout: Pin<Box<Callout>>,
    st: SpinLock<TfdState>,
    clock: u32,
    wait: WaitQueue,
    notify: Notify,
    ino: u64,
    birth: Timespec,
    uid: u32,
    gid: u32,
}

/// Every `timerfd`, for clock jumps (`timerfd_list`).
static TIMERFDS: SpinLock<Vec<Weak<Timerfd>>> = SpinLock::new(Vec::new());
static TFD_INO: AtomicU64 = AtomicU64::new(1);

impl Timerfd {
    /// Its clock, for process information (`timerfd_fill_kinfo`).
    pub(crate) fn clock(&self) -> u32 {
        self.clock
    }

    /// `kern_timerfd_create`'s object.
    pub(crate) fn new(id: u32, uid: u32, gid: u32) -> Result<Arc<Self>, Errno> {
        if !matches!(id, clock::REALTIME | clock::MONOTONIC | clock::UPTIME) {
            return Err(Errno::EINVAL);
        }
        let birth = crate::fs::now();
        let t = Arc::new(Self {
            callout: Callout::new(),
            st: SpinLock::new(TfdState {
                value: None,
                interval: 0,
                timflags: 0,
                count: 0,
                expired: false,
                boottime: boottime(),
                jumped: Jump::None,
                atime: Timespec::default(),
                mtime: Timespec::default(),
            }),
            clock: id,
            wait: WaitQueue::new(),
            notify: Notify::new(),
            ino: TFD_INO.fetch_add(1, Ordering::Relaxed),
            birth,
            uid,
            gid,
        });
        let mut all = TIMERFDS.lock();
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&t));
        Ok(t)
    }

    fn wakeup(&self) {
        self.wait.wake_all();
        self.notify.notify(0);
    }

    fn arm(&self, at: u64) {
        let me = core::ptr::from_ref(self).expose_provenance();
        self.callout.as_ref().reset_at(at, Self::expire, me);
    }

    /// `timerfd_expire`.
    fn expire(_s: &Sleepable, arg: usize) {
        // SAFETY: the callout is drained before the timerfd goes.
        let me = unsafe { &*core::ptr::with_exposed_provenance::<Self>(arg) };
        {
            let mut st = me.st.lock();
            let Some(value) = st.value else {
                return;
            };
            let t = now();
            let ti = i64::try_from(t).unwrap_or(i64::MAX);
            if ti < value {
                me.arm(value.cast_unsigned());
                return;
            }
            st.count += 1;
            st.expired = true;
            if st.interval != 0 {
                let mut next = t.saturating_add(st.interval);
                // Missed periods count too.
                let diff = (ti - value).cast_unsigned();
                st.count += diff / st.interval;
                next -= diff % st.interval;
                st.value = Some(i64::try_from(next).unwrap_or(i64::MAX));
                me.arm(next);
            } else {
                st.value = None;
            }
        }
        me.wakeup();
    }

    /// `timerfd_curval`.
    fn curval(st: &TfdState) -> Itimerspec {
        let value = st.value.map_or(0, |v| {
            u64::try_from(v.saturating_sub(i64::try_from(now()).unwrap_or(i64::MAX)))
                .unwrap_or(0)
                .max(1)
        });
        Itimerspec {
            it_interval: ns_ts(st.interval),
            it_value: ns_ts(value),
        }
    }

    pub(crate) fn gettime(&self) -> Itimerspec {
        Self::curval(&self.st.lock())
    }

    /// `kern_timerfd_settime`: the old value, or `ECANCELED` for an
    /// absolute real-time timer with an unread jump.
    pub(crate) fn settime(&self, flags: u32, new: &Itimerspec) -> Result<Itimerspec, Errno> {
        if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
            return Err(Errno::EINVAL);
        }
        let value = ts_ns(&new.it_value)?;
        let interval = ts_ns(&new.it_interval)?;
        let mut err = Ok(());
        let mut st = self.st.lock();
        st.mtime = crate::fs::now();
        st.timflags = flags;
        let old = Self::curval(&st);
        st.interval = interval;
        if value != 0 {
            let at = if flags & TFD_TIMER_ABSTIME == 0 {
                i128::from(value) + i128::from(now())
            } else {
                if self.clock == clock::REALTIME && st.jumped == Jump::Canceled {
                    err = Err(Errno::ECANCELED);
                }
                if self.clock == clock::REALTIME {
                    i128::from(value) - i128::from(st.boottime)
                } else {
                    i128::from(value)
                }
            };
            let at = i64::try_from(at).unwrap_or(i64::MAX);
            st.value = Some(at);
            self.arm(u64::try_from(at).unwrap_or(0));
        } else {
            st.value = None;
            self.callout.stop();
        }
        st.count = 0;
        st.expired = false;
        st.jumped = Jump::None;
        drop(st);
        err.map(|()| old)
    }

    /// `timerfd_read`.
    pub(crate) fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        if uio.resid() < 8 {
            return Err(Errno::EINVAL);
        }
        let n = loop {
            {
                let mut st = self.st.lock();
                st.atime = crate::fs::now();
                if matches!(st.jumped, Jump::ZeroRead | Jump::Canceled) {
                    let canceled = st.jumped == Jump::Canceled;
                    st.jumped = Jump::Read;
                    st.count = 0;
                    return if canceled {
                        Err(Errno::ECANCELED)
                    } else {
                        Ok(0)
                    };
                }
                st.jumped = Jump::None;
                if st.count > 0 {
                    break core::mem::take(&mut st.count);
                }
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _w = sched::wait_message("tfdrd");
            crate::signal::sleep(
                s,
                &self.wait,
                || {
                    let st = self.st.lock();
                    st.count > 0 || matches!(st.jumped, Jump::ZeroRead | Jump::Canceled)
                },
                None,
            )?;
        };
        uio.copy_out(&n.to_ne_bytes())
    }

    fn readable(&self) -> Option<u64> {
        let st = self.st.lock();
        (st.count > 0 && st.jumped != Jump::Read).then_some(st.count)
    }

    /// `timerfd_poll`.
    pub(crate) fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if let Some(w) = w {
            self.notify.record(w);
        }
        if events & (POLLIN | POLLRDNORM) != 0 && self.readable().is_some() {
            events & (POLLIN | POLLRDNORM)
        } else {
            0
        }
    }

    /// `timerfd_kqfilter`.
    pub(crate) fn kqfilter(self: &Arc<Self>, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        if kev.filter != EVFILT_READ {
            return Err(Errno::EINVAL);
        }
        Ok(Box::new(TimerfdFilter(Arc::clone(self))))
    }

    /// `timerfd_stat`.
    pub(crate) fn stat(&self) -> Stat {
        let st = self.st.lock();
        Stat {
            st_nlink: 1,
            st_uid: self.uid,
            st_gid: self.gid,
            st_blksize: i32::try_from(vm::PAGE_SIZE).unwrap_or(4096),
            st_atim: st.atime,
            st_mtim: st.mtime,
            st_ctim: st.mtime,
            st_ino: self.ino,
            st_birthtim: self.birth,
            ..Stat::default()
        }
    }

    /// The real-time clock was set (`timerfd_jumped`).
    fn jumped(&self, bt: u64) {
        let mut st = self.st.lock();
        if self.clock != clock::REALTIME
            || st.timflags & TFD_TIMER_ABSTIME == 0
            || st.boottime == bt
        {
            return;
        }
        if let Some(value) = st.value {
            if st.timflags & TFD_TIMER_CANCEL_ON_SET != 0 {
                st.jumped = Jump::Canceled;
            } else if bt < st.boottime {
                st.jumped = Jump::ZeroRead;
            }
            // Not inside the interval loop: the deadline moves with the
            // clock.
            if !st.expired {
                let v = i128::from(value) - (i128::from(bt) - i128::from(st.boottime));
                let v = i64::try_from(v).unwrap_or(i64::MAX);
                st.value = Some(v);
                if self.callout.stop() {
                    self.arm(u64::try_from(v).unwrap_or(0));
                }
            }
        }
        st.boottime = bt;
        let wake = matches!(st.jumped, Jump::ZeroRead | Jump::Canceled);
        drop(st);
        if wake {
            self.wakeup();
        }
    }
}

/// The real-time clock was set: absolute real-time timers move.
pub(crate) fn clock_jumped() {
    let bt = boottime();
    let all: Vec<Arc<Timerfd>> = TIMERFDS.lock().iter().filter_map(Weak::upgrade).collect();
    for t in all {
        t.jumped(bt);
    }
}

/// `timerfd_rfiltops`.
struct TimerfdFilter(Arc<Timerfd>);

impl Filter for TimerfdFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.0.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.0.notify.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let st = self.0.st.lock();
        note.kev.data = st.count.cast_signed();
        st.count > 0 && st.jumped != Jump::Read
    }
}

// --- Interval and POSIX timers ---

/// Timer ids a process may have (`TIMER_MAX`); 0 to 2 are the interval
/// timers.
pub(crate) const TIMER_MAX: usize = 32;

/// How a POSIX timer tells of its expiry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sigev {
    pub(crate) notify: i32,
    pub(crate) signo: u32,
    pub(crate) value: u64,
    pub(crate) tid: i32,
}

struct PosixState {
    /// When it fires next on its clock (0: disarmed), and its period.
    value: u64,
    interval: u64,
    overrun: i32,
    overrun_last: i32,
}

/// A POSIX timer (`struct itimer`).
struct PosixTimer {
    callout: Pin<Box<Callout>>,
    p: Weak<Process>,
    id: i32,
    clock: u32,
    sigev: Sigev,
    st: SpinLock<PosixState>,
}

impl PosixTimer {
    fn arm(&self, value: u64) {
        let Ok(at) = uptime_of(self.clock, value) else {
            return;
        };
        let me = core::ptr::from_ref(self).expose_provenance();
        self.callout.as_ref().reset_at(at, Self::expire, me);
    }

    /// `realtimer_expire_l`.
    fn expire(_s: &Sleepable, arg: usize) {
        // SAFETY: the callout is drained before the timer goes.
        let me = unsafe { &*core::ptr::with_exposed_provenance::<Self>(arg) };
        let Ok(cts) = crate::syscall::clock_ns(me.clock) else {
            return;
        };
        {
            let mut st = me.st.lock();
            if st.value == 0 {
                return;
            }
            if cts < st.value {
                me.arm(st.value);
                return;
            }
            if st.interval != 0 {
                st.value = st.value.saturating_add(st.interval);
                if cts >= st.value {
                    let missed = (cts - st.value + 1).div_ceil(st.interval);
                    st.overrun = st
                        .overrun
                        .saturating_add(i32::try_from(missed).unwrap_or(i32::MAX));
                    st.value = cts.saturating_add(st.interval) - (cts - st.value) % st.interval;
                }
                me.arm(st.value);
            } else {
                st.value = 0;
            }
        }
        me.fire();
    }

    /// `itimer_fire`: the signal, unless one from this timer still waits,
    /// which counts an overrun instead.
    fn fire(&self) {
        if self.sigev.notify == SIGEV_NONE {
            return;
        }
        let Some(p) = self.p.upgrade() else {
            return;
        };
        let target = if self.sigev.notify == SIGEV_THREAD_ID {
            match crate::uthread::find(self.sigev.tid) {
                Some(t) if Arc::ptr_eq(t.process(), &p) => Some(t),
                _ => {
                    let mut st = self.st.lock();
                    st.value = 0;
                    st.interval = 0;
                    drop(st);
                    self.callout.stop();
                    return;
                }
            }
        } else {
            None
        };
        if crate::signal::timer_queued(&p, self.id) {
            let mut st = self.st.lock();
            st.overrun = st.overrun.saturating_add(1);
            return;
        }
        let overrun = {
            let mut st = self.st.lock();
            st.overrun_last = st.overrun;
            core::mem::take(&mut st.overrun)
        };
        let mut info = crate::signal::user_info(self.sigev.signo, 0, abi::signal::code::SI_TIMER);
        info.si_pid = 0;
        info.si_value = self.sigev.value;
        info.si_trapno = self.id;
        info._reason[0] = overrun;
        let _ = match &target {
            Some(t) => crate::signal::post_thread(t, info),
            None => crate::signal::post(&p, info),
        };
        // The process may go with this, and the timer with it: nothing
        // of `self` is used after.
        drop(target);
        drop(p);
    }
}

/// One interval timer (`ITIMER_REAL`, `VIRTUAL`, `PROF`).
#[derive(Clone, Copy, Debug, Default)]
struct Itimer {
    /// When it fires next (0: disarmed): uptime for `ITIMER_REAL`, the
    /// process's CPU time for the others.
    value: u64,
    interval: u64,
}

struct TimersState {
    itimers: [Itimer; 3],
}

/// A process's timers, made on first use.
pub(crate) struct Timers {
    /// `p_itcallout`, and the CPU-time timers' poll.
    real: Pin<Box<Callout>>,
    cpu: Pin<Box<Callout>>,
    p: Weak<Process>,
    st: SpinLock<TimersState>,
    posix: SpinLock<[Option<Pin<Box<PosixTimer>>>; TIMER_MAX]>,
}

/// How often the CPU-time timers look at the time used, at most.
const CPU_POLL: u64 = 10_000_000;

/// FreeBSD's `tick`, in nanoseconds (`hz` = 1000).
const TICK: u64 = 1_000_000;

impl Timers {
    pub(crate) fn new(p: &Arc<Process>) -> Box<Self> {
        Box::new(Self {
            real: Callout::new(),
            cpu: Callout::new(),
            p: Arc::downgrade(p),
            st: SpinLock::new(TimersState {
                itimers: [Itimer::default(); 3],
            }),
            posix: SpinLock::new([const { None }; TIMER_MAX]),
        })
    }

    fn cpu_now(p: &Process) -> u64 {
        crate::time::ticks_to_ns(p.runtime())
    }

    /// `kern_getitimer`: the time left and the period.
    pub(crate) fn getitimer(&self, which: usize) -> (u64, u64) {
        let it = self.st.lock().itimers[which];
        if it.value == 0 {
            return (0, it.interval);
        }
        let base = if which == 0 {
            now()
        } else {
            self.p.upgrade().map_or(0, |p| Self::cpu_now(&p))
        };
        (it.value.saturating_sub(base), it.interval)
    }

    /// `kern_setitimer`: sets `which` to fire in `value` ns, then every
    /// `interval`; returns the old setting as [`Timers::getitimer`] does.
    pub(crate) fn setitimer(&self, which: usize, value: u64, interval: u64) -> (u64, u64) {
        let old = self.getitimer(which);
        let mut st = self.st.lock();
        let base = if which == 0 {
            now()
        } else {
            self.p.upgrade().map_or(0, |p| Self::cpu_now(&p))
        };
        let (mut value, mut interval) = (value, interval);
        if which != 0 {
            // The CPU-time timers count in ticks.
            if value != 0 && value < TICK {
                value = TICK;
            }
            if interval != 0 && interval < TICK {
                interval = TICK;
            }
        }
        st.itimers[which] = Itimer {
            value: if value == 0 {
                0
            } else {
                base.saturating_add(value)
            },
            interval: if value == 0 { 0 } else { interval },
        };
        let me = core::ptr::from_ref(self).expose_provenance();
        if which == 0 {
            if value == 0 {
                self.real.stop();
            } else {
                self.real
                    .as_ref()
                    .reset_at(st.itimers[0].value, Self::real_expire, me);
            }
        } else if value != 0 {
            self.cpu
                .as_ref()
                .reset(value.min(CPU_POLL), Self::cpu_expire, me);
        }
        old
    }

    /// `realitexpire`: `SIGALRM`, and the next period, skipping those
    /// missed.
    fn real_expire(_s: &Sleepable, arg: usize) {
        // SAFETY: the callout is drained before the timers go.
        let me = unsafe { &*core::ptr::with_exposed_provenance::<Self>(arg) };
        let Some(p) = me.p.upgrade() else {
            return;
        };
        let fire = {
            let mut st = me.st.lock();
            let it = &mut st.itimers[0];
            let t = now();
            if it.value == 0 {
                false
            } else if t < it.value {
                me.real.as_ref().reset_at(it.value, Self::real_expire, arg);
                false
            } else {
                if it.interval == 0 {
                    it.value = 0;
                } else {
                    while it.value <= t {
                        it.value += it.interval;
                    }
                    me.real.as_ref().reset_at(it.value, Self::real_expire, arg);
                }
                true
            }
        };
        if fire {
            let _ = crate::signal::post(
                &p,
                crate::signal::user_info(abi::signal::SIGALRM, 0, abi::signal::code::SI_KERNEL),
            );
        }
        drop(p);
    }

    /// The CPU-time timers' poll: `SIGVTALRM` and `SIGPROF` once the
    /// process has used their time (FreeBSD counts them down in the
    /// statistics clock; these count all CPU time, not user time apart).
    fn cpu_expire(_s: &Sleepable, arg: usize) {
        // SAFETY: as for `real_expire`.
        let me = unsafe { &*core::ptr::with_exposed_provenance::<Self>(arg) };
        let Some(p) = me.p.upgrade() else {
            return;
        };
        let used = Self::cpu_now(&p);
        let mut fire = [false; 2];
        let mut next = u64::MAX;
        {
            let mut st = me.st.lock();
            for (i, f) in fire.iter_mut().enumerate() {
                let it = &mut st.itimers[i + 1];
                if it.value == 0 {
                    continue;
                }
                if used >= it.value {
                    *f = true;
                    it.value = if it.interval == 0 {
                        0
                    } else {
                        used.saturating_add(it.interval)
                    };
                }
                if it.value != 0 {
                    next = next.min(it.value - used.min(it.value));
                }
            }
            if next != u64::MAX {
                me.cpu
                    .as_ref()
                    .reset(next.clamp(1, CPU_POLL), Self::cpu_expire, arg);
            }
        }
        for (i, sig) in [abi::signal::SIGVTALRM, abi::signal::SIGPROF]
            .into_iter()
            .enumerate()
        {
            if fire[i] {
                let _ = crate::signal::post(
                    &p,
                    crate::signal::user_info(sig, 0, abi::signal::code::SI_KERNEL),
                );
            }
        }
        drop(p);
    }

    /// `kern_ktimer_create`: the new timer's id.
    pub(crate) fn create(&self, clock_id: u32, sigev: Option<Sigev>) -> Result<i32, Errno> {
        if !matches!(
            clock_id,
            clock::REALTIME | clock::MONOTONIC | clock::UPTIME | clock::TAI
        ) {
            return Err(Errno::EINVAL);
        }
        if let Some(e) = &sigev {
            if !matches!(e.notify, SIGEV_NONE | SIGEV_SIGNAL | SIGEV_THREAD_ID) {
                return Err(Errno::EINVAL);
            }
            if matches!(e.notify, SIGEV_SIGNAL | SIGEV_THREAD_ID)
                && !(1..=abi::signal::SIG_MAXSIG).contains(&e.signo)
            {
                return Err(Errno::EINVAL);
            }
        }
        let mut all = self.posix.lock();
        let id = (3..TIMER_MAX)
            .find(|&i| all[i].is_none())
            .ok_or(Errno::EAGAIN)?;
        let id32 = i32::try_from(id).unwrap_or(0);
        let sigev = sigev.unwrap_or(Sigev {
            notify: SIGEV_SIGNAL,
            signo: abi::signal::SIGALRM,
            value: u64::from(id32.cast_unsigned()),
            tid: 0,
        });
        all[id] = Some(Box::pin(PosixTimer {
            callout: Callout::new(),
            p: Weak::clone(&self.p),
            id: id32,
            clock: clock_id,
            sigev,
            st: SpinLock::new(PosixState {
                value: 0,
                interval: 0,
                overrun: 0,
                overrun_last: 0,
            }),
        }));
        Ok(id32)
    }

    fn with_timer<R>(&self, id: i32, f: impl FnOnce(&PosixTimer) -> R) -> Result<R, Errno> {
        let i = usize::try_from(id)
            .ok()
            .filter(|&i| (3..TIMER_MAX).contains(&i));
        let all = self.posix.lock();
        let t = i.and_then(|i| all[i].as_ref()).ok_or(Errno::EINVAL)?;
        Ok(f(t))
    }

    /// `kern_ktimer_delete`.
    pub(crate) fn delete(&self, id: i32) -> Result<(), Errno> {
        let i = usize::try_from(id)
            .ok()
            .filter(|&i| i < TIMER_MAX)
            .ok_or(Errno::EINVAL)?;
        let t = self.posix.lock()[i].take().ok_or(Errno::EINVAL)?;
        // Dropping it drains its callout.
        drop(t);
        Ok(())
    }

    /// `realtimer_gettime`.
    pub(crate) fn gettime(&self, id: i32) -> Result<Itimerspec, Errno> {
        self.with_timer(id, |t| {
            let cts = crate::syscall::clock_ns(t.clock).unwrap_or(0);
            let st = t.st.lock();
            let value = if st.value == 0 {
                0
            } else {
                st.value.saturating_sub(cts).max(1)
            };
            Itimerspec {
                it_interval: ns_ts(st.interval),
                it_value: ns_ts(value),
            }
        })
    }

    /// `realtimer_settime`: the old value.
    pub(crate) fn settime(
        &self,
        id: i32,
        flags: u32,
        val: &Itimerspec,
    ) -> Result<Itimerspec, Errno> {
        let fix = |ts: &Timespec| -> Result<u64, Errno> {
            let ns = ts_ns(ts)?;
            Ok(if ts.tv_sec == 0 && ns != 0 && ns < TICK {
                TICK
            } else {
                ns
            })
        };
        let value = fix(&val.it_value)?;
        let interval = if value != 0 {
            fix(&val.it_interval)?
        } else {
            0
        };
        let old = self.gettime(id)?;
        self.with_timer(id, |t| {
            let cts = crate::syscall::clock_ns(t.clock)?;
            let mut st = t.st.lock();
            st.interval = interval;
            if value != 0 {
                st.value = if flags & abi::event::TIMER_ABSTIME == 0 {
                    cts.saturating_add(value)
                } else {
                    value
                };
                t.arm(st.value);
            } else {
                st.value = 0;
                t.callout.stop();
            }
            Ok(old)
        })?
    }

    /// `kern_ktimer_getoverrun`.
    pub(crate) fn getoverrun(&self, id: i32) -> Result<i32, Errno> {
        self.with_timer(id, |t| t.st.lock().overrun_last)
    }

    /// `itimers_exit`: everything goes.
    pub(crate) fn exit(&self) {
        for which in 0..3 {
            self.setitimer(which, 0, 0);
        }
        self.real.drain();
        self.cpu.drain();
        self.exec();
    }

    /// `itimers_exec`: POSIX timers go; interval timers stay.
    pub(crate) fn exec(&self) {
        let gone: Vec<_> = self
            .posix
            .lock()
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        drop(gone);
    }
}
