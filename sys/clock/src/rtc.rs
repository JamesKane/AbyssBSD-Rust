// SPDX-License-Identifier: BSD-2-Clause
//! Registered time-of-day clocks (FreeBSD's `subr_rtc.c`).
//!
//! - A clock registers with its resolution and flags ([`register`]); the
//!   list stays sorted by resolution, a new clock going after those as
//!   fine as it, and each registration prints FreeBSD's "registered as a
//!   time-of-day clock" line.
//! - [`read`] (`inittodr`) asks them in that order and takes the first
//!   answer, adjusted by half the clock's resolution (a clock that
//!   truncates to whole seconds is, on average, half a second behind)
//!   and by [`utc_offset`], unless the clock asked not to be.
//! - [`resettodr`] writes the system time back to every clock, unless
//!   `machdep.disable_rtc_set` says not to: on the `rtc` thread, as
//!   FreeBSD's settime tasks run on `taskqueue_thread`, so a clock's
//!   driver may sleep. A clock that asked to be written at an offset into
//!   the second ([`schedule`]) is written then. The time written is
//!   [`utc_offset`] behind and half the resolution ahead, unless the
//!   clock takes it raw ([`flags::SETTIME_NO_ADJ`]).
//! - `machdep.adjkerntz`, `machdep.wall_cmos_clock`, and
//!   `machdep.disable_rtc_set` are the kernel's sysctls over [`ADJKERNTZ`],
//!   [`WALL_CMOS_CLOCK`], and [`DISABLE_RTC_SET`]; `debug.clock_show_io`
//!   over [`SHOW_IO`] makes the drivers print what they read and write.
//!
//! Behavior reference: sys/kern/subr_rtc.c, sys/kern/subr_clock.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use abi::errno::Errno;
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};

use crate::DateTime;
use crate::bcd::Bcd;

/// `CLOCKF_*` (`sys/clock.h`).
pub mod flags {
    /// The clock is written with a zero time and gets the time itself
    /// (implies [`SETTIME_NO_ADJ`]).
    pub const SETTIME_NO_TS: u32 = 0x1;
    /// The time written is the system's, unadjusted: the driver applies
    /// `utc_offset` and any rounding.
    pub const SETTIME_NO_ADJ: u32 = 0x2;
    /// The time read is taken as it is.
    pub const GETTIME_NO_ADJ: u32 = 0x4;
}

/// `CLOCK_DBG_READ`, `CLOCK_DBG_WRITE`.
pub const DBG_READ: i32 = 0x1;
/// See [`DBG_READ`].
pub const DBG_WRITE: i32 = 0x2;

/// `machdep.adjkerntz`: the local offset from UTC, in seconds.
pub static ADJKERNTZ: AtomicI32 = AtomicI32::new(0);
/// `machdep.wall_cmos_clock`: the clocks keep local time.
pub static WALL_CMOS_CLOCK: AtomicI32 = AtomicI32::new(0);
/// `machdep.disable_rtc_set`: [`resettodr`] writes nothing.
pub static DISABLE_RTC_SET: AtomicI32 = AtomicI32::new(0);
/// `debug.clock_show_io`: print reads (1), writes (2), or both (3).
pub static SHOW_IO: AtomicI32 = AtomicI32::new(0);

/// A time since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timespec {
    /// Seconds.
    pub sec: i64,
    /// Nanoseconds, below 1e9.
    pub nsec: u32,
}

impl Timespec {
    const NANOS: u32 = 1_000_000_000;

    /// From nanoseconds since the epoch.
    #[must_use]
    pub fn from_ns(ns: u64) -> Self {
        Self {
            sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
            nsec: u32::try_from(ns % 1_000_000_000).unwrap_or(0),
        }
    }

    /// In nanoseconds since the epoch, if not before it.
    #[must_use]
    pub fn as_ns(&self) -> Option<u64> {
        u64::try_from(self.sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::from(self.nsec))
    }

    /// `timespecadd` of `ns` nanoseconds.
    #[must_use]
    pub fn plus_ns(self, ns: u32) -> Self {
        let n = self.nsec + ns % Self::NANOS;
        let carry = i64::from(ns / Self::NANOS) + i64::from(n / Self::NANOS);
        Self {
            sec: self.sec.saturating_add(carry),
            nsec: n % Self::NANOS,
        }
    }

    /// The calendar time, if not before the epoch.
    #[must_use]
    pub fn to_datetime(&self) -> Option<DateTime> {
        Some(DateTime::from_unix(
            u64::try_from(self.sec).ok()?,
            self.nsec,
        ))
    }

    /// A calendar time's, if it is valid.
    #[must_use]
    pub fn of(t: &DateTime) -> Option<Self> {
        Some(Self {
            sec: i64::try_from(t.unix_seconds()?).ok()?,
            nsec: t.nanosecond,
        })
    }
}

/// A battery-backed clock's driver (FreeBSD's `clock_if.m`).
pub trait RealTimeClock: Send + Sync {
    /// Its name (`rx88030`).
    fn name(&self) -> &str;

    /// `CLOCK_GETTIME`: the time it keeps.
    ///
    /// # Errors
    ///
    /// Whatever stopped the read: `EINVAL` for a time that is not one, an
    /// I/O error from its bus.
    fn gettime(&self, s: &Sleepable) -> Result<DateTime, Errno>;

    /// `CLOCK_SETTIME`: sets it to `ts`, adjusted as its flags say.
    ///
    /// # Errors
    ///
    /// Whatever stopped the write; `ENODEV` if it cannot be set.
    fn settime(&self, s: &Sleepable, ts: Timespec) -> Result<(), Errno> {
        let _ = (s, ts);
        Err(Errno::ENODEV)
    }
}

/// What the kernel gives the clocks.
pub trait Ops: Sync {
    /// The system time in nanoseconds since the epoch (`getnanotime`),
    /// if it is known.
    fn realtime_ns(&self) -> Option<u64>;
    /// Waits until `cond` holds or `ns` nanoseconds pass, on `q`.
    fn wait_timeout(&self, s: &Sleepable, q: &WaitQueue, cond: &mut dyn FnMut() -> bool, ns: u64);
    /// A console line.
    fn log(&self, args: core::fmt::Arguments<'_>);
    /// `bootverbose`.
    fn bootverbose(&self) -> bool;
}

/// A registered clock (`struct rtc_instance`).
struct Instance {
    clock: Arc<dyn RealTimeClock>,
    /// In microseconds.
    resolution: u32,
    flags: u32,
    /// Nanoseconds into the second to write it at; 0 for at once.
    schedns: AtomicU32,
    /// A write is due ([`resettodr`]).
    pending: AtomicBool,
    /// Its write is running.
    busy: AtomicBool,
    /// Unregistered: no more writes.
    gone: AtomicBool,
}

impl Instance {
    /// Half the resolution (`resadj`), in nanoseconds.
    fn resadj_ns(&self) -> u32 {
        self.resolution / 2 * 1000
    }
}

static OPS: Once<&'static dyn Ops> = Once::new();
static CLOCKS: SpinLock<Vec<Arc<Instance>>> = SpinLock::new(Vec::new());
/// The `rtc` thread waits here for writes, and [`drain`] and
/// [`unregister`] for them to finish.
static WORK: WaitQueue = WaitQueue::new();
static DONE: WaitQueue = WaitQueue::new();
/// `debug.clock_do_io=1` asked for every clock to be read.
static READ_ALL: AtomicBool = AtomicBool::new(false);

/// A console line, through the kernel once [`init`] has run (a clock
/// driver's `device_printf`); nothing before.
pub fn log(args: core::fmt::Arguments<'_>) {
    if let Some(o) = OPS.get() {
        o.log(args);
    }
}

/// Starts the `rtc` thread, which writes the clocks.
///
/// # Errors
///
/// If the thread cannot start.
pub fn init(ops: &'static dyn Ops) -> Result<(), sched::SpawnError> {
    if OPS.set(ops).is_err() {
        return Ok(());
    }
    sched::spawn("rtc", sched::Priority::KERNEL, settime_thread).map(|_| ())
}

/// `utc_offset`: how far the clocks' time is ahead of UTC, in seconds
/// (`machdep.adjkerntz` while `machdep.wall_cmos_clock` is set).
#[must_use]
pub fn utc_offset() -> i64 {
    if WALL_CMOS_CLOCK.load(Ordering::Relaxed) != 0 {
        i64::from(ADJKERNTZ.load(Ordering::Relaxed))
    } else {
        0
    }
}

/// `clock_register_flags`: adds `clock`, `resolution` microseconds per
/// tick, in order of resolution, and prints that it did.
pub fn register(clock: Arc<dyn RealTimeClock>, resolution: u32, flags: u32) {
    let name: String = clock.name().into();
    let inst = Arc::new(Instance {
        clock,
        resolution,
        flags,
        schedns: AtomicU32::new(0),
        pending: AtomicBool::new(false),
        busy: AtomicBool::new(false),
        gone: AtomicBool::new(false),
    });
    {
        let mut v = CLOCKS.lock();
        let at = v
            .iter()
            .position(|c| c.resolution > resolution)
            .unwrap_or(v.len());
        v.insert(at, inst);
    }
    log(format_args!(
        "{name}: registered as a time-of-day clock, resolution {}.{:06}s",
        resolution / 1_000_000,
        resolution % 1_000_000
    ));
}

fn find(clock: &Arc<dyn RealTimeClock>) -> Option<Arc<Instance>> {
    CLOCKS
        .lock()
        .iter()
        .find(|c| Arc::ptr_eq(&c.clock, clock))
        .cloned()
}

/// `clock_schedule`: writes to `clock` happen `offset_ns` into a second.
pub fn schedule(clock: &Arc<dyn RealTimeClock>, offset_ns: u32) {
    if let Some(c) = find(clock) {
        c.schedns.store(offset_ns, Ordering::Relaxed);
    }
}

/// `clock_unregister`: removes `clock`, cancelling a write not begun and
/// waiting for one under way.
pub fn unregister(s: &Sleepable, clock: &Arc<dyn RealTimeClock>) {
    let inst = {
        let mut v = CLOCKS.lock();
        let at = v.iter().position(|c| Arc::ptr_eq(&c.clock, clock));
        at.map(|i| v.remove(i))
    };
    let Some(inst) = inst else { return };
    // SeqCst with the writer's `busy` store and `gone` load: either it sees
    // `gone`, or this sees it `busy` and waits.
    inst.gone.store(true, Ordering::SeqCst);
    inst.pending.store(false, Ordering::Release);
    if let Some(o) = OPS.get() {
        while inst.busy.load(Ordering::SeqCst) {
            o.wait_timeout(
                s,
                &DONE,
                &mut || !inst.busy.load(Ordering::Acquire),
                10_000_000,
            );
        }
    }
}

/// Prints a driver's read or write (`clock_dbgprint_*`) when
/// `debug.clock_show_io` asks for it: "`name`: read  at `now`: `what`".
pub fn dbgprint(name: &str, rw: i32, what: &dyn core::fmt::Display) {
    if SHOW_IO.load(Ordering::Relaxed) & rw == 0 {
        return;
    }
    let now = OPS
        .get()
        .and_then(|o| o.realtime_ns())
        .map(Timespec::from_ns)
        .and_then(|t| t.to_datetime())
        .unwrap_or(DateTime::from_unix(0, 0));
    log(format_args!(
        "{name}: {} at {:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:09}: {what}",
        if rw & DBG_READ != 0 { "read " } else { "write" },
        now.year,
        now.month,
        now.day,
        now.hour,
        now.minute,
        now.second,
        now.nanosecond
    ));
}

/// [`dbgprint`] of a BCD time.
pub fn dbgprint_bcd(name: &str, rw: i32, b: &Bcd) {
    dbgprint(name, rw, b);
}

/// [`dbgprint`] of an error number.
pub fn dbgprint_err(name: &str, rw: i32, e: Errno) {
    dbgprint(name, rw, &format_args!("error = {}", e.get()));
}

/// One clock's answer, adjusted as its flags say.
fn adjusted(c: &Instance, s: &Sleepable) -> Result<Timespec, Errno> {
    let t = c.clock.gettime(s)?;
    let mut ts = Timespec::of(&t).ok_or(Errno::EINVAL)?;
    if c.flags & flags::GETTIME_NO_ADJ == 0 {
        ts = ts.plus_ns(c.resadj_ns());
        ts.sec = ts.sec.saturating_add(utc_offset());
    }
    if ts.sec < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(ts)
}

/// `read_clocks` for `inittodr`: the first registered clock's time, as a
/// system time, with its name; `ENXIO` with no clock registered, or the
/// last clock's error.
///
/// # Errors
///
/// As above; [`inittodr_failure`] says how FreeBSD reports each.
pub fn read(s: &Sleepable) -> Result<(String, Timespec), Errno> {
    let clocks = CLOCKS.lock().clone();
    let mut err = Errno::ENXIO;
    for c in &clocks {
        match adjusted(c, s) {
            Ok(ts) => {
                if OPS.get().is_some_and(|o| o.bootverbose()) {
                    log(format_args!(
                        "{}: providing initial system time",
                        c.clock.name()
                    ));
                }
                return Ok((c.clock.name().into(), ts));
            }
            Err(e) => err = e,
        }
    }
    Err(err)
}

/// `inittodr`'s warning when no clock gave the time.
#[must_use]
pub fn inittodr_failure(e: Errno) -> String {
    let what = match e {
        Errno::ENXIO => String::from("Warning: no time-of-day clock registered, "),
        Errno::EINVAL => String::from("Warning: bad time from time-of-day clock, "),
        e => alloc::format!("Error reading time-of-day clock ({}), ", e.get()),
    };
    what + "system time will not be set accurately"
}

/// Every registered clock's answer, unadjusted, in order (diagnostics,
/// `debug.clock_do_io=1`, and tests).
#[must_use]
pub fn read_all(s: &Sleepable) -> Vec<(String, Result<DateTime, Errno>)> {
    let clocks = CLOCKS.lock().clone();
    clocks
        .iter()
        .map(|c| (String::from(c.clock.name()), c.clock.gettime(s)))
        .collect()
}

/// `debug.clock_do_io=1`: every clock is read, on the `rtc` thread, and
/// the answers dropped (a driver's `debug.clock_show_io` lines show
/// them).
pub fn read_discard() {
    READ_ALL.store(true, Ordering::Release);
    WORK.wake_all();
}

/// The registered clocks' names and resolutions, in order.
#[must_use]
pub fn registered() -> Vec<(String, u32)> {
    CLOCKS
        .lock()
        .iter()
        .map(|c| (String::from(c.clock.name()), c.resolution))
        .collect()
}

/// `resettodr`: writes the system time to every clock, unless
/// `machdep.disable_rtc_set`; the writes happen on the `rtc` thread.
pub fn resettodr() {
    if DISABLE_RTC_SET.load(Ordering::Relaxed) != 0 {
        return;
    }
    let clocks = CLOCKS.lock().clone();
    if clocks.is_empty() {
        return;
    }
    for c in &clocks {
        c.pending.store(true, Ordering::Release);
    }
    WORK.wake_all();
}

/// Waits up to `ns` nanoseconds for the writes [`resettodr`] asked for to
/// finish (at shutdown, before the machine goes down under them).
pub fn drain(s: &Sleepable, ns: u64) {
    let Some(o) = OPS.get() else { return };
    let idle = || {
        CLOCKS
            .lock()
            .iter()
            .all(|c| !c.pending.load(Ordering::Acquire) && !c.busy.load(Ordering::Acquire))
    };
    o.wait_timeout(s, &DONE, &mut || idle(), ns);
}

/// Nanoseconds from `now` to the next `schedns` into a second.
fn wait_for(schedns: u32, now_ns: u64) -> u64 {
    let into = now_ns % 1_000_000_000;
    let at = u64::from(schedns);
    if at >= into {
        at - into
    } else {
        at + 1_000_000_000 - into
    }
}

/// `settime_task_func` for each pending clock, soonest first.
fn settime_thread(s: &Sleepable) {
    let Some(ops) = OPS.get() else { return };
    loop {
        let any = || {
            READ_ALL.load(Ordering::Acquire)
                || CLOCKS
                    .lock()
                    .iter()
                    .any(|c| c.pending.load(Ordering::Acquire))
        };
        WORK.wait_until(s, any);
        if READ_ALL.swap(false, Ordering::AcqRel) && read_all(s).is_empty() {
            log(format_args!("No registered RTC clocks"));
        }
        loop {
            let now = ops.realtime_ns().unwrap_or(0);
            let due = CLOCKS
                .lock()
                .iter()
                .filter(|c| c.pending.load(Ordering::Acquire))
                .min_by_key(|c| match c.schedns.load(Ordering::Relaxed) {
                    0 => 0,
                    n => wait_for(n, now),
                })
                .cloned();
            let Some(c) = due else { break };
            let at = c.schedns.load(Ordering::Relaxed);
            if at != 0 {
                let wait = wait_for(at, now);
                let q = WaitQueue::new();
                ops.wait_timeout(s, &q, &mut || false, wait);
            }
            // SeqCst: see `unregister`.
            c.busy.store(true, Ordering::SeqCst);
            if c.pending.swap(false, Ordering::AcqRel) && !c.gone.load(Ordering::SeqCst) {
                settime(*ops, &c, s);
            }
            c.busy.store(false, Ordering::Release);
            DONE.wake_all();
        }
    }
}

/// One clock's write.
fn settime(ops: &dyn Ops, c: &Instance, s: &Sleepable) {
    let Some(now) = ops.realtime_ns() else { return };
    let ts = if c.flags & flags::SETTIME_NO_TS != 0 {
        Timespec::default()
    } else {
        let mut ts = Timespec::from_ns(now);
        if c.flags & flags::SETTIME_NO_ADJ == 0 {
            ts.sec = ts.sec.saturating_sub(utc_offset());
            ts = ts.plus_ns(c.resadj_ns());
        }
        ts
    };
    if let Err(e) = c.clock.settime(s, ts)
        && ops.bootverbose()
    {
        log(format_args!(
            "{}: CLOCK_SETTIME error {}",
            c.clock.name(),
            e.get()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespecs() {
        let t = Timespec::from_ns(1_792_326_896_999_999_999);
        assert_eq!((t.sec, t.nsec), (1_792_326_896, 999_999_999));
        assert_eq!(
            t.plus_ns(500_000_000),
            Timespec {
                sec: 1_792_326_897,
                nsec: 499_999_999
            }
        );
        assert_eq!(t.as_ns(), Some(1_792_326_896_999_999_999));
        assert_eq!(Timespec { sec: -1, nsec: 0 }.as_ns(), None);
        // A write one nanosecond into the second, from just before it and
        // just after.
        assert_eq!(wait_for(1, 5_999_999_999), 2);
        assert_eq!(wait_for(1, 6_000_000_002), 999_999_999);
        assert_eq!(wait_for(999_999_000, 6_000_000_000), 999_999_000);
    }

    #[test]
    fn failures_read_as_freebsd_prints_them() {
        assert_eq!(
            inittodr_failure(Errno::EIO),
            "Error reading time-of-day clock (5), system time will not be set accurately"
        );
        assert!(inittodr_failure(Errno::ENXIO).starts_with("Warning: no time-of-day clock"));
    }
}
