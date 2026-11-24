// SPDX-License-Identifier: BSD-2-Clause
//! Shared/exclusive locks.
//!
//! Many readers or one writer. Writers are preferred: once a writer waits,
//! new readers wait too, so a stream of readers cannot starve it. Waiting
//! threads sleep on wait queues. [`RwLock`] holders must not sleep (the
//! witness checks); [`SxLock`] holders may.
//!
//! The state is one word: a flag for "a writer holds it", two flags that
//! say which queues have sleepers, a count of readers, and above them the
//! writer's address (shifted down by the alignment; 45 bits).
//! A writer takes the lock with one compare-and-swap from the unlocked word
//! (the word then names the holder, which the "is it running" spin
//! consults). A reader takes it with one `fetch_add` and, if a writer
//! holds the lock or waits, gives the hold back and waits: the common
//! acquisition and release of readers, however many, never retry (a
//! compare-and-swap loop on a word four CPUs share lost half its time to
//! failed attempts; FreeBSD's `rw_rlock` loops so). A reader that gave its
//! hold back last, with sleepers waiting, wakes them as a releasing reader
//! does. A sleeper sets its bit while holding its queue's lock, in the same
//! critical section that queues it; an unlocker that sees a bit takes that
//! queue's lock to wake, so no wakeup is lost. Woken threads compete again
//! rather than being handed the lock.
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
use crate::{Sleepable, Thread, WaitQueue};

/// A writer holds the lock.
const WRITER: usize = 1;
/// Readers are sleeping on `readers_q`.
const READ_WAITERS: usize = 2;
/// Writers are sleeping on `writers_q`.
const WRITE_WAITERS: usize = 4;
const WAITERS: usize = READ_WAITERS | WRITE_WAITERS;
/// What keeps a new reader out: a writer holding the lock, or one waiting
/// (writers are preferred).
const BARRED: usize = WRITER | WRITE_WAITERS;
const SHIFT: u32 = 3;
const ONE_READER: usize = 1 << SHIFT;
/// Bits of the reader count (counts, held or only tried, never reach this
/// many: that is one per thread, at most).
const COUNT_BITS: u32 = 16;
const COUNT_MASK: usize = ((1 << COUNT_BITS) - 1) << SHIFT;
/// Where the writer's address (divided by 8) sits.
const OWNER_SHIFT: u32 = SHIFT + COUNT_BITS;
const OWNER_MASK: usize = !0 << OWNER_SHIFT;
const UNLOCKED: usize = 0;
/// Spins on a lock held by readers before sleeping.
const READER_SPINS: u32 = 1000;
/// Looks at a held lock whose holder has not recorded itself, taken as a
/// running holder (see `writer_running`).
const OWNER_UNSET_LOOKS: u32 = 8;

const fn readers(w: usize) -> usize {
    (w & COUNT_MASK) >> SHIFT
}

/// The owner field for the thread at `addr`: its address divided by 8, or
/// zero if that does not fit (a kernel address is canonical in 48 bits and
/// threads are 8-aligned, so it does; zero is "unknown", and a lock held by
/// an unknown thread is waited for a few looks, not by spinning on it).
fn owner_field(addr: usize) -> usize {
    let a = addr.cast_signed();
    // Canonical: bits 63..47 all equal; aligned: low three bits clear.
    if (a << 16) >> 16 != a || addr & 7 != 0 {
        return 0;
    }
    ((addr >> SHIFT) << OWNER_SHIFT) & OWNER_MASK
}

/// The thread address in the owner field of `w` (null if none).
fn owner_of(w: usize) -> *const Thread {
    let bits = (w & OWNER_MASK) >> OWNER_SHIFT;
    // Back to 48 bits, sign-extended from bit 47.
    let addr = ((bits << SHIFT).cast_signed() << 16) >> 16;
    ptr::with_exposed_provenance(addr.cast_unsigned())
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

    /// Whether the writer holding the lock `w` is on a CPU. `unset` counts
    /// the looks that found no owner named (an address that did not fit the
    /// field): a few are taken as a holder that is running, so that
    /// sleeping does not cost a wakeup per handoff, many as one that is
    /// not.
    fn writer_running(w: usize, unset: &mut u32) -> bool {
        let owner = owner_of(w);
        if w & OWNER_MASK == 0 {
            *unset += 1;
            return *unset <= OWNER_UNSET_LOOKS;
        }
        *unset = 0;
        // SAFETY: the writer was alive while it held the lock; a released
        // and exited writer is freed only after leaving every CPU, and the
        // answer only steers spinning.
        unsafe { (*owner).on_cpu.load(Ordering::Relaxed) }
    }

    #[inline]
    fn read(&self, s: &Sleepable) {
        // Acquire: the last writer's writes are visible to us. The hold is
        // ours if nothing bars readers; else the count was a mistake and
        // goes back (a writer's unlock does not wait for it).
        let old = hal::atomic::fetch_add_acquire(&self.word, ONE_READER);
        if old & BARRED != 0 {
            self.read_release();
            self.read_slow(s);
        }
        // Recorded once held, so waiting is not "sleeping while holding".
        witness::acquire(self.class, ptr::from_ref(self).addr());
    }

    /// Takes a read hold if readers may enter now.
    fn try_read(&self) -> bool {
        let mut w = self.word.load(Ordering::Relaxed);
        // Only after the fast path was barred: the contended
        // compare-and-swap (`hal::atomic::cas`).
        while w & BARRED == 0 {
            match hal::atomic::cas(&self.word, w, w + ONE_READER) {
                Ok(_) => return true,
                Err(v) => w = v,
            }
        }
        false
    }

    #[cold]
    fn read_slow(&self, s: &Sleepable) {
        let mut note = super::contention::Note::new(self.class);
        let max_backoff = super::max_backoff();
        let mut backoff = 1;
        let mut unset = 0;
        loop {
            if self.try_read() {
                return;
            }
            let w = self.word.load(Ordering::Relaxed);
            if w & WRITER != 0 && Self::writer_running(w, &mut unset) {
                note.spins += 1;
                for _ in 0..backoff {
                    hal::spin_hint();
                }
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
            let mut got = false;
            note.sleeps += 1;
            self.readers_q.wait_until(s, || {
                let mut w = self.word.load(Ordering::Relaxed);
                loop {
                    if w & BARRED == 0 {
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
            unset = 0;
        }
    }

    #[inline]
    fn read_unlock(&self) {
        witness::release(ptr::from_ref(self).addr());
        self.read_release();
    }

    /// Gives up a reader's count (a hold, or a try that was barred).
    #[inline]
    fn read_release(&self) {
        // Release: our reads happen before a writer's writes.
        let old = hal::atomic::fetch_sub_release(&self.word, ONE_READER);
        // The last reader out, with no writer holding the lock (its unlock
        // wakes) and sleepers waiting: they are woken. Counts that were
        // only tries are counted here too, so a writer that slept for them
        // is not forgotten.
        if readers(old) == 1 && old & WRITER == 0 && old & WAITERS != 0 {
            self.wake_after_readers();
        }
    }

    #[inline]
    fn write(&self, s: &Sleepable) {
        let me = owner_field(ptr::from_ref(crate::current()).expose_provenance());
        if self
            .word
            .compare_exchange(UNLOCKED, WRITER | me, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.write_slow(s, me);
        }
        witness::acquire(self.class, ptr::from_ref(self).addr());
    }

    /// Takes the write hold if the lock is free (waiter bits may be set;
    /// they are kept).
    fn try_write(&self, me: usize) -> bool {
        let mut w = self.word.load(Ordering::Relaxed);
        while w & WRITER == 0 && readers(w) == 0 {
            match hal::atomic::cas(&self.word, w, w | WRITER | me) {
                Ok(_) => return true,
                Err(v) => w = v,
            }
        }
        false
    }

    #[cold]
    fn write_slow(&self, s: &Sleepable, me: usize) {
        let mut note = super::contention::Note::new(self.class);
        let max_backoff = super::max_backoff();
        let mut backoff = 1;
        let mut reader_spins = 0;
        let mut unset = 0;
        loop {
            if self.try_write(me) {
                return;
            }
            let w = self.word.load(Ordering::Relaxed);
            let spin = if w & WRITER != 0 {
                Self::writer_running(w, &mut unset)
            } else {
                reader_spins += 1;
                reader_spins < READER_SPINS
            };
            if spin {
                note.spins += 1;
                for _ in 0..backoff {
                    hal::spin_hint();
                }
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
            let mut got = false;
            note.sleeps += 1;
            self.writers_q.wait_until(s, || {
                let mut w = self.word.load(Ordering::Relaxed);
                loop {
                    if w & WRITER == 0 && readers(w) == 0 {
                        // Keep both bits: other writers may still sleep.
                        match self.word.compare_exchange_weak(
                            w,
                            w | WRITER | me,
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
            unset = 0;
        }
    }

    #[inline]
    fn write_unlock(&self) {
        witness::release(ptr::from_ref(self).addr());
        // Release: our writes are visible to the next holder. The word that
        // names only us is the common case; a sleeper, or a reader's count
        // that is about to be given back, is not.
        let w = self.word.load(Ordering::Relaxed);
        if w & (WAITERS | COUNT_MASK) != 0
            || self
                .word
                .compare_exchange(w, UNLOCKED, Ordering::Release, Ordering::Relaxed)
                .is_err()
        {
            self.write_release();
        }
    }

    /// Clears the writer's hold when the word holds more: sleepers to wake
    /// (writers are preferred: if any sleep, they are woken and readers keep
    /// sleeping, their bit staying set so the next release wakes them), and
    /// counts of readers that were barred and are giving them back.
    #[cold]
    fn write_release(&self) {
        let mut w = self.word.load(Ordering::Relaxed);
        loop {
            let writers = w & WRITE_WAITERS != 0;
            let cleared = if writers { WRITE_WAITERS } else { READ_WAITERS };
            let new = w & !(WRITER | OWNER_MASK | cleared);
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
                Err(v) => w = v,
            }
        }
    }

    /// The last count of readers is gone and sleepers wait (and no writer
    /// holds the lock): wakes the writers if there are any, else the
    /// readers, clearing the bit it wakes for.
    #[cold]
    fn wake_after_readers(&self) {
        let mut w = self.word.load(Ordering::Relaxed);
        loop {
            // Readers (or tries) arrived meanwhile, or a writer holds the
            // lock: whoever is last out, or the writer's unlock, wakes.
            if readers(w) != 0 || w & WRITER != 0 || w & WAITERS == 0 {
                return;
            }
            let writers = w & WRITE_WAITERS != 0;
            let cleared = if writers { WRITE_WAITERS } else { READ_WAITERS };
            match self.word.compare_exchange_weak(
                w,
                w & !cleared,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if writers {
                        self.writers_q.wake_all();
                    } else {
                        self.readers_q.wake_all();
                    }
                    return;
                }
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
            pub fn read(&self, s: &Sleepable) -> ReadGuard<'_, T> {
                self.core.read(s);
                ReadGuard {
                    core: &self.core,
                    value: &self.value,
                    _not_send: PhantomData,
                }
            }

            /// Exclusive access.
            pub fn write(&self, s: &Sleepable) -> WriteGuard<'_, T> {
                self.core.write(s);
                WriteGuard {
                    core: &self.core,
                    value: &self.value,
                    _not_send: PhantomData,
                }
            }

            /// Exclusive access if nobody holds the lock now, without
            /// waiting (for taking several locks against their usual order
            /// and backing off).
            pub fn try_write(&self) -> Option<WriteGuard<'_, T>> {
                let me = owner_field(ptr::from_ref(crate::current()).expose_provenance());
                if !self.core.try_write(me) {
                    return None;
                }
                witness::acquire_try(self.core.class, ptr::from_ref(&self.core).addr());
                Some(WriteGuard {
                    core: &self.core,
                    value: &self.value,
                    _not_send: PhantomData,
                })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_field_round_trips_canonical_addresses() {
        for addr in [
            0xffff_8000_1234_5678usize,
            0xffff_ffff_8000_0000 + 0x1c0,
            0x0000_7fff_dead_bef8,
            0xffff_8000_0000_0008,
        ] {
            let f = owner_field(addr);
            assert_ne!(f, 0, "{addr:#x}");
            // Only the owner bits are used.
            assert_eq!(f & !OWNER_MASK, 0);
            assert_eq!(owner_of(WRITER | f).addr(), addr, "{addr:#x}");
        }
    }

    #[test]
    fn an_address_that_does_not_fit_names_no_owner() {
        // Not canonical (bits 63..47 differ), and not 8-aligned.
        assert_eq!(owner_field(0x0001_0000_0000_0000), 0);
        assert_eq!(owner_field(0x8000_0000_0000_0000), 0);
        assert_eq!(owner_field(0xffff_8000_1234_5679), 0);
        assert!(owner_of(WRITER).is_null());
    }

    #[test]
    fn readers_count_apart_from_the_owner() {
        let f = owner_field(0xffff_8000_1234_5678);
        let mut w = WRITER | f;
        // Counts that were only tries, added while a writer holds the lock,
        // leave the owner alone.
        for _ in 0..1000 {
            w += ONE_READER;
        }
        assert_eq!(readers(w), 1000);
        assert_eq!(w & OWNER_MASK, f);
        assert_eq!(owner_of(w).addr(), 0xffff_8000_1234_5678);
        for _ in 0..1000 {
            w -= ONE_READER;
        }
        assert_eq!(w, WRITER | f);
        // The flags are below the count.
        assert_eq!(readers(WAITERS | WRITER), 0);
        assert_eq!(readers(ONE_READER | WAITERS), 1);
    }
}
