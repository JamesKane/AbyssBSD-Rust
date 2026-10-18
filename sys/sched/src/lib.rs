// SPDX-License-Identifier: BSD-2-Clause
//! Kernel threads and the scheduler.
//!
//! Every CPU has a [`PerCpu`] block (reached through `hal::cpu_local`)
//! holding its current thread, its idle thread, and its run queue.
//! Switching follows one protocol everywhere:
//!
//! 1. With interrupts masked, the outgoing thread's state decides its fate:
//!    `Running` goes back on the run queue, `Sleeping` is already on a wait
//!    queue, `Runnable` was already requeued by a waker, `Dead` goes to the
//!    reaper.
//! 2. The best runnable thread is taken from this CPU's queue (or stolen
//!    from another CPU's, or the idle thread runs).
//! 3. Before resuming it, the CPU waits for the thread's `on_cpu` flag to
//!    clear: a thread woken on one CPU while still saving its registers on
//!    another must not resume early.
//! 4. After the switch, the incoming side clears the outgoing thread's
//!    `on_cpu` ([`finish_switch`]).
//!
//! Locks nest as wait queue → thread → run queue.
//!
//! Behavior reference: sys/kern/sched_ule.c, sys/kern/kern_synch.c,
//! sys/kern/kern_thread.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod list;
mod runq;
mod thread;
mod wait;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::Cell;
use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use hal::{CpuId, MAX_CPUS};
use sync::{Once, SpinLock};

use crate::list::ThreadList;
use crate::runq::RunQueue;
pub use crate::thread::{Name, Priority, State, Thread, Tid};
pub use crate::wait::WaitQueue;

/// A kernel stack, owned by the platform's allocator.
#[derive(Debug)]
pub struct Stack {
    /// The initial stack pointer (16-byte aligned).
    pub top: u64,
    /// Whatever the platform needs to free it.
    pub handle: usize,
}

/// What the scheduler needs from the rest of the kernel.
pub trait Platform: Sync {
    /// A fresh kernel stack.
    fn alloc_stack(&self) -> Option<Stack>;
    /// Frees a stack from [`Platform::alloc_stack`]; never called on the
    /// stack in use.
    fn free_stack(&self, stack: Stack);
    /// A monotonic clock for CPU-time accounting.
    fn now(&self) -> u64;
    /// Makes `cpu` reschedule soon (an IPI, or nothing if it is this CPU).
    fn kick(&self, cpu: CpuId);
    /// Reprograms this CPU's event timer because what it waits for may
    /// have changed (a switch, or a newly queued thread). Interrupts are
    /// masked.
    fn rearm_timer(&self);
}

static PLATFORM: Once<&'static dyn Platform> = Once::new();

fn platform() -> &'static dyn Platform {
    *PLATFORM.get().expect("scheduler used before init")
}

/// A CPU's scheduling state.
pub struct PerCpu {
    id: CpuId,
    /// The running thread; owns one reference (from `Arc::into_raw`).
    current: Cell<*const Thread>,
    idle: Cell<*const Thread>,
    /// The thread just switched away from, for [`finish_switch`].
    prev: Cell<*const Thread>,
    /// The previous thread's reference, when the switch path still owns
    /// it (it slept or died rather than going back on a queue).
    prev_owned: Cell<Option<Arc<Thread>>>,
    runq: SpinLock<RunQueue>,
    need_resched: AtomicBool,
    /// Nesting depth of preemption-disabled sections.
    preempt_off: Cell<u32>,
}

// SAFETY: the `Cell`s are only touched by the owning CPU with interrupts
// masked; other CPUs use only `runq` (locked) and `need_resched` (atomic).
unsafe impl Sync for PerCpu {}

impl fmt::Debug for PerCpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerCpu")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl PerCpu {
    /// The CPU's number.
    #[must_use]
    pub fn id(&self) -> CpuId {
        self.id
    }

    /// Asks the CPU to reschedule at its next opportunity.
    pub fn request_resched(&self) {
        // Relaxed: a hint; the run-queue lock orders the queue contents.
        self.need_resched.store(true, Ordering::Relaxed);
    }
}

static CPUS: [AtomicPtr<PerCpu>; MAX_CPUS] = [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];
static ONLINE: AtomicUsize = AtomicUsize::new(0);

/// This CPU's block. Panics before [`init_cpu`].
#[must_use]
pub fn this_cpu() -> &'static PerCpu {
    let p = hal::cpu_local().cast::<PerCpu>();
    assert!(!p.is_null(), "scheduler not running on this CPU");
    // SAFETY: set by `init_cpu` to a leaked, never-freed block.
    unsafe { &*p }
}

fn cpu(id: CpuId) -> Option<&'static PerCpu> {
    let p = CPUS.get(id.index())?.load(Ordering::Acquire);
    // SAFETY: published blocks are leaked and never freed.
    unsafe { p.as_ref() }
}

/// CPUs running the scheduler.
#[must_use]
pub fn online_cpus() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// Sets the platform hooks. Call once, before [`init_cpu`].
pub fn init(p: &'static dyn Platform) {
    assert!(PLATFORM.set(p).is_ok(), "sched::init called twice");
}

/// Starts scheduling on this CPU: the code running now becomes a thread
/// named `name`, and an idle thread is created. Interrupts must be masked.
pub fn init_cpu(name: &str) {
    let id = hal::cpu_id();
    let pcpu = Box::leak(Box::new(PerCpu {
        id,
        current: Cell::new(ptr::null()),
        idle: Cell::new(ptr::null()),
        prev: Cell::new(ptr::null()),
        prev_owned: Cell::new(None),
        runq: SpinLock::new(RunQueue::new()),
        need_resched: AtomicBool::new(false),
        preempt_off: Cell::new(0),
    }));
    let boot = Arc::new(Thread::new(name, Priority::KERNEL, id, None, None, false));
    {
        let mut inner = boot.inner.lock();
        inner.state = State::Running;
        inner.since = platform().now();
    }
    boot.on_cpu.store(true, Ordering::Relaxed);
    pcpu.current.set(Arc::into_raw(boot));

    let stack = platform().alloc_stack().expect("idle thread stack");
    let idle = Arc::new(Thread::new(
        "idle",
        Priority::IDLE_THREAD,
        id,
        Some(stack),
        Some(Box::new(idle_loop)),
        true,
    ));
    idle.inner.lock().bound = Some(id);
    pcpu.idle.set(Arc::into_raw(idle));

    // SAFETY: the block is leaked, so it outlives every reader.
    unsafe { hal::set_cpu_local(ptr::from_mut(pcpu).cast()) };
    CPUS[id.index()].store(pcpu, Ordering::Release);
    ONLINE.fetch_add(1, Ordering::AcqRel);
}

/// The running thread.
#[must_use]
pub fn current() -> &'static Thread {
    // SAFETY: `current` always holds a live reference while the thread
    // runs, and the running thread cannot be freed under itself.
    unsafe { &*this_cpu().current.get() }
}

/// A new reference to the running thread.
pub(crate) fn current_arc() -> Arc<Thread> {
    let p = this_cpu().current.get();
    // SAFETY: `current` holds a reference; bump the count for the copy.
    unsafe {
        Arc::increment_strong_count(p);
        Arc::from_raw(p)
    }
}

/// Error from [`spawn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// No kernel stack.
    NoStack,
}

/// Starts a kernel thread running `f` at `priority`.
pub fn spawn(
    name: &str,
    priority: Priority,
    f: impl FnOnce() + Send + 'static,
) -> Result<Arc<Thread>, SpawnError> {
    spawn_on(name, priority, None, f)
}

/// Starts a kernel thread bound to `cpu` (or unbound with `None`).
pub fn spawn_on(
    name: &str,
    priority: Priority,
    bound: Option<CpuId>,
    f: impl FnOnce() + Send + 'static,
) -> Result<Arc<Thread>, SpawnError> {
    let stack = platform().alloc_stack().ok_or(SpawnError::NoStack)?;
    let here = hal::cpu_id();
    let t = Arc::new(Thread::new(
        name,
        priority,
        bound.unwrap_or(here),
        Some(stack),
        Some(Box::new(f)),
        false,
    ));
    t.inner.lock().bound = bound;
    make_runnable(Arc::clone(&t));
    Ok(t)
}

/// Picks the CPU a waking thread should run on: its binding, else the
/// least loaded CPU, preferring the one it last ran on.
fn choose_cpu(last: CpuId, bound: Option<CpuId>) -> CpuId {
    if let Some(b) = bound {
        return b;
    }
    let load = |c: &PerCpu| c.runq.lock().len();
    let mut best = last;
    let mut best_load = cpu(last).map_or(usize::MAX, load);
    if best_load == 0 {
        return best;
    }
    for i in 0..online_cpus() {
        let Some(c) = cpu(CpuId(u32::try_from(i).unwrap_or(0))) else {
            continue;
        };
        let l = load(c);
        if l < best_load {
            best = c.id;
            best_load = l;
        }
    }
    best
}

/// Makes a sleeping or new thread runnable and queues it. Threads in any
/// other state are left alone (a racing wakeup already did it).
pub(crate) fn make_runnable(t: Arc<Thread>) {
    let mut inner = t.inner.lock();
    if !matches!(inner.state, State::Sleeping | State::New) {
        return;
    }
    inner.state = State::Runnable;
    let target = choose_cpu(inner.cpu, inner.bound);
    inner.cpu = target;
    let prio = inner.priority;
    let Some(c) = cpu(target) else {
        panic!("waking {} onto offline {target}", t.name());
    };
    drop(inner);
    c.runq.lock().push(t, prio);
    // Preempt the target if the newcomer beats what it runs. Reading the
    // remote current thread's priority is racy but only steers a hint.
    // SAFETY: `current` is always a live thread on an online CPU.
    let running = unsafe { &*c.current.get() };
    let running_prio = if running.idle {
        Priority::IDLE_THREAD
    } else {
        running
            .inner
            .try_lock()
            .map_or(Priority::KERNEL, |i| i.priority)
    };
    let local = c.id == hal::cpu_id();
    if prio.beats(running_prio) {
        c.request_resched();
        if !local {
            platform().kick(c.id);
        }
    }
    if local {
        // A queued thread may need the slice timer started.
        let irq = hal::irq::disable();
        platform().rearm_timer();
        hal::irq::restore(irq);
    }
}

/// Gives up the CPU to an equal or better runnable thread, if there is
/// one.
pub fn yield_now() {
    let irq = hal::irq::disable();
    switch_away();
    hal::irq::restore(irq);
}

/// Disables preemption on this CPU until the guard drops.
pub fn preempt_disable() -> PreemptGuard {
    let irq = hal::irq::disable();
    let c = this_cpu();
    c.preempt_off.set(c.preempt_off.get() + 1);
    hal::irq::restore(irq);
    PreemptGuard { _private: () }
}

/// Proof that preemption is disabled; see [`preempt_disable`].
#[derive(Debug)]
pub struct PreemptGuard {
    _private: (),
}

impl Drop for PreemptGuard {
    fn drop(&mut self) {
        let irq = hal::irq::disable();
        let c = this_cpu();
        let n = c.preempt_off.get() - 1;
        c.preempt_off.set(n);
        let resched = n == 0 && c.need_resched.load(Ordering::Relaxed);
        if resched && hal::irq::were_enabled(&irq) {
            switch_away();
        }
        hal::irq::restore(irq);
    }
}

/// When the current time slice ends, if threads are waiting for this CPU
/// (nothing to preempt otherwise). Interrupts must be masked.
#[must_use]
pub fn slice_deadline(slice: u64) -> Option<u64> {
    let c = this_cpu();
    // SAFETY: `current` is live while it runs on this CPU.
    let cur = unsafe { &*c.current.get() };
    if cur.idle || c.runq.lock().is_empty() {
        return None;
    }
    let since = cur.inner.lock().since;
    Some(since.saturating_add(slice))
}

/// Timer-interrupt bookkeeping: asks for a reschedule if the current
/// thread has used up its slice and others are waiting. Interrupts must
/// be masked.
pub fn tick(slice: u64) {
    let c = this_cpu();
    if slice_deadline(slice).is_some_and(|d| platform().now() >= d) {
        c.request_resched();
    }
}

/// Called on the way out of an interrupt handler, interrupts masked: if a
/// reschedule is pending and preemption is allowed, switches threads.
pub fn preempt_point() {
    let c = this_cpu();
    if c.preempt_off.get() == 0 && c.need_resched.load(Ordering::Relaxed) {
        switch_away();
    }
}

/// Switches to the best runnable thread. Interrupts must be masked. The
/// current thread's state says what happens to it (see the crate docs).
pub(crate) fn switch_away() {
    let c = this_cpu();
    c.need_resched.store(false, Ordering::Relaxed);
    // SAFETY: we own `current`'s reference until we hand it on below.
    let cur = unsafe { Arc::from_raw(c.current.get()) };
    let cur_ptr = Arc::as_ptr(&cur);
    let now = platform().now();

    let mut inner = cur.inner.lock();
    let state = inner.state;
    let elapsed = now.saturating_sub(inner.since);
    let prio = inner.priority;
    if state == State::Running {
        inner.state = State::Runnable;
    }
    // Release the thread's lock before touching any queue (lock order),
    // and never carry it across the switch.
    drop(inner);
    cur.runtime.fetch_add(elapsed, Ordering::Relaxed);
    let owned = match state {
        // The idle thread is never queued; it is the fallback.
        State::Running if cur.idle => None,
        State::Running => {
            c.runq.lock().push(Arc::clone(&cur), prio);
            Some(cur)
        }
        State::Sleeping | State::Runnable | State::Dead => Some(cur),
        State::New => unreachable!("a new thread cannot be running"),
    };

    let next = pick_next(c);
    let next_ptr = Arc::as_ptr(&next);
    if ptr::eq(next_ptr, cur_ptr) {
        // Still the best: keep running. Both references are ours.
        let mut inner = next.inner.lock();
        inner.state = State::Running;
        inner.since = now;
        drop(inner);
        drop(owned);
        c.current.set(Arc::into_raw(next));
        platform().rearm_timer();
        return;
    }

    // Wait until the CPU that last ran `next` has saved its registers.
    while next.on_cpu.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    next.on_cpu.store(true, Ordering::Relaxed);
    {
        let mut inner = next.inner.lock();
        inner.state = State::Running;
        inner.cpu = c.id;
        inner.since = now;
    }
    next.switches.fetch_add(1, Ordering::Relaxed);
    // `owned` is dropped by the incoming side unless the thread is dead,
    // in which case the reaper takes it.
    let keep = match owned {
        Some(t) if state == State::Running => {
            // Queued already; the queue holds its own reference.
            drop(t);
            None
        }
        other => other,
    };
    c.prev.set(cur_ptr);
    c.prev_owned.set(keep);
    c.current.set(Arc::into_raw(next));

    // SAFETY: interrupts are masked; `next`'s context was saved by a
    // switch or made by `Context::init`, its stack is mapped, and no other
    // CPU runs it (`on_cpu` was clear). `cur_ptr` stays valid: the run
    // queue, a wait queue, or `prev_owned` holds a reference.
    unsafe {
        hal::switch((*cur_ptr).context.get(), (*next_ptr).context.get());
    }
    finish_switch();
    platform().rearm_timer();
}

/// Takes the next thread for `c`: its own queue, then another CPU's, then
/// its idle thread.
fn pick_next(c: &PerCpu) -> Arc<Thread> {
    if let Some(t) = c.runq.lock().pop() {
        return t;
    }
    if let Some(t) = steal(c) {
        return t;
    }
    let idle = c.idle.get();
    // SAFETY: the idle thread's reference is held by `idle` forever; give
    // the caller its own.
    unsafe {
        Arc::increment_strong_count(idle);
        Arc::from_raw(idle)
    }
}

/// Takes a queued, unbound thread from the busiest other CPU.
fn steal(c: &PerCpu) -> Option<Arc<Thread>> {
    let mut busiest: Option<(&PerCpu, usize)> = None;
    for i in 0..online_cpus() {
        let Some(o) = cpu(CpuId(u32::try_from(i).unwrap_or(0))) else {
            continue;
        };
        if o.id == c.id {
            continue;
        }
        let n = o.runq.lock().len();
        if n > busiest.map_or(0, |b| b.1) {
            busiest = Some((o, n));
        }
    }
    let (o, _) = busiest?;
    let t = o
        .runq
        .lock()
        .pop_if(|t| t.inner.try_lock().is_some_and(|i| i.bound.is_none()))?;
    t.inner.lock().cpu = c.id;
    Some(t)
}

/// Completes a switch on the incoming side: releases the outgoing thread
/// to other CPUs and disposes of it if it died.
fn finish_switch() {
    let c = this_cpu();
    let prev = c.prev.replace(ptr::null());
    let owned = c.prev_owned.take();
    if prev.is_null() {
        return;
    }
    // SAFETY: `prev` is kept alive by a queue or by `owned`.
    let prev_thread = unsafe { &*prev };
    // Release: everything the outgoing thread did, and its saved context,
    // is visible to the next CPU that resumes it.
    prev_thread.on_cpu.store(false, Ordering::Release);
    if let Some(t) = owned
        && t.inner.lock().state == State::Dead
    {
        ZOMBIES.lock().push_back(t);
        REAPER.wake_one();
    }
}

/// The first code a new thread runs.
pub(crate) extern "C" fn thread_start(_: usize) -> ! {
    finish_switch();
    let entry = current().entry.lock().take();
    hal::irq::enable();
    if let Some(f) = entry {
        f();
    }
    exit()
}

/// Ends the current thread.
pub fn exit() -> ! {
    let me = current();
    // Release pairs with `has_exited`'s Acquire.
    me.exited.store(true, Ordering::Release);
    me.joiners.wake_all();
    let _irq = hal::irq::disable();
    me.inner.lock().state = State::Dead;
    switch_away();
    unreachable!("a dead thread was resumed");
}

static ZOMBIES: SpinLock<ThreadList> = SpinLock::new(ThreadList::new());
static REAPER: WaitQueue = WaitQueue::new();

/// Frees the stacks of exited threads. Runs as its own thread.
pub fn reaper() -> ! {
    loop {
        REAPER.wait_until(|| !ZOMBIES.lock().is_empty());
        while let Some(t) = ZOMBIES.lock().pop_front() {
            // The thread is off every CPU: `finish_switch` queued it only
            // after clearing `on_cpu`.
            if let Some(stack) = t.stack.lock().take() {
                platform().free_stack(stack);
            }
        }
    }
}

fn idle_loop() {
    loop {
        let irq = hal::irq::disable();
        let c = this_cpu();
        if c.runq.lock().is_empty() && !c.need_resched.load(Ordering::Relaxed) {
            hal::wait_for_interrupt();
        }
        switch_away();
        hal::irq::restore(irq);
    }
}

/// A snapshot of one CPU, for diagnostics.
#[derive(Clone, Copy, Debug)]
pub struct CpuSnapshot {
    /// The CPU.
    pub id: CpuId,
    /// Threads waiting on its run queue.
    pub queued: usize,
}

/// Per-CPU snapshots.
pub fn snapshot() -> impl Iterator<Item = CpuSnapshot> {
    (0..online_cpus()).filter_map(|i| {
        let c = cpu(CpuId(u32::try_from(i).ok()?))?;
        Some(CpuSnapshot {
            id: c.id,
            queued: c.runq.lock().len(),
        })
    })
}
