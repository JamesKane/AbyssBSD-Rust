// SPDX-License-Identifier: BSD-2-Clause
//! Sleepable mutexes with priority propagation.
//!
//! The owner word holds the owning thread's address, with bit 0 set when
//! threads are waiting (contested). An uncontended lock and unlock are one
//! compare-and-swap each; a contested unlock takes the slow path because
//! the swap fails on the set bit, so a wakeup cannot be lost.
//!
//! A thread that cannot get the lock spins while the owner is on a CPU (it
//! will likely release soon), backing off exponentially, and blocks on the
//! mutex's turnstile (its waiter list) only once the owner is off-CPU.
//! Before sleeping it lends its priority to the owner, and on along the
//! chain if the owner is itself blocked on a mutex, so a low-priority owner
//! cannot be starved by middling threads while an urgent one waits.
//!
//! A contested unlock releases the lock and wakes every waiter; they
//! compete again, and the losers block again (lending their priority to the
//! winner). Handing the lock straight to a sleeping waiter would be fairer,
//! but every handoff would then cost a wakeup while running threads stand
//! by: a convoy. The releasing thread then drops any priority it no longer
//! needs, found from its list of contested mutexes.
//!
//! Behavior reference: sys/kern/kern_mutex.c, sys/kern/subr_turnstile.c.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use sync::SpinLock;

use super::witness::{self, LockClass};
use crate::Sleepable;
use crate::list::ThreadList;
use crate::thread::{Priority, State, Thread};

const CONTESTED: usize = 1;
/// How far priority lending follows a chain of blocked owners.
const PROPAGATE_DEPTH: usize = 8;

/// The type-independent part of a mutex. In order (`repr(C)`): the owner
/// word first, on the line a value's first fields follow.
#[derive(Debug)]
#[repr(C)]
pub struct MutexCore {
    owner: AtomicUsize,
    waiters: SpinLock<ThreadList>,
    class: &'static LockClass,
}

impl MutexCore {
    const fn new(class: &'static LockClass) -> Self {
        Self {
            owner: AtomicUsize::new(0),
            waiters: SpinLock::new(ThreadList::new()),
            class,
        }
    }

    fn owner_thread(&self) -> *const Thread {
        ptr::with_exposed_provenance(self.owner.load(Ordering::Relaxed) & !CONTESTED)
    }

    #[inline]
    fn lock(&self, s: &Sleepable) {
        let me = crate::current();
        let me_word = ptr::from_ref(me).expose_provenance();
        witness::acquire(self.class, ptr::from_ref(self).addr());
        // Acquire: the previous owner's writes are visible to us.
        if self
            .owner
            .compare_exchange(0, me_word, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_slow(s, me, me_word);
        }
    }

    #[cold]
    fn lock_slow(&self, _s: &Sleepable, me: &Thread, me_word: usize) {
        let max_backoff = super::max_backoff();
        let mut backoff = 1;
        loop {
            let word = self.owner.load(Ordering::Relaxed);
            if word == 0 {
                if self
                    .owner
                    .compare_exchange_weak(0, me_word, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
                continue;
            }
            let owner: *const Thread = ptr::with_exposed_provenance(word & !CONTESTED);
            // SAFETY: the owner was alive when it owned the lock. If it has
            // released since, it may have exited, but threads are freed
            // only after the reaper has seen them off every CPU, and this
            // read only steers spinning.
            let running = unsafe { (*owner).on_cpu.load(Ordering::Relaxed) };
            if running {
                for _ in 0..backoff {
                    core::hint::spin_loop();
                }
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
            if self.block(me, me_word) {
                return;
            }
            backoff = 1;
        }
    }

    /// Sleeps on the turnstile until the lock is released. Returns true if
    /// it took the lock instead (it was free by the time we looked).
    fn block(&self, me: &Thread, me_word: usize) -> bool {
        let irq = hal::irq::disable();
        witness::check_sleep(hal::irq::were_enabled(&irq));
        let mut q = self.waiters.lock();
        // Mark contested; if the lock was freed meanwhile, take it.
        let mut cur = self.owner.load(Ordering::Relaxed);
        loop {
            if cur == 0 {
                let waiters = !q.is_empty();
                match self.owner.compare_exchange(
                    0,
                    me_word | if waiters { CONTESTED } else { 0 },
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        if waiters {
                            me.inner.lock().contested.insert(self);
                        }
                        drop(q);
                        hal::irq::restore(irq);
                        return true;
                    }
                    Err(v) => {
                        cur = v;
                        continue;
                    }
                }
            }
            match self.owner.compare_exchange(
                cur,
                cur | CONTESTED,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
        let owner: *const Thread = ptr::with_exposed_provenance(cur & !CONTESTED);
        let my_prio = {
            let mut inner = me.inner.lock();
            inner.state = State::Sleeping;
            inner.blocked_on = self;
            inner.priority
        };
        q.push_back(crate::current_arc());
        // SAFETY: the owner cannot release (and so cannot exit) while we
        // hold the waiter lock with the contested bit set: its unlock
        // needs this lock.
        unsafe {
            (*owner).inner.lock().contested.insert(self);
            propagate(owner, my_prio);
        }
        drop(q);
        crate::switch_away();
        hal::irq::restore(irq);
        false
    }

    #[inline]
    fn unlock(&self) {
        let me = crate::current();
        let me_word = ptr::from_ref(me).expose_provenance();
        witness::release(ptr::from_ref(self).addr());
        // Release: our writes are visible to the next owner.
        if self
            .owner
            .compare_exchange(me_word, 0, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            self.unlock_slow(me);
        }
    }

    /// A contested unlock: release, and wake every waiter to compete.
    #[cold]
    fn unlock_slow(&self, me: &Thread) {
        let irq = hal::irq::disable();
        let mut q = self.waiters.lock();
        let mut woken = ThreadList::new();
        while let Some(t) = q.pop_front() {
            t.inner.lock().blocked_on = ptr::null();
            woken.push_back(t);
        }
        self.owner.store(0, Ordering::Release);
        me.inner.lock().contested.remove(self);
        drop(q);
        unlend(me);
        while let Some(t) = woken.pop_front() {
            crate::make_runnable(t);
        }
        hal::irq::restore(irq);
    }

    /// The best priority among this mutex's waiters.
    fn best_waiter(&self) -> Option<Priority> {
        self.waiters
            .lock()
            .iter()
            .map(|t| t.inner.lock().priority)
            .min()
    }
}

/// Lends `prio` to `owner` and onward along the chain of blocked owners.
///
/// # Safety
///
/// `owner` must be alive (see the caller).
unsafe fn propagate(owner: *const Thread, prio: Priority) {
    let mut t = owner;
    for _ in 0..PROPAGATE_DEPTH {
        if t.is_null() {
            return;
        }
        // SAFETY: per the contract for the first; for the rest, a thread
        // blocked on a mutex owned by `t` keeps that chain stable enough:
        // owners in the chain are blocked and cannot exit.
        let th = unsafe { &*t };
        let mut inner = th.inner.lock();
        if !prio.beats(inner.priority) {
            return;
        }
        crate::set_priority_locked(th, &mut inner, prio);
        let next = inner.blocked_on;
        drop(inner);
        if next.is_null() {
            return;
        }
        // SAFETY: a mutex outlives the threads blocked on it.
        t = unsafe { (*next).owner_thread() };
    }
}

/// Recomputes the releasing thread's priority: its base, or what waiters
/// on contested mutexes it still holds need.
fn unlend(me: &Thread) {
    let (base, (held, n)) = {
        let inner = me.inner.lock();
        (inner.base, inner.contested.snapshot())
    };
    let mut want = base;
    for &m in &held[..n] {
        // SAFETY: a held mutex is alive.
        if let Some(p) = unsafe { (*m).best_waiter() }
            && p.beats(want)
        {
            want = p;
        }
    }
    let mut inner = me.inner.lock();
    if inner.priority != want {
        crate::set_priority_locked(me, &mut inner, want);
    }
}

/// A sleepable mutex protecting a `T`, which follows its lock word
/// (`repr(C)`), so taking the lock brings in the value's first fields.
#[repr(C)]
pub struct Mutex<T> {
    core: MutexCore,
    value: UnsafeCell<T>,
}

// SAFETY: the mutex serializes access to `value`.
unsafe impl<T: Send> Sync for Mutex<T> {}
// SAFETY: moving the mutex moves the value.
unsafe impl<T: Send> Send for Mutex<T> {}

impl<T> core::fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mutex")
            .field("class", &self.core.class.name)
            .finish_non_exhaustive()
    }
}

impl<T> Mutex<T> {
    /// A new unlocked mutex of `class`.
    pub const fn new(class: &'static LockClass, value: T) -> Self {
        Self {
            core: MutexCore::new(class),
            value: UnsafeCell::new(value),
        }
    }

    /// The value, without taking the lock.
    ///
    /// # Safety
    ///
    /// No thread may hold or take the lock, or otherwise reach the value,
    /// while the reference lives: the caller knows by some other rule that
    /// it alone can.
    pub unsafe fn get_unlocked(&self) -> &T {
        // SAFETY: per the caller's contract, nothing writes the value
        // while the reference lives.
        unsafe { &*self.value.get() }
    }

    /// Locks if nobody holds the lock now, without waiting.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let me = ptr::from_ref(crate::current()).expose_provenance();
        self.core
            .owner
            .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        witness::acquire(self.core.class, ptr::from_ref(&self.core).addr());
        Some(MutexGuard {
            mutex: self,
            _not_send: PhantomData,
        })
    }

    /// Locks, sleeping if needed. Thread context only.
    pub fn lock(&self, s: &Sleepable) -> MutexGuard<'_, T> {
        self.core.lock(s);
        MutexGuard {
            mutex: self,
            _not_send: PhantomData,
        }
    }

    /// Whether some thread holds the lock (racy; for diagnostics).
    pub fn is_locked(&self) -> bool {
        self.core.owner.load(Ordering::Relaxed) != 0
    }

    /// Whether the running thread holds the lock (exact: only this thread
    /// makes it so or not).
    pub fn is_mine(&self) -> bool {
        ptr::eq(self.core.owner_thread(), crate::current())
    }
}

/// Proof that a [`Mutex`] is held; unlocks on drop. Not `Send`: the lock
/// is recorded as held by this thread.
#[derive(Debug)]
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves the lock is held.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves the lock is held, exclusively.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.core.unlock();
    }
}
