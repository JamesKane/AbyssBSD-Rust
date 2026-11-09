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

use alloc::boxed::Box;
use core::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

use hal::MAX_CPUS;

/// The CPUs that can run: [`PerCpuStats`] keeps a row for each. One until
/// the kernel knows how many it started.
static POSSIBLE: AtomicUsize = AtomicUsize::new(1);

/// Sets how many CPUs there are (`mp_ncpus`), once they are started and
/// before anything sizes per-CPU statistics by it.
pub fn set_possible(n: usize) {
    // Relaxed: read by later code on CPUs started after this store's
    // release by the scheduler's own publication.
    POSSIBLE.store(n.clamp(1, MAX_CPUS), Ordering::Relaxed);
}

/// One CPU's statistics, starting on a cache line of their own.
#[repr(C, align(64))]
struct Row<const N: usize>([AtomicU64; N]);

/// A statistics structure of `N` words kept per CPU (`counter(9)`'s
/// `counter_u64_t`, an array of them): an update touches only this CPU's
/// row, a read sums the rows.
pub struct PerCpuStats<const N: usize> {
    rows: Box<[Row<N>]>,
}

impl<const N: usize> core::fmt::Debug for PerCpuStats<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PerCpuStats")
            .field("rows", &self.rows.len())
            .finish()
    }
}

impl<const N: usize> PerCpuStats<N> {
    /// All zero, a row for each CPU there is now.
    #[must_use]
    pub fn new() -> Self {
        let rows = Box::<[Row<N>]>::new_zeroed_slice(POSSIBLE.load(Ordering::Relaxed));
        // SAFETY: a row is atomics only, for which all-zero bytes are
        // valid (zero).
        let rows = unsafe { rows.assume_init() };
        Self { rows }
    }

    /// This CPU's row; another's (a CPU beyond the rows, or a migration
    /// between the look and the add) only shares its line, as the adds
    /// are atomic.
    #[inline]
    fn mine(&self) -> &Row<N> {
        &self.rows[hal::cpu_id().index() % self.rows.len()]
    }

    /// Adds `n` to word `i` (`counter_u64_add`).
    #[inline]
    pub fn add(&self, i: usize, n: u64) {
        if let Some(c) = self.mine().0.get(i) {
            // Relaxed: a statistic.
            c.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Takes `n` from word `i`.
    #[inline]
    pub fn sub(&self, i: usize, n: u64) {
        if let Some(c) = self.mine().0.get(i) {
            // Relaxed: a statistic.
            c.fetch_sub(n, Ordering::Relaxed);
        }
    }

    /// Word `i` summed over the CPUs (`counter_u64_fetch`).
    #[must_use]
    pub fn get(&self, i: usize) -> u64 {
        if i >= N {
            return 0;
        }
        self.rows
            .iter()
            // Relaxed: a statistic.
            .fold(0u64, |a, r| a.wrapping_add(r.0[i].load(Ordering::Relaxed)))
    }

    /// Every word to zero (`counter_u64_zero`).
    pub fn clear(&self) {
        for r in &*self.rows {
            for c in &r.0 {
                // Relaxed: a statistic.
                c.store(0, Ordering::Relaxed);
            }
        }
    }
}

impl<const N: usize> Default for PerCpuStats<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// One statistic kept per CPU (`counter_u64_t`).
#[derive(Debug, Default)]
pub struct PerCpuU64(PerCpuStats<1>);

impl PerCpuU64 {
    #[must_use]
    pub fn new() -> Self {
        Self(PerCpuStats::new())
    }

    /// `counter_u64_add`.
    #[inline]
    pub fn add(&self, n: u64) {
        self.0.add(0, n);
    }

    /// `counter_u64_fetch`.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.get(0)
    }
}

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

    #[test]
    fn stats_sum_rows() {
        let s = PerCpuStats::<3>::new();
        s.add(1, 5);
        s.sub(1, 2);
        s.add(7, 1);
        assert_eq!(s.get(1), 3);
        assert_eq!(s.get(7), 0);
        s.clear();
        assert_eq!(s.get(1), 0);
        assert_eq!(align_of::<Row<3>>(), 64);
    }
}
