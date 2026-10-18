// SPDX-License-Identifier: BSD-2-Clause
//! Kernel threads.

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
            let lent = self.priority.beats(self.base);
            self.base = base;
            if !lent || base.beats(self.priority) {
                self.priority = base;
            }
        }
        self.priority
    }
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

type Entry = Box<dyn FnOnce(&crate::Sleepable) + Send>;

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
    /// Locks held, for the witness and priority un-lending. Only the
    /// thread itself touches it.
    pub(crate) held: UnsafeCell<crate::lock::witness::Held>,
    /// FP/SIMD state, for threads that use it. Written by the CPU switching
    /// the thread out, read by the one switching it in (the `on_cpu`
    /// handoff orders the two).
    pub(crate) fpu: Option<FpuArea>,
}

// SAFETY: `link` is protected by the owning list's lock, `context` and
// `fpu` by the `on_cpu` handoff, and `held` is only touched by the thread
// itself (see
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
        Self {
            // Relaxed: uniqueness is all that matters.
            tid: Tid(NEXT_TID.fetch_add(1, Ordering::Relaxed)),
            name: Name::new(name),
            inner: SpinLock::new(Inner {
                state: State::New,
                priority,
                base: priority,
                blocked_on: core::ptr::null(),
                contested: Contested::new(),
                cpu,
                bound: None,
                since: 0,
                ts: (priority >= Priority::TIMESHARE && priority < Priority::IDLE)
                    .then(|| crate::ts::Usage::new(priority.get() - Priority::TIMESHARE.get(), 0)),
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
            held: UnsafeCell::new(crate::lock::witness::Held::new()),
            fpu,
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
}

impl fmt::Debug for Thread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Thread")
            .field("tid", &self.tid)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
