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
}

#[global_allocator]
static HEAP: Heap<KernelPages, { hal::MAX_CPUS }> = Heap::new(KernelPages);

/// Heap occupancy, after returning this CPU's cached objects.
pub(crate) fn heap_stats() -> HeapStats {
    HEAP.drain_this_cpu();
    HEAP.stats()
}
