// SPDX-License-Identifier: BSD-2-Clause
//! The part of video memory the driver gives to buffers: a pool of pages
//! handed out in contiguous runs, first fit, and taken back when the buffer
//! goes. Everything below it is the driver's own (rings, page tables, display
//! surfaces) and everything above it is the security processor's.

use alloc::sync::Arc;
use alloc::vec::Vec;

use sync::SpinLock;

/// Where the pool starts and ends in video memory, in bytes: after the
/// driver's own areas (the last, the fence pages, end below 200 MiB) and
/// at the end of the window the processor has onto video memory (the card's
/// first BAR is 256 MiB; the security processor's trusted memory zone is at
/// 480 MiB, past it).
pub(crate) const POOL_START: u64 = 224 << 20;
pub(crate) const POOL_END: u64 = 256 << 20;

const PAGE: u64 = 4096;

/// The free runs, as (first page, pages), in address order, never adjacent.
#[derive(Debug)]
pub(crate) struct VramPool {
    free: SpinLock<Vec<(u64, u64)>>,
    total: u64,
}

/// A run taken from the pool; dropping it gives the pages back.
#[derive(Debug)]
pub(crate) struct VramBlock {
    pool: Arc<VramPool>,
    first: u64,
    pages: u64,
}

impl VramPool {
    /// A pool of the pages `start..end` (byte offsets in video memory).
    pub(crate) fn new(start: u64, end: u64) -> Arc<Self> {
        let first = start / PAGE;
        let pages = (end - start) / PAGE;
        Arc::new(Self {
            free: SpinLock::new(alloc::vec![(first, pages)]),
            total: pages * PAGE,
        })
    }

    /// Its size in bytes.
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Bytes in use.
    pub(crate) fn used(&self) -> u64 {
        self.total - self.free.lock().iter().map(|r| r.1 * PAGE).sum::<u64>()
    }

    /// A run of `bytes` (rounded up to pages), or `None` if no free run is
    /// that long.
    pub(crate) fn alloc(self: &Arc<Self>, bytes: u64) -> Option<Arc<VramBlock>> {
        let pages = bytes.div_ceil(PAGE).max(1);
        let mut free = self.free.lock();
        let i = free.iter().position(|r| r.1 >= pages)?;
        let (first, len) = free[i];
        if len == pages {
            free.remove(i);
        } else {
            free[i] = (first + pages, len - pages);
        }
        Some(Arc::new(VramBlock {
            pool: Arc::clone(self),
            first,
            pages,
        }))
    }

    fn give_back(&self, first: u64, pages: u64) {
        let mut free = self.free.lock();
        let at = free.partition_point(|r| r.0 < first);
        free.insert(at, (first, pages));
        // Join with the run after, then the run before.
        if at + 1 < free.len() && free[at].0 + free[at].1 == free[at + 1].0 {
            free[at].1 += free[at + 1].1;
            free.remove(at + 1);
        }
        if at > 0 && free[at - 1].0 + free[at - 1].1 == free[at].0 {
            free[at - 1].1 += free[at].1;
            free.remove(at);
        }
    }
}

impl VramBlock {
    /// Its first byte, as an offset in video memory.
    pub(crate) fn offset(&self) -> u64 {
        self.first * PAGE
    }
}

impl Drop for VramBlock {
    fn drop(&mut self) {
        self.pool.give_back(self.first, self.pages);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn runs_are_handed_out_first_fit_and_taken_back_whole() {
        let pool = VramPool::new(16 * MIB, 32 * MIB);
        assert_eq!(pool.total(), 16 * MIB);
        let a = pool.alloc(4 * MIB).unwrap();
        let b = pool.alloc(4 * MIB + 1).unwrap();
        assert_eq!(a.offset(), 16 * MIB);
        assert_eq!(b.offset(), 20 * MIB);
        // The extra byte took a whole page.
        assert_eq!(pool.used(), 8 * MIB + PAGE);
        drop(a);
        // The freed run is first, and fits what is no longer than it.
        assert_eq!(pool.alloc(2 * MIB).unwrap().offset(), 16 * MIB);
        drop(b);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn neighbors_join_and_a_hole_too_small_is_skipped() {
        let pool = VramPool::new(0, 16 * MIB);
        let a = pool.alloc(4 * MIB).unwrap();
        let b = pool.alloc(4 * MIB).unwrap();
        let c = pool.alloc(8 * MIB).unwrap();
        assert!(pool.alloc(PAGE).is_none());
        drop(b);
        // 4 MiB free in the middle: 5 MiB does not fit.
        assert!(pool.alloc(5 * MIB).is_none());
        drop(a);
        // Joined with its neighbor: 8 MiB from the start.
        let d = pool.alloc(8 * MIB).unwrap();
        assert_eq!(d.offset(), 0);
        drop(c);
        drop(d);
        assert_eq!(pool.used(), 0);
        assert_eq!(pool.alloc(16 * MIB).unwrap().offset(), 0);
    }
}
