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
//! 3. A thread is queued only once it is off every CPU: a wakeup waits for
//!    a thread still saving its registers elsewhere (`on_cpu`), and
//!    stealing skips such threads. So a switching CPU never waits for
//!    another CPU.
//! 4. After the switch, the incoming side clears the outgoing thread's
//!    `on_cpu` ([`finish_switch`]).
//!
//! Locks nest as wait queue → thread → run queue.
//!
//! An idle CPU that has been switching often spins for a while before
//! halting, and a waker skips the IPI to an idle CPU that is still
//! spinning: in rapid hand-offs between CPUs, waking from a halt would cost
//! more than the work. A CPU that switches rarely halts at once.
//!
//! Behavior reference: sys/kern/sched_ule.c, sys/kern/kern_synch.c,
//! sys/kern/kern_thread.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod context;
mod list;
pub mod lock;
pub mod pctcpu;
mod runq;
mod thread;
mod ts;
mod wait;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::Cell;
use core::fmt;
use core::ptr;
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU16, AtomicUsize, Ordering, fence,
};

use hal::{CpuId, MAX_CPUS};
use sync::{Once, SpinLock};

pub use crate::context::{IrqCx, Sleepable};
use crate::list::ThreadList;
use crate::runq::RunQueue;
pub use crate::thread::{Name, Priority, Report, State, Thread, Tid};
pub use crate::wait::{Interrupted, WaitQueue};

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
    /// A fresh kernel stack of at least `min_bytes`, or the platform's
    /// default size when `None`.
    fn alloc_stack(&self, min_bytes: Option<usize>) -> Option<Stack>;
    /// Frees a stack from [`Platform::alloc_stack`]; never called on the
    /// stack in use.
    fn free_stack(&self, stack: Stack);
    /// A monotonic clock for CPU-time accounting.
    fn now(&self) -> u64;
    /// [`Platform::now`]'s ticks per second.
    fn frequency(&self) -> u64;
    /// Interrupts `cpu` so it notices its run queue changed: on arrival it
    /// re-arms its timer and passes a preemption point (which switches if
    /// [`PerCpu::request_resched`] was called).
    fn kick(&self, cpu: CpuId);
    /// Reprograms this CPU's event timer because what it waits for may
    /// have changed (a switch, or a newly queued thread). Interrupts are
    /// masked.
    fn rearm_timer(&self);
    /// Points the MMU at kernel-only tables (no user half), so a CPU can
    /// let go of the user address space it had loaded. Interrupts are
    /// masked.
    fn load_kernel_tables(&self);
}

/// A user address space, as the scheduler sees one: something to load
/// into the MMU when a thread running in it is switched in.
pub trait AddressSpace: Send + Sync {
    /// Makes this the address space the MMU translates user addresses
    /// with, on this CPU. Called with interrupts masked.
    fn activate(&self);
    /// This CPU is about to load another address space instead. Called
    /// with interrupts masked.
    fn deactivate(&self) {}
    /// The concrete type, for the kernel's own use.
    fn as_any(&self) -> &dyn core::any::Any;
}

static PLATFORM: Once<&'static dyn Platform> = Once::new();

fn platform() -> &'static dyn Platform {
    *PLATFORM.get().expect("scheduler used before init")
}

/// The platform clock, or 0 before [`init`] (and on the host).
fn platform_now() -> u64 {
    PLATFORM.get().map_or(0, |p| p.now())
}

/// The platform clock in FreeBSD's `hz` ticks ([`pctcpu::HZ`]), or 0
/// before [`init`].
fn hz_now() -> u64 {
    PLATFORM.get().map_or(0, |p| {
        let f = u128::from(p.frequency().max(1));
        u64::try_from(u128::from(p.now()) * u128::from(pctcpu::HZ) / f).unwrap_or(u64::MAX)
    })
}

/// The platform clock (see [`Platform::now`]); 0 before [`init`].
#[must_use]
pub fn now() -> u64 {
    platform_now()
}

/// A thread leaves its sleep (woken, interrupted, or never gone): its wait
/// message and channel are cleared, as `sleepq_resume_thread` does, and
/// its CPU-share window passes over the time asleep.
pub(crate) fn mark_awake(inner: &mut thread::Inner, state: State) {
    inner.state = state;
    inner.wmesg = "";
    inner.wchan = 0;
    inner.sintr = false;
    inner.slept_at = 0;
    inner.pct.update(hz_now(), false);
}

/// Names the current thread's sleeps `msg` (FreeBSD's wait message, as
/// `piperd` or `nanslp`) until the guard drops, which brings back the
/// previous one.
pub fn wait_message(msg: &'static str) -> WaitMessage {
    let me = current();
    let previous = core::mem::replace(&mut me.inner.lock().next_wmesg, msg);
    WaitMessage { previous }
}

/// What the kernel learns of each named sleep, once the thread is awake
/// again with interrupts back as they were: its wait message and when it
/// began, in platform time (for ktrace's `KTR_CSW`).
pub type SleepHook = fn(&'static str, u64);

static SLEEP_HOOK: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Sets the [`SleepHook`].
pub fn set_sleep_hook(hook: SleepHook) {
    SLEEP_HOOK.store(hook as *mut (), Ordering::Release);
}

/// Tells the [`SleepHook`] of a sleep on `wmesg` that began at `since`.
pub(crate) fn slept(wmesg: &'static str, since: u64) {
    let h = SLEEP_HOOK.load(Ordering::Acquire);
    if h.is_null() || wmesg.is_empty() {
        return;
    }
    // SAFETY: only `set_sleep_hook` stores here, a `SleepHook`.
    let hook: SleepHook = unsafe { core::mem::transmute::<*mut (), SleepHook>(h) };
    hook(wmesg, since);
}

/// Restores the previous wait message when dropped (see
/// [`wait_message`]).
#[must_use = "the wait message lasts as long as the guard"]
pub struct WaitMessage {
    previous: &'static str,
}

impl Drop for WaitMessage {
    fn drop(&mut self) {
        current().inner.lock().next_wmesg = self.previous;
    }
}

impl fmt::Debug for WaitMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitMessage").finish_non_exhaustive()
    }
}

/// Where a statistics-clock tick found its CPU: FreeBSD's `CP_*` states
/// but `CP_NICE`, which the kernel tells from the process's nice value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickState {
    /// A thread in user mode.
    User,
    /// A thread in the kernel.
    System,
    /// An interrupt thread.
    Interrupt,
    /// The idle thread.
    Idle,
}

/// A statistics-clock tick on this CPU (FreeBSD's `statclock`), which
/// interrupted user mode if `user`: charged to the running thread (its
/// user, system, or interrupt ticks) and to its CPU-share window
/// (`sched_clock`). Returns where it found the CPU. Interrupt context.
pub fn stat_tick(user: bool) -> TickState {
    let t = current();
    if t.idle {
        return TickState::Idle;
    }
    let Some(mut inner) = t.inner.try_lock() else {
        // Being changed by this CPU itself: a tick in the kernel.
        t.stat_ticks[1].fetch_add(1, Ordering::Relaxed);
        return TickState::System;
    };
    let state = if user {
        TickState::User
    } else if inner.base.beats(Priority::REALTIME) {
        TickState::Interrupt
    } else {
        TickState::System
    };
    inner.pct.update(hz_now(), true);
    drop(inner);
    let i = match state {
        TickState::User => 0,
        TickState::System => 1,
        TickState::Interrupt | TickState::Idle => 2,
    };
    t.stat_ticks[i].fetch_add(1, Ordering::Relaxed);
    state
}

/// Time-sharing accounting for `ev` (see [`thread::Event`]).
fn account(inner: &mut thread::Inner, ev: thread::Event, now: u64) -> Priority {
    let f = platform().frequency();
    inner.account(ev, now, f * ts::WINDOW_SECONDS, f * ts::WAIT_STEP_MS / 1000)
}

/// A run queue whose length can be read without its lock.
struct LockedRunQueue {
    q: SpinLock<RunQueue>,
    /// The queue's length as of the last unlock.
    len: AtomicUsize,
}

impl LockedRunQueue {
    const fn new() -> Self {
        Self {
            q: SpinLock::new(RunQueue::new()),
            len: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> RunQueueGuard<'_> {
        RunQueueGuard {
            q: self.q.lock(),
            len: &self.len,
        }
    }

    /// The length at the last unlock (racy; exact under the lock).
    fn len_hint(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }
}

/// A locked run queue; publishes its length when unlocked.
struct RunQueueGuard<'a> {
    q: sync::SpinGuard<'a, RunQueue>,
    len: &'a AtomicUsize,
}

impl core::ops::Deref for RunQueueGuard<'_> {
    type Target = RunQueue;
    fn deref(&self) -> &RunQueue {
        &self.q
    }
}

impl core::ops::DerefMut for RunQueueGuard<'_> {
    fn deref_mut(&mut self) -> &mut RunQueue {
        &mut self.q
    }
}

impl Drop for RunQueueGuard<'_> {
    fn drop(&mut self) {
        // Before the lock is released (fields drop after this).
        self.len.store(self.q.len(), Ordering::Relaxed);
    }
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
    /// The previous thread is moving to the CPU it is bound to: once it is
    /// off this one, it is woken there.
    prev_moving: Cell<bool>,
    runq: LockedRunQueue,
    need_resched: AtomicBool,
    /// When the running thread's slice started, in platform time.
    slice_start: Cell<u64>,
    /// The running thread's priority, for remote wakeups deciding whether
    /// to preempt.
    current_prio: AtomicU8,
    /// Relative capacity (1024 for the fastest CPUs).
    capacity: AtomicU16,
    /// The CPU shares its core with SMT siblings.
    smt: AtomicBool,
    /// The idle thread is halted (or about to): a waker must interrupt it.
    /// While the idle thread spins this is false and a waker need not.
    halted: AtomicBool,
    /// Context switches in the current and the previous window.
    switches: Cell<(u32, u32)>,
    /// When the current switch-counting window ends, in platform time.
    window_end: Cell<u64>,
    /// The address space loaded in the MMU. Kernel threads (with none)
    /// leave it loaded, as FreeBSD's do, so switching between a process
    /// and kernel threads reloads nothing.
    space: Cell<Option<Arc<dyn AddressSpace>>>,
    /// The user thread registers as last saved or loaded here, so a switch
    /// writes only those that differ (kernel threads leave them be).
    user_tls: Cell<[u64; 2]>,
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
    hal::critical::set_preempt_handler(owed_preempt);
}

/// A critical section ended with a preemption owed: switch now if still
/// wanted and allowed.
fn owed_preempt() {
    let irq = hal::irq::disable();
    hal::critical::owe_preempt(false);
    if hal::irq::were_enabled(&irq)
        && !hal::interrupt::active()
        && hal::critical::depth() == 0
        && this_cpu().need_resched.load(Ordering::Relaxed)
    {
        switch_away();
    }
    hal::irq::restore(irq);
}

fn new_percpu(id: CpuId) -> &'static mut PerCpu {
    Box::leak(Box::new(PerCpu {
        id,
        current: Cell::new(ptr::null()),
        idle: Cell::new(ptr::null()),
        prev: Cell::new(ptr::null()),
        prev_owned: Cell::new(None),
        prev_moving: Cell::new(false),
        runq: LockedRunQueue::new(),
        slice_start: Cell::new(0),
        need_resched: AtomicBool::new(false),
        current_prio: AtomicU8::new(Priority::KERNEL.get()),
        capacity: AtomicU16::new(1024),
        smt: AtomicBool::new(false),
        halted: AtomicBool::new(false),
        switches: Cell::new((0, 0)),
        window_end: Cell::new(0),
        space: Cell::new(None),
        user_tls: Cell::new([0; 2]),
    }))
}

/// Makes `t` the thread already running on this CPU.
fn adopt_running(pcpu: &PerCpu, t: Arc<Thread>) {
    {
        let mut inner = t.inner.lock();
        inner.state = State::Running;
        inner.since = platform().now();
        pcpu.slice_start.set(inner.since);
        pcpu.current_prio
            .store(inner.priority.get(), Ordering::Relaxed);
    }
    t.on_cpu.store(true, Ordering::Relaxed);
    set_current(pcpu, Arc::into_raw(t));
}

/// Makes `t` this CPU's running thread, for the scheduler and for
/// [`current`]. Interrupts must be masked.
fn set_current(pcpu: &PerCpu, t: *const Thread) {
    pcpu.current.set(t);
    // SAFETY: interrupts are masked (our contract), and `t` holds the
    // reference `current` owns while the thread runs here.
    unsafe { hal::set_current_thread(t.cast()) };
}

fn publish(pcpu: &'static mut PerCpu) {
    let id = pcpu.id;
    // SAFETY: the block is leaked, so it outlives every reader.
    unsafe { hal::set_cpu_local(ptr::from_mut(pcpu).cast()) };
    CPUS[id.index()].store(pcpu, Ordering::Release);
    ONLINE.fetch_add(1, Ordering::AcqRel);
}

/// Starts scheduling on the boot CPU: the code running now becomes a
/// thread named `name`, and an idle thread is created. Interrupts must be
/// masked.
pub fn init_cpu(name: &str) {
    let id = hal::cpu_id();
    let pcpu = new_percpu(id);
    let boot = Arc::new(Thread::new(name, Priority::KERNEL, id, None, None, false));
    adopt_running(pcpu, boot);

    let stack = platform().alloc_stack(None).expect("idle thread stack");
    let idle = Arc::new(Thread::new(
        "idle",
        Priority::IDLE_THREAD,
        id,
        Some(stack),
        Some(thread::Entry::Closure(Box::new(|_: &Sleepable| {
            idle_loop()
        }))),
        true,
    ));
    idle.inner.lock().bound = Some(id);
    pcpu.idle.set(Arc::into_raw(idle));
    publish(pcpu);
}

/// Starts scheduling on another CPU: the code running now (on a stack the
/// caller keeps forever) becomes this CPU's idle thread, and the CPU
/// starts taking work. Interrupts must be masked; they are enabled here.
pub fn run_idle() -> ! {
    let id = hal::cpu_id();
    let pcpu = new_percpu(id);
    let idle = Arc::new(Thread::new(
        "idle",
        Priority::IDLE_THREAD,
        id,
        None,
        None,
        true,
    ));
    idle.inner.lock().bound = Some(id);
    pcpu.idle.set(Arc::into_raw(Arc::clone(&idle)));
    adopt_running(pcpu, idle);
    publish(pcpu);
    hal::irq::enable();
    idle_loop();
    unreachable!("the idle loop returned")
}

/// The running thread.
#[must_use]
pub fn current() -> &'static Thread {
    // The HAL's read cannot be split by a preemption (which could move the
    // thread and return another CPU's).
    let p = hal::current_thread().cast::<Thread>();
    #[cfg(not(target_os = "none"))]
    if p.is_null() {
        return host::current();
    }
    assert!(!p.is_null(), "scheduler not running on this CPU");
    // SAFETY: `current` always holds a live reference while the thread
    // runs, and the running thread cannot be freed under itself.
    unsafe { &*p }
}

/// A new reference to the running thread.
#[must_use]
pub fn current_thread() -> Arc<Thread> {
    current_arc()
}

/// A new reference to the running thread.
pub(crate) fn current_arc() -> Arc<Thread> {
    let p = ptr::from_ref(current());
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
    /// No memory for the FP/SIMD save area.
    NoFpuArea,
}

/// Options for a new thread; see [`Builder::spawn`].
#[derive(Clone, Copy, Debug)]
pub struct Builder<'a> {
    name: &'a str,
    priority: Priority,
    bound: Option<CpuId>,
    fpu: bool,
    /// The FP/SIMD registers start as a copy of the creating thread's.
    fpu_inherit: bool,
    stack: Option<usize>,
}

impl<'a> Builder<'a> {
    /// A kernel thread named `name` at [`Priority::KERNEL`], unbound,
    /// without FP/SIMD state.
    #[must_use]
    pub const fn new(name: &'a str) -> Self {
        Self {
            name,
            priority: Priority::KERNEL,
            bound: None,
            fpu: false,
            fpu_inherit: false,
            stack: None,
        }
    }

    /// Gives the thread a stack of at least `bytes` rather than the
    /// default (for deep recursion such as the AML interpreter).
    #[must_use]
    pub const fn stack(mut self, bytes: usize) -> Self {
        self.stack = Some(bytes);
        self
    }

    /// Runs at `p`.
    #[must_use]
    pub const fn priority(mut self, p: Priority) -> Self {
        self.priority = p;
        self
    }

    /// Runs only on `cpu`.
    #[must_use]
    pub const fn bind(mut self, cpu: CpuId) -> Self {
        self.bound = Some(cpu);
        self
    }

    /// Gives the thread FP/SIMD registers of its own, saved and restored
    /// across switches.
    #[must_use]
    pub const fn fpu(mut self) -> Self {
        self.fpu = true;
        self
    }

    /// Gives the thread FP/SIMD registers of its own, starting as a copy of
    /// the calling thread's (a forked user thread's). The kernel itself
    /// never uses them, so the live registers are the caller's.
    #[must_use]
    pub const fn fpu_inherit(mut self) -> Self {
        self.fpu = true;
        self.fpu_inherit = true;
        self
    }

    /// Starts the thread running `f`, which gets the thread's [`Sleepable`].
    pub fn spawn(
        self,
        f: impl FnOnce(&Sleepable) + Send + 'static,
    ) -> Result<Arc<Thread>, SpawnError> {
        self.start(thread::Entry::Closure(Box::new(f)))
    }

    /// Starts the thread running `f(arg)`, which never returns: a thread
    /// that leaves for user mode, and ends in a system call on a fresh
    /// stack. `arg` is typically an `Arc::into_raw`.
    pub fn spawn_raw(
        self,
        f: fn(&Sleepable, usize) -> !,
        arg: usize,
    ) -> Result<Arc<Thread>, SpawnError> {
        self.start(thread::Entry::Raw(f, arg))
    }

    fn start(self, entry: thread::Entry) -> Result<Arc<Thread>, SpawnError> {
        let fpu = if self.fpu {
            let area = thread::FpuArea::new().ok_or(SpawnError::NoFpuArea)?;
            if self.fpu_inherit {
                // SAFETY: the area is this size and alignment; saving only
                // reads the registers.
                unsafe { hal::fpu::save(area.as_ptr()) };
            }
            Some(area)
        } else {
            None
        };
        let stack = platform()
            .alloc_stack(self.stack)
            .ok_or(SpawnError::NoStack)?;
        let here = hal::cpu_id();
        let t = Arc::new(Thread::with_fpu(
            self.name,
            self.priority,
            self.bound.unwrap_or(here),
            Some(stack),
            Some(entry),
            false,
            fpu,
        ));
        t.inner.lock().bound = self.bound;
        make_runnable(Arc::clone(&t));
        Ok(t)
    }
}

/// Starts a kernel thread running `f` at `priority`.
pub fn spawn(
    name: &str,
    priority: Priority,
    f: impl FnOnce(&Sleepable) + Send + 'static,
) -> Result<Arc<Thread>, SpawnError> {
    Builder::new(name).priority(priority).spawn(f)
}

/// Starts a kernel thread bound to `cpu` (or unbound with `None`).
pub fn spawn_on(
    name: &str,
    priority: Priority,
    bound: Option<CpuId>,
    f: impl FnOnce(&Sleepable) + Send + 'static,
) -> Result<Arc<Thread>, SpawnError> {
    let b = Builder::new(name).priority(priority);
    match bound {
        Some(c) => b.bind(c),
        None => b,
    }
    .spawn(f)
}

/// Sets `cpu`'s relative capacity (1024 = the fastest CPUs).
pub fn set_capacity(cpu: CpuId, capacity: u16) {
    if let Some(c) = self::cpu(cpu) {
        c.capacity.store(capacity, Ordering::Relaxed);
    }
}

/// Records whether `cpu` shares its core with SMT siblings.
pub fn set_smt(cpu: CpuId, smt: bool) {
    if let Some(c) = self::cpu(cpu) {
        c.smt.store(smt, Ordering::Relaxed);
    }
}

/// Whether a CPU with (`load`, `capacity`) is a better place for a thread
/// than one with (`best_load`, `best_capacity`): less loaded, or as loaded
/// and faster.
fn better_cpu(load: usize, capacity: u16, best_load: usize, best_capacity: u16) -> bool {
    load < best_load || (load == best_load && capacity > best_capacity)
}

/// Binds the current thread to `cpu`, or lets it run anywhere (`None`),
/// and moves it there now if it runs elsewhere.
pub fn move_to(cpu: Option<CpuId>) {
    current().inner.lock().bound = cpu;
    if cpu.is_some_and(|c| c != hal::cpu_id()) {
        yield_now();
    }
}

/// Picks the CPU a waking thread should run on: its binding, else the
/// least loaded CPU, preferring the one it last ran on.
fn choose_cpu(last: CpuId, bound: Option<CpuId>) -> CpuId {
    if let Some(b) = bound {
        return b;
    }
    // Waiting threads plus the running one, unless the CPU is idle.
    let load = |c: &PerCpu| {
        let busy = c.current_prio.load(Ordering::Relaxed) != Priority::IDLE_THREAD.get();
        c.runq.len_hint() + usize::from(busy)
    };
    let capacity = |c: &PerCpu| c.capacity.load(Ordering::Relaxed);
    let mut best = last;
    let (mut best_load, mut best_capacity) =
        cpu(last).map_or((usize::MAX, 0), |c| (load(c), capacity(c)));
    // Its last CPU, if idle and of the fastest class: warm caches.
    if best_load == 0 && best_capacity == 1024 {
        return best;
    }
    for i in 0..online_cpus() {
        let Some(c) = cpu(CpuId(u32::try_from(i).unwrap_or(0))) else {
            continue;
        };
        let (l, cap) = (load(c), capacity(c));
        if better_cpu(l, cap, best_load, best_capacity) {
            best = c.id;
            best_load = l;
            best_capacity = cap;
        }
    }
    best
}

/// Makes a sleeping or new thread runnable and queues it. Threads in any
/// other state are left alone (a racing wakeup already did it).
pub(crate) fn make_runnable(t: Arc<Thread>) {
    // A thread woken while still switching out on some CPU is queued only
    // once its registers are saved. So a queued thread is never on a CPU
    // (except one requeued by its own CPU, which only that CPU takes), and
    // a switching CPU never waits for another: no cycle of CPUs waiting on
    // each other's outgoing threads can form. That CPU is mid-switch with
    // interrupts masked and waits for nothing, so this spin is short.
    while t.on_cpu.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let mut inner = t.inner.lock();
    if !matches!(inner.state, State::Sleeping | State::New) {
        return;
    }
    mark_awake(&mut inner, State::Runnable);
    // Time asleep lowers usage, raising a time-sharing thread's priority.
    let prio = account(&mut inner, thread::Event::Woken, platform().now());
    let target = choose_cpu(inner.cpu, inner.bound);
    inner.cpu = target;
    let Some(c) = cpu(target) else {
        panic!("waking {} onto offline {target}", t.name());
    };
    // Queue under the thread lock, so a priority change never sees the
    // thread runnable but on no queue.
    let was_empty = {
        let mut q = c.runq.lock();
        let empty = q.is_empty();
        q.push(Arc::clone(&t), prio);
        empty
    };
    drop(inner);
    drop(t);
    // Preempt the target if the newcomer beats what it runs (racy, but
    // only a hint: the target rechecks its queue when it reschedules).
    let running_prio = Priority::new(c.current_prio.load(Ordering::Relaxed));
    let local = c.id == hal::cpu_id();
    let preempt = prio.beats(running_prio);
    if preempt {
        c.request_resched();
    }
    // A remote CPU needs a nudge to preempt, or to start its slice timer
    // now that it has a waiting thread.
    if !local && (preempt || was_empty) {
        // Pairs with the idle loop's fence: an idle CPU that is still
        // spinning will see the queued thread without an IPI.
        fence(Ordering::SeqCst);
        let spinning = c.current_prio.load(Ordering::Relaxed) == Priority::IDLE_THREAD.get()
            && !c.halted.load(Ordering::Relaxed);
        if !spinning {
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

/// Changes `t`'s effective priority, requeueing it if it is waiting to
/// run. The caller holds `t`'s lock (`inner`).
pub(crate) fn set_priority_locked(t: &Thread, inner: &mut thread::Inner, p: Priority) {
    if inner.priority == p {
        return;
    }
    inner.priority = p;
    match inner.state {
        State::Runnable => {
            if let Some(c) = cpu(inner.cpu) {
                let mut q = c.runq.lock();
                if let Some(arc) = q.remove(t) {
                    q.push(arc, p);
                }
                drop(q);
                if p.beats(Priority::new(c.current_prio.load(Ordering::Relaxed))) {
                    c.request_resched();
                    if c.id != hal::cpu_id() {
                        platform().kick(c.id);
                    }
                }
            }
        }
        State::Running => {
            if let Some(c) = cpu(inner.cpu) {
                c.current_prio.store(p.get(), Ordering::Relaxed);
            }
        }
        State::New | State::Sleeping | State::Dead => {}
    }
}

/// Sets the current thread's base priority.
pub fn set_base_priority(p: Priority) {
    let me = current();
    let mut inner = me.inner.lock();
    let lent = inner.priority.beats(inner.floor());
    inner.base = p;
    let floor = inner.floor();
    if !lent || floor.beats(inner.priority) {
        set_priority_locked(me, &mut inner, floor);
    }
}

/// Sets `t`'s base priority to `p` (`rtprio`): in the time-sharing range
/// it moves with the thread's CPU use from there, outside it (real-time
/// and idle classes) it stays put.
pub fn set_thread_priority(t: &Thread, p: Priority) {
    let mut inner = t.inner.lock();
    let timeshare = p >= Priority::TIMESHARE && p < Priority::IDLE;
    inner.ts = if timeshare {
        let nice = p.get() - Priority::TIMESHARE.get();
        Some(inner.ts.unwrap_or_else(|| ts::Usage::new(nice, 0)))
    } else {
        None
    };
    let lent = inner.priority.beats(inner.floor());
    inner.base = p;
    let floor = inner.floor();
    if !lent || floor.beats(inner.priority) {
        set_priority_locked(t, &mut inner, floor);
    }
}

/// Lends `t` priority `p` while it holds user mutexes better threads
/// wait for, or ends the lending (`None`): FreeBSD's
/// `sched_lend_user_prio`. A better priority a kernel mutex's waiter lent
/// stays until that mutex is released.
pub fn lend_user_priority(t: &Thread, p: Option<Priority>) {
    let mut inner = t.inner.lock();
    if inner.user_lent == p {
        return;
    }
    let lent = inner.priority.beats(inner.floor());
    inner.user_lent = p;
    let floor = inner.floor();
    if floor != inner.priority && (!lent || floor.beats(inner.priority)) {
        set_priority_locked(t, &mut inner, floor);
    }
}

/// `t`'s priority in user mode: its base, or a better one user threads
/// lent it (FreeBSD's `td_user_pri`).
#[must_use]
pub fn user_priority(t: &Thread) -> Priority {
    t.inner.lock().floor()
}

/// The priority `t` asked for (FreeBSD's `td_base_user_pri`).
#[must_use]
pub fn base_priority(t: &Thread) -> Priority {
    t.inner.lock().base
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
    hal::critical::enter();
    PreemptGuard { _private: () }
}

/// Whether the current CPU has preemption disabled.
#[must_use]
pub fn preemption_disabled() -> bool {
    hal::critical::depth() > 0
}

/// Proof that preemption is disabled; see [`preempt_disable`].
#[derive(Debug)]
pub struct PreemptGuard {
    _private: (),
}

impl Drop for PreemptGuard {
    fn drop(&mut self) {
        // Runs an owed preemption (`owed_preempt`) at the outermost level.
        hal::critical::exit();
    }
}

/// The current thread's CPU time, in platform time units, its running
/// slice included (what `calcru` brings up to date for the caller;
/// [`Thread::runtime`] is as of the last switch).
#[must_use]
pub fn current_runtime() -> u64 {
    let t = current();
    let since = t.inner.lock().since;
    t.runtime() + platform().now().saturating_sub(since)
}

/// When the current time slice ends, if threads are waiting for this CPU
/// (nothing to preempt otherwise). Interrupts must be masked.
#[must_use]
pub fn slice_deadline(slice: u64) -> Option<u64> {
    let c = this_cpu();
    // SAFETY: `current` is live while it runs on this CPU.
    let cur = unsafe { &*c.current.get() };
    // A pending reschedule (held off by a preemption-disabled section)
    // needs no timer: it happens when the section ends.
    if cur.idle || c.need_resched.load(Ordering::Relaxed) || c.runq.len_hint() == 0 {
        return None;
    }
    Some(c.slice_start.get().saturating_add(slice))
}

/// Timer-interrupt bookkeeping: asks for a reschedule if the current
/// thread has used up its slice and others are waiting. Interrupts must
/// be masked.
pub fn tick(slice: u64) {
    let c = this_cpu();
    let now = platform().now();
    if slice_deadline(slice).is_some_and(|d| now >= d) {
        c.request_resched();
    }
    // Age the waiting time-sharing threads: their usage falls, so their
    // priority rises until they run.
    let mut q = c.runq.lock();
    q.reprioritize(|t| {
        t.inner
            .try_lock()
            .map(|mut i| account(&mut i, thread::Event::Waiting, now))
    });
}

/// Called on the way out of an interrupt handler, interrupts masked: if a
/// reschedule is pending and preemption is allowed, switches threads.
pub fn preempt_point() {
    let c = this_cpu();
    if c.need_resched.load(Ordering::Relaxed) {
        if hal::critical::depth() == 0 {
            switch_away();
        } else {
            // The critical section's end will switch.
            hal::critical::owe_preempt(true);
        }
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
    if !cur.idle {
        // `sched_switch`: the time since the last update was spent running.
        inner.pct.update(hz_now(), true);
        match state {
            State::Running => cur.involuntary.fetch_add(1, Ordering::Relaxed),
            _ => cur.voluntary.fetch_add(1, Ordering::Relaxed),
        };
    }
    let prio = account(
        &mut inner,
        thread::Event::Descheduled {
            requeued: state == State::Running && !cur.idle,
        },
        now,
    );
    // A running thread bound elsewhere goes there instead of back on this
    // CPU's queue: it is woken once it is off this CPU (`finish_switch`).
    let moving = state == State::Running && !cur.idle && inner.bound.is_some_and(|b| b != c.id);
    if moving {
        inner.state = State::Sleeping;
    } else if state == State::Running {
        inner.state = State::Runnable;
        if !cur.idle {
            // Queued under the thread lock (see `make_runnable`).
            c.runq.lock().push(Arc::clone(&cur), prio);
        }
    }
    c.prev_moving.set(moving);
    // Never carry the thread lock across the switch.
    drop(inner);
    cur.runtime.fetch_add(elapsed, Ordering::Relaxed);
    let owned = match state {
        // The idle thread is never queued; it is the fallback.
        State::Running if cur.idle => None,
        // Requeued (the queue has its own reference), or moving (owned
        // here until it is woken on its CPU).
        State::Running => Some(cur),
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
        c.slice_start.set(now);
        let p = account(&mut inner, thread::Event::Scheduled, now);
        c.current_prio.store(p.get(), Ordering::Relaxed);
        drop(inner);
        drop(owned);
        set_current(c, Arc::into_raw(next));
        platform().rearm_timer();
        return;
    }

    // Queued threads are off every CPU (see `make_runnable`), so this
    // never waits in practice; the Acquire pairs with the Release in
    // `finish_switch`, ordering the saved context before we load it.
    while next.on_cpu.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    next.on_cpu.store(true, Ordering::Relaxed);
    {
        let mut inner = next.inner.lock();
        inner.state = State::Running;
        // The incoming thread's window: it did not run meanwhile.
        inner.pct.update(hz_now(), false);
        inner.cpu = c.id;
        inner.since = now;
        c.slice_start.set(now);
        let p = account(&mut inner, thread::Event::Scheduled, now);
        c.current_prio.store(p.get(), Ordering::Relaxed);
    }
    next.switches.fetch_add(1, Ordering::Relaxed);
    count_switch(c, now);
    // `owned` is dropped by the incoming side unless the thread is dead,
    // in which case the reaper takes it.
    let keep = match owned {
        Some(t) if state == State::Running && !moving => {
            // Queued already; the queue holds its own reference.
            drop(t);
            None
        }
        other => other,
    };
    c.prev.set(cur_ptr);
    c.prev_owned.set(keep);
    set_current(c, Arc::into_raw(next));

    // SAFETY: interrupts are masked; `next`'s context was saved by a
    // switch or made by `Context::init`, its stack is mapped, and no other
    // CPU runs it (`on_cpu` was clear). `cur_ptr` stays valid: the run
    // queue, a wait queue, or `prev_owned` holds a reference.
    unsafe {
        if let Some(f) = &(*cur_ptr).fpu {
            hal::fpu::save(f.as_ptr());
        }
        // A user thread's thread registers may have changed (`sysarch`,
        // or user mode itself): keep them for its return.
        if (*(*cur_ptr).space.get()).is_some() {
            let regs = [hal::user::tls(), hal::user::tls2()];
            *(*cur_ptr).tls.get() = regs;
            c.user_tls.set(regs);
        }
        hal::switch((*cur_ptr).context.get(), (*next_ptr).context.get());
    }
    finish_switch();
    restore_fpu();
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
        // Not a thread still switching out (one its CPU requeued itself):
        // only that CPU may take it (see `make_runnable`).
        .pop_if(|t| {
            !t.on_cpu.load(Ordering::Acquire)
                && t.inner.try_lock().is_some_and(|i| i.bound.is_none())
        })?;
    t.inner.lock().cpu = c.id;
    Some(t)
}

/// Completes a switch on the incoming side: releases the outgoing thread
/// to other CPUs and disposes of it if it died.
fn finish_switch() {
    let c = this_cpu();
    load_space(c);
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
    let moving = c.prev_moving.replace(false);
    if let Some(t) = owned {
        if moving {
            // Off this CPU now: wake it where it is bound.
            make_runnable(t);
        } else if t.inner.lock().state == State::Dead {
            ZOMBIES.lock().push_back(t);
            REAPER.wake_one();
        }
    }
}

/// Loads the current thread's address space into the MMU, unless it has
/// none or it is loaded already. Interrupts are masked.
fn load_space(c: &PerCpu) {
    let t = current();
    // SAFETY: only the thread itself writes its `space` and `tls`, and it
    // is the current thread here (or its previous CPU's writes are ordered
    // before this by the `on_cpu` handoff).
    let (want, tls) = unsafe { (&*t.space.get(), *t.tls.get()) };
    let Some(want) = want else {
        return;
    };
    // A user thread: traps from user mode land on its stack, and its
    // thread registers are back (those that differ from what is loaded).
    let loaded = c.user_tls.get();
    // SAFETY: interrupts are masked and `t` is about to run here.
    unsafe {
        hal::user::set_kernel_stack(t.kstack_top);
        if loaded[0] != tls[0] {
            hal::user::set_tls(tls[0]);
        }
        if loaded[1] != tls[1] {
            hal::user::set_tls2(tls[1]);
        }
    }
    c.user_tls.set(tls);
    let loaded = c.space.take();
    if loaded.as_ref().is_some_and(|l| Arc::ptr_eq(l, want)) {
        c.space.set(loaded);
        return;
    }
    if let Some(l) = &loaded {
        l.deactivate();
    }
    want.activate();
    c.space.set(Some(Arc::clone(want)));
    // The previously loaded space, if this was its last reference, goes
    // here; its owner tore its mappings down already, so little is left.
    drop(loaded);
}

/// Sets the current thread's user address space (at `exec`, or when a new
/// process's first thread starts) and loads it. With `None` the thread
/// runs on whatever is loaded, like a kernel thread. Returns the old one.
pub fn set_address_space(space: Option<Arc<dyn AddressSpace>>) -> Option<Arc<dyn AddressSpace>> {
    let irq = hal::irq::disable();
    let c = this_cpu();
    // SAFETY: the current thread writes its own `space` (and `tls`), with
    // interrupts masked so no switch reads them meanwhile.
    let old = unsafe {
        let t = current();
        if (*t.space.get()).is_some() {
            // What it runs with now, which may not be what was loaded: kept,
            // so the load below (and later switches) compare against it.
            let regs = [hal::user::tls(), hal::user::tls2()];
            *t.tls.get() = regs;
            c.user_tls.set(regs);
        }
        core::mem::replace(&mut *t.space.get(), space)
    };
    load_space(c);
    hal::irq::restore(irq);
    old
}

/// Drops this CPU's hold on the address space it has loaded, when the
/// current thread runs in none (a process's last thread exited, or a
/// test is done). `to_kernel` must first point the MMU at kernel-only
/// tables, so nothing still walks the space's tables when they are freed.
pub fn unload_address_space(to_kernel: impl FnOnce()) {
    let irq = hal::irq::disable();
    let c = this_cpu();
    // SAFETY: only the current thread writes its own `space`.
    let mine = unsafe { (*current().space.get()).is_some() };
    let loaded = if mine { None } else { c.space.take() };
    if let Some(l) = &loaded {
        to_kernel();
        l.deactivate();
    }
    hal::irq::restore(irq);
    drop(loaded);
}

/// Sets what the kernel runs the current thread for (FreeBSD's
/// `td_proc`); returns the old value.
pub fn set_owner(
    owner: Option<Arc<dyn core::any::Any + Send + Sync>>,
) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
    // SAFETY: only the current thread writes its own `owner`.
    unsafe { core::mem::replace(&mut *current().owner.get(), owner) }
}

/// What the kernel runs the current thread for, if it said.
#[must_use]
pub fn owner() -> Option<Arc<dyn core::any::Any + Send + Sync>> {
    // SAFETY: only the current thread writes it, and that is us.
    unsafe { (*current().owner.get()).clone() }
}

/// Runs `f` on what the kernel runs the current thread for, borrowed: no
/// reference count changes, for paths as short as `getpid`. `f` must not
/// call [`set_owner`].
pub fn with_owner<R>(f: impl FnOnce(Option<&(dyn core::any::Any + Send + Sync)>) -> R) -> R {
    // SAFETY: only the current thread writes it, and that is us; `f`
    // does not, so the value outlives the borrow.
    f(unsafe { (*current().owner.get()).as_deref() })
}

/// Runs `f` on the current thread's user address space, borrowed (no
/// reference count changes). `f` must not call [`set_address_space`].
pub fn with_address_space<R>(f: impl FnOnce(Option<&dyn AddressSpace>) -> R) -> R {
    // SAFETY: only the current thread writes it, and `f` does not, so the
    // value outlives the borrow; a switch only reads it.
    f(unsafe { (*current().space.get()).as_deref() })
}

/// The current thread's user address space.
#[must_use]
pub fn address_space() -> Option<Arc<dyn AddressSpace>> {
    // SAFETY: only the current thread writes it, and that is us.
    unsafe { (*current().space.get()).clone() }
}

/// Loads the current thread's FP/SIMD registers, if it has its own.
fn restore_fpu() {
    if let Some(f) = &current().fpu {
        // SAFETY: the area holds this thread's state, saved when it last
        // left a CPU (or the initial state); `on_cpu` ordered that save
        // before this load.
        unsafe { hal::fpu::restore(f.as_ptr()) };
    }
}

/// The first code a new thread runs.
pub(crate) extern "C" fn thread_start(_: usize) -> ! {
    // The switch that got here did not return into `switch_away`, so do
    // its epilogue: release the previous thread and arm the slice timer.
    finish_switch();
    restore_fpu();
    platform().rearm_timer();
    let entry = current().entry.lock().take();
    hal::irq::enable();
    // SAFETY: a fresh thread, interrupts enabled, holding nothing: it may
    // sleep.
    let s = unsafe { Sleepable::assume() };
    match entry {
        Some(thread::Entry::Closure(f)) => f(&s),
        Some(thread::Entry::Raw(f, arg)) => f(&s, arg),
        None => {}
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
pub fn reaper(s: &Sleepable) -> ! {
    loop {
        REAPER.wait_until(s, || !ZOMBIES.lock().is_empty());
        loop {
            // A separate statement: the list lock (which masks interrupts)
            // must not be held while the stack is freed, since freeing
            // may wait for other CPUs.
            let next = ZOMBIES.lock().pop_front();
            let Some(t) = next else {
                break;
            };
            // The thread is off every CPU: `finish_switch` queued it only
            // after clearing `on_cpu`.
            // Taken in its own statement, for the same reason.
            let stack = t.stack.lock().take();
            if let Some(stack) = stack {
                platform().free_stack(stack);
            }
        }
    }
}

/// Idle-spin parameters (FreeBSD's `sched_idlespins`,
/// `sched_idlespinthresh`, and its switch-count window of one stat tick).
const IDLE_SPINS: u32 = 10_000;
const IDLE_SPIN_THRESHOLD: u32 = 157;
const SWITCH_WINDOW_MS: u64 = 8;

/// Counts a context switch on `c` at `now`. Interrupts must be masked.
fn count_switch(c: &PerCpu, now: u64) {
    let (mut cur, mut prev) = c.switches.get();
    let window = platform().frequency() * SWITCH_WINDOW_MS / 1000;
    if now >= c.window_end.get() {
        // A window passed; if more than one did, the last one was empty.
        prev = if now < c.window_end.get().saturating_add(window) {
            cur
        } else {
            0
        };
        cur = 0;
        c.window_end.set(now.saturating_add(window));
    }
    c.switches.set((cur.saturating_add(1), prev));
}

/// Whether `c` has been switching often enough that its idle thread
/// should spin before halting. Interrupts must be masked.
fn switching_often(c: &PerCpu) -> bool {
    let (cur, prev) = c.switches.get();
    let window = platform().frequency() * SWITCH_WINDOW_MS / 1000;
    let stale = platform().now() >= c.window_end.get().saturating_add(window);
    !stale && cur + prev > IDLE_SPIN_THRESHOLD
}

/// About to halt: lets go of the user address space this CPU still has
/// loaded, as FreeBSD's idle thread runs on the kernel's pmap. Shootdowns
/// for that space then pass over this CPU instead of waking it.
fn release_space(c: &PerCpu) {
    let irq = hal::irq::disable();
    let loaded = c.space.take();
    if let Some(l) = &loaded {
        platform().load_kernel_tables();
        l.deactivate();
    }
    hal::irq::restore(irq);
    // A last reference frees the tables, with interrupts enabled.
    drop(loaded);
}

fn idle_loop() {
    loop {
        let irq = hal::irq::disable();
        let c = this_cpu();
        // As ULE's TDQ_IDLESPIN: never on an SMT sibling, where spinning
        // takes the core from the other thread.
        let spin = !c.smt.load(Ordering::Relaxed) && switching_often(c);
        hal::irq::restore(irq);
        if spin {
            // A waker sees `halted` clear and sets `need_resched` instead
            // of sending an IPI.
            for _ in 0..IDLE_SPINS {
                if c.need_resched.load(Ordering::Relaxed) {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        release_space(c);
        let irq = hal::irq::disable();
        c.halted.store(true, Ordering::Relaxed);
        // Pairs with the waker's fence in `make_runnable`: either it sees
        // `halted` and interrupts us, or we see its thread queued.
        fence(Ordering::SeqCst);
        let before = c.switches.get();
        if c.runq.lock().is_empty() && !c.need_resched.load(Ordering::Relaxed) {
            hal::wait_for_interrupt();
        }
        c.halted.store(false, Ordering::Relaxed);
        switch_away();
        // A wakeup that switched to no thread (an interrupt served from
        // idle) counts as a switch, as ULE counts it: a CPU taking frequent
        // interrupts spins rather than halting between them.
        if c.switches.get() == before {
            count_switch(c, platform().now());
        }
        hal::irq::restore(irq);
    }
}

/// Threads running or waiting to run, but the idle threads (FreeBSD's
/// `sched_load`, the load average's sample).
#[must_use]
pub fn load() -> usize {
    (0..online_cpus())
        .filter_map(|i| cpu(CpuId(u32::try_from(i).ok()?)))
        .map(|c| {
            let running =
                usize::from(c.current_prio.load(Ordering::Relaxed) != Priority::IDLE_THREAD.get());
            c.runq.len_hint() + running
        })
        .sum()
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

/// Host builds (other crates' unit tests): each OS thread is a thread of
/// its own, made at first use, so locks, their owners, and the witness work
/// as in the kernel. Nothing schedules: a test that contends a lock across
/// OS threads would block forever, so tests keep their objects to one.
#[cfg(not(target_os = "none"))]
mod host {
    extern crate std;

    use alloc::boxed::Box;

    use super::{Priority, Thread};

    std::thread_local! {
        static ME: &'static Thread = Box::leak(Box::new(Thread::new(
            "host",
            Priority::KERNEL,
            hal::CpuId(0),
            None,
            None,
            false,
        )));
    }

    pub(super) fn current() -> &'static Thread {
        ME.with(|t| *t)
    }
}

#[cfg(test)]
mod tests {
    use super::better_cpu;

    #[test]
    fn sleepable_locks_work_on_the_host() {
        use crate::lock::{LockClass, Mutex, SxLock};
        static M: LockClass = LockClass::new("test mutex", 100);
        static S: LockClass = LockClass::new("test sx", 200);
        // SAFETY: a host test thread may sleep.
        let s = unsafe { crate::Sleepable::assume() };
        let m = Mutex::new(&M, 1);
        let x = SxLock::new(&S, 2);
        {
            let a = x.read(&s);
            let b = x.read(&s);
            *m.lock(&s) += *a + *b;
        }
        *x.write(&s) += 1;
        assert_eq!((*m.lock(&s), *x.read(&s)), (5, 3));
    }

    #[test]
    fn placement_prefers_idle_then_fast() {
        assert!(better_cpu(0, 512, 1, 1024), "idle little beats busy big");
        assert!(better_cpu(1, 1024, 1, 512), "equal load: faster wins");
        assert!(!better_cpu(1, 512, 1, 1024));
        assert!(!better_cpu(2, 1024, 1, 512), "load first");
    }
}
