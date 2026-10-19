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
//! - Programming the timer is costly (under a hypervisor, a VM exit), so
//!   it is reprogrammed only to move the deadline earlier. A deadline that
//!   is no longer needed is left to fire: the handler finds nothing due
//!   and programs the next one, if any. Context switches and cancelled
//!   callouts therefore touch no timer hardware.
//! - Timers ([`Timer`], FreeBSD's callouts) are caller-owned and pinned;
//!   arming one allocates nothing. Each CPU keeps its armed timers in a
//!   binary heap and runs their callbacks in interrupt context.
//!
//! Behavior reference: sys/kern/kern_tc.c, sys/kern/kern_clocksource.c,
//! sys/kern/kern_timeout.c.

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::marker::PhantomPinned;
use core::pin::{Pin, pin};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hal::{CpuId, MAX_CPUS};
use sched::{IrqCx, Sleepable};
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

/// The Unix time, in nanoseconds, at counter zero (0 until a clock has
/// been read).
static REALTIME_BASE: AtomicU64 = AtomicU64::new(0);

/// Sets the time of day from the real-time clocks (FreeBSD's
/// `inittodr`).
pub(crate) fn set_realtime() {
    match clock::read() {
        Some((name, t)) => {
            let unix_ns = t.unix_seconds().unwrap_or(0) * 1_000_000_000 + u64::from(t.nanosecond);
            REALTIME_BASE.store(unix_ns.saturating_sub(now_ns()), Ordering::Release);
            crate::shared_page::set_realtime(unix_ns);
            kprintln!("clock: {t} from {name}");
        }
        None => kprintln!("clock: no real-time clock answered; the time of day is unknown"),
    }
}

/// The Unix time in nanoseconds, if a real-time clock has set it.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn realtime_ns() -> Option<u64> {
    let base = REALTIME_BASE.load(Ordering::Acquire);
    (base != 0).then(|| base + now_ns())
}

/// The Unix time, in nanoseconds, at boot (0 while unknown).
pub(crate) fn realtime_base_ns() -> u64 {
    REALTIME_BASE.load(Ordering::Acquire)
}

/// The raw counter and, at that count, the nanoseconds since boot.
pub(crate) fn counter_and_uptime() -> (u64, u64) {
    let c = counter();
    (
        c,
        ticks_to_ns(c.saturating_sub(EPOCH.load(Ordering::Relaxed))),
    )
}

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

/// A timer callback and its argument (FreeBSD's callout function and its
/// `void *`). It runs in interrupt context on the timer's CPU, so it gets
/// an [`IrqCx`] and cannot sleep.
pub(crate) type TimerFn = fn(usize, &IrqCx);

/// Not in a CPU's queue.
const NOT_QUEUED: usize = usize::MAX;

/// A caller-owned timer, like FreeBSD's `struct callout`: arming and
/// stopping it allocate nothing. It belongs to the CPU that created it and
/// fires there. It must be pinned to be armed, and dropping it stops it
/// and waits for a running callback, so a callback never outlives it.
pub(crate) struct Timer {
    cpu: CpuId,
    /// Guarded by `QUEUES[cpu]`.
    st: UnsafeCell<TimerState>,
    _pinned: PhantomPinned,
}

struct TimerState {
    deadline: u64,
    /// Index in the CPU's heap, or [`NOT_QUEUED`].
    slot: usize,
    func: TimerFn,
    arg: usize,
}

// SAFETY: the state is only touched under the owning CPU's queue lock.
unsafe impl Sync for Timer {}
// SAFETY: as above; the CPU is fixed.
unsafe impl Send for Timer {}

impl core::fmt::Debug for Timer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Timer")
            .field("cpu", &self.cpu)
            .finish_non_exhaustive()
    }
}

/// One CPU's armed timers: a binary min-heap on deadline.
struct CpuQueue {
    heap: Vec<*const Timer>,
    /// The timer whose callback is running now (for [`Timer::drain`]).
    running: *const Timer,
}

// SAFETY: the pointers are to pinned timers that stay alive while queued
// (a timer leaves its queue before it is dropped).
unsafe impl Send for CpuQueue {}

static QUEUES: [SpinLock<CpuQueue>; MAX_CPUS] = [const {
    SpinLock::new(CpuQueue {
        heap: Vec::new(),
        running: ptr::null(),
    })
}; MAX_CPUS];

impl CpuQueue {
    /// # Safety (all methods)
    ///
    /// Every timer in the heap is alive, and the caller holds this queue's
    /// lock, which guards those timers' state.
    fn st(t: *const Timer) -> &'static mut TimerState {
        // SAFETY: see above.
        unsafe { &mut *(*t).st.get() }
    }

    fn deadline(&self, i: usize) -> u64 {
        Self::st(self.heap[i]).deadline
    }

    fn place(&mut self, i: usize, t: *const Timer) {
        self.heap[i] = t;
        Self::st(t).slot = i;
    }

    fn sift_up(&mut self, mut i: usize) {
        let t = self.heap[i];
        let d = Self::st(t).deadline;
        while i > 0 {
            let parent = (i - 1) / 2;
            if self.deadline(parent) <= d {
                break;
            }
            self.place(i, self.heap[parent]);
            i = parent;
        }
        self.place(i, t);
    }

    fn sift_down(&mut self, mut i: usize) {
        let t = self.heap[i];
        let d = Self::st(t).deadline;
        loop {
            let l = 2 * i + 1;
            if l >= self.heap.len() {
                break;
            }
            let r = l + 1;
            let c = if r < self.heap.len() && self.deadline(r) < self.deadline(l) {
                r
            } else {
                l
            };
            if self.deadline(c) >= d {
                break;
            }
            self.place(i, self.heap[c]);
            i = c;
        }
        self.place(i, t);
    }

    fn push(&mut self, t: *const Timer) {
        self.heap.push(t);
        self.sift_up(self.heap.len() - 1);
    }

    fn remove(&mut self, i: usize) -> *const Timer {
        let t = self.heap.swap_remove(i);
        Self::st(t).slot = NOT_QUEUED;
        if i < self.heap.len() {
            // The last timer moved into the hole: restore heap order
            // around it, in whichever direction it is out of place.
            let moved = self.heap[i];
            Self::st(moved).slot = i;
            self.sift_down(i);
            self.sift_up(Self::st(moved).slot);
        }
        t
    }

    fn first_deadline(&self) -> u64 {
        if self.heap.is_empty() {
            UNARMED
        } else {
            self.deadline(0)
        }
    }
}

/// Room for this many armed timers per CPU before the heap grows.
const QUEUE_RESERVE: usize = 64;

/// Sets up this CPU's timer queue (its heap is allocated once, here, so
/// steady-state arming allocates nothing and heap accounting is stable).
pub(crate) fn init_cpu() {
    let mut q = QUEUES[hal::cpu_id().index()].lock();
    let need = QUEUE_RESERVE.saturating_sub(q.heap.len());
    q.heap.reserve(need);
}

impl Timer {
    /// A disarmed timer on this CPU.
    pub(crate) fn new() -> Self {
        Self {
            cpu: hal::cpu_id(),
            st: UnsafeCell::new(TimerState {
                deadline: 0,
                slot: NOT_QUEUED,
                func: |_, _| {},
                arg: 0,
            }),
            _pinned: PhantomPinned,
        }
    }

    /// Arms the timer to run `func(arg)` `ns` nanoseconds from now,
    /// replacing any pending arming (FreeBSD's `callout_reset`).
    pub(crate) fn reset(self: Pin<&Self>, ns: u64, func: TimerFn, arg: usize) {
        let deadline = counter().saturating_add(ns_to_ticks(ns));
        let me = ptr::from_ref(self.get_ref());
        let c = self.cpu.index();
        // The queue lock masks interrupts, as `reprogram` needs.
        let mut q = QUEUES[c].lock();
        let st = CpuQueue::st(me);
        if st.slot != NOT_QUEUED {
            q.remove(st.slot);
        }
        st.deadline = deadline;
        st.func = func;
        st.arg = arg;
        q.push(me);
        NEXT_CALLOUT[c].store(q.first_deadline(), Ordering::Relaxed);
        // Only a deadline earlier than the armed one needs the timer.
        if deadline < PROGRAMMED[c].load(Ordering::Relaxed) {
            if self.cpu == hal::cpu_id() {
                reprogram();
            } else {
                crate::intc::send_resched(self.cpu);
            }
        }
        drop(q);
    }

    /// Disarms the timer. Returns whether it was pending (FreeBSD's
    /// `callout_stop`); a callback already running is not waited for.
    pub(crate) fn stop(&self) -> bool {
        let me = ptr::from_ref(self);
        let c = self.cpu.index();
        let mut q = QUEUES[c].lock();
        let slot = CpuQueue::st(me).slot;
        if slot == NOT_QUEUED {
            return false;
        }
        q.remove(slot);
        NEXT_CALLOUT[c].store(q.first_deadline(), Ordering::Relaxed);
        true
    }

    /// Disarms the timer and waits for a running callback to finish
    /// (FreeBSD's `callout_drain`). Not from the timer's own callback.
    pub(crate) fn drain(&self) -> bool {
        let me = ptr::from_ref(self);
        loop {
            if self.stop() {
                return true;
            }
            if !ptr::eq(QUEUES[self.cpu.index()].lock().running, me) {
                return false;
            }
            core::hint::spin_loop();
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.drain();
    }
}

/// No deadline programmed.
const UNARMED: u64 = u64::MAX;

/// Each CPU's earliest pending callout deadline, or [`UNARMED`]. Never
/// later than the truth: a cancel leaves it early, which costs at most a
/// timer interrupt that finds nothing due. Written only by its own CPU.
static NEXT_CALLOUT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(UNARMED) }; MAX_CPUS];

/// Each CPU's programmed deadline, in counter ticks, or [`UNARMED`].
/// Written only by its own CPU with interrupts masked.
static PROGRAMMED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(UNARMED) }; MAX_CPUS];

/// Makes sure this CPU's event timer fires by its next deadline.
/// Interrupts must be masked.
fn reprogram() {
    let cpu = hal::cpu_id();
    let next_callout =
        Some(NEXT_CALLOUT[cpu.index()].load(Ordering::Relaxed)).filter(|&d| d != UNARMED);
    let slice = sched::slice_deadline(ns_to_ticks(SLICE_NS));
    let programmed = &PROGRAMMED[cpu.index()];
    if let Some(deadline) = [next_callout, slice].into_iter().flatten().min()
        && deadline < programmed.load(Ordering::Relaxed)
    {
        programmed.store(deadline, Ordering::Relaxed);
        set_deadline(deadline);
    }
}

fn set_deadline(ticks: u64) {
    #[cfg(target_arch = "x86_64")]
    arch::apic::set_deadline(ticks);
    #[cfg(target_arch = "aarch64")]
    arch::timer::set_deadline(ticks);
}

/// Timer interrupts taken, per CPU (diagnostics).
static TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The event timer fired: runs due callouts, checks the time slice, and
/// re-arms. Interrupt context, interrupts masked.
pub(crate) fn on_timer(cx: &IrqCx) {
    let cpu = hal::cpu_id();
    TICKS[cpu.index()].fetch_add(1, Ordering::Relaxed);
    // The one-shot deadline is spent.
    PROGRAMMED[cpu.index()].store(UNARMED, Ordering::Relaxed);
    loop {
        let now = counter();
        let mut q = QUEUES[cpu.index()].lock();
        let first = q.first_deadline();
        if first > now {
            NEXT_CALLOUT[cpu.index()].store(first, Ordering::Relaxed);
            break;
        }
        let t = q.remove(0);
        let (func, arg) = {
            let st = CpuQueue::st(t);
            (st.func, st.arg)
        };
        q.running = t;
        drop(q);
        // The timer may be dropped once its callback returns: only the
        // queue is touched afterwards.
        func(arg, cx);
        QUEUES[cpu.index()].lock().running = ptr::null();
    }
    sched::tick(ns_to_ticks(SLICE_NS));
    reprogram();
    // The generic timer's interrupt is a level: left enabled with a past
    // deadline it fires again at once. (x86's one-shot modes disarm
    // themselves.)
    #[cfg(target_arch = "aarch64")]
    if PROGRAMMED[cpu.index()].load(Ordering::Relaxed) == UNARMED {
        arch::timer::cancel_deadline();
    }
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

/// A thread waiting for a timer, woken by [`wake_waiter`].
struct Waiter<'a> {
    q: &'a sched::WaitQueue,
    fired: AtomicBool,
}

/// Timer callback: `arg` is a `&Waiter`.
fn wake_waiter(arg: usize, _: &IrqCx) {
    // SAFETY: the waiter outlives its timer (declared before it, so the
    // timer drops, and drains, first).
    let w = unsafe { &*ptr::with_exposed_provenance::<Waiter<'_>>(arg) };
    // Release pairs with the waiter's Acquire.
    w.fired.store(true, Ordering::Release);
    w.q.wake_all();
}

/// Sleeps the current thread for at least `ns` nanoseconds.
pub(crate) fn sleep_ns(s: &Sleepable, ns: u64) {
    let q = sched::WaitQueue::new();
    let w = Waiter {
        q: &q,
        fired: AtomicBool::new(false),
    };
    let t = pin!(Timer::new());
    t.as_ref()
        .reset(ns, wake_waiter, ptr::from_ref(&w).expose_provenance());
    q.wait_until(s, || w.fired.load(Ordering::Acquire));
}

/// Waits on `q` until `cond` holds or `ns` nanoseconds pass. Returns
/// whether `cond` held.
pub(crate) fn wait_timeout(
    s: &Sleepable,
    q: &sched::WaitQueue,
    mut cond: impl FnMut() -> bool,
    ns: u64,
) -> bool {
    let w = Waiter {
        q,
        fired: AtomicBool::new(false),
    };
    let t = pin!(Timer::new());
    t.as_ref()
        .reset(ns, wake_waiter, ptr::from_ref(&w).expose_provenance());
    let mut ok = false;
    q.wait_until(s, || {
        ok = cond();
        ok || w.fired.load(Ordering::Acquire)
    });
    t.stop();
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
