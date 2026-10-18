// SPDX-License-Identifier: BSD-2-Clause
//! Epoch-based reclamation for read-mostly data.
//!
//! Readers enter a section (preemption disabled, so it must not sleep)
//! and read shared pointers freely. A writer that unlinks an object calls
//! [`Epoch::synchronize`], which returns once every reader that might
//! still see the old object has left its section; then the object can be
//! freed.
//!
//! Each CPU publishes the global epoch it observed on entry (0 when
//! outside a section). `synchronize` advances the global epoch and waits
//! until no CPU is still inside a section entered under an older one.
//!
//! Behavior reference: sys/kern/subr_epoch.c (the interface).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use hal::MAX_CPUS;

use crate::PreemptGuard;

/// An epoch domain.
#[derive(Debug)]
pub struct Epoch {
    global: AtomicU64,
    observed: [AtomicU64; MAX_CPUS],
    depth: [AtomicU32; MAX_CPUS],
}

impl Epoch {
    /// A new domain.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            global: AtomicU64::new(1),
            observed: [const { AtomicU64::new(0) }; MAX_CPUS],
            depth: [const { AtomicU32::new(0) }; MAX_CPUS],
        }
    }

    /// Enters a read section on this CPU (sections nest).
    pub fn enter(&self) -> EpochGuard<'_> {
        let preempt = crate::preempt_disable();
        let cpu = hal::cpu_id().index();
        // Relaxed: only this CPU writes its slots.
        if self.depth[cpu].fetch_add(1, Ordering::Relaxed) == 0 {
            let e = self.global.load(Ordering::Relaxed);
            self.observed[cpu].store(e, Ordering::Relaxed);
            // The store must be visible before our reads of shared data:
            // a full fence pairs with the one in `synchronize`.
            core::sync::atomic::fence(Ordering::SeqCst);
        }
        EpochGuard {
            epoch: self,
            cpu,
            _preempt: preempt,
        }
    }

    /// Waits until every section that began before this call has ended.
    /// Thread context only (it sleeps).
    pub fn synchronize(&self, mut wait: impl FnMut()) {
        let target = self.global.fetch_add(1, Ordering::SeqCst) + 1;
        core::sync::atomic::fence(Ordering::SeqCst);
        for cpu in 0..crate::online_cpus().min(MAX_CPUS) {
            loop {
                let e = self.observed[cpu].load(Ordering::Acquire);
                if e == 0 || e >= target {
                    break;
                }
                wait();
            }
        }
    }
}

impl Default for Epoch {
    fn default() -> Self {
        Self::new()
    }
}

/// A read section; leaves on drop.
#[derive(Debug)]
pub struct EpochGuard<'a> {
    epoch: &'a Epoch,
    cpu: usize,
    _preempt: PreemptGuard,
}

impl Drop for EpochGuard<'_> {
    fn drop(&mut self) {
        if self.epoch.depth[self.cpu].fetch_sub(1, Ordering::Relaxed) == 1 {
            // Release: our reads happen before a writer sees us leave.
            self.epoch.observed[self.cpu].store(0, Ordering::Release);
        }
    }
}
