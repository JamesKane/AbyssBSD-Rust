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
    cpus: [Slot; MAX_CPUS],
}

/// One CPU's part, on a cache line of its own: only that CPU writes it.
#[derive(Debug)]
#[repr(align(64))]
struct Slot {
    /// The global epoch seen on entering, or 0 outside a section.
    observed: AtomicU64,
    depth: AtomicU32,
}

impl Epoch {
    /// A new domain.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            global: AtomicU64::new(1),
            cpus: [const {
                Slot {
                    observed: AtomicU64::new(0),
                    depth: AtomicU32::new(0),
                }
            }; MAX_CPUS],
        }
    }

    /// Enters a read section on this CPU (sections nest).
    pub fn enter(&self) -> EpochGuard<'_> {
        let preempt = crate::preempt_disable();
        let cpu = hal::cpu_id().index();
        let slot = &self.cpus[cpu];
        let depth = enter_depth(&slot.depth);
        if depth == 0 {
            let e = self.global.load(Ordering::Relaxed);
            // The store must be visible before our reads of shared data:
            // a full fence pairs with the one in `synchronize`. On x86 a
            // locked exchange is one (FreeBSD's `smr_enter`).
            #[cfg(target_arch = "x86_64")]
            slot.observed.swap(e, Ordering::SeqCst);
            #[cfg(not(target_arch = "x86_64"))]
            {
                slot.observed.store(e, Ordering::Relaxed);
                core::sync::atomic::fence(Ordering::SeqCst);
            }
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
                let e = self.cpus[cpu].observed.load(Ordering::Acquire);
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

/// Raises a slot's depth, returning the old one. In the kernel only this
/// CPU writes its slot, and an interrupt's section in between ends before
/// it returns: no atomic increment is needed. Host threads (tests) all
/// share one CPU number, so they need one.
fn enter_depth(depth: &AtomicU32) -> u32 {
    if cfg!(target_os = "none") {
        let d = depth.load(Ordering::Relaxed);
        depth.store(d + 1, Ordering::Relaxed);
        d
    } else {
        depth.fetch_add(1, Ordering::Relaxed)
    }
}

/// Lowers a slot's depth, returning the old one (as [`enter_depth`]).
fn leave_depth(depth: &AtomicU32) -> u32 {
    if cfg!(target_os = "none") {
        let d = depth.load(Ordering::Relaxed);
        depth.store(d - 1, Ordering::Relaxed);
        d
    } else {
        depth.fetch_sub(1, Ordering::Relaxed)
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
        let slot = &self.epoch.cpus[self.cpu];
        let depth = leave_depth(&slot.depth);
        if depth == 1 {
            // Release: our reads happen before a writer sees us leave.
            slot.observed.store(0, Ordering::Release);
        }
    }
}
