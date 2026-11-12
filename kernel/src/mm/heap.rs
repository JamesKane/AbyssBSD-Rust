// SPDX-License-Identifier: BSD-2-Clause
//! The kernel heap: `sys/vm`'s slab allocator over buddy pages reached
//! through the DMAP, installed as the global allocator.

use core::ptr::NonNull;

use bootproto::layout::{dmap, dmap_to_phys};
use vm::Order;
use vm::slab::{Heap, HeapStats, PageSource};

/// Heap pages: buddy blocks, addressed through the DMAP.
#[derive(Debug)]
pub(crate) struct KernelPages;

// SAFETY: buddy blocks are exclusively the caller's until freed, are RAM
// mapped read/write in the DMAP, and are aligned to their size physically;
// the DMAP base is 1 GiB aligned, so they are virtually too.
unsafe impl PageSource for KernelPages {
    fn alloc_pages(&self, order: Order) -> Option<NonNull<u8>> {
        let pa = super::alloc_pages(order)?;
        let va = dmap(pa)?;
        NonNull::new(va.get() as *mut u8)
    }

    unsafe fn free_pages(&self, p: NonNull<u8>, order: Order) {
        let pa = dmap_to_phys(addr::VirtAddr::new(p.as_ptr() as u64))
            .expect("heap page outside the DMAP");
        super::free_pages(pa, order);
    }

    /// Frees clean file pages nothing maps, without waiting for any lock
    /// (`vm::object::reclaim_clean`), and returns the pages freed to the
    /// buddy allocator so they can merge: a block of `order` needs its
    /// `2^order` pages free together, so the batch grows with the order.
    /// FreeBSD's `malloc` needs no physically contiguous pages (`kmem`
    /// maps single pages into kernel virtual memory) and waits for the
    /// page daemon instead (docs/compat-deviations.md).
    fn reclaim(&self, order: Order) -> bool {
        let want = usize::try_from(order.pages() * 4)
            .unwrap_or(usize::MAX)
            .max(RECLAIM_BATCH);
        let freed = vm::object::reclaim_clean(want);
        super::drain_page_cache();
        freed > 0
    }
}

/// Pages the heap reclaims at a time when the buddy allocator is out of
/// the block it needs.
const RECLAIM_BATCH: usize = 256;

/// The heap's zones (ADR-0024), under FreeBSD's names: the objects a walk
/// over many descriptors touches, kept together whatever else of their
/// size comes and goes.
const ZONES: [vm::slab::ZoneSpec; 3] = [
    vm::slab::ZoneSpec::of_arc::<crate::fs::file::File>("Files"),
    vm::slab::ZoneSpec::of_arc::<crate::fs::pipe::Pipe>("pipe"),
    vm::slab::ZoneSpec::of_arc::<event::Knote>("KNOTE"),
];

#[global_allocator]
static HEAP: Heap<KernelPages, { hal::MAX_CPUS }> = Heap::with_zones(KernelPages, &ZONES);

/// Each zone's name, object size, and objects out of its slabs
/// (`vm.zone_stats`).
pub(crate) fn zone_stats() -> alloc::vec::Vec<vm::slab::ZoneStats> {
    HEAP.zone_stats().collect()
}

/// Gives back all the magazines the heap's depots hold: memory is short.
pub(crate) fn heap_reclaim() {
    HEAP.reclaim();
}

/// Trims the heap's depots to their working sets (`uma_reclaim`'s
/// `UMA_RECLAIM_TRIM`).
pub(crate) fn heap_trim() {
    HEAP.trim();
}

/// Trims the heap's depots to their working sets, as often as `UMA_TIMEOUT`
/// has `uma_timeout` run.
pub(crate) fn start_heap_trim() {
    let _ = sched::spawn("heaptrim", sched::Priority::KERNEL, |s| {
        loop {
            crate::time::sleep_ns(s, vm::slab::TRIM_SECONDS * 1_000_000_000);
            HEAP.trim();
        }
    });
}

/// Heap occupancy, after returning this CPU's cached objects.
pub(crate) fn heap_stats() -> HeapStats {
    HEAP.drain_this_cpu();
    HEAP.stats()
}
