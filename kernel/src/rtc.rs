// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of the time-of-day clocks ([`clock::rtc`]): what the
//! clocks need from it, their sysctls, the periodic write-back, and the
//! write at shutdown.
//!
//! - `machdep.adjkerntz` (writing it writes the clocks, as `adjkerntz(8)`
//!   expects), `machdep.wall_cmos_clock`, `machdep.disable_rtc_set`
//!   (`subr_clock.c`, `subr_rtc.c`).
//! - `debug.clock_show_io` and `debug.clock_do_io` (1 reads every clock
//!   and discards it, 2 writes them).
//! - `machdep.rtc_save_period` (`kern_ntptime.c`): every that many seconds,
//!   and at shutdown, the clocks are written while NTP says the time is
//!   good; with nothing disciplining the clock (no `ntpd`), it never is.
//!
//! Behavior reference: sys/kern/subr_rtc.c, sys/kern/subr_clock.c,
//! sys/kern/kern_ntptime.c.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::sysctl::ctlflag;
use sched::{Sleepable, WaitQueue};
use sysctl::{Number, Value};

/// `machdep.rtc_save_period`, in seconds (0: never).
static SAVE_PERIOD: AtomicI32 = AtomicI32::new(1800);
/// The periodic writer waits here; a new period wakes it.
static PERIOD_CHANGED: WaitQueue = WaitQueue::new();
static PERIOD_GEN: AtomicI32 = AtomicI32::new(0);

struct Ops;

impl clock::rtc::Ops for Ops {
    fn realtime_ns(&self) -> Option<u64> {
        (crate::time::realtime_base_ns() != 0)
            .then(|| crate::shared_page::to_ns(crate::shared_page::realtime()))
    }

    fn wait_timeout(&self, s: &Sleepable, q: &WaitQueue, cond: &mut dyn FnMut() -> bool, ns: u64) {
        crate::time::wait_timeout(s, q, cond, ns);
    }

    fn log(&self, args: core::fmt::Arguments<'_>) {
        kprintln!("{args}");
    }

    fn bootverbose(&self) -> bool {
        bus::bootverbose()
    }
}

static OPS: Ops = Ops;

/// Starts the clocks' writer, before any clock registers.
pub(crate) fn init() {
    if clock::rtc::init(&OPS).is_err() {
        kprintln!("rtc: could not start the clock writer");
    }
}

/// The sysctls, and the periodic write-back.
pub(crate) fn sysctl_init(s: &Sleepable) {
    use clock::rtc::{ADJKERNTZ, DISABLE_RTC_SET, SHOW_IO, WALL_CMOS_CLOCK};
    crate::sysctl::with(s, |t| {
        let md = t.child(t.root(), "machdep").expect("machdep node");
        t.add_writable(
            md,
            Number::Auto,
            "disable_rtc_set",
            "Disallow adjusting time-of-day clock",
            0,
            Value::Int(Box::new(|| DISABLE_RTC_SET.load(Ordering::Relaxed))),
            sysctl::int_writer(|v| {
                DISABLE_RTC_SET.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("machdep.disable_rtc_set");
        t.add_writable(
            md,
            Number::Auto,
            "wall_cmos_clock",
            "Enables application of machdep.adjkerntz",
            0,
            Value::Int(Box::new(|| WALL_CMOS_CLOCK.load(Ordering::Relaxed))),
            sysctl::int_writer(|v| {
                WALL_CMOS_CLOCK.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("machdep.wall_cmos_clock");
        t.add_writable(
            md,
            Number::Auto,
            "adjkerntz",
            "Local offset from UTC in seconds",
            ctlflag::MPSAFE,
            Value::Int(Box::new(|| ADJKERNTZ.load(Ordering::Relaxed))),
            sysctl::int_writer(|v| {
                // `sysctl_machdep_adjkerntz`: a new offset is written to
                // the clocks.
                ADJKERNTZ.store(v, Ordering::Relaxed);
                clock::resettodr();
                Ok(())
            }),
        )
        .expect("machdep.adjkerntz");
        t.add_writable(
            md,
            Number::Auto,
            "rtc_save_period",
            "Save system time to RTC with this period (in seconds)",
            ctlflag::TUN | ctlflag::MPSAFE,
            Value::Int(Box::new(|| SAVE_PERIOD.load(Ordering::Relaxed))),
            sysctl::int_writer(|v| {
                SAVE_PERIOD.store(v, Ordering::Relaxed);
                PERIOD_GEN.fetch_add(1, Ordering::Release);
                PERIOD_CHANGED.wake_all();
                Ok(())
            }),
        )
        .expect("machdep.rtc_save_period");
        let debug = t.child(t.root(), "debug").expect("debug node");
        t.add_writable(
            debug,
            Number::Auto,
            "clock_show_io",
            "Enable debug printing of RTC clock I/O; 1=reads, 2=writes, 3=both.",
            ctlflag::TUN,
            Value::Int(Box::new(|| SHOW_IO.load(Ordering::Relaxed))),
            sysctl::int_writer(|v| {
                SHOW_IO.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("debug.clock_show_io");
        t.add_writable(
            debug,
            Number::Auto,
            "clock_do_io",
            "Trigger one-time IO on RTC clocks; 1=read (and discard), 2=write",
            ctlflag::MPSAFE,
            Value::int(0),
            sysctl::int_writer(|v| match v {
                clock::rtc::DBG_READ => {
                    clock::rtc::read_discard();
                    Ok(())
                }
                clock::rtc::DBG_WRITE => {
                    clock::resettodr();
                    Ok(())
                }
                _ => Err(abi::errno::Errno::EINVAL),
            }),
        )
        .expect("debug.clock_do_io");
    });
    if sched::spawn("rtc_save", sched::Priority::KERNEL, periodic).is_err() {
        kprintln!("rtc: could not start the periodic writer");
    }
}

/// `periodic_resettodr`: writes the clocks every `machdep.rtc_save_period`
/// seconds while NTP says the time is good.
fn periodic(s: &Sleepable) {
    loop {
        let g = PERIOD_GEN.load(Ordering::Acquire);
        let secs = SAVE_PERIOD.load(Ordering::Relaxed);
        if secs <= 0 {
            PERIOD_CHANGED.wait_until(s, || PERIOD_GEN.load(Ordering::Acquire) != g);
            continue;
        }
        let ns = u64::try_from(secs).unwrap_or(1) * 1_000_000_000;
        let changed = crate::time::wait_timeout(
            s,
            &PERIOD_CHANGED,
            || PERIOD_GEN.load(Ordering::Acquire) != g,
            ns,
        );
        if !changed && !crate::ntp::time_error() {
            clock::resettodr();
        }
    }
}

/// `shutdown_resettodr`: the clocks get the time before the machine goes
/// down, if it is good, with up to two seconds for the writes.
pub(crate) fn shutdown(s: &Sleepable) {
    if SAVE_PERIOD.load(Ordering::Relaxed) > 0 && !crate::ntp::time_error() {
        clock::resettodr();
        clock::drain(s, 2_000_000_000);
    }
}
