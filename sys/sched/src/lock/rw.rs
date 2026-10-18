// SPDX-License-Identifier: BSD-2-Clause
//! Shared/exclusive locks.
//!
//! Many readers or one writer. Writers are preferred: once a writer waits,
//! new readers wait too, so a stream of readers cannot starve it. Waiting
//! threads sleep on wait queues. [`RwLock`] holders must not sleep (the
//! witness checks); [`SxLock`] holders may.
//!
//! The state is one word, so uncontended operations are one
//! compare-and-swap: either [`READ`] plus a reader count (unlocked is
//! `READ` with no readers), or the writing thread's address. Two bits say
//! which queues have sleepers. A sleeper sets its bit while holding its
//! queue's lock, in the same critical section that queues it; an unlocker
//! that sees a bit takes that queue's lock to wake, so no wakeup is lost.
//! Woken threads compete again rather than being handed the lock.
//!
//! A thread spins rather than sleeps while a writer holding the lock is on
//! a CPU, and for a short while when readers hold it (their count says
//! nothing about whether they run).
//!
//! Behavior reference: sys/kern/kern_rwlock.c, sys/kern/kern_sx.c.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::witness::{self, LockClass};
use crate::{Thread, WaitQueue};

/// Read mode (or unlocked); the count is in the bits above [`SHIFT`].
const READ: usize = 1;
/// Readers are sleeping on `readers_q`.
const READ_WAITERS: usize = 2;
/// Writers are sleeping on `writers_q`.
const WRITE_WAITERS: usize = 4;
const FLAGS: usize = READ | READ_WAITERS | WRITE_WAITERS;
const SHIFT: u32 = 3;
const ONE_READER: usize = 1 << SHIFT;
const UNLOCKED: usize = READ;
/// Spins on a lock held by readers before sleeping.
const READER_SPINS: u32 = 1000;
const MAX_BACKOFF: u32 = 64;

const fn readers(w: usize) -> usize {
    w >> SHIFT
}

#[derive(Debug)]
struct RwCore {
    word: AtomicUsize,
    readers_q: WaitQueue,
    writers_q: WaitQueue,
    class: &'static LockClass,
}

impl RwCore {
    const fn new(class: &'static LockClass) -> Self {
        Self {
            word: AtomicUsize::new(UNLOCKED),
            readers_q: WaitQueue::new(),
            writers_q: WaitQueue::new(),
            class,
        }
    }

    /// Whether the writer holding `w` (not read mode) is on a CPU.
    fn writer_running(w: usize) -> bool {
        let owner: *const Thread = ptr::with_exposed_provenance(w & !FLAGS);
        // SAFETY: the writer was alive while it held the lock; a released
        // and exited writer is freed only after leaving every CPU, and the
        // answer only steers spinning.
        !owner.is_null() && unsafe { (*owner).on_cpu.load(Ordering::Relaxed) }
    }

    #[inline]
    fn read(&self) {
        let w = self.word.load(Ordering::Relaxed);
        // Acquire: the last writer's writes are visible to us.
        if w & (READ | WRITE_WAITERS) != READ
            || self
                .word
                .compare_exchange_weak(w, w + ONE_READER, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
        {
            self.read_slow();
        }
        // Recorded once held, so waiting is not "sleeping while holding".
        witness::acquire(self.class, ptr::from_ref(self).addr());
    }

    /// Takes a read hold if readers may enter now.
    fn try_read(&self) -> bool {
        let mut w = self.word.load(Ordering::Relaxed);
        while w & (READ | WRITE_WAITERS) == READ {
            match self.word.compare_exchange_weak(
                w,
                w + ONE_READER,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(v) => w = v,
            }
        }
        false
    }

    #[cold]
    fn read_slow(&self) {
        let mut backoff = 1;
        loop {
            if self.try_read() {
                return;
            }
            let w = self.word.load(Ordering::Relaxed);
            if w & READ == 0 && Self::writer_running(w) {
                for _ in 0..backoff {
                    core::hint::spin_loop();
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
            let mut got = false;
            self.readers_q.wait_until(|| {
                let mut w = self.word.load(Ordering::Relaxed);
                loop {
                    if w & (READ | WRITE_WAITERS) == READ {
                        match self.word.compare_exchange_weak(
                            w,
                            w + ONE_READER,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => {
                                got = true;
                                return true;
                            }
                            Err(v) => w = v,
                        }
                    } else if w & READ_WAITERS != 0 {
                        return false;
                    } else {
                        match self.word.compare_exchange_weak(
                            w,
                            w | READ_WAITERS,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => return false,
                            Err(v) => w = v,
                        }
                    }
                }
            });
            if got {
                return;
            }
            backoff = 1;
        }
    }

    #[inline]
    fn read_unlock(&self) {
        witness::release(ptr::from_ref(self).addr());
        let mut w = self.word.load(Ordering::Relaxed);
        loop {
            // The last reader out wakes sleepers; others just leave.
            let last_with_waiters = readers(w) == 1 && w & (READ_WAITERS | WRITE_WAITERS) != 0;
            if last_with_waiters {
                self.release_and_wake(w);
                return;
            }
            // Release: our reads happen before a writer's writes.
            match self.word.compare_exchange_weak(
                w,
                w - ONE_READER,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(v) => w = v,
            }
        }
    }

    #[inline]
    fn write(&self) {
        let me = ptr::from_ref(crate::current()).expose_provenance();
        if self
            .word
            .compare_exchange(UNLOCKED, me, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.write_slow(me);
        }
        witness::acquire(self.class, ptr::from_ref(self).addr());
    }

    /// Takes the write hold if the lock is free (waiter bits may be set;
    /// `READ_WAITERS` is kept, `WRITE_WAITERS` is the caller's business).
    fn try_write(&self, me: usize) -> bool {
        let mut w = self.word.load(Ordering::Relaxed);
        while w & READ != 0 && readers(w) == 0 {
            let keep = w & (READ_WAITERS | WRITE_WAITERS);
            match self.word.compare_exchange_weak(
                w,
                me | keep,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(v) => w = v,
            }
        }
        false
    }

    #[cold]
    fn write_slow(&self, me: usize) {
        let mut backoff = 1;
        let mut reader_spins = 0;
        loop {
            if self.try_write(me) {
                return;
            }
            let w = self.word.load(Ordering::Relaxed);
            let spin = if w & READ == 0 {
                Self::writer_running(w)
            } else {
                reader_spins += 1;
                reader_spins < READER_SPINS
            };
            if spin {
                for _ in 0..backoff {
                    core::hint::spin_loop();
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
            let mut got = false;
            self.writers_q.wait_until(|| {
                let mut w = self.word.load(Ordering::Relaxed);
                loop {
                    if w & READ != 0 && readers(w) == 0 {
                        // Keep both bits: other writers may still sleep.
                        let keep = w & (READ_WAITERS | WRITE_WAITERS);
                        match self.word.compare_exchange_weak(
                            w,
                            me | keep,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => {
                                got = true;
                                return true;
                            }
                            Err(v) => w = v,
                        }
                    } else if w & WRITE_WAITERS != 0 {
                        return false;
                    } else {
                        match self.word.compare_exchange_weak(
                            w,
                            w | WRITE_WAITERS,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => return false,
                            Err(v) => w = v,
                        }
                    }
                }
            });
            if got {
                return;
            }
            backoff = 1;
            reader_spins = 0;
        }
    }

    #[inline]
    fn write_unlock(&self) {
        witness::release(ptr::from_ref(self).addr());
        let w = self.word.load(Ordering::Relaxed);
        // Release: our writes are visible to the next holder.
        if w & (READ_WAITERS | WRITE_WAITERS) != 0
            || self
                .word
                .compare_exchange(w, UNLOCKED, Ordering::Release, Ordering::Relaxed)
                .is_err()
        {
            self.release_and_wake(self.word.load(Ordering::Relaxed));
        }
    }

    /// Releases the last hold (the writer's, or the last reader's) when
    /// sleepers may be waiting. Writers are preferred: if any sleep, they
    /// are woken and readers keep sleeping (their bit stays set, so the
    /// next release wakes them).
    #[cold]
    fn release_and_wake(&self, mut w: usize) {
        loop {
            // Readers arrived meanwhile: we are not the last; just leave.
            if w & READ != 0 && readers(w) > 1 {
                match self.word.compare_exchange_weak(
                    w,
                    w - ONE_READER,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(v) => {
                        w = v;
                        continue;
                    }
                }
            }
            let writers = w & WRITE_WAITERS != 0;
            let new = UNLOCKED | if writers { w & READ_WAITERS } else { 0 };
            match self
                .word
                .compare_exchange_weak(w, new, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => {
                    if writers {
                        self.writers_q.wake_all();
                    } else if w & READ_WAITERS != 0 {
                        self.readers_q.wake_all();
                    }
                    return;
                }
                // Waiter bits were set, or readers arrived.
                Err(v) => w = v,
            }
        }
    }
}

macro_rules! shared_exclusive {
    ($(#[$doc:meta])* $name:ident, $sleepable:literal) => {
        $(#[$doc])*
        pub struct $name<T> {
            core: RwCore,
            value: UnsafeCell<T>,
        }

        // SAFETY: the lock gives shared access to readers (needs `Sync`)
        // and exclusive access to one writer (needs `Send`).
        unsafe impl<T: Send + Sync> Sync for $name<T> {}
        // SAFETY: moving the lock moves the value.
        unsafe impl<T: Send> Send for $name<T> {}

        impl<T> core::fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("class", &self.core.class.name)
                    .finish_non_exhaustive()
            }
        }

        impl<T> $name<T> {
            /// A new unlocked lock of `class`.
            ///
            /// # Panics
            ///
            /// At compile time (in const contexts) if the class's sleep
            /// rule does not match the lock kind.
            pub const fn new(class: &'static LockClass, value: T) -> Self {
                assert!(class.sleepable == $sleepable, "lock class sleep rule mismatch");
                Self {
                    core: RwCore::new(class),
                    value: UnsafeCell::new(value),
                }
            }

            /// Shared access, sleeping while a writer holds or waits.
            pub fn read(&self) -> ReadGuard<'_, T> {
                self.core.read();
                ReadGuard {
                    core: &self.core,
                    value: &self.value,
                    _not_send: PhantomData,
                }
            }

            /// Exclusive access.
            pub fn write(&self) -> WriteGuard<'_, T> {
                self.core.write();
                WriteGuard {
                    core: &self.core,
                    value: &self.value,
                    _not_send: PhantomData,
                }
            }
        }
    };
}

shared_exclusive!(
    /// A reader/writer lock whose holders must not sleep.
    RwLock,
    false
);
shared_exclusive!(
    /// A shared/exclusive lock whose holders may sleep.
    SxLock,
    true
);

/// Shared access to a [`RwLock`] or [`SxLock`].
#[derive(Debug)]
pub struct ReadGuard<'a, T> {
    core: &'a RwCore,
    value: &'a UnsafeCell<T>,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: readers share; no writer holds the lock.
        unsafe { &*self.value.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        self.core.read_unlock();
    }
}

/// Exclusive access to a [`RwLock`] or [`SxLock`].
#[derive(Debug)]
pub struct WriteGuard<'a, T> {
    core: &'a RwCore,
    value: &'a UnsafeCell<T>,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the writer holds the lock exclusively.
        unsafe { &*self.value.get() }
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above.
        unsafe { &mut *self.value.get() }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.core.write_unlock();
    }
}
