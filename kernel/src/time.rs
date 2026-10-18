// SPDX-License-Identifier: BSD-2-Clause
//! Timekeeping and timers.
//!
//! - The clock is the CPU counter (the TSC, calibrated against the ACPI PM
//!   timer, or the generic timer's virtual count), converted to
//!   nanoseconds since boot.
//! - Each CPU has a one-shot event timer (the local APIC timer or the
//!   virtual timer). It is programmed for the earliest of that CPU's
//!   pending callouts and, when threads are waiting to run, the end of the
//!   current time slice. With neither, it stays off (tickless idle).
//! - Callouts run in interrupt context on the CPU that armed them.
//!
//! Behavior reference: sys/kern/kern_tc.c, sys/kern/kern_clocksource.c,
//! sys/kern/kern_timeout.c.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hal::{CpuId, MAX_CPUS};
use sync::SpinLock;

use crate::arch;

/// Nanoseconds in a second.
pub(crate) const NANOS: u64 = 1_000_000_000;

/// The scheduler's time slice.
pub(crate) const SLICE_NS: u64 = 10_000_000;

/// Counter ticks per second.
static FREQUENCY: AtomicU64 = AtomicU64::new(0);
/// Counter value at boot (time zero).
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Counter ticks per second.
pub(crate) fn frequency() -> u64 {
    FREQUENCY.load(Ordering::Relaxed)
}

/// The raw counter.
pub(crate) fn counter() -> u64 {
    #[cfg(target_arch = "x86_64")]
    return arch::cpu::timestamp();
    #[cfg(target_arch = "aarch64")]
    return arch::timer::counter();
}

/// Converts counter ticks to nanoseconds.
pub(crate) fn ticks_to_ns(ticks: u64) -> u64 {
    let f = frequency().max(1);
    u64::try_from(u128::from(ticks) * u128::from(NANOS) / u128::from(f)).unwrap_or(u64::MAX)
}

/// Converts nanoseconds to counter ticks.
pub(crate) fn ns_to_ticks(ns: u64) -> u64 {
    u64::try_from(u128::from(ns) * u128::from(frequency()) / u128::from(NANOS)).unwrap_or(u64::MAX)
}

/// Nanoseconds since boot.
pub(crate) fn now_ns() -> u64 {
    ticks_to_ns(counter().saturating_sub(EPOCH.load(Ordering::Relaxed)))
}

/// Sets the clock rate. Call once on the boot CPU.
pub(crate) fn init_clock(frequency: u64) {
    FREQUENCY.store(frequency, Ordering::Relaxed);
    EPOCH.store(counter(), Ordering::Relaxed);
    kprintln!(
        "time: counter at {}.{:03} MHz",
        frequency / 1_000_000,
        (frequency / 1000) % 1000
    );
}

type Callback = Box<dyn FnOnce() + Send>;

/// One CPU's pending callouts, keyed by (deadline in ticks, id).
struct CpuTimers {
    pending: BTreeMap<(u64, u64), Callback>,
    /// The callout running now, if any (for [`Callout::cancel`]).
    running: Option<u64>,
}

static TIMERS: [SpinLock<CpuTimers>; MAX_CPUS] = [const {
    SpinLock::new(CpuTimers {
        pending: BTreeMap::new(),
        running: None,
    })
}; MAX_CPUS];
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A pending callout; cancel it or let it fire.
#[derive(Debug)]
#[must_use = "a callout that is never cancelled runs anyway; drop with `forget` to make that explicit"]
pub(crate) struct Callout {
    cpu: CpuId,
    key: (u64, u64),
}

impl Callout {
    /// Runs `f` in interrupt context on this CPU, `ns` nanoseconds from
    /// now.
    pub(crate) fn after(ns: u64, f: impl FnOnce() + Send + 'static) -> Self {
        let deadline = counter().saturating_add(ns_to_ticks(ns));
        let irq = hal::irq::disable();
        let cpu = hal::cpu_id();
        // Relaxed: uniqueness only.
        let key = (deadline, NEXT_ID.fetch_add(1, Ordering::Relaxed));
        TIMERS[cpu.index()].lock().pending.insert(key, Box::new(f));
        reprogram();
        hal::irq::restore(irq);
        Self { cpu, key }
    }

    /// Cancels the callout. Returns whether it was cancelled before
    /// running; if it is running on another CPU, waits for it to finish.
    pub(crate) fn cancel(self) -> bool {
        loop {
            let mut t = TIMERS[self.cpu.index()].lock();
            if t.pending.remove(&self.key).is_some() {
                return true;
            }
            if t.running != Some(self.key.1) {
                return false;
            }
            drop(t);
            core::hint::spin_loop();
        }
    }

    /// Lets the callout fire without keeping a handle.
    pub(crate) fn forget(self) {}
}

/// Programs this CPU's event timer for its next deadline. Interrupts must
/// be masked.
fn reprogram() {
    let cpu = hal::cpu_id();
    let next_callout = TIMERS[cpu.index()]
        .lock()
        .pending
        .first_key_value()
        .map(|(k, _)| k.0);
    let slice = sched::slice_deadline(ns_to_ticks(SLICE_NS));
    match [next_callout, slice].into_iter().flatten().min() {
        Some(deadline) => set_deadline(deadline),
        None => cancel_deadline(),
    }
}

fn set_deadline(ticks: u64) {
    #[cfg(target_arch = "x86_64")]
    arch::apic::set_deadline(ticks);
    #[cfg(target_arch = "aarch64")]
    arch::timer::set_deadline(ticks);
}

fn cancel_deadline() {
    #[cfg(target_arch = "x86_64")]
    arch::apic::cancel_deadline();
    #[cfg(target_arch = "aarch64")]
    arch::timer::cancel_deadline();
}

/// Timer interrupts taken, per CPU (diagnostics).
static TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The event timer fired: runs due callouts, checks the time slice, and
/// re-arms. Interrupt context, interrupts masked.
pub(crate) fn on_timer() {
    let cpu = hal::cpu_id();
    TICKS[cpu.index()].fetch_add(1, Ordering::Relaxed);
    loop {
        let now = counter();
        let mut t = TIMERS[cpu.index()].lock();
        let Some(entry) = t.pending.first_entry() else {
            break;
        };
        if entry.key().0 > now {
            break;
        }
        let ((_, id), f) = entry.remove_entry();
        t.running = Some(id);
        drop(t);
        f();
        TIMERS[cpu.index()].lock().running = None;
    }
    sched::tick(ns_to_ticks(SLICE_NS));
    reprogram();
}

/// Re-arms this CPU's timer after a context switch or wakeup changed what
/// it should wait for. Interrupts must be masked.
pub(crate) fn rearm() {
    reprogram();
}

/// Timer interrupts this CPU has taken.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn ticks(cpu: CpuId) -> u64 {
    TICKS[cpu.index()].load(Ordering::Relaxed)
}

/// Sleeps the current thread for at least `ns` nanoseconds.
pub(crate) fn sleep_ns(ns: u64) {
    let q = Arc::new(sched::WaitQueue::new());
    let done = Arc::new(AtomicBool::new(false));
    let (q2, done2) = (Arc::clone(&q), Arc::clone(&done));
    Callout::after(ns, move || {
        // Release pairs with the waiter's Acquire.
        done2.store(true, Ordering::Release);
        q2.wake_all();
    })
    .forget();
    q.wait_until(|| done.load(Ordering::Acquire));
}

/// Waits on `q` until `cond` holds or `ns` nanoseconds pass. Returns
/// whether `cond` held.
pub(crate) fn wait_timeout(
    q: &Arc<sched::WaitQueue>,
    mut cond: impl FnMut() -> bool,
    ns: u64,
) -> bool {
    let expired = Arc::new(AtomicBool::new(false));
    let (q2, expired2) = (Arc::clone(q), Arc::clone(&expired));
    let callout = Callout::after(ns, move || {
        expired2.store(true, Ordering::Release);
        q2.wake_all();
    });
    let mut ok = false;
    q.wait_until(|| {
        ok = cond();
        ok || expired.load(Ordering::Acquire)
    });
    callout.cancel();
    ok
}

/// Busy-waits `ns` nanoseconds on the counter.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub(crate) fn spin_ns(ns: u64) {
    let end = counter().saturating_add(ns_to_ticks(ns));
    while counter() < end {
        core::hint::spin_loop();
    }
}
