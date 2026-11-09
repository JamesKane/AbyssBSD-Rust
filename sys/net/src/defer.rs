// SPDX-License-Identifier: BSD-2-Clause
//! Wakeups owed until a receive batch's flush points: a driver brackets
//! the frames of one pass in a [`Batch`], and a socket that the frames
//! make readable or writable is woken at the next [`Batch::flush`] (every
//! few frames, and at the batch's end) rather than once per frame, so a
//! thread woken by the first ACK of a pass finds the room the rest made,
//! instead of waking, taking a little, and sleeping again while the pass
//! goes on. Under a hypervisor each wakeup of a halted CPU is an exit on
//! both ends.
//!
//! FreeBSD gets the same effect less directly: its sender, woken on the
//! first ACK of a pass, spins on the connection lock the interrupt thread
//! holds for the others (`lock_delay`) instead of sleeping again.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use hal::MAX_CPUS;
use sync::PreemptLock;

/// How many frames a pass handles between flushes.
pub const EVERY: usize = 16;

/// Something to run at a batch's flush point.
pub trait Deferred: Send + Sync {
    /// Does what was put off.
    fn run(&self);
}

struct Slot {
    /// A batch runs on this CPU.
    active: AtomicBool,
    owed: PreemptLock<Vec<Arc<dyn Deferred>>>,
}

static SLOTS: [Slot; MAX_CPUS] = [const {
    Slot {
        active: AtomicBool::new(false),
        owed: PreemptLock::new(Vec::new()),
    }
}; MAX_CPUS];

#[cfg(not(test))]
fn here() -> usize {
    hal::cpu_id().index() % MAX_CPUS
}

/// On the host every thread is CPU 0; each test thread gets a slot of its
/// own instead, so tests running at once keep apart.
#[cfg(test)]
fn here() -> usize {
    use core::sync::atomic::AtomicUsize;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::thread_local! {
        static SLOT: usize = NEXT.fetch_add(1, Ordering::Relaxed) % MAX_CPUS;
    }
    SLOT.with(|s| *s)
}

/// A receive pass on this CPU, from [`begin`] until dropped (which
/// flushes). A batch begun while another runs on the CPU (an interrupt
/// thread preempting another) leaves the flushing to the outer one.
#[derive(Debug)]
pub struct Batch {
    cpu: usize,
    outer: bool,
}

/// Begins a batch on this CPU.
#[must_use]
pub fn begin() -> Batch {
    let cpu = here();
    let outer = !SLOTS[cpu].active.swap(true, Ordering::Relaxed);
    Batch { cpu, outer }
}

impl Batch {
    /// Runs what is owed so far (on the batch's CPU, even if its thread
    /// has moved since).
    pub fn flush(&self) {
        let slot = &SLOTS[self.cpu];
        loop {
            // One at a time, so nothing runs under the lock and the list
            // keeps its room.
            let Some(d) = slot.owed.lock().pop() else {
                return;
            };
            d.run();
        }
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        if self.outer {
            SLOTS[self.cpu].active.store(false, Ordering::Relaxed);
            self.flush();
        }
    }
}

/// Puts `d` off to the flush point of the batch running on this CPU, if
/// one is; gives it back otherwise, to run now. The caller deduplicates
/// (each is owed once until it runs).
///
/// # Errors
///
/// `Err(d)` when no batch runs here.
pub fn defer(d: Arc<dyn Deferred>) -> Result<(), Arc<dyn Deferred>> {
    let slot = &SLOTS[here()];
    if !slot.active.load(Ordering::Relaxed) {
        return Err(d);
    }
    let mut owed = slot.owed.lock();
    // Rechecked under the lock: an outer batch that ended since has
    // flushed, and would not see this.
    if !slot.active.load(Ordering::Relaxed) {
        return Err(d);
    }
    owed.push(d);
    Ok(())
}

/// Whether a batch runs on this CPU.
#[must_use]
pub fn active() -> bool {
    SLOTS[here()].active.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;

    struct Count(AtomicUsize);

    impl Deferred for Count {
        fn run(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn owed_work_waits_for_the_outer_batch() {
        let c = Arc::new(Count(AtomicUsize::new(0)));
        let d = || Arc::clone(&c) as Arc<dyn Deferred>;
        // No batch: given back.
        assert!(!active());
        assert!(defer(d()).is_err());
        let outer = begin();
        assert!(active());
        assert!(defer(d()).is_ok());
        assert!(defer(d()).is_ok());
        assert_eq!(c.0.load(Ordering::Relaxed), 0);
        outer.flush();
        assert_eq!(c.0.load(Ordering::Relaxed), 2);
        {
            // An inner batch leaves its work to the outer one.
            let inner = begin();
            assert!(defer(d()).is_ok());
            drop(inner);
            assert!(active());
            assert_eq!(c.0.load(Ordering::Relaxed), 2);
        }
        assert!(defer(d()).is_ok());
        drop(outer);
        assert_eq!(c.0.load(Ordering::Relaxed), 4);
        assert!(!active());
        assert!(defer(d()).is_err());
    }
}
