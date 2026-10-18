// SPDX-License-Identifier: BSD-2-Clause
//! Sleepable mutexes with priority propagation.
//!
//! The owner word holds the owning thread's address, with bit 0 set when
//! threads are waiting (contested). An uncontested lock and unlock are one
//! compare-and-swap each; a contested unlock takes the slow path because
//! the swap fails on the set bit, so a wakeup cannot be lost.
//!
//! A thread that cannot get the lock spins briefly while the owner is on a
//! CPU (it will likely release soon), then blocks on the mutex's turnstile
//! (its waiter list). Before sleeping it lends its priority to the owner,
//! and on along the chain if the owner is itself blocked on a mutex, so a
//! low-priority owner cannot be starved by middling threads while an
//! urgent one waits. Unlock hands the lock directly to the most urgent
//! waiter and drops any priority the releasing thread no longer needs.
//!
//! Behavior reference: sys/kern/kern_mutex.c, sys/kern/subr_turnstile.c.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::sync::Arc;
use sync::SpinLock;

use super::witness::{self, LockClass};
use crate::list::ThreadList;
use crate::thread::{Priority, State, Thread};

const CONTESTED: usize = 1;
/// Spins while the owner runs, before blocking.
const SPIN_LIMIT: u32 = 2000;
/// How far priority lending follows a chain of blocked owners.
const PROPAGATE_DEPTH: usize = 8;

/// The type-independent part of a mutex.
#[derive(Debug)]
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

    fn lock(&self) {
        let me = crate::current();
        let me_word = ptr::from_ref(me).expose_provenance();
        witness::acquire(self.class, ptr::from_ref(self).addr(), self);
        let mut spins = 0;
        loop {
            // Acquire: the previous owner's writes are visible to us.
            if self
                .owner
                .compare_exchange_weak(0, me_word, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            let owner = self.owner_thread();
            // SAFETY: an owner stays alive while it owns the lock; if it
            // released meanwhile, `on_cpu` of a live thread is still safe
            // to read because threads are freed only after they exit, and
            // an exiting thread holds no mutexes.
            let running = !owner.is_null() && unsafe { (*owner).on_cpu.load(Ordering::Relaxed) };
            if running && spins < SPIN_LIMIT {
                spins += 1;
                core::hint::spin_loop();
                continue;
            }
            spins = 0;
            self.block(me, me_word);
            // Handoff: unlock made us the owner before waking us.
            if self.owner.load(Ordering::Acquire) & !CONTESTED == me_word {
                return;
            }
        }
    }

    /// Sleeps on the turnstile until the lock is handed to us (or freed).
    fn block(&self, me: &Thread, me_word: usize) {
        let irq = hal::irq::disable();
        witness::check_sleep(hal::irq::were_enabled(&irq));
        let mut q = self.waiters.lock();
        // Mark contested; if the lock was freed meanwhile, take it.
        let mut cur = self.owner.load(Ordering::Relaxed);
        loop {
            if cur == 0 {
                match self.owner.compare_exchange(
                    0,
                    me_word | if q.is_empty() { 0 } else { CONTESTED },
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        drop(q);
                        hal::irq::restore(irq);
                        return;
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
        unsafe { propagate(owner, my_prio) };
        drop(q);
        crate::switch_away();
        hal::irq::restore(irq);
    }

    fn unlock(&self) {
        let me = crate::current();
        let me_word = ptr::from_ref(me).expose_provenance();
        witness::release(ptr::from_ref(self).addr());
        // Release: our writes are visible to the next owner.
        if self
            .owner
            .compare_exchange(me_word, 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        let irq = hal::irq::disable();
        let mut q = self.waiters.lock();
        // Hand off to the most urgent waiter (first among equals).
        let best = q
            .iter()
            .min_by_key(|t| t.inner.lock().priority)
            .map(ptr::from_ref);
        let next = best.and_then(|p| {
            // SAFETY: `p` is on the list we hold.
            q.remove(unsafe { &*p })
        });
        match &next {
            Some(t) => {
                let word = Arc::as_ptr(t).expose_provenance();
                let contested = if q.is_empty() { 0 } else { CONTESTED };
                self.owner.store(word | contested, Ordering::Release);
                t.inner.lock().blocked_on = ptr::null();
            }
            None => self.owner.store(0, Ordering::Release),
        }
        // Waiters left behind lend their priority to the new owner.
        let remaining = q.iter().map(|t| t.inner.lock().priority).min();
        drop(q);
        unlend(me);
        if let Some(t) = next {
            if let Some(p) = remaining {
                // SAFETY: we hold a reference to the new owner.
                unsafe { propagate(Arc::as_ptr(&t), p) };
            }
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
/// on mutexes it still holds need.
fn unlend(me: &Thread) {
    let mut want = me.inner.lock().base;
    for m in witness::held_mutexes().mutexes() {
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

/// A sleepable mutex protecting a `T`.
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

    /// Locks, sleeping if needed. Thread context only.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.core.lock();
        MutexGuard {
            mutex: self,
            _not_send: PhantomData,
        }
    }

    /// Whether some thread holds the lock (racy; for diagnostics).
    pub fn is_locked(&self) -> bool {
        self.core.owner.load(Ordering::Relaxed) != 0
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
