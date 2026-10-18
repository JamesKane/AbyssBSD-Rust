// SPDX-License-Identifier: BSD-2-Clause
//! Kernel stacks: fresh pages in kernel virtual memory with an unmapped
//! guard page below, so an overflow faults instead of corrupting memory.

use addr::VirtAddr;
use bootproto::layout::{KVA_BASE, KVA_SIZE};
use pt::Attrs;
use sync::SpinLock;
use vm::vmem::{Vmem, VmemError};

/// The kernel virtual memory arena.
static KVA: SpinLock<Vmem<1024>> = SpinLock::new(Vmem::new(KVA_BASE, KVA_SIZE, 4096));

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
        super::unmap_and_free(self.bottom(), self.pages * 4096);
        if let Err(e) = KVA.lock().free(self.guard.get(), (self.pages + 1) * 4096) {
            panic!("kernel stack {:?}: {e}", self.guard);
        }
    }
}

/// Bytes of kernel virtual memory in use.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn kva_allocated() -> u64 {
    KVA.lock().allocated()
}
