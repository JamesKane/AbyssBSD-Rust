// SPDX-License-Identifier: BSD-2-Clause
//! Kernel synchronization primitives.
//!
//! [`SpinLock`] is a ticket lock that masks interrupts on the holding CPU,
//! so state shared with interrupt handlers can use it. [`PreemptLock`] is
//! the same lock for state interrupt handlers never touch: holding it only
//! holds off preemption (FreeBSD's spin versus default mutexes), which
//! costs one per-CPU increment rather than masking interrupts (about 12 ns
//! a lock under bhyve), and leaves the profiler able to see inside it. The
//! rest of the set
//! (sleepable mutexes, rw/sx locks, epoch) builds on the scheduler
//! (docs/ARCHITECTURE.md §5.1). [`PerCpuCounter`] counts without sharing
//! cache lines between CPUs.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

mod percpu;

pub use percpu::PerCpuCounter;

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

/// A FIFO ticket spin lock. Holding it masks interrupts on this CPU.
#[derive(Debug)]
pub struct SpinLock<T> {
    next: AtomicU32,
    serving: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: the lock serializes all access to `value`.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves the value.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// A new unlocked lock.
    pub const fn new(value: T) -> Self {
        Self {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Masks interrupts and spins until the lock is held.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        let irq = hal::irq::disable();
        // Relaxed: taking a ticket orders nothing; the Acquire load of
        // `serving` below is what synchronizes with the previous holder.
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            core::hint::spin_loop();
        }
        SpinGuard {
            lock: self,
            irq: Some(irq),
        }
    }

    /// The value, through exclusive access to the lock (no locking needed).
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Takes the lock only if nobody holds or waits for it.
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        let irq = hal::irq::disable();
        let serving = self.serving.load(Ordering::Relaxed);
        // Acquire on success pairs with the Release in the guard's drop.
        match self.next.compare_exchange(
            serving,
            serving.wrapping_add(1),
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => Some(SpinGuard {
                lock: self,
                irq: Some(irq),
            }),
            Err(_) => {
                hal::irq::restore(irq);
                None
            }
        }
    }

    /// Breaks the lock and returns its value, for the panic path only,
    /// where the holder may be the code that panicked.
    ///
    /// # Safety
    ///
    /// No other code may run concurrently with the returned reference, and
    /// the lock must not be used normally afterwards.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn force(&self) -> &mut T {
        // SAFETY: the caller guarantees exclusive access.
        unsafe { &mut *self.value.get() }
    }
}

/// Proof that a [`SpinLock`] is held; releases it on drop.
#[derive(Debug)]
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    irq: Option<hal::irq::Saved>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves the lock is held.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves the lock is held, exclusively.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        // Only the holder writes `serving`, so a plain store does (no
        // read-modify-write). Release publishes our writes to the next
        // holder.
        let s = self.lock.serving.load(Ordering::Relaxed);
        self.lock
            .serving
            .store(s.wrapping_add(1), Ordering::Release);
        if let Some(irq) = self.irq.take() {
            hal::irq::restore(irq);
        }
    }
}

/// A FIFO ticket spin lock for state no interrupt handler touches: holding
/// it holds off preemption on this CPU, not interrupts. Taking it in an
/// interrupt handler could spin forever on the holder it interrupted, so
/// debug builds check that it never is.
#[derive(Debug)]
pub struct PreemptLock<T> {
    next: AtomicU32,
    serving: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: the lock serializes all access to `value`.
unsafe impl<T: Send> Sync for PreemptLock<T> {}
// SAFETY: moving the lock moves the value.
unsafe impl<T: Send> Send for PreemptLock<T> {}

impl<T> PreemptLock<T> {
    /// A new unlocked lock.
    pub const fn new(value: T) -> Self {
        Self {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Holds off preemption and spins until the lock is held.
    pub fn lock(&self) -> PreemptLockGuard<'_, T> {
        hal::critical::enter();
        debug_assert!(
            !hal::interrupt::active(),
            "a PreemptLock taken in an interrupt handler"
        );
        // As in `SpinLock::lock`.
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            core::hint::spin_loop();
        }
        PreemptLockGuard { lock: self }
    }

    /// Takes the lock only if nobody holds or waits for it.
    pub fn try_lock(&self) -> Option<PreemptLockGuard<'_, T>> {
        hal::critical::enter();
        let serving = self.serving.load(Ordering::Relaxed);
        // As in `SpinLock::try_lock`.
        match self.next.compare_exchange(
            serving,
            serving.wrapping_add(1),
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => Some(PreemptLockGuard { lock: self }),
            Err(_) => {
                hal::critical::exit();
                None
            }
        }
    }

    /// The value, through exclusive access to the lock (no locking needed).
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

/// Proof that a [`PreemptLock`] is held; releases it on drop.
#[derive(Debug)]
pub struct PreemptLockGuard<'a, T> {
    lock: &'a PreemptLock<T>,
}

impl<T> Deref for PreemptLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves the lock is held.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for PreemptLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves the lock is held, exclusively.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for PreemptLockGuard<'_, T> {
    fn drop(&mut self) {
        // As in `SpinGuard`'s drop.
        let s = self.lock.serving.load(Ordering::Relaxed);
        self.lock
            .serving
            .store(s.wrapping_add(1), Ordering::Release);
        hal::critical::exit();
    }
}

/// A value set once, then read without locking.
#[derive(Debug)]
pub struct Once<T> {
    state: AtomicU8,
    value: UnsafeCell<Option<T>>,
}

const UNSET: u8 = 0;
const SETTING: u8 = 1;
const SET: u8 = 2;

// SAFETY: the value is written once, before `state` becomes SET with
// Release, and only read after observing SET with Acquire.
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    /// An unset cell.
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(UNSET),
            value: UnsafeCell::new(None),
        }
    }

    /// Sets the value; returns it back if the cell was already set.
    pub fn set(&self, v: T) -> Result<(), T> {
        // Acquire/Relaxed: winning the race orders nothing yet; the
        // Release store below publishes the value.
        if self
            .state
            .compare_exchange(UNSET, SETTING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(v);
        }
        // SAFETY: SETTING gives this caller exclusive access.
        unsafe { *self.value.get() = Some(v) };
        self.state.store(SET, Ordering::Release);
        Ok(())
    }

    /// The value, if set.
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == SET {
            // SAFETY: SET means the value is written and never changes.
            unsafe { (*self.value.get()).as_ref() }
        } else {
            None
        }
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn serializes_threads() {
        let lock = Arc::new(SpinLock::new(0u64));
        let threads: std::vec::Vec<_> = (0..4)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        *lock.lock() += 1;
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(*lock.lock(), 40_000);
    }

    #[test]
    fn once_sets_once() {
        let o = Once::new();
        assert_eq!(o.get(), None);
        assert_eq!(o.set(1), Ok(()));
        assert_eq!(o.set(2), Err(2));
        assert_eq!(o.get(), Some(&1));
    }

    #[test]
    fn try_lock_fails_when_held() {
        let lock = SpinLock::new(());
        let g = lock.lock();
        assert!(lock.try_lock().is_none());
        drop(g);
        assert!(lock.try_lock().is_some());
    }
}
