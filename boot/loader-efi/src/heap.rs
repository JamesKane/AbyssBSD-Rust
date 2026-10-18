// SPDX-License-Identifier: BSD-2-Clause
//! The loader's heap: UEFI pool memory, available only while boot services
//! run. After [`disable`] every allocation fails, so a stray allocation
//! after `ExitBootServices` aborts instead of corrupting memory.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicPtr, Ordering};

use efi::{MemoryType, SystemTable, raw};

static SYSTEM_TABLE: AtomicPtr<raw::SystemTable> = AtomicPtr::new(core::ptr::null_mut());

/// Enables the heap.
pub(crate) fn enable(st: &SystemTable) {
    SYSTEM_TABLE.store(st.as_ptr().cast_mut(), Ordering::Release);
}

/// Disables the heap for good, before boot services are exited.
pub(crate) fn disable() {
    SYSTEM_TABLE.store(core::ptr::null_mut(), Ordering::Release);
}

/// Pool memory is 8-byte aligned. Larger alignments over-allocate and keep
/// the pool pointer just below the aligned block.
struct Pool;

const HEADER: usize = size_of::<usize>();

// SAFETY: allocations come from firmware pool memory, sized and aligned as
// requested; `dealloc` returns exactly what `alloc` obtained.
unsafe impl GlobalAlloc for Pool {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let st = SYSTEM_TABLE.load(Ordering::Acquire);
        // SAFETY: non-null only while boot services run (see `disable`).
        let Some(st) = (unsafe { SystemTable::from_raw(st) }) else {
            return core::ptr::null_mut();
        };
        let align = layout.align().max(HEADER);
        let Some(size) = layout.size().checked_add(align + HEADER) else {
            return core::ptr::null_mut();
        };
        let Ok(raw) = st.allocate_pool(MemoryType::LOADER_DATA, size) else {
            return core::ptr::null_mut();
        };
        let user = (raw as usize + HEADER).next_multiple_of(align);
        // SAFETY: `user - HEADER` is inside the allocation (user ≥ raw +
        // HEADER) and usize-aligned since `align` ≥ HEADER.
        unsafe { ((user - HEADER) as *mut usize).write(raw as usize) };
        user as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let st = SYSTEM_TABLE.load(Ordering::Acquire);
        // SAFETY: as in `alloc`.
        let Some(st) = (unsafe { SystemTable::from_raw(st) }) else {
            return;
        };
        // SAFETY: `alloc` stored the pool pointer just below `ptr`.
        let raw = unsafe { ((ptr as usize - HEADER) as *const usize).read() };
        // SAFETY: `raw` came from `allocate_pool` and is freed once.
        let _ = unsafe { st.free_pool(raw as *mut u8) };
    }
}

#[global_allocator]
static HEAP: Pool = Pool;
