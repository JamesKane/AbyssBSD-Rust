// SPDX-License-Identifier: BSD-2-Clause
//! The statistics clock and the load average.
//!
//! Every CPU samples itself `STATHZ` times a second, as FreeBSD's
//! `statclock` does: each tick counts toward the CPU's time in user mode,
//! user mode at a positive nice value, the kernel, interrupt threads, or
//! idle (`kern.cp_time`, `kern.cp_times`, which `top`, `vmstat`, and
//! `systat` turn into percentages), and toward the running thread's user,
//! system, and interrupt ticks, by which its run time is divided between
//! user and system time (`calcru`).
//!
//! The clock stops while its CPU idles, as FreeBSD's event-timer code
//! skips statistics ticks then: the ticks an idle stretch would have taken
//! are counted as idle when the CPU next runs a thread.
//!
//! The load average (`vm.loadavg`, `getloadavg`) is FreeBSD's `loadav`:
//! every five seconds the threads running or waiting to run decay into
//! three exponential averages, over one, five, and fifteen minutes, in
//! `FSCALE` fixed point.
//!
//! Behavior reference: sys/kern/kern_clock.c (`statclock`),
//! sys/kern/kern_clocksource.c, sys/kern/kern_synch.c (`loadav`),
//! sys/kern/kern_resource.c (`calcru1`).

use alloc::boxed::Box;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hal::MAX_CPUS;
use sched::{IrqCx, TickState};
use sync::Once;

use crate::time::{NANOS, Timer};

/// Statistics-clock ticks per second (`stathz`, FreeBSD's 127 with
/// one-shot event timers).
pub(crate) const STATHZ: u64 = 127;
/// `hz`, as `kern.clockrate` reports it.
pub(crate) const HZ: i32 = 1000;
/// `profhz`.
pub(crate) const PROFHZ: i32 = 8128;

/// `CP_USER` and the others: the `kern.cp_time` slots.
pub(crate) const CP_USER: usize = 0;
pub(crate) const CP_NICE: usize = 1;
pub(crate) const CP_SYS: usize = 2;
pub(crate) const CP_INTR: usize = 3;
pub(crate) const CP_IDLE: usize = 4;
pub(crate) const CPUSTATES: usize = 5;

/// Each CPU's ticks in each state.
static CP_TIME: [[AtomicU64; CPUSTATES]; MAX_CPUS] =
    [const { [const { AtomicU64::new(0) }; CPUSTATES] }; MAX_CPUS];
/// Each CPU's timer, made at start-up.
static TIMERS: [Once<Pin<&'static Timer>>; MAX_CPUS] = [const { Once::new() }; MAX_CPUS];
/// The timer is armed (the CPU is busy).
static ARMED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// When the CPU's clock stopped, in counter ticks, while it is stopped.
static STOPPED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// The interrupt being handled on each CPU came from user mode.
static FROM_USER: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// Interrupts taken (`vm.stats.sys.v_intr`).
pub(crate) static INTERRUPTS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The period, in nanoseconds.
const PERIOD_NS: u64 = NANOS / STATHZ;

/// Makes this CPU's clock. Once per CPU, at start-up, after its timer
/// queue; it starts with the CPU's first busy moment.
pub(crate) fn start_cpu() {
    let cpu = hal::cpu_id().index();
    let t: Pin<&'static Timer> = Pin::static_ref(Box::leak(Box::new(Timer::new())));
    let _ = TIMERS[cpu].set(t);
    STOPPED[cpu].store(crate::time::counter(), Ordering::Relaxed);
}

/// Notes where the interrupt about to be handled came from.
pub(crate) fn interrupted(from_user: bool) {
    let cpu = hal::cpu_id().index();
    FROM_USER[cpu].store(from_user, Ordering::Relaxed);
    INTERRUPTS[cpu].fetch_add(1, Ordering::Relaxed);
}

/// This CPU may be about to run a thread (a switch or a wakeup re-armed
/// its timer): restarts a stopped clock, counting the ticks it missed as
/// idle. Interrupts are masked.
pub(crate) fn resume() {
    let cpu = hal::cpu_id().index();
    if ARMED[cpu].load(Ordering::Relaxed) || sched::current().is_idle() {
        return;
    }
    let Some(t) = TIMERS[cpu].get() else {
        return;
    };
    let now = crate::time::counter();
    let stopped = STOPPED[cpu].load(Ordering::Relaxed);
    let missed = crate::time::ticks_to_ns(now.saturating_sub(stopped)) / PERIOD_NS;
    CP_TIME[cpu][CP_IDLE].fetch_add(missed, Ordering::Relaxed);
    ARMED[cpu].store(true, Ordering::Relaxed);
    t.reset(PERIOD_NS, tick, cpu);
}

/// One statistics tick (`statclock`): charged to the state the CPU was
/// found in. An idle CPU's clock stops until [`resume`].
fn tick(cpu: usize, _: &IrqCx) {
    let user = FROM_USER[cpu].load(Ordering::Relaxed);
    let state = match sched::stat_tick(user) {
        TickState::User if nice_positive() => CP_NICE,
        TickState::User => CP_USER,
        TickState::System => CP_SYS,
        TickState::Interrupt => CP_INTR,
        TickState::Idle => CP_IDLE,
    };
    CP_TIME[cpu][state].fetch_add(1, Ordering::Relaxed);
    if state == CP_IDLE {
        ARMED[cpu].store(false, Ordering::Relaxed);
        STOPPED[cpu].store(crate::time::counter(), Ordering::Relaxed);
        return;
    }
    if let Some(t) = TIMERS[cpu].get() {
        t.reset(PERIOD_NS, tick, cpu);
    }
}

/// Whether the running thread's process has a positive nice value
/// (`CP_NICE` rather than `CP_USER`).
fn nice_positive() -> bool {
    sched::with_owner(|o| {
        o.and_then(|o| o.downcast_ref::<crate::uthread::UserThread>())
            .is_some_and(|t| t.process().nice.load(Ordering::Relaxed) > 0)
    })
}

/// CPU `cpu`'s ticks in each state (`kern.cp_times`' slice for it).
pub(crate) fn cp_time(cpu: usize) -> [u64; CPUSTATES] {
    let Some(c) = CP_TIME.get(cpu) else {
        return [0; CPUSTATES];
    };
    let mut v = [0, 1, 2, 3, 4].map(|i| c[i].load(Ordering::Relaxed));
    // A stopped clock's idle stretch so far.
    if !ARMED[cpu].load(Ordering::Relaxed) && TIMERS[cpu].get().is_some() {
        let stopped = STOPPED[cpu].load(Ordering::Relaxed);
        let now = crate::time::counter();
        v[CP_IDLE] += crate::time::ticks_to_ns(now.saturating_sub(stopped)) / PERIOD_NS;
    }
    v
}

/// A thread's run time divided into user and system time, in
/// microseconds, by its statistics ticks (`calcru1`): the user share is
/// the run time times user ticks over all ticks; with no ticks yet, all of
/// it is system time, as FreeBSD's.
pub(crate) fn split_runtime(runtime_us: u64, ticks: [u64; 3]) -> (u64, u64) {
    let total = ticks[0] + ticks[1] + ticks[2];
    if total == 0 {
        return (0, runtime_us);
    }
    let user = u64::try_from(u128::from(runtime_us) * u128::from(ticks[0]) / u128::from(total))
        .unwrap_or(runtime_us);
    (user, runtime_us - user)
}

/// `FSHIFT` and `FSCALE`: the load average's fixed point.
pub(crate) const FSHIFT: u32 = 11;
pub(crate) const FSCALE: u64 = 1 << FSHIFT;

/// `cexp`: exp(-1/12), exp(-1/60), and exp(-1/180) in `FSCALE` fixed
/// point (truncated, as FreeBSD's initializers are), each average's decay
/// per five-second sample.
const CEXP: [u64; 3] = [1884, 2014, 2036];

/// The averages (`averunnable.ldavg`).
static LOADAV: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The load averages, `FSCALE` fixed point.
pub(crate) fn loadavg() -> [u64; 3] {
    [0, 1, 2].map(|i| LOADAV[i].load(Ordering::Relaxed))
}

/// One sample: `nrun` threads running or waiting to run.
fn sample(nrun: u64) {
    for (avg, cexp) in LOADAV.iter().zip(CEXP) {
        let old = avg.load(Ordering::Relaxed);
        avg.store(
            (cexp * old + nrun * FSCALE * (FSCALE - cexp)) >> FSHIFT,
            Ordering::Relaxed,
        );
    }
}

/// Samples the load every five seconds, from a kernel thread (FreeBSD's
/// `loadav` callout).
pub(crate) fn start_loadav() {
    let spawned = sched::spawn("loadav", sched::Priority::KERNEL, |s| {
        loop {
            crate::time::sleep_ns(s, 5 * NANOS);
            // This thread is running: it does not count itself.
            sample(u64::try_from(sched::load().saturating_sub(1)).unwrap_or(0));
        }
    });
    if spawned.is_err() {
        kprintln!("statclock: no thread for the load average");
    }
}
