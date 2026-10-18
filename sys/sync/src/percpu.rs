// SPDX-License-Identifier: BSD-2-Clause
//! Per-CPU counters.
//!
//! A [`PerCpuCounter`] keeps one signed count per CPU, each on its own cache
//! line, so frequent updates from many CPUs never contend: each CPU only
//! touches its own line. Reading the total sums every CPU's count. An
//! increment and its matching decrement may happen on different CPUs (the
//! thread migrated), so individual counts can go negative; only the sum is
//! meaningful.
//!
//! Gates (revocable references to driver bindings) and interrupt-handler
//! teardown use these to count calls in flight (docs/DRIVER-MODEL.md
//! §8.3, §9.2).
//!
//! Behavior reference: sys/kern/subr_counter.c (counter(9)).

use core::sync::atomic::{AtomicI64, Ordering};

use hal::MAX_CPUS;

/// One CPU's count, alone on its cache line.
#[repr(align(64))]
#[derive(Debug)]
struct Slot(AtomicI64);

/// A counter with one count per CPU.
#[derive(Debug)]
pub struct PerCpuCounter {
    slots: [Slot; MAX_CPUS],
}

impl PerCpuCounter {
    /// A counter at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { Slot(AtomicI64::new(0)) }; MAX_CPUS],
        }
    }

    /// Adds `n` to this CPU's count. The add is atomic because a migration
    /// between finding the CPU and adding could land it on another CPU's
    /// slot; that is rare, so the line is almost never shared. `order`
    /// orders the add like any atomic read-modify-write.
    #[inline]
    pub fn add(&self, n: i64, order: Ordering) {
        self.slots[hal::cpu_id().index()].0.fetch_add(n, order);
    }

    /// The sum over every CPU. Updates made meanwhile may or may not be
    /// counted; with no updates in flight it is exact. `order` applies to
    /// each slot's load.
    #[must_use]
    pub fn sum(&self, order: Ordering) -> i64 {
        self.slots.iter().map(|s| s.0.load(order)).sum()
    }
}

impl Default for PerCpuCounter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_across_slots() {
        let c = PerCpuCounter::new();
        assert_eq!(c.sum(Ordering::Relaxed), 0);
        c.add(5, Ordering::Relaxed);
        c.add(-2, Ordering::Relaxed);
        // A decrement recorded on another CPU's slot (a migrated thread).
        c.slots[3].0.fetch_sub(3, Ordering::Relaxed);
        assert_eq!(c.sum(Ordering::Relaxed), 0);
        assert_eq!(
            c.slots[3].0.load(Ordering::Relaxed),
            -3,
            "slots go negative"
        );
        assert_eq!(align_of::<Slot>(), 64);
    }
}
