// SPDX-License-Identifier: BSD-2-Clause
//! A preemptible epoch (docs/NETWORKING.md §6): readers enter and exit
//! sections without locks or waiting; a writer that unlinked an object
//! waits for, or defers its free past, every section that might still
//! see it (FreeBSD's `net_epoch_preempt`).
//!
//! A section may be preempted and resume on another CPU, so readers are
//! counted rather than tied to a CPU: each CPU has a count of entries and
//! one of exits for each of two phases, on its own cache line. A section
//! counts in the phase current when it entered, and leaves through any
//! CPU's exit count; only sums matter. A grace period moves new sections
//! to the other phase and waits until the old phase's exits equal its
//! entries (reading exits first, so a section seen leaving is also seen
//! entering). It first waits out the other phase too: a reader that read
//! the phase before the last switch but counted itself after it is in
//! that phase, and could still see what this grace period is for. This is
//! sleepable RCU's scheme; FreeBSD's epochs use per-CPU records with the
//! same guarantee.
//!
//! Deferred frees run in the order they were asked for, each after a
//! grace period that started after it was asked for (`epoch_call`).
//! Nothing may sleep inside a section; the kernel checks that.
//!
//! Behavior reference: sys/kern/subr_epoch.c (`epoch_enter_preempt`,
//! `epoch_wait_preempt`, `epoch_call`, `epoch_drain_callbacks`).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

use hal::MAX_CPUS;
use sync::SpinLock;

/// One CPU's counts for the two phases, alone on its cache line.
#[repr(align(64))]
#[derive(Debug)]
struct Slot {
    enters: [AtomicU64; 2],
    exits: [AtomicU64; 2],
}

/// A deferred free and the grace period it waits for.
type Deferred = (u64, Box<dyn FnOnce() + Send>);

/// An epoch.
pub struct Epoch {
    name: &'static str,
    /// The phase new sections count in.
    phase: AtomicUsize,
    slots: [Slot; MAX_CPUS],
    /// Grace periods begun and finished, numbered from 1.
    started: AtomicU64,
    done: AtomicU64,
    /// Held by the one grace period in progress.
    busy: AtomicBool,
    deferred: SpinLock<VecDeque<Deferred>>,
}

impl core::fmt::Debug for Epoch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Epoch")
            .field("name", &self.name)
            .field("done", &self.done.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// A read section; it ends when dropped (`NET_EPOCH_EXIT`). It stays with
/// its thread.
#[must_use = "a section ends when dropped"]
pub struct Section<'a> {
    epoch: &'a Epoch,
    phase: usize,
    _thread: PhantomData<*const ()>,
}

impl core::fmt::Debug for Section<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Section")
            .field("epoch", &self.epoch.name)
            .field("phase", &self.phase)
            .finish()
    }
}

impl Drop for Section<'_> {
    fn drop(&mut self) {
        // Release: the section's reads happen before a grace period that
        // sees it gone.
        self.epoch.slot().exits[self.phase].fetch_add(1, Ordering::Release);
    }
}

impl Epoch {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            phase: AtomicUsize::new(0),
            slots: [const {
                Slot {
                    enters: [AtomicU64::new(0), AtomicU64::new(0)],
                    exits: [AtomicU64::new(0), AtomicU64::new(0)],
                }
            }; MAX_CPUS],
            started: AtomicU64::new(0),
            done: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            deferred: SpinLock::new(VecDeque::new()),
        }
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    fn slot(&self) -> &Slot {
        &self.slots[hal::cpu_id().index() % MAX_CPUS]
    }

    /// Enters a section (`NET_EPOCH_ENTER`).
    pub fn enter(&self) -> Section<'_> {
        let phase = self.phase.load(Ordering::Relaxed);
        self.enter_in(phase)
    }

    fn enter_in(&self, phase: usize) -> Section<'_> {
        self.slot().enters[phase].fetch_add(1, Ordering::SeqCst);
        // The count is visible before anything the section reads. A
        // locked add already orders later loads on x86; elsewhere (and
        // for Miri's model) it takes a fence.
        #[cfg(any(not(target_arch = "x86_64"), miri))]
        fence(Ordering::SeqCst);
        Section {
            epoch: self,
            phase,
            _thread: PhantomData,
        }
    }

    /// Whether no section counted in `phase` is still in it.
    fn drained(&self, phase: usize) -> bool {
        let exits: u64 = self
            .slots
            .iter()
            .map(|s| s.exits[phase].load(Ordering::Acquire))
            .fold(0, u64::wrapping_add);
        fence(Ordering::SeqCst);
        let enters: u64 = self
            .slots
            .iter()
            .map(|s| s.enters[phase].load(Ordering::Acquire))
            .fold(0, u64::wrapping_add);
        enters == exits
    }

    /// Waits for every section begun before it to end
    /// (`epoch_wait_preempt`), calling `pause` while it waits (to yield
    /// the CPU). Not to be called inside a section.
    pub fn synchronize(&self, pause: &mut dyn FnMut()) {
        while self
            .busy
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            pause();
        }
        let n = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        let old = self.phase.load(Ordering::Relaxed);
        while !self.drained(old ^ 1) {
            pause();
        }
        self.phase.store(old ^ 1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        while !self.drained(old) {
            pause();
        }
        self.done.store(n, Ordering::Release);
        self.busy.store(false, Ordering::Release);
    }

    /// Runs `f` after every section that might see what the caller
    /// unlinked has ended (`epoch_call`): after a grace period begun after
    /// this call. Deferred calls run in the order made, from
    /// [`Epoch::reclaim`].
    pub fn call(&self, f: Box<dyn FnOnce() + Send>) {
        // SeqCst: the caller's unlink comes before the grace period count
        // read here, so a later grace period's switch follows it.
        fence(Ordering::SeqCst);
        let need = self.started.load(Ordering::SeqCst) + 1;
        self.deferred.lock().push_back((need, f));
    }

    /// Deferred calls waiting.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.deferred.lock().len()
    }

    /// Runs the deferred calls whose grace period has passed, waiting for
    /// one first if any is due (the epoch's callback task). Returns how
    /// many ran.
    pub fn reclaim(&self, pause: &mut dyn FnMut()) -> usize {
        let Some(need) = self.deferred.lock().front().map(|d| d.0) else {
            return 0;
        };
        if self.done.load(Ordering::Acquire) < need {
            self.synchronize(pause);
        }
        self.run_due()
    }

    /// Runs the deferred calls already due, without waiting.
    pub fn run_due(&self) -> usize {
        let done = self.done.load(Ordering::Acquire);
        let mut ran = 0;
        loop {
            let f = {
                let mut q = self.deferred.lock();
                match q.front() {
                    Some(d) if d.0 <= done => q.pop_front().map(|d| d.1),
                    _ => None,
                }
            };
            let Some(f) = f else {
                return ran;
            };
            f();
            ran += 1;
        }
    }

    /// Runs deferred calls until none is left (`epoch_drain_callbacks`).
    pub fn drain(&self, pause: &mut dyn FnMut()) {
        while self.pending() > 0 {
            self.reclaim(pause);
        }
    }
}
