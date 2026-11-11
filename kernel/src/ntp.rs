// SPDX-License-Identifier: BSD-2-Clause
//! The NTP kernel interface: `ntp_adjtime`, `ntp_gettime`, `adjtime`'s
//! slew, and `kern.ntp_pll`, over the discipline in [`clock::ntp`]. The
//! timehands ([`crate::shared_page`]) call [`seconds`] as the time of day
//! passes each second and scale the counter by what it returns.
//!
//! Behavior reference: sys/kern/kern_ntptime.c.

use alloc::boxed::Box;

use abi::time::{Timespec, Timeval};
use abi::timex::{Ntptimeval, Timex};
use clock::ntp::Discipline;
use sched::Sleepable;
use sync::SpinLock;
use sysctl::{Number, Value};

/// The discipline (`ntp_lock` and what it guards). Taken with the
/// timehands' writer lock held, so never the other way round.
static NTP: SpinLock<Discipline> = SpinLock::new(Discipline::new());

/// The time of day reached second `*sec`, `count` seconds after the last
/// one handled (FreeBSD's loop around `ntp_update_second`): each may be
/// moved by a leap second, which moves the boot time `*boot_sec` with
/// it. Returns the rate adjustment for the coming second.
pub(crate) fn seconds(sec: &mut i64, boot_sec: &mut i64, count: i64) -> i64 {
    let mut d = NTP.lock();
    let mut adj = 0;
    for _ in 0..count {
        let was = *sec;
        adj = d.update_second(sec);
        if *sec != was {
            *boot_sec += *sec - was;
        }
    }
    adj
}

/// `ntp_is_time_error`: whether the discipline says the time is not to
/// be trusted (unsynchronized, as it is until `ntpd` says otherwise).
pub(crate) fn time_error() -> bool {
    Discipline::is_time_error(NTP.lock().status())
}

/// The TAI offset in seconds (0 until `ntpd` sets it).
pub(crate) fn tai() -> i64 {
    NTP.lock().tai()
}

/// The discipline's frequency (`l_fp` ns/s), slew left (us), and status,
/// for tests.
#[cfg(feature = "ktest")]
pub(crate) fn state() -> (i64, i64, i32) {
    let d = NTP.lock();
    (d.time_freq(), d.adjtime_left(), d.status())
}

/// The time of day as a timespec.
fn nanotime() -> Timespec {
    let ns = crate::shared_page::to_ns(crate::shared_page::realtime());
    Timespec {
        tv_sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from(ns % 1_000_000_000).unwrap_or(0),
    }
}

/// `ntp_gettime1`: the time with its error bounds and leap state.
pub(crate) fn gettime() -> Ntptimeval {
    let d = NTP.lock();
    let r = d.reading();
    Ntptimeval {
        time: nanotime(),
        maxerror: r.maxerror,
        esterror: r.esterror,
        tai: r.tai,
        time_state: r.state,
        _pad: 0,
    }
}

/// `kern_ntp_adjtime` once the caller's privilege is checked (any mode
/// needs `PRIV_NTP_ADJTIME`): returns the clock state.
pub(crate) fn adjtime_ntp(tx: &mut Timex) -> i32 {
    // `time_uptime`: whole seconds of the timehands' uptime.
    let uptime = crate::shared_page::uptime().sec;
    NTP.lock().adjust(tx, uptime)
}

/// `kern_adjtime` once the privilege is checked: sets the correction to
/// slew, if `delta` is given, and returns the one left before.
pub(crate) fn adjtime(delta: Option<&Timeval>) -> Timeval {
    let us = delta.map(|d| d.tv_sec.saturating_mul(1_000_000).saturating_add(d.tv_usec));
    let old = NTP.lock().set_adjtime(us);
    let mut tv = Timeval {
        tv_sec: old / 1_000_000,
        tv_usec: old % 1_000_000,
    };
    if tv.tv_usec < 0 {
        tv.tv_usec += 1_000_000;
        tv.tv_sec -= 1;
    }
    tv
}

// SAFETY: integers and a timespec, the tail padding an explicit field;
// any bits valid.
unsafe impl crate::uaccess::Plain for Ntptimeval {}
// SAFETY: integers, the padding explicit fields; any bits valid.
unsafe impl crate::uaccess::Plain for Timex {}

/// Adds `kern.ntp_pll` (with `PPS_SYNC`, as GENERIC has it).
pub(crate) fn sysctl_init(s: &Sleepable) {
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        let n = t
            .add_node(kern, Number::Auto, "ntp_pll", "")
            .expect("kern.ntp_pll");
        t.add_value(
            n,
            Number::Auto,
            "gettime",
            "",
            Value::Opaque {
                format: "S,ntptimeval",
                read: Box::new(|out| out.extend_from_slice(crate::uaccess::bytes_of(&gettime()))),
            },
        )
        .expect("kern.ntp_pll.gettime");
        t.add_writable(
            n,
            Number::Auto,
            "pps_shiftmax",
            "Max interval duration (sec) (shift)",
            0,
            Value::Int(Box::new(|| NTP.lock().pps_shiftmax())),
            sysctl::int_writer(|v| {
                NTP.lock().set_pps_shiftmax(v);
                Ok(())
            }),
        )
        .expect("kern.ntp_pll.pps_shiftmax");
        t.add_writable(
            n,
            Number::Auto,
            "pps_shift",
            "Interval duration (sec) (shift)",
            0,
            Value::Int(Box::new(|| NTP.lock().pps_shift())),
            sysctl::int_writer(|v| {
                NTP.lock().set_pps_shift(v);
                Ok(())
            }),
        )
        .expect("kern.ntp_pll.pps_shift");
        t.add_value(
            n,
            Number::Auto,
            "time_monitor",
            "Last time offset scaled (ns)",
            Value::Long(Box::new(|| NTP.lock().time_monitor())),
        )
        .expect("kern.ntp_pll.time_monitor");
        t.add_value(
            n,
            Number::Auto,
            "pps_freq",
            "Scaled frequency offset (ns/sec)",
            Value::S64(Box::new(|| NTP.lock().pps_freq())),
        )
        .expect("kern.ntp_pll.pps_freq");
        t.add_value(
            n,
            Number::Auto,
            "time_freq",
            "Frequency offset (ns/sec)",
            Value::S64(Box::new(|| NTP.lock().time_freq())),
        )
        .expect("kern.ntp_pll.time_freq");
    });
}
