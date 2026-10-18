// SPDX-License-Identifier: BSD-2-Clause
//! A resource arena for address ranges: kernel virtual memory for stacks
//! and mappings.
//!
//! The free ranges live in a fixed-capacity sorted array, so the arena
//! never allocates memory itself and can be used before (and by) the
//! heap. Allocation is first fit with alignment; freeing coalesces with
//! both neighbors and refuses ranges that overlap free space, which
//! catches double frees.
//!
//! Behavior reference: sys/kern/subr_vmem.c (the interface, not the
//! boundary-tag implementation).

use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: u64,
    end: u64,
}

/// Why an arena operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmemError {
    /// No free range is large enough.
    Exhausted,
    /// The size or alignment is zero, not a multiple of the quantum, or
    /// the alignment is not a power of two.
    BadRequest,
    /// The free-range array is full (the arena is too fragmented).
    TooFragmented,
    /// The freed range is outside the arena or overlaps free space.
    NotAllocated { start: u64, size: u64 },
}

impl fmt::Display for VmemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted => f.write_str("arena exhausted"),
            Self::BadRequest => f.write_str("bad size or alignment"),
            Self::TooFragmented => f.write_str("arena too fragmented"),
            Self::NotAllocated { start, size } => {
                write!(f, "{start:#x}+{size:#x} was not allocated")
            }
        }
    }
}

/// An arena over `[base, base + size)` holding up to `N` free ranges.
#[derive(Debug)]
pub struct Vmem<const N: usize> {
    base: u64,
    size: u64,
    quantum: u64,
    spans: [Span; N],
    len: usize,
    allocated: u64,
}

impl<const N: usize> Vmem<N> {
    /// An arena with all of `[base, base + size)` free. `quantum` (a power
    /// of two) is the allocation granule; `base` and `size` must be
    /// multiples of it.
    #[must_use]
    pub const fn new(base: u64, size: u64, quantum: u64) -> Self {
        assert!(N > 0 && quantum.is_power_of_two());
        assert!(base.is_multiple_of(quantum) && size.is_multiple_of(quantum));
        assert!(base.checked_add(size).is_some());
        let mut spans = [Span { start: 0, end: 0 }; N];
        spans[0] = Span {
            start: base,
            end: base + size,
        };
        Self {
            base,
            size,
            quantum,
            spans,
            len: if size > 0 { 1 } else { 0 },
            allocated: 0,
        }
    }

    /// Allocates `size` bytes aligned to `align`.
    pub fn alloc(&mut self, size: u64, align: u64) -> Result<u64, VmemError> {
        let align = align.max(self.quantum);
        if size == 0 || !size.is_multiple_of(self.quantum) || !align.is_power_of_two() {
            return Err(VmemError::BadRequest);
        }
        let mut short_of_slots = false;
        for i in 0..self.len {
            let Span { start, end } = self.spans[i];
            let Some(at) = start.checked_next_multiple_of(align) else {
                continue;
            };
            if at.checked_add(size).is_none_or(|e| e > end) {
                continue;
            }
            let (left, right) = (at > start, at + size < end);
            match (left, right) {
                (false, false) => self.remove(i),
                (true, false) => self.spans[i].end = at,
                (false, true) => self.spans[i].start = at + size,
                (true, true) => {
                    if self.len == N {
                        // Splitting needs a slot; an exact fit elsewhere
                        // might not.
                        short_of_slots = true;
                        continue;
                    }
                    self.spans[i].end = at;
                    self.insert(
                        i + 1,
                        Span {
                            start: at + size,
                            end,
                        },
                    );
                }
            }
            self.allocated += size;
            return Ok(at);
        }
        Err(if short_of_slots {
            VmemError::TooFragmented
        } else {
            VmemError::Exhausted
        })
    }

    /// Returns `[start, start + size)`, which [`Vmem::alloc`] handed out
    /// (in whole or as part of a larger allocation).
    pub fn free(&mut self, start: u64, size: u64) -> Result<(), VmemError> {
        let bad = VmemError::NotAllocated { start, size };
        let end = start.checked_add(size).ok_or(bad)?;
        if size == 0
            || !start.is_multiple_of(self.quantum)
            || !size.is_multiple_of(self.quantum)
            || start < self.base
            || end > self.base + self.size
        {
            return Err(bad);
        }
        let p = self.spans[..self.len].partition_point(|s| s.start < start);
        let left = p.checked_sub(1).map(|i| self.spans[i]);
        let right = (p < self.len).then(|| self.spans[p]);
        if left.is_some_and(|l| l.end > start) || right.is_some_and(|r| r.start < end) {
            return Err(bad);
        }
        let joins_left = left.is_some_and(|l| l.end == start);
        let joins_right = right.is_some_and(|r| r.start == end);
        match (joins_left, joins_right) {
            (true, true) => {
                self.spans[p - 1].end = self.spans[p].end;
                self.remove(p);
            }
            (true, false) => self.spans[p - 1].end = end,
            (false, true) => self.spans[p].start = start,
            (false, false) => {
                if self.len == N {
                    return Err(VmemError::TooFragmented);
                }
                self.insert(p, Span { start, end });
            }
        }
        self.allocated -= size;
        Ok(())
    }

    /// Bytes currently allocated.
    #[must_use]
    pub const fn allocated(&self) -> u64 {
        self.allocated
    }

    /// The arena's whole range.
    #[must_use]
    pub const fn range(&self) -> (u64, u64) {
        (self.base, self.base + self.size)
    }

    /// Number of free ranges (a fragmentation measure).
    #[must_use]
    pub const fn free_ranges(&self) -> usize {
        self.len
    }

    fn remove(&mut self, i: usize) {
        self.spans.copy_within(i + 1..self.len, i);
        self.len -= 1;
    }

    fn insert(&mut self, i: usize, s: Span) {
        self.spans.copy_within(i..self.len, i + 1);
        self.spans[i] = s;
        self.len += 1;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    const Q: u64 = 0x1000;

    #[test]
    fn allocates_aligned_and_coalesces() {
        let mut v = Vmem::<8>::new(0x10_0000, 0x10_0000, Q);
        let a = v.alloc(Q, Q).unwrap();
        assert_eq!(a, 0x10_0000);
        let b = v.alloc(4 * Q, 0x4_0000).unwrap();
        assert_eq!(b, 0x14_0000);
        assert_eq!(v.free_ranges(), 2);
        assert_eq!(v.allocated(), 5 * Q);
        v.free(b, 4 * Q).unwrap();
        v.free(a, Q).unwrap();
        assert_eq!(v.free_ranges(), 1);
        assert_eq!(v.allocated(), 0);
        assert_eq!(v.alloc(0x10_0000, Q), Ok(0x10_0000));
        assert_eq!(v.alloc(Q, Q), Err(VmemError::Exhausted));
    }

    #[test]
    fn refuses_double_and_foreign_frees() {
        let mut v = Vmem::<8>::new(0, 16 * Q, Q);
        let a = v.alloc(2 * Q, Q).unwrap();
        v.free(a, 2 * Q).unwrap();
        assert!(matches!(
            v.free(a, 2 * Q),
            Err(VmemError::NotAllocated { .. })
        ));
        let b = v.alloc(4 * Q, Q).unwrap();
        assert!(matches!(
            v.free(b + 2 * Q, 4 * Q),
            Err(VmemError::NotAllocated { .. })
        ));
        assert!(matches!(
            v.free(64 * Q, Q),
            Err(VmemError::NotAllocated { .. })
        ));
        assert_eq!(v.alloc(Q + 1, Q), Err(VmemError::BadRequest));
        assert_eq!(v.alloc(Q, 3 * Q), Err(VmemError::BadRequest));
        // Partial frees of an allocation are allowed.
        v.free(b + Q, Q).unwrap();
        v.free(b, Q).unwrap();
        v.free(b + 2 * Q, 2 * Q).unwrap();
        assert_eq!(v.allocated(), 0);
        assert_eq!(v.free_ranges(), 1);
    }

    #[test]
    fn reports_fragmentation() {
        let mut v = Vmem::<2>::new(0, 64 * Q, Q);
        let ranges: Vec<u64> = (0..8).map(|_| v.alloc(Q, Q).unwrap()).collect();
        // Free every other page: the second free range needs a slot.
        v.free(ranges[1], Q).unwrap();
        assert_eq!(v.free(ranges[3], Q), Err(VmemError::TooFragmented));
        // An aligned request that would split the tail range also needs one.
        assert_eq!(v.alloc(Q, 32 * Q), Err(VmemError::TooFragmented));
        assert_eq!(v.alloc(Q, Q), Ok(ranges[1]));
    }

    #[test]
    fn churn_balances() {
        let mut v = Vmem::<256>::new(0xffff_c000_0000_0000, 1 << 30, Q);
        let mut rng = 0x9e37_79b9_u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut live: Vec<(u64, u64)> = Vec::new();
        for _ in 0..10_000 {
            if next() % 2 == 0 || live.is_empty() {
                let size = (1 + next() % 8) * Q;
                let align = Q << (next() % 4);
                if let Ok(a) = v.alloc(size, align) {
                    assert_eq!(a % align, 0);
                    live.push((a, size));
                }
            } else {
                let k = usize::try_from(next()).unwrap() % live.len();
                let (a, s) = live.swap_remove(k);
                v.free(a, s).unwrap();
            }
            assert_eq!(v.allocated(), live.iter().map(|l| l.1).sum::<u64>());
        }
        live.sort_unstable();
        for w in live.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0);
        }
        for (a, s) in live {
            v.free(a, s).unwrap();
        }
        assert_eq!(v.free_ranges(), 1);
    }
}
