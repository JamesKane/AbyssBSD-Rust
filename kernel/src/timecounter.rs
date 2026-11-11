// SPDX-License-Identifier: BSD-2-Clause
//! What `kern.timecounter`, `kern.eventtimer`, and (amd64)
//! `machdep.tsc_freq` show of the clock: the counter the timehands read
//! ([`crate::shared_page`]) and each CPU's event timer ([`crate::time`]).
//!
//! There is one timecounter and one event timer, the hardware the kernel
//! runs on: on amd64 the TSC (`TSC-low` when read shifted, as FreeBSD
//! names it) and the local APIC timer, on arm64 the generic timer's
//! counter and virtual timer, named as FreeBSD's drivers name them.
//! `choice` lists them as FreeBSD does, with the dummy counter that is
//! always last there. Selecting the one in use succeeds; another name
//! fails, as there is nothing else to switch to.
//!
//! Behavior reference: sys/kern/kern_tc.c, sys/kern/kern_et.c,
//! sys/kern/kern_clocksource.c, sys/kern/kern_time.c
//! (`nanosleep_precise`), sys/x86/x86/tsc.c, sys/x86/x86/local_apic.c,
//! sys/arm/arm/generic_timer.c.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, Ordering};

use abi::errno::Errno;
use sched::Sleepable;
use sysctl::{NodeId, Number, Tree, Value};

/// `ET_FLAGS_*`.
const ET_FLAGS_PERIODIC: i32 = 1;
const ET_FLAGS_ONESHOT: i32 = 2;
const ET_FLAGS_PERCPU: i32 = 4;
#[cfg(target_arch = "x86_64")]
const ET_FLAGS_C3STOP: i32 = 8;

/// `kern.timecounter.stepwarnings`: log each step of the time of day.
pub(crate) static STEP_WARNINGS: AtomicI32 = AtomicI32::new(0);
/// `kern.timecounter.alloweddeviation` (`tc_timepercentage`), in
/// percent; FreeBSD's callouts use it to coalesce, these do not.
static ALLOWED_DEVIATION: AtomicI32 = AtomicI32::new(5);
/// `kern.timecounter.nanosleep_precise`.
static NANOSLEEP_PRECISE: AtomicU8 = AtomicU8::new(1);
/// `kern.eventtimer.periodic`: always one-shot here; the setting is kept.
static PERIODIC: AtomicI32 = AtomicI32::new(0);
/// `kern.eventtimer.idletick`.
static IDLETICK: AtomicU32 = AtomicU32::new(0);
/// `kern.eventtimer.singlemul`: FreeBSD's for `hz` 1000.
static SINGLEMUL: AtomicI32 = AtomicI32::new(2);

/// The timecounter in use: its name and quality.
fn timecounter() -> (&'static str, i32) {
    #[cfg(target_arch = "x86_64")]
    {
        let (_, shift) = crate::shared_page::tc_frequency();
        let name = if shift > 0 { "TSC-low" } else { "TSC" };
        let quality = if crate::arch::cpu::tsc_invariant() {
            1000
        } else {
            800
        };
        (name, quality)
    }
    #[cfg(target_arch = "aarch64")]
    {
        ("ARM MPCore Timecounter", 1000)
    }
}

/// The event timer: its name, quality, flags, and rate.
fn eventtimer() -> (&'static str, i32, i32, u64) {
    #[cfg(target_arch = "x86_64")]
    {
        let arat = crate::arch::cpu::apic_timer_always_running();
        let mut flags = ET_FLAGS_PERIODIC | ET_FLAGS_ONESHOT | ET_FLAGS_PERCPU;
        if !arat {
            flags |= ET_FLAGS_C3STOP;
        }
        let freq = crate::arch::apic::timer_frequency(crate::time::frequency());
        ("LAPIC", if arat { 600 } else { 100 }, flags, freq)
    }
    #[cfg(target_arch = "aarch64")]
    {
        let _ = ET_FLAGS_PERIODIC;
        (
            "ARM MPCore Eventtimer",
            1000,
            ET_FLAGS_ONESHOT | ET_FLAGS_PERCPU,
            crate::time::frequency(),
        )
    }
}

/// A read-only `int` that is always `v`.
fn fixed_int(t: &mut Tree, parent: NodeId, name: &str, descr: &str, v: i32) {
    t.add_value(parent, Number::Auto, name, descr, Value::int(v))
        .expect("a timecounter sysctl");
}

/// A settable `int` kept in `cell`.
fn rw_int(t: &mut Tree, parent: NodeId, name: &str, descr: &str, cell: &'static AtomicI32) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        0,
        Value::Int(Box::new(|| cell.load(Ordering::Relaxed))),
        sysctl::int_writer(|v| {
            cell.store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("a timecounter sysctl");
}

/// A string that may be set only to what it is (`name`): another value
/// fails with `err` (`EINVAL` for a timecounter, `ENOENT` for an event
/// timer), as nothing else is there to choose.
fn selector(
    t: &mut Tree,
    parent: NodeId,
    name: &str,
    descr: &str,
    current: &'static str,
    err: Errno,
) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        0,
        Value::text(current),
        sysctl::str_writer(32, move |b| {
            if b.eq_ignore_ascii_case(current.as_bytes()) {
                Ok(())
            } else {
                Err(err)
            }
        }),
    )
    .expect("a timecounter sysctl");
}

/// Adds `kern.timecounter`, `kern.eventtimer`, and `machdep.tsc_freq`.
pub(crate) fn sysctl_init(s: &Sleepable) {
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        let tc = t
            .add_node(kern, Number::Auto, "timecounter", "")
            .expect("kern.timecounter");
        let (name, quality) = timecounter();
        rw_int(t, tc, "stepwarnings", "Log time steps", &STEP_WARNINGS);
        fixed_int(
            t,
            tc,
            "timehands_count",
            "Count of timehands in rotation",
            4,
        );
        rw_int(
            t,
            tc,
            "alloweddeviation",
            "Allowed time interval deviation in percents",
            &ALLOWED_DEVIATION,
        );
        selector(
            t,
            tc,
            "hardware",
            "Timecounter hardware selected",
            name,
            Errno::EINVAL,
        );
        t.add_value(
            tc,
            Number::Auto,
            "choice",
            "Timecounter hardware detected",
            Value::Str(Box::new(move |out: &mut String| {
                out.push_str(&format!("{name}({quality}) dummy(-1000000)"));
            })),
        )
        .expect("kern.timecounter.choice");
        fixed_int(
            t,
            tc,
            "tick",
            "Approximate number of hardclock ticks in a millisecond",
            1,
        );
        t.add_writable(
            tc,
            Number::Auto,
            "fast_gettime",
            "Enable fast time of day",
            0,
            Value::Int(Box::new(|| i32::from(crate::shared_page::fast_gettime()))),
            sysctl::int_writer(|v| {
                crate::shared_page::set_fast_gettime(v != 0);
                Ok(())
            }),
        )
        .expect("kern.timecounter.fast_gettime");
        t.add_writable(
            tc,
            Number::Auto,
            "nanosleep_precise",
            "clock_nanosleep() with CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_UPTIME and nanosleep(2) use precise clock",
            0,
            Value::U8(Box::new(|| NANOSLEEP_PRECISE.load(Ordering::Relaxed))),
            sysctl::u8_writer(|v| {
                NANOSLEEP_PRECISE.store(u8::from(v != 0), Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.timecounter.nanosleep_precise");
        let tcs = t
            .add_node(tc, Number::Auto, "tc", "")
            .expect("kern.timecounter.tc");
        let me = t
            .add_node(tcs, Number::Auto, name, "timecounter description")
            .expect("kern.timecounter.tc.<name>");
        t.add_value(
            me,
            Number::Auto,
            "mask",
            "mask for implemented bits",
            Value::Uint(Box::new(|| u32::MAX)),
        )
        .expect("tc mask");
        t.add_value(
            me,
            Number::Auto,
            "counter",
            "current timecounter value",
            Value::Uint(Box::new(crate::shared_page::timecount)),
        )
        .expect("tc counter");
        t.add_value(
            me,
            Number::Auto,
            "frequency",
            "timecounter frequency",
            Value::U64(Box::new(|| crate::shared_page::tc_frequency().0)),
        )
        .expect("tc frequency");
        fixed_int(t, me, "quality", "goodness of time counter", quality);
        #[cfg(target_arch = "x86_64")]
        {
            fixed_int(
                t,
                tc,
                "invariant_tsc",
                "Indicates whether the TSC is P-state invariant",
                i32::from(crate::arch::cpu::tsc_invariant()),
            );
            fixed_int(
                t,
                tc,
                "smp_tsc",
                "Indicates whether the TSC is safe to use in SMP mode",
                1,
            );
            fixed_int(
                t,
                tc,
                "smp_tsc_adjust",
                "Try to adjust TSC on APs to match BSP",
                0,
            );
            fixed_int(
                t,
                tc,
                "tsc_shift",
                "Shift to pre-apply for the maximum TSC frequency",
                1,
            );
            let machdep = t.child(t.root(), "machdep").expect("machdep node");
            t.add_value(
                machdep,
                Number::Auto,
                "tsc_freq",
                "Time Stamp Counter frequency",
                Value::U64(Box::new(crate::time::frequency)),
            )
            .expect("machdep.tsc_freq");
        }

        let et = t
            .add_node(kern, Number::Auto, "eventtimer", "Event timers")
            .expect("kern.eventtimer");
        let (et_name, et_quality, et_flags, et_freq) = eventtimer();
        let ets = t
            .add_node(et, Number::Auto, "et", "")
            .expect("kern.eventtimer.et");
        let me = t
            .add_node(ets, Number::Auto, et_name, "event timer description")
            .expect("kern.eventtimer.et.<name>");
        fixed_int(t, me, "flags", "Event timer capabilities", et_flags);
        t.add_value(
            me,
            Number::Auto,
            "frequency",
            "Event timer base frequency",
            Value::U64(Box::new(move || et_freq)),
        )
        .expect("et frequency");
        fixed_int(t, me, "quality", "Goodness of event timer", et_quality);
        t.add_value(
            et,
            Number::Auto,
            "choice",
            "Present event timers",
            Value::Str(Box::new(move |out: &mut String| {
                out.push_str(&format!("{et_name}({et_quality})"));
            })),
        )
        .expect("kern.eventtimer.choice");
        rw_int(
            t,
            et,
            "singlemul",
            "Multiplier for periodic mode",
            &SINGLEMUL,
        );
        t.add_writable(
            et,
            Number::Auto,
            "idletick",
            "Run periodic events when idle",
            0,
            Value::Uint(Box::new(|| IDLETICK.load(Ordering::Relaxed))),
            Arc::new(|b: &[u8]| {
                let v = b
                    .first_chunk::<4>()
                    .map(|c| u32::from_ne_bytes(*c))
                    .ok_or(Errno::EINVAL)?;
                IDLETICK.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.eventtimer.idletick");
        selector(t, et, "timer", "Chosen event timer", et_name, Errno::ENOENT);
        rw_int(
            t,
            et,
            "periodic",
            "Enable event timer periodic mode",
            &PERIODIC,
        );
    });
}
