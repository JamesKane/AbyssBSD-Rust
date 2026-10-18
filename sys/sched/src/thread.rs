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
    pub(crate) priority: Priority,
    /// The CPU the thread last ran on, or is queued for.
    pub(crate) cpu: CpuId,
    /// Pinned to one CPU.
    pub(crate) bound: Option<CpuId>,
    /// When the thread last went on a CPU, in platform time.
    pub(crate) since: u64,
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

type Entry = Box<dyn FnOnce() + Send>;

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
}

// SAFETY: `link` is protected by the owning list's lock and `context` by
// the `on_cpu` handoff (see the field docs); everything else is atomic or
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
                cpu,
                bound: None,
                since: 0,
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
    pub fn join(&self) {
        self.joiners.wait_until(|| self.has_exited());
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
