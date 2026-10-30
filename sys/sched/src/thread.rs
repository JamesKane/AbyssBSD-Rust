// SPDX-License-Identifier: BSD-2-Clause
//! Kernel threads.

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use hal::CpuId;
use sync::SpinLock;

use crate::Stack;
use crate::list::Link;
use crate::wait::WaitQueue;

/// A thread ID, unique for the life of the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tid(pub u64);

/// A scheduling priority: lower is more urgent, as in FreeBSD.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(u8);

impl Priority {
    /// Interrupt threads.
    pub const INTERRUPT: Self = Self(16);
    /// Real-time kernel threads.
    pub const REALTIME: Self = Self(48);
    /// Ordinary kernel threads.
    pub const KERNEL: Self = Self(80);
    /// The best time-sharing priority.
    pub const TIMESHARE: Self = Self(120);
    /// Idle-class work.
    pub const IDLE: Self = Self(224);
    /// The per-CPU idle threads, which never sit on a run queue.
    pub const IDLE_THREAD: Self = Self(255);

    /// `p` as a priority.
    #[must_use]
    pub const fn new(p: u8) -> Self {
        Self(p)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Whether `self` should run before `other`.
    #[must_use]
    pub const fn beats(self, other: Self) -> bool {
        self.0 < other.0
    }
}

/// What a thread is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Created, never run.
    New,
    /// On a run queue.
    Runnable,
    /// On a CPU.
    Running,
    /// On a wait queue.
    Sleeping,
    /// Exited; waiting to be reaped.
    Dead,
}

/// Scheduling state, under the thread's lock.
#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) state: State,
    /// The effective priority: `base`, or better while lent by waiters on
    /// a mutex this thread holds.
    pub(crate) priority: Priority,
    /// The priority the thread asked for.
    pub(crate) base: Priority,
    /// A better priority lent to the thread by user threads it keeps
    /// waiting (priority-inheritance and -protection mutexes; FreeBSD's
    /// `td_lend_user_pri`).
    pub(crate) user_lent: Option<Priority>,
    /// The mutex the thread is blocked on, for priority propagation.
    pub(crate) blocked_on: *const crate::lock::MutexCore,
    /// Mutexes this thread owns that have waiters, for priority
    /// un-lending (FreeBSD's `td_contested`).
    pub(crate) contested: Contested,
    /// The CPU the thread last ran on, or is queued for.
    pub(crate) cpu: CpuId,
    /// Pinned to one CPU.
    pub(crate) bound: Option<CpuId>,
    /// When the thread last went on a CPU, in platform time.
    pub(crate) since: u64,
    /// Time-sharing state, for threads in the time-sharing range.
    pub(crate) ts: Option<crate::ts::Usage>,
    /// The recent CPU share (ULE's `ts_ticks` window), in `pctcpu::HZ`
    /// ticks.
    pub(crate) pct: crate::pctcpu::PctCpu,
    /// What the sleep in progress waits for (`td_wmesg`; empty when the
    /// thread is not asleep), and what its next sleeps are called (set
    /// by [`crate::wait_message`]).
    pub(crate) wmesg: &'static str,
    pub(crate) next_wmesg: &'static str,
    /// The queue of the sleep in progress (`td_wchan`), 0 when awake.
    pub(crate) wchan: usize,
    /// The sleep in progress may be interrupted (`TDF_SINTR`).
    pub(crate) sintr: bool,
    /// When the thread last went to sleep, in platform time
    /// (`td_slptick`); 0 while it has not slept since it woke.
    pub(crate) slept_at: u64,
}

/// A small set of mutexes (see [`Inner::contested`]).
#[derive(Debug)]
pub(crate) struct Contested {
    items: [*const crate::lock::MutexCore; Self::CAPACITY],
    len: usize,
}

impl Contested {
    /// Beyond this many contested mutexes held at once, further ones are
    /// not tracked: their waiters still lend priority, but releasing
    /// another mutex may drop it early. Holding this many is a bug anyway.
    const CAPACITY: usize = 16;

    const fn new() -> Self {
        Self {
            items: [core::ptr::null(); Self::CAPACITY],
            len: 0,
        }
    }

    pub(crate) fn insert(&mut self, m: *const crate::lock::MutexCore) {
        if !self.items[..self.len].contains(&m) && self.len < Self::CAPACITY {
            self.items[self.len] = m;
            self.len += 1;
        }
    }

    pub(crate) fn remove(&mut self, m: *const crate::lock::MutexCore) {
        if let Some(i) = self.items[..self.len].iter().position(|&x| x == m) {
            self.len -= 1;
            self.items[i] = self.items[self.len];
        }
    }

    /// A copy, to examine without the thread's lock.
    pub(crate) fn snapshot(&self) -> ([*const crate::lock::MutexCore; Self::CAPACITY], usize) {
        (self.items, self.len)
    }
}

/// What happened to a thread, for time-sharing accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Woken, about to be queued.
    Woken,
    /// Coming off its CPU; `requeued` if it goes back on a run queue.
    Descheduled { requeued: bool },
    /// Coming off its run queue onto a CPU.
    Scheduled,
    /// Still waiting on a run queue (aging).
    Waiting,
}

impl Inner {
    /// The priority the thread runs at unless a kernel mutex's waiter
    /// lends it a better one: its base, or what user threads lent it.
    pub(crate) fn floor(&self) -> Priority {
        match self.user_lent {
            Some(l) if l.beats(self.base) => l,
            _ => self.base,
        }
    }

    /// Updates time-sharing state for `ev` at `now` and recomputes the base
    /// priority; the effective priority follows unless a mutex waiter lent
    /// a better one. Returns the effective priority.
    pub(crate) fn account(&mut self, ev: Event, now: u64, window: u64, step: u64) -> Priority {
        if let Some(u) = &mut self.ts {
            match ev {
                Event::Woken => {
                    u.update(now, false, window);
                    u.enqueue(now);
                }
                Event::Descheduled { requeued } => {
                    u.update(now, true, window);
                    if requeued {
                        u.enqueue(now);
                    }
                }
                Event::Scheduled => {
                    u.update(now, false, window);
                    u.dequeue();
                }
                Event::Waiting => u.update(now, false, window),
            }
            let base = u.priority(now, step);
            let lent = self.priority.beats(self.floor());
            self.base = base;
            let floor = self.floor();
            if !lent || floor.beats(self.priority) {
                self.priority = floor;
            }
        }
        self.priority
    }
}

/// What [`Thread::report`] tells.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub state: State,
    /// The wait message and channel of the sleep in progress (`""` and 0
    /// when the thread is not asleep).
    pub wmesg: &'static str,
    pub wchan: usize,
    /// The sleep may be interrupted by a signal (`TDF_SINTR`).
    pub interruptible: bool,
    /// When the thread went to sleep, in platform time; 0 if it is not
    /// asleep.
    pub slept_at: u64,
    /// The effective, base, and user-mode priorities (`td_priority`,
    /// `td_base_pri`, `td_user_pri`).
    pub priority: Priority,
    pub base: Priority,
    pub user: Priority,
    /// Recent CPU share, a fraction of [`crate::pctcpu::FSCALE`].
    pub pctcpu: u32,
    /// The CPU it runs on or last ran on.
    pub cpu: CpuId,
    /// A per-CPU idle thread.
    pub idle: bool,
}

/// A fixed-size thread name.
#[derive(Clone, Copy)]
pub struct Name {
    bytes: [u8; 24],
    len: u8,
}

impl Name {
    fn new(s: &str) -> Self {
        let mut bytes = [0; 24];
        // Truncate at a character boundary.
        let mut len = s.len().min(bytes.len());
        while !s.is_char_boundary(len) {
            len -= 1;
        }
        bytes[..len].copy_from_slice(&s.as_bytes()[..len]);
        Self {
            bytes,
            len: u8::try_from(len).unwrap_or(0),
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("?")
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// What a new thread runs.
pub(crate) enum Entry {
    /// A closure, which returns when the thread is done.
    Closure(Box<dyn FnOnce(&crate::Sleepable) + Send>),
    /// A function that never returns (it leaves for user mode), with its
    /// argument: nothing is left to free on a stack it abandons.
    Raw(fn(&crate::Sleepable, usize) -> !, usize),
}

/// A thread's saved FP/SIMD registers (`hal::fpu`).
pub(crate) struct FpuArea {
    ptr: core::ptr::NonNull<u8>,
    layout: core::alloc::Layout,
}

impl FpuArea {
    /// A save area holding the initial state, or `None` when out of
    /// memory.
    pub(crate) fn new() -> Option<Self> {
        let layout =
            core::alloc::Layout::from_size_align(hal::fpu::area_size().max(16), hal::fpu::ALIGN)
                .ok()?;
        // SAFETY: nonzero size.
        let ptr = core::ptr::NonNull::new(unsafe { alloc::alloc::alloc(layout) })?;
        // SAFETY: a fresh allocation of the right size and alignment.
        unsafe { hal::fpu::init(ptr.as_ptr()) };
        Some(Self { ptr, layout })
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for FpuArea {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// A kernel thread.
pub struct Thread {
    tid: Tid,
    name: Name,
    pub(crate) inner: SpinLock<Inner>,
    /// Membership in one run queue, wait queue, or the zombie list.
    pub(crate) link: UnsafeCell<Link>,
    /// Saved registers while not running. Written by the switch code on
    /// the CPU leaving the thread, read by the CPU resuming it after
    /// `on_cpu` clears.
    pub(crate) context: UnsafeCell<hal::Context>,
    /// Set while a CPU is running the thread or still saving its context.
    pub(crate) on_cpu: AtomicBool,
    pub(crate) idle: bool,
    pub(crate) stack: SpinLock<Option<Stack>>,
    /// The body, taken when the thread first runs.
    pub(crate) entry: SpinLock<Option<Entry>>,
    pub(crate) exited: AtomicBool,
    pub(crate) joiners: WaitQueue,
    /// CPU time consumed, in platform time units.
    pub(crate) runtime: AtomicU64,
    pub(crate) switches: AtomicU64,
    /// Switches away because the thread slept (`ru_nvcsw`) and because it
    /// was preempted (`ru_nivcsw`).
    pub(crate) voluntary: AtomicU64,
    pub(crate) involuntary: AtomicU64,
    /// Statistics-clock ticks that found the thread in user mode, in the
    /// kernel, and in an interrupt thread (`td_uticks`, `td_sticks`,
    /// `td_iticks`): how its run time divides (`calcru`).
    pub(crate) stat_ticks: [AtomicU64; 3],
    /// Locks held, for the witness and priority un-lending. Only the
    /// thread itself touches it.
    pub(crate) held: UnsafeCell<crate::lock::witness::Held>,
    /// FP/SIMD state, for threads that use it. Written by the CPU switching
    /// the thread out, read by the one switching it in (the `on_cpu`
    /// handoff orders the two).
    pub(crate) fpu: Option<FpuArea>,
    /// The user address space the thread runs in, if any. Written only by
    /// the thread itself ([`crate::set_address_space`]); read by the CPU
    /// switching it in (the `on_cpu` handoff orders the two).
    pub(crate) space: UnsafeCell<Option<alloc::sync::Arc<dyn crate::AddressSpace>>>,
    /// The top of the kernel stack, where traps from user mode land (0 for
    /// threads on a borrowed stack, which never enter user mode).
    pub(crate) kstack_top: u64,
    /// The user thread registers (`hal::user::tls` and `tls2`) while the
    /// thread is off CPU (same rules as `space`).
    pub(crate) tls: UnsafeCell<[u64; 2]>,
    /// What the kernel runs the thread for (its process), opaque here.
    /// Written only by the thread itself.
    pub(crate) owner: UnsafeCell<Option<alloc::sync::Arc<dyn core::any::Any + Send + Sync>>>,
    /// Work to do before the thread next returns to user mode, as bits
    /// the kernel defines (FreeBSD's `td_ast`).
    pub(crate) ast: AtomicU32,
    /// Set by [`Thread::interrupt`]: interruptible sleeps end at once
    /// until the kernel clears it.
    pub(crate) interrupted: AtomicBool,
    /// The queue of the interruptible sleep in progress, or null; set by
    /// the sleeper under the queue's lock.
    pub(crate) wchan: AtomicPtr<WaitQueue>,
    /// Held by [`Thread::interrupt`] while it uses `wchan`, and taken by a
    /// sleeper before it leaves an interruptible sleep, so the queue
    /// outlives every use of the pointer.
    pub(crate) abort: SpinLock<()>,
}

// SAFETY: `link` is protected by the owning list's lock, `context`, `fpu`,
// and `space` by the `on_cpu` handoff, and `held` (and writes to `space`)
// only by the thread itself (see
// the field docs); `Inner::blocked_on` is read under the thread lock and
// points at a mutex that outlives the wait; everything else is atomic or
// locked.
unsafe impl Sync for Thread {}
// SAFETY: as above.
unsafe impl Send for Thread {}

static NEXT_TID: AtomicU64 = AtomicU64::new(100_000);

impl Thread {
    pub(crate) fn new(
        name: &str,
        priority: Priority,
        cpu: CpuId,
        stack: Option<Stack>,
        entry: Option<Entry>,
        idle: bool,
    ) -> Self {
        Self::with_fpu(name, priority, cpu, stack, entry, idle, None)
    }

    pub(crate) fn with_fpu(
        name: &str,
        priority: Priority,
        cpu: CpuId,
        stack: Option<Stack>,
        entry: Option<Entry>,
        idle: bool,
        fpu: Option<FpuArea>,
    ) -> Self {
        let context = match &stack {
            Some(s) => hal::Context::init(s.top, crate::thread_start, 0),
            None => hal::Context::default(),
        };
        let kstack_top = stack.as_ref().map_or(0, |s| s.top);
        Self {
            // Relaxed: uniqueness is all that matters.
            tid: Tid(NEXT_TID.fetch_add(1, Ordering::Relaxed)),
            name: Name::new(name),
            inner: SpinLock::new(Inner {
                state: State::New,
                priority,
                base: priority,
                user_lent: None,
                blocked_on: core::ptr::null(),
                contested: Contested::new(),
                cpu,
                bound: None,
                since: 0,
                ts: (priority >= Priority::TIMESHARE && priority < Priority::IDLE)
                    .then(|| crate::ts::Usage::new(priority.get() - Priority::TIMESHARE.get(), 0)),
                pct: crate::pctcpu::PctCpu::new(crate::hz_now()),
                wmesg: "",
                next_wmesg: "",
                wchan: 0,
                sintr: false,
                slept_at: 0,
            }),
            link: UnsafeCell::new(Link::new()),
            context: UnsafeCell::new(context),
            on_cpu: AtomicBool::new(false),
            idle,
            stack: SpinLock::new(stack),
            entry: SpinLock::new(entry),
            exited: AtomicBool::new(false),
            joiners: WaitQueue::new(),
            runtime: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            voluntary: AtomicU64::new(0),
            involuntary: AtomicU64::new(0),
            stat_ticks: [const { AtomicU64::new(0) }; 3],
            held: UnsafeCell::new(crate::lock::witness::Held::new()),
            fpu,
            space: UnsafeCell::new(None),
            kstack_top,
            tls: UnsafeCell::new([0; 2]),
            owner: UnsafeCell::new(None),
            ast: AtomicU32::new(0),
            interrupted: AtomicBool::new(false),
            wchan: AtomicPtr::new(core::ptr::null_mut()),
            abort: SpinLock::new(()),
        }
    }

    /// The thread's ID.
    #[must_use]
    pub fn tid(&self) -> Tid {
        self.tid
    }

    /// The thread's name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> State {
        self.inner.lock().state
    }

    /// The current priority.
    #[must_use]
    pub fn priority(&self) -> Priority {
        self.inner.lock().priority
    }

    /// The CPU the thread last ran on.
    #[must_use]
    pub fn last_cpu(&self) -> CpuId {
        self.inner.lock().cpu
    }

    /// The frame pointer the thread was switched out with, where a walk
    /// of its kernel stack starts; `None` while a CPU runs it (its stack
    /// changes as one looks: FreeBSD's `KKST_STATE_RUNNING`).
    #[must_use]
    pub fn saved_frame_pointer(&self) -> Option<u64> {
        if self.on_cpu.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: read-only and racy by design (a diagnostic, as
        // `stack_save_td`): the thread is off every CPU, so the words are
        // the last switch's; a switch starting meanwhile only makes the
        // walk stop early, as every frame read is checked.
        let fp = unsafe { (*self.context.get()).frame_pointer() };
        Some(fp)
    }

    /// Whether this is a per-CPU idle thread.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// Whether the thread has exited.
    #[must_use]
    pub fn has_exited(&self) -> bool {
        // Acquire pairs with the Release in `exit`, so the thread's last
        // writes are visible to whoever saw it exit.
        self.exited.load(Ordering::Acquire)
    }

    /// Waits for the thread to exit.
    pub fn join(&self, s: &crate::Sleepable) {
        self.joiners.wait_until(s, || self.has_exited());
    }

    /// CPU time consumed, in platform time units, as of its last switch.
    #[must_use]
    pub fn runtime(&self) -> u64 {
        self.runtime.load(Ordering::Relaxed)
    }

    /// Times the thread has been switched to.
    #[must_use]
    pub fn switches(&self) -> u64 {
        self.switches.load(Ordering::Relaxed)
    }

    /// Switches away because the thread slept and because it was
    /// preempted (`ru_nvcsw`, `ru_nivcsw`).
    #[must_use]
    pub fn context_switches(&self) -> (u64, u64) {
        (
            self.voluntary.load(Ordering::Relaxed),
            self.involuntary.load(Ordering::Relaxed),
        )
    }

    /// Statistics-clock ticks charged to the thread: in user mode, in the
    /// kernel, and as an interrupt thread (see [`crate::stat_tick`]).
    #[must_use]
    pub fn stat_ticks(&self) -> [u64; 3] {
        [0, 1, 2].map(|i| self.stat_ticks[i].load(Ordering::Relaxed))
    }

    /// What the thread is doing, for process listings: its state, the
    /// sleep it is in, its priorities, and its recent CPU share.
    #[must_use]
    pub fn report(&self) -> Report {
        let now = crate::hz_now();
        let mut inner = self.inner.lock();
        let running = inner.state == State::Running;
        inner.pct.update(now, running);
        Report {
            state: inner.state,
            wmesg: inner.wmesg,
            wchan: inner.wchan,
            interruptible: inner.sintr,
            slept_at: inner.slept_at,
            priority: inner.priority,
            base: inner.base,
            user: inner.floor(),
            pctcpu: inner.pct.fraction(),
            cpu: inner.cpu,
            idle: self.idle,
        }
    }

    /// Asks for `bits` of work before the thread next returns to user mode
    /// (the kernel's return path checks [`Thread::ast_pending`]). Another
    /// CPU running the thread is not told; [`Thread::interrupt`] is.
    /// Where the thread's saved FPU state is (`hal::fpu`'s layout) and
    /// its size, if it has one: for a debugger to read or change while the
    /// thread is stopped. The state is there only while the thread is off
    /// CPU (switched out); it loads it from there as it next runs.
    #[must_use]
    pub fn fpu_area(&self) -> Option<(*mut u8, usize)> {
        let f = self.fpu.as_ref()?;
        Some((f.as_ptr(), f.layout.size()))
    }

    /// The thread's user thread registers (`hal::user::tls`, `tls2`) as
    /// it left the CPU.
    ///
    /// # Safety
    ///
    /// The thread must be off CPU, and stay off while this reads.
    #[must_use]
    pub unsafe fn user_tls(&self) -> [u64; 2] {
        // SAFETY: written only as the thread switches out; the caller
        // guarantees it is off CPU.
        unsafe { *self.tls.get() }
    }

    pub fn request_ast(&self, bits: u32) {
        self.ast.fetch_or(bits, Ordering::SeqCst);
    }

    /// Whether any work is pending for the return to user mode: one load,
    /// for the return path.
    #[must_use]
    pub fn ast_pending(&self) -> bool {
        self.ast.load(Ordering::Relaxed) != 0
    }

    /// Takes the pending work bits.
    pub fn take_ast(&self) -> u32 {
        self.ast.swap(0, Ordering::SeqCst)
    }

    /// Whether interruptible sleeps end at once (see [`Thread::interrupt`]).
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    /// Lets interruptible sleeps proceed again (the kernel found nothing
    /// left to interrupt for).
    pub fn clear_interrupted(&self) {
        self.interrupted.store(false, Ordering::SeqCst);
    }

    /// Sets `ast` bits for the thread's next return to user mode, and makes
    /// another CPU running it enter the kernel, without ending a sleep
    /// (FreeBSD's `sched_affinity`, which leaves a sleeping thread to
    /// choose its CPU when it wakes).
    pub fn poke(&self, ast: u32) {
        self.request_ast(ast);
        let (state, cpu) = {
            let inner = self.inner.lock();
            (inner.state, inner.cpu)
        };
        if state == State::Running && cpu != hal::cpu_id() {
            crate::platform().kick(cpu);
        }
    }

    /// Interrupts the thread (FreeBSD's `tdsignal` wakeup): sets `ast` bits
    /// and the interrupted flag, ends an interruptible sleep in progress,
    /// and makes another CPU running the thread enter the kernel, so the
    /// work is done before the thread next runs in user mode. Sleeps that
    /// cannot be interrupted are left alone.
    pub fn interrupt(self: &alloc::sync::Arc<Self>, ast: u32) {
        // SeqCst pairs with the sleeper's: it publishes `wchan` and then
        // checks the flag, and we set the flag and then read `wchan`, so
        // one of us sees the other.
        self.interrupted.store(true, Ordering::SeqCst);
        self.request_ast(ast);
        let woken = {
            let _abort = self.abort.lock();
            let q = self.wchan.load(Ordering::SeqCst);
            // SAFETY: a sleeper takes `abort` before leaving its sleep, so
            // the queue it published is alive while we hold it.
            unsafe { q.as_ref() }.and_then(|q| q.remove(self))
        };
        if let Some(t) = woken {
            crate::make_runnable(t);
            return;
        }
        let (state, cpu) = {
            let inner = self.inner.lock();
            (inner.state, inner.cpu)
        };
        if state == State::Running && cpu != hal::cpu_id() {
            crate::platform().kick(cpu);
        }
    }
}

impl fmt::Debug for Thread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Thread")
            .field("tid", &self.tid)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
