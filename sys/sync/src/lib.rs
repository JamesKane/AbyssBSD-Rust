// SPDX-License-Identifier: BSD-2-Clause
//! Kernel synchronization primitives.
//!
//! Phase 1 has one CPU and no interrupts enabled, so only an early
//! [`SpinLock`] exists: a ticket lock that serializes the console and the
//! log. The full set (interrupt-aware spin locks, sleepable mutexes,
//! rw/sx locks, epoch) arrives in Phase 2 (docs/ARCHITECTURE.md §5.1).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

/// A FIFO ticket spin lock.
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

    /// Spins until the lock is held.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        // Relaxed: taking a ticket orders nothing; the Acquire load of
        // `serving` below is what synchronizes with the previous holder.
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self }
    }

    /// Takes the lock only if nobody holds or waits for it.
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        let serving = self.serving.load(Ordering::Relaxed);
        // Acquire on success pairs with the Release in `unlock`.
        self.next
            .compare_exchange(serving, serving + 1, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinGuard { lock: self })
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
        // Release publishes our writes to the next holder.
        self.lock.serving.fetch_add(1, Ordering::Release);
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
    fn try_lock_fails_when_held() {
        let lock = SpinLock::new(());
        let g = lock.lock();
        assert!(lock.try_lock().is_none());
        drop(g);
        assert!(lock.try_lock().is_some());
    }
}
