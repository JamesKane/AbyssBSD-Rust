// SPDX-License-Identifier: BSD-2-Clause
//! The mbuf and cluster zones: their limits (`kern.ipc.nmbufs`,
//! `kern.ipc.nmbclusters`, and the jumbo sizes') and counts.
//!
//! Memory comes from the kernel heap, whose size classes already keep
//! per-CPU magazines (UMA's buckets); a zone adds the limit and the counts
//! `netstat -m` reports. The limit is charged in batches: each CPU holds
//! credit taken from the zone's total, so the common allocation and free
//! touch only that CPU's cache line, with one atomic add each (and no
//! separate counts: what is in use is what is charged less the credit),
//! as UMA counts items in its per-CPU
//! buckets against `uz_max_items`. A zone may therefore run short by up to
//! a batch per CPU before its limit.
//!
//! Behavior reference: sys/kern/kern_mbuf.c (`tunable_mbinit`,
//! `mbuf_init`), sys/vm/uma_core.c (`zone_alloc_limit`).

use core::alloc::Layout;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use hal::MAX_CPUS;

use crate::{MCLBYTES, MJUM9BYTES, MJUM16BYTES, MJUMPAGESIZE, MSIZE};

/// Credit a CPU takes from a zone's total at once.
const BATCH: i64 = 32;

/// One CPU's credit and counts, alone on its cache line.
#[repr(align(64))]
#[derive(Debug)]
struct Slot {
    /// Items this CPU may still allocate without asking the zone.
    credit: AtomicI64,
    fails: AtomicU64,
}

/// A zone of equal-sized buffers with a limit.
#[derive(Debug)]
pub struct Zone {
    name: &'static str,
    size: u32,
    /// The most items there may be; `u64::MAX` until the kernel sets it.
    limit: AtomicU64,
    /// Items charged to the limit: those in use plus the CPUs' credit.
    taken: AtomicU64,
    slots: [Slot; MAX_CPUS],
}

/// A zone's counts, as `vm.zone_stats` gives them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub size: u32,
    pub limit: u64,
    pub used: u64,
    pub fails: u64,
}

impl Zone {
    #[must_use]
    pub const fn new(name: &'static str, size: u32) -> Self {
        Self {
            name,
            size,
            limit: AtomicU64::new(u64::MAX),
            taken: AtomicU64::new(0),
            slots: [const {
                Slot {
                    credit: AtomicI64::new(0),
                    fails: AtomicU64::new(0),
                }
            }; MAX_CPUS],
        }
    }

    /// UMA's name for it (`vmstat -z`).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Its buffers' size.
    #[must_use]
    pub const fn size(&self) -> u32 {
        self.size
    }

    /// `uma_zone_set_max`.
    pub fn set_limit(&self, n: u64) {
        self.limit.store(n, Ordering::Relaxed);
    }

    #[must_use]
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    fn slot(&self) -> &Slot {
        &self.slots[hal::cpu_id().index() % MAX_CPUS]
    }

    /// Charges one item to the limit; false if it is reached.
    fn charge(&self) -> bool {
        let s = self.slot();
        if s.credit.fetch_sub(1, Ordering::Relaxed) > 0 {
            return true;
        }
        s.credit.fetch_add(1, Ordering::Relaxed);
        let limit = self.limit.load(Ordering::Relaxed);
        let mut t = self.taken.load(Ordering::Relaxed);
        loop {
            // A batch if it fits, else the last items one by one.
            let want = if t.saturating_add(BATCH as u64) <= limit {
                BATCH as u64
            } else if t < limit {
                1
            } else {
                return false;
            };
            match self.taken.compare_exchange_weak(
                t,
                t + want,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // One of them is this allocation's.
                    #[allow(clippy::cast_possible_wrap, reason = "at most BATCH")]
                    s.credit.fetch_add(want as i64 - 1, Ordering::Relaxed);
                    return true;
                }
                Err(now) => t = now,
            }
        }
    }

    /// Returns one item's charge, and a batch to the zone when this CPU
    /// holds more than two.
    fn uncharge(&self) {
        let s = self.slot();
        let c = s.credit.fetch_add(1, Ordering::Relaxed) + 1;
        if c > 2 * BATCH
            && s.credit
                .compare_exchange(c, c - BATCH, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.taken.fetch_sub(BATCH as u64, Ordering::Relaxed);
        }
    }

    fn layout(&self) -> Layout {
        // SAFETY: sizes are nonzero multiples of 16; 16 is a power of two.
        unsafe { Layout::from_size_align_unchecked(self.size as usize, 16) }
    }

    /// A buffer, uninitialized; `None` at the limit or out of memory.
    pub(crate) fn alloc(&self) -> Option<NonNull<u8>> {
        let s = self.slot();
        if !self.charge() {
            s.fails.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // SAFETY: the layout has a nonzero size.
        let p = NonNull::new(unsafe { alloc::alloc::alloc(self.layout()) });
        if p.is_none() {
            self.uncharge();
            s.fails.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        p
    }

    /// Returns a buffer.
    ///
    /// # Safety
    ///
    /// `p` came from this zone's [`Zone::alloc`] and is not used after.
    pub(crate) unsafe fn free(&self, p: NonNull<u8>) {
        // SAFETY: allocated with this layout, per the contract.
        unsafe { alloc::alloc::dealloc(p.as_ptr(), self.layout()) };
        self.uncharge();
    }

    /// Its counts. Allocations and frees in flight may be missed.
    #[must_use]
    pub fn stats(&self) -> Stats {
        // In use: what is charged, less what the CPUs hold as credit.
        let mut credit = 0i64;
        let mut fails = 0u64;
        for s in &self.slots {
            credit = credit.wrapping_add(s.credit.load(Ordering::Relaxed));
            fails = fails.wrapping_add(s.fails.load(Ordering::Relaxed));
        }
        let taken = self.taken.load(Ordering::Relaxed);
        Stats {
            size: self.size,
            limit: self.limit(),
            used: taken.saturating_sub(u64::try_from(credit).unwrap_or(0)),
            fails,
        }
    }
}

/// `zone_mbuf`, `zone_clust`, `zone_jumbop`, `zone_jumbo9`,
/// `zone_jumbo16`.
pub static MBUF: Zone = Zone::new("mbuf", MSIZE);
pub static CLUSTER: Zone = Zone::new("mbuf_cluster", MCLBYTES);
pub static JUMBOP: Zone = Zone::new("mbuf_jumbo_page", MJUMPAGESIZE);
pub static JUMBO9: Zone = Zone::new("mbuf_jumbo_9k", MJUM9BYTES);
pub static JUMBO16: Zone = Zone::new("mbuf_jumbo_16k", MJUM16BYTES);

/// The limits `tunable_mbinit` derives from memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub maxmbufmem: u64,
    pub nmbclusters: u64,
    pub nmbjumbop: u64,
    pub nmbjumbo9: u64,
    pub nmbjumbo16: u64,
    pub nmbufs: u64,
}

impl Limits {
    /// The defaults for `realmem` bytes of kernel memory: half of it for
    /// mbufs, a quarter of that in each of the 2 KiB and page clusters, a
    /// sixth in each jumbo size, and at least as many mbufs as clusters.
    #[must_use]
    pub const fn for_memory(realmem: u64) -> Self {
        let maxmbufmem = realmem / 2;
        let nmbclusters = maxmbufmem / MCLBYTES as u64 / 4;
        let nmbjumbop = maxmbufmem / MJUMPAGESIZE as u64 / 4;
        let nmbjumbo9 = maxmbufmem / MJUM9BYTES as u64 / 6;
        let nmbjumbo16 = maxmbufmem / MJUM16BYTES as u64 / 6;
        let clusters = nmbclusters + nmbjumbop + nmbjumbo9 + nmbjumbo16;
        let by_mem = maxmbufmem / MSIZE as u64 / 5;
        Self {
            maxmbufmem,
            nmbclusters,
            nmbjumbop,
            nmbjumbo9,
            nmbjumbo16,
            nmbufs: if by_mem > clusters { by_mem } else { clusters },
        }
    }

    /// Sets the zones' limits.
    pub fn apply(&self) {
        MBUF.set_limit(self.nmbufs);
        CLUSTER.set_limit(self.nmbclusters);
        JUMBOP.set_limit(self.nmbjumbop);
        JUMBO9.set_limit(self.nmbjumbo9);
        JUMBO16.set_limit(self.nmbjumbo16);
    }
}
