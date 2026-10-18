// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's memory for `vm::dma`: buddy pages (below a device's limit
//! when it cannot reach all RAM), the direct map for access, and the
//! architecture's cache maintenance for non-coherent devices.

use core::ptr::NonNull;

use addr::{PhysAddr, VirtAddr};
use bootproto::layout::{dmap, dmap_to_phys};
use vm::dma::DmaMemory;
use vm::{Order, Pfn};

use super::PHYS;

/// The kernel's [`DmaMemory`].
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct KernelDma;

/// The order covering `bytes` at `align` (buddy blocks are aligned to
/// their size).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
fn order_for(bytes: u64, align: u64) -> Option<Order> {
    Order::for_bytes(bytes.max(align))
}

// SAFETY: blocks come from the buddy allocator (exclusively ours until
// freed, physically contiguous, size-aligned) and are reached through the
// direct map, which maps all RAM read-write.
unsafe impl DmaMemory for KernelDma {
    fn alloc(&self, bytes: u64, align: u64, below: u64) -> Option<u64> {
        let order = order_for(bytes, align)?;
        let mut phys = PHYS.lock();
        let phys = phys.as_mut()?;
        let pfn = if below >= top_of_ram_locked(phys) {
            phys.alloc(order, vm::phys::DomainId(0))
        } else {
            phys.alloc_below(order, Pfn::containing(PhysAddr::new(below)))
        }?;
        Some(pfn.addr().get())
    }

    unsafe fn free(&self, pa: u64, bytes: u64) {
        // The same order `alloc` chose (alignment only ever raised it to
        // at least the size, which `for_bytes` of the size reproduces when
        // the caller passes what `alloc` allocated).
        if let Some(order) = Order::for_bytes(bytes) {
            super::free_pages(PhysAddr::new(pa), order);
        }
    }

    fn virt(&self, pa: u64) -> NonNull<u8> {
        let va = dmap(PhysAddr::new(pa)).expect("DMA memory in the direct map");
        NonNull::new(va.get() as *mut u8).expect("nonzero direct-map address")
    }

    fn phys(&self, va: *const u8) -> Option<u64> {
        let v = VirtAddr::new(va as u64);
        dmap_to_phys(v)
            .map(PhysAddr::get)
            .or_else(|| super::translate(v).map(|t| t.pa.get()))
    }

    fn clean(&self, va: *const u8, len: usize) {
        #[cfg(target_arch = "aarch64")]
        crate::arch::cache::clean_to_poc(va as usize, len);
        #[cfg(target_arch = "x86_64")]
        let _ = (va, len);
    }

    fn invalidate(&self, va: *const u8, len: usize) {
        #[cfg(target_arch = "aarch64")]
        crate::arch::cache::invalidate_to_poc(va as usize, len);
        #[cfg(target_arch = "x86_64")]
        let _ = (va, len);
    }
}

/// The kernel's DMA memory.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) static KERNEL_DMA: KernelDma = KernelDma;

fn top_of_ram_locked(phys: &vm::phys::PhysAllocator<'_>) -> u64 {
    phys.segments()
        .map(|s| s.end.addr().get())
        .max()
        .unwrap_or(0)
}

/// One past the highest physical address of managed RAM.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn top_of_ram() -> u64 {
    PHYS.lock().as_ref().map_or(0, top_of_ram_locked)
}
