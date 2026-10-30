// SPDX-License-Identifier: BSD-2-Clause
//! umtx: the kernel half of user-space synchronization (`_umtx_op`).
//!
//! User code keeps its mutexes, condition variables, reader-writer locks,
//! and semaphores in its own memory and takes them without the kernel
//! while nobody contends; this crate is what it calls when it must wait,
//! or wake a waiter (docs/MULTIUSER.md §5).
//!
//! A [`Key`] names a word: by address space and address, or, for an
//! object several processes map, by memory object and offset. Keys hash to
//! a fixed array of [chains](Chain), each a spin lock over the queues of
//! the keys with waiters there. The lock is never held across an access
//! to user memory, which may fault and sleep: an operation that must read
//! or write the word around queueing marks the chain *busy*, which
//! serializes such operations without a spin lock. A waiter is queued
//! before it compares the word, so a wakeup between the two is never
//! lost.
//!
//! The kernel supplies the rest through [`Env`] (user memory, keys,
//! sleeping, the clocks) and [`Thread`] (waking a thread, its priority).
//! The protocols here follow FreeBSD's, operation by operation, so that
//! libthr sees the same results, errors, and word states.
//!
//! Behavior reference: sys/kern/kern_umtx.c, sys/sys/umtx.h.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod cond;
mod mutex;
mod pi;
mod robust;
mod rw;
mod sem;
#[cfg(test)]
mod tests;
mod wait;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use abi::errno::Errno;
use abi::time::Timespec;
use sync::PreemptLock;

pub use cond::CvTime;
pub use mutex::Mode;
pub use robust::Robust;
pub use wait::Width;

/// A user address that could not be read or written (`EFAULT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault;

impl From<Fault> for Errno {
    fn from(_: Fault) -> Self {
        Errno::EFAULT
    }
}

/// What a key's word is used as (FreeBSD's `TYPE_*`): the same word used
/// two ways is two keys, with separate waiters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    SimpleWait,
    Cv,
    Sem,
    SimpleLock,
    NormalUmutex,
    PiUmutex,
    PiRobustUmutex,
    PpUmutex,
    PpRobustUmutex,
    Rwlock,
    Shm,
}

impl Type {
    /// Which of the two chain arrays the type's keys hash into: waits,
    /// condition variables, and semaphores apart from locks, so a mutex's
    /// busy chain never holds up the condition variable it goes with.
    const fn table(self) -> usize {
        match self {
            Type::SimpleWait | Type::Cv | Type::Sem => 1,
            _ => 0,
        }
    }
}

/// Whether a key may be shared between processes (FreeBSD's
/// `THREAD_SHARE`, `PROCESS_SHARE`, `AUTO_SHARE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    /// Private to the address space.
    Thread,
    /// By memory object, whatever the mapping.
    Process,
    /// By memory object if the mapping is shared (inherited shared by
    /// children), else private.
    Auto,
}

impl Share {
    /// What an object's flags ask for (`GET_SHARE`).
    #[must_use]
    pub const fn of(flags: u32) -> Self {
        if flags & abi::umtx::USYNC_PROCESS_SHARED == 0 {
            Share::Thread
        } else {
            Share::Process
        }
    }
}

/// Which word a key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Id {
    /// An address in one address space (identified by any number unique
    /// among live address spaces).
    Private { space: usize, addr: u64 },
    /// An offset in a memory object (identified likewise).
    Shared { object: usize, offset: u64 },
}

/// A word, as `_umtx_op` names it.
#[derive(Clone)]
pub struct Key {
    pub ty: Type,
    pub id: Id,
    /// Keeps a shared key's memory object alive while the key is in use.
    pub hold: Option<Arc<dyn Any + Send + Sync>>,
}

impl core::fmt::Debug for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Key")
            .field("ty", &self.ty)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Key {
    /// A private key for `addr` in address space `space`.
    #[must_use]
    pub const fn private(ty: Type, space: usize, addr: u64) -> Self {
        Self {
            ty,
            id: Id::Private { space, addr },
            hold: None,
        }
    }

    fn matches(&self, other: &Key) -> bool {
        self.ty == other.ty && self.id == other.id
    }

    /// Whether the key names an object other processes may map.
    #[must_use]
    pub const fn is_shared(&self) -> bool {
        matches!(self.id, Id::Shared { .. })
    }

    /// The chain index (FreeBSD's `umtxq_hash`: a multiplicative hash of
    /// the two halves).
    fn hash(&self) -> usize {
        const GOLDEN_RATIO_32: u32 = 1_640_531_527;
        let (a, b) = match self.id {
            Id::Private { space, addr } => (space as u64, addr),
            Id::Shared { object, offset } => (object as u64, offset),
        };
        #[allow(clippy::cast_possible_truncation, reason = "a hash")]
        let n = a.wrapping_add(b) as u32;
        (n.wrapping_mul(GOLDEN_RATIO_32) >> (32 - 9)) as usize % CHAINS
    }
}

/// The chains per table.
const CHAINS: usize = 512;

/// Most user priority numbers mean nothing beside another's: the
/// time-sharing range lends nothing (FreeBSD's `UPRI`, so a busy thread
/// cannot keep a boosted priority by holding a mutex), and the worst
/// priority means none.
pub const PRI_MAX: u8 = 255;
pub const PRI_MIN_REALTIME: u8 = 48;
pub const PRI_MIN_TIMESHARE: u8 = 120;
pub const PRI_MAX_TIMESHARE: u8 = 223;

/// The kernel's handle on a thread that may wait here.
pub trait Thread: Send + Sync + 'static {
    /// Wakes the thread from [`Env::sleep`] or [`Env::park`], which then
    /// rechecks its condition.
    fn unpark(&self);
    /// The thread's priority in user mode, lent priority included
    /// (FreeBSD's `td_user_pri`).
    fn user_pri(&self) -> u8;
    /// The priority the thread asked for (`td_base_user_pri`).
    fn base_user_pri(&self) -> u8;
    /// Lends the thread priority `pri` in user mode; [`PRI_MAX`] ends the
    /// lending (`sched_lend_user_prio`).
    fn lend_user_pri(&self, pri: u8);
}

/// `UPRI(td)`.
fn upri<T: Thread>(w: &Waiter<T>) -> u8 {
    let p = w.thread.user_pri();
    if (PRI_MIN_TIMESHARE..=PRI_MAX_TIMESHARE).contains(&p) {
        PRI_MAX_TIMESHARE
    } else {
        p
    }
}

/// When a wait gives up: `end` on clock `clock`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    pub clock: u32,
    pub end: Timespec,
}

/// A timeout as `_umtx_op` takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeout {
    pub clock: u32,
    /// A time on the clock, not a duration.
    pub absolute: bool,
    pub time: Timespec,
}

/// The calling thread, as an operation sees it.
pub trait Env {
    type T: Thread;

    /// The caller's queue entry.
    fn me(&self) -> &Arc<Waiter<Self::T>>;

    /// Single-word accesses to the caller's memory, each atomic (and
    /// compare-and-swap a full barrier). `cas32` returns what the word
    /// held: `old` exactly when it stored `new`.
    fn load32(&self, addr: u64) -> Result<u32, Fault>;
    fn load64(&self, addr: u64) -> Result<u64, Fault>;
    fn store32(&self, addr: u64, v: u32) -> Result<(), Fault>;
    fn cas32(&self, addr: u64, old: u32, new: u32) -> Result<u32, Fault>;
    fn cas64(&self, addr: u64, old: u64, new: u64) -> Result<u64, Fault>;

    /// The key for the word at `addr` (FreeBSD's `umtx_key_get`).
    fn key(&self, addr: u64, ty: Type, share: Share) -> Result<Key, Errno>;

    /// Whether the caller should stop retrying a lock word that keeps
    /// changing because its process is being stopped or single-threaded
    /// (`thread_check_susp`): `EINTR` if it is exiting, else when
    /// stopping a wait for the stop if `sleep`, otherwise `ERESTART`.
    fn check_susp(&self, sleep: bool) -> Result<(), Errno>;

    /// Sleeps until `done()` holds, `deadline` passes (`ETIMEDOUT`), or a
    /// signal interrupts (`EINTR`, or `ERESTART` for a handler that
    /// restarts). `done` is checked so that an [`Thread::unpark`] after it
    /// became true is never missed.
    fn sleep(&self, done: &dyn Fn() -> bool, deadline: Option<&Deadline>) -> Result<(), Errno>;

    /// Sleeps, uninterruptibly, until `done()` holds.
    fn park(&self, done: &dyn Fn() -> bool);

    /// The time on `clock`.
    fn now(&self, clock: u32) -> Result<Timespec, Errno>;

    /// The thread `tid`: in the caller's process, or in any if `shared`.
    fn find(&self, tid: u32, shared: bool) -> Option<Arc<Waiter<Self::T>>>;

    /// Whether the caller may set real-time priorities
    /// (`PRIV_SCHED_RTPRIO`).
    fn may_rtprio(&self) -> bool;
}

/// FreeBSD's `umtx_abs_timeout_init`.
fn deadline<E: Env>(env: &E, t: &Timeout) -> Result<Deadline, Errno> {
    let end = if t.absolute {
        t.time
    } else {
        let now = env.now(t.clock)?;
        let mut sec = now.tv_sec.saturating_add(t.time.tv_sec);
        let mut nsec = now.tv_nsec + t.time.tv_nsec;
        if nsec >= 1_000_000_000 {
            nsec -= 1_000_000_000;
            sec = sec.saturating_add(1);
        }
        Timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    };
    Ok(Deadline {
        clock: t.clock,
        end,
    })
}

/// A thread's entry on the queues (FreeBSD's `struct umtx_q`), one per
/// thread for its life.
pub struct Waiter<T: Thread> {
    pub thread: T,
    pub tid: u32,
    /// On a queue (`UQF_UMTXQ`); changed under the chain lock, read by
    /// the sleeper without it.
    queued: AtomicBool,
    /// Waiting for a busy chain to be free.
    busy_wait: AtomicBool,
    /// Priority-inheritance state, under [`Umtx::pi_lock`].
    pi: PreemptLock<pi::WaiterPi<T>>,
    /// The robust-mutex lists.
    pub robust: PreemptLock<Robust>,
}

impl<T: Thread> core::fmt::Debug for Waiter<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Waiter")
            .field("tid", &self.tid)
            .field("queued", &self.is_queued())
            .finish_non_exhaustive()
    }
}

impl<T: Thread> Waiter<T> {
    #[must_use]
    pub fn new(thread: T, tid: u32) -> Self {
        Self {
            thread,
            tid,
            queued: AtomicBool::new(false),
            busy_wait: AtomicBool::new(false),
            pi: PreemptLock::new(pi::WaiterPi::new()),
            robust: PreemptLock::new(Robust::default()),
        }
    }

    fn is_queued(&self) -> bool {
        self.queued.load(Ordering::Acquire)
    }
}

/// The two queues a key may have: rwlock writers wait on the exclusive
/// one, everyone else on the shared one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Q {
    Shared,
    Exclusive,
}

struct Queue<T: Thread> {
    key: Key,
    q: Q,
    waiters: VecDeque<Arc<Waiter<T>>>,
}

/// One chain's state.
struct ChainState<T: Thread> {
    busy: bool,
    busy_waiters: VecDeque<Arc<Waiter<T>>>,
    queues: Vec<Queue<T>>,
    /// The priority-inheritance mutexes whose keys hash here.
    pis: Vec<Arc<pi::Pi<T>>>,
}

/// A chain: some keys' queues and the lock over them.
struct Chain<T: Thread> {
    state: PreemptLock<ChainState<T>>,
}

impl<T: Thread> Chain<T> {
    const fn new() -> Self {
        Self {
            state: PreemptLock::new(ChainState {
                busy: false,
                busy_waiters: VecDeque::new(),
                queues: Vec::new(),
                pis: Vec::new(),
            }),
        }
    }
}

type Guard<'a, T> = sync::PreemptLockGuard<'a, ChainState<T>>;

impl<T: Thread> ChainState<T> {
    fn queue(&mut self, key: &Key, q: Q) -> Option<&mut Queue<T>> {
        self.queues
            .iter_mut()
            .find(|x| x.q == q && x.key.matches(key))
    }

    /// Queues `w` under `key` (`umtxq_insert_queue`).
    fn insert(&mut self, key: &Key, q: Q, w: &Arc<Waiter<T>>) {
        debug_assert!(!w.is_queued());
        match self.queue(key, q) {
            Some(x) => x.waiters.push_back(w.clone()),
            None => {
                let mut waiters = VecDeque::new();
                waiters.push_back(w.clone());
                self.queues.push(Queue {
                    key: key.clone(),
                    q,
                    waiters,
                });
            }
        }
        w.queued.store(true, Ordering::Release);
    }

    /// Takes `w` off `key`'s queue if it is on it (`umtxq_remove_queue`).
    /// Returns how many waited there before.
    fn remove(&mut self, key: &Key, q: Q, w: &Waiter<T>) -> usize {
        if !w.is_queued() {
            return 0;
        }
        let Some(i) = self
            .queues
            .iter()
            .position(|x| x.q == q && x.key.matches(key))
        else {
            return 0;
        };
        let x = &mut self.queues[i];
        let len = x.waiters.len();
        if let Some(j) = x.waiters.iter().position(|y| core::ptr::eq(&**y, w)) {
            x.waiters.remove(j);
            w.queued.store(false, Ordering::Release);
        }
        if x.waiters.is_empty() {
            self.queues.swap_remove(i);
        }
        len
    }

    /// How many wait on `key`'s shared queue (`umtxq_count`).
    fn count(&mut self, key: &Key) -> usize {
        self.queue(key, Q::Shared).map_or(0, |x| x.waiters.len())
    }

    /// The first waiter on `key`'s shared queue, and how many there are.
    fn first(&mut self, key: &Key) -> (Option<Arc<Waiter<T>>>, usize) {
        self.queue(key, Q::Shared)
            .map_or((None, 0), |x| (x.waiters.front().cloned(), x.waiters.len()))
    }

    /// Wakes up to `n` of `key`'s waiters on queue `q`, longest waiting
    /// first (`umtxq_signal_queue`). Returns how many.
    fn signal(&mut self, key: &Key, n: usize, q: Q) -> usize {
        let Some(i) = self
            .queues
            .iter()
            .position(|x| x.q == q && x.key.matches(key))
        else {
            return 0;
        };
        let x = &mut self.queues[i];
        let mut woken = 0;
        while woken < n {
            let Some(w) = x.waiters.pop_front() else {
                break;
            };
            w.queued.store(false, Ordering::Release);
            w.thread.unpark();
            woken += 1;
        }
        if x.waiters.is_empty() {
            self.queues.swap_remove(i);
        }
        woken
    }

    /// Takes `w`, wherever it is queued under `key`, off and wakes it
    /// (`umtxq_signal_thread`).
    fn signal_thread(&mut self, key: &Key, w: &Waiter<T>) {
        self.remove(key, Q::Shared, w);
        w.thread.unpark();
    }
}

/// Every chain, and the lock over priority-inheritance state.
pub struct Umtx<T: Thread> {
    chains: [[Chain<T>; CHAINS]; 2],
    /// FreeBSD's `umtx_lock`: owners and blocked lists of
    /// priority-inheritance mutexes, and what each thread owns and is
    /// blocked on. Taken under a chain lock, never the other way.
    pi_lock: PreemptLock<()>,
}

impl<T: Thread> core::fmt::Debug for Umtx<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Umtx").finish_non_exhaustive()
    }
}

impl<T: Thread> Default for Umtx<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// `n`, waking all.
const ALL: usize = usize::MAX;

impl<T: Thread> Umtx<T> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            chains: [
                [const { Chain::new() }; CHAINS],
                [const { Chain::new() }; CHAINS],
            ],
            pi_lock: PreemptLock::new(()),
        }
    }

    fn chain(&self, key: &Key) -> &Chain<T> {
        &self.chains[key.ty.table()][key.hash()]
    }

    fn lock(&self, key: &Key) -> Guard<'_, T> {
        self.chain(key).state.lock()
    }

    /// Marks `key`'s chain busy, waiting while another operation has it
    /// (`umtxq_busy`); `g` is its lock, dropped while waiting and taken
    /// again.
    fn busy<'a, E: Env<T = T>>(&'a self, env: &E, key: &Key, mut g: Guard<'a, T>) -> Guard<'a, T> {
        let me = env.me();
        while g.busy {
            me.busy_wait.store(true, Ordering::Release);
            g.busy_waiters.push_back(me.clone());
            drop(g);
            // Woken when the chain is no longer busy; another operation
            // may take it first, as on FreeBSD.
            env.park(&|| !me.busy_wait.load(Ordering::Acquire));
            g = self.lock(key);
        }
        g.busy = true;
        g
    }

    /// Ends a busy period and wakes the next operation waiting for the
    /// chain, if any (`umtxq_unbusy`). The chain is not handed over: one
    /// already running takes it at once, rather than every operation
    /// after waiting for the woken thread to run.
    fn unbusy(g: &mut Guard<'_, T>) {
        debug_assert!(g.busy);
        g.busy = false;
        if let Some(w) = g.busy_waiters.pop_front() {
            w.busy_wait.store(false, Ordering::Release);
            w.thread.unpark();
        }
    }

    fn busy_unlocked<E: Env<T = T>>(&self, env: &E, key: &Key) {
        drop(self.busy(env, key, self.lock(key)));
    }

    fn unbusy_unlocked(&self, key: &Key) {
        Self::unbusy(&mut self.lock(key));
    }

    /// Sleeps while `me` is queued (`umtxq_sleep`): `Ok` once a waker
    /// takes it off; the chain lock is not held.
    fn sleep<E: Env<T = T>>(env: &E, deadline: Option<&Deadline>) -> Result<(), Errno> {
        let me = env.me();
        env.sleep(&|| !me.is_queued(), deadline)
    }

    /// Thread exit, and exec for every thread (`umtx_thread_cleanup`):
    /// gives up what priority-inheritance mutexes left the thread, then
    /// releases the robust mutexes it still holds.
    pub fn thread_exit<E: Env<T = T>>(&self, env: &E) {
        self.pi_cleanup(env.me());
        robust::cleanup(self, env);
    }
}
