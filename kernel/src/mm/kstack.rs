// SPDX-License-Identifier: BSD-2-Clause
//! Kernel stacks: fresh pages in kernel virtual memory with an unmapped
//! guard page below, so an overflow faults instead of corrupting memory.
//!
//! A freed stack stays mapped in a small cache for the next thread, as
//! FreeBSD's kernel-stack cache does: unmapping one means a TLB shootdown
//! of every other CPU, and a thread a `fork` makes would pay for it at
//! every exit. Only stacks of the usual size are kept, and they are not
//! zeroed again.

use addr::VirtAddr;
use pt::Attrs;
use sync::SpinLock;
use vm::vmem::VmemError;

use super::KVA;

/// Why a stack could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StackError {
    /// No kernel virtual memory.
    Address(VmemError),
    /// No frames, or the mapping failed.
    Memory(pt::MapError),
}

/// A kernel stack; freed (unmapped, frames returned) on drop.
#[derive(Debug)]
pub(crate) struct KernelStack {
    /// The guard page's address; the stack proper starts one page up.
    guard: VirtAddr,
    pages: u64,
}

impl KernelStack {
    /// A stack of `pages` pages.
    pub(crate) fn new(pages: u64) -> Result<Self, StackError> {
        if pages == CACHED_PAGES
            && let Some(guard) = CACHE.lock().take()
        {
            return Ok(Self { guard, pages });
        }
        let span = (pages + 1) * 4096;
        let guard = VirtAddr::new(KVA.lock().alloc(span, 4096).map_err(StackError::Address)?);
        if let Err(e) = super::map_fresh(guard + 4096, pages * 4096, Attrs::KERNEL_RW) {
            // Cannot fail: the range was just allocated.
            let _ = KVA.lock().free(guard.get(), span);
            return Err(StackError::Memory(e));
        }
        Ok(Self { guard, pages })
    }

    /// The initial stack pointer (one past the highest byte).
    pub(crate) fn top(&self) -> VirtAddr {
        self.guard + (self.pages + 1) * 4096
    }

    /// The lowest usable byte.
    pub(crate) fn bottom(&self) -> VirtAddr {
        self.guard + 4096
    }

    /// The guard page.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub(crate) fn guard(&self) -> VirtAddr {
        self.guard
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        if self.pages == CACHED_PAGES && CACHE.lock().put(self.guard) {
            return;
        }
        super::unmap_and_free(self.bottom(), self.pages * 4096);
        if let Err(e) = KVA.lock().free(self.guard.get(), (self.pages + 1) * 4096) {
            panic!("kernel stack {:?}: {e}", self.guard);
        }
    }
}

/// The size of the stacks the cache keeps: threads' usual one.
const CACHED_PAGES: u64 = crate::threads::STACK_PAGES;

/// How many freed stacks stay mapped for reuse.
const CACHED: usize = 32;

/// Freed stacks, still mapped: their guard pages.
static CACHE: SpinLock<Cache> = SpinLock::new(Cache {
    guards: [VirtAddr::ZERO; CACHED],
    len: 0,
});

struct Cache {
    guards: [VirtAddr; CACHED],
    len: usize,
}

impl Cache {
    fn take(&mut self) -> Option<VirtAddr> {
        self.len = self.len.checked_sub(1)?;
        Some(self.guards[self.len])
    }

    /// Keeps a freed stack, if there is room.
    fn put(&mut self, guard: VirtAddr) -> bool {
        let Some(slot) = self.guards.get_mut(self.len) else {
            return false;
        };
        *slot = guard;
        self.len += 1;
        true
    }
}

/// Bytes of kernel virtual memory in use, cached stacks aside (they are
/// free in all but name).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn kva_allocated() -> u64 {
    let cached = CACHE.lock().len as u64 * (CACHED_PAGES + 1) * 4096;
    KVA.lock().allocated().saturating_sub(cached)
}

/// Pages held by cached stacks, for leak checks to count as free.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn cached_pages() -> u64 {
    CACHE.lock().len as u64 * CACHED_PAGES
}
