// SPDX-License-Identifier: BSD-2-Clause
//! Contended acquisitions of sleepable locks, by class: how often a lock
//! was found held, how many backoff rounds were spun on it, and how often
//! its taker slept (FreeBSD's `LOCK_PROFILING`, much reduced). Only the
//! slow paths count, so an uncontended lock pays nothing; the profiler
//! reports and clears the counts per benchmark.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use super::witness::LockClass;

/// Classes counted at once; contention past that many goes uncounted.
const SLOTS: usize = 64;

struct Slot {
    class: AtomicPtr<LockClass>,
    taken: AtomicU64,
    spins: AtomicU64,
    sleeps: AtomicU64,
}

static SLOTS_IN_USE: [Slot; SLOTS] = [const {
    Slot {
        class: AtomicPtr::new(ptr::null_mut()),
        taken: AtomicU64::new(0),
        spins: AtomicU64::new(0),
        sleeps: AtomicU64::new(0),
    }
}; SLOTS];

/// One contended acquisition under way: counted as it ends.
pub(super) struct Note {
    class: &'static LockClass,
    pub(super) spins: u64,
    pub(super) sleeps: u64,
}

impl Note {
    pub(super) fn new(class: &'static LockClass) -> Self {
        Self {
            class,
            spins: 0,
            sleeps: 0,
        }
    }
}

impl Drop for Note {
    fn drop(&mut self) {
        let want = ptr::from_ref(self.class).cast_mut();
        for s in &SLOTS_IN_USE {
            let found = match s.class.compare_exchange(
                ptr::null_mut(),
                want,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => true,
                Err(have) => have == want,
            };
            if found {
                s.taken.fetch_add(1, Ordering::Relaxed);
                s.spins.fetch_add(self.spins, Ordering::Relaxed);
                s.sleeps.fetch_add(self.sleeps, Ordering::Relaxed);
                return;
            }
        }
    }
}

/// Calls `f` with each class's name and counts (times found held, rounds
/// spun, sleeps) since the last call, and clears them.
pub fn take(mut f: impl FnMut(&'static str, u64, u64, u64)) {
    for s in &SLOTS_IN_USE {
        let class = s.class.load(Ordering::Acquire);
        // SAFETY: only `Note::drop` stores here, from a `&'static
        // LockClass`.
        let Some(class) = (unsafe { class.as_ref() }) else {
            break;
        };
        let taken = s.taken.swap(0, Ordering::Relaxed);
        let spins = s.spins.swap(0, Ordering::Relaxed);
        let sleeps = s.sleeps.swap(0, Ordering::Relaxed);
        if taken != 0 {
            f(class.name, taken, spins, sleeps);
        }
    }
}
