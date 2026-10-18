// SPDX-License-Identifier: BSD-2-Clause
//! What the kernel provides to the device manager.

use sched::Sleepable;
use sysctl::Tree;
use vm::dma::DmaMemory;

/// Kernel services the manager needs.
pub trait Platform: Sync {
    /// Maps `len` bytes of device registers at physical `pa` uncached;
    /// returns the virtual address.
    fn map_device(&self, pa: u64, len: u64) -> Option<usize>;
    /// Unmaps a mapping from [`Platform::map_device`].
    fn unmap_device(&self, va: usize, len: u64);
    /// Memory for DMA.
    fn dma_memory(&self) -> &'static dyn DmaMemory;
    /// One past the highest RAM address.
    fn top_of_ram(&self) -> u64;
    /// Whether `[pa, pa + len)` overlaps a range leaf drivers may not map
    /// (interrupt controllers, IOMMUs, ECAM, consoles).
    fn sensitive(&self, pa: u64, len: u64) -> bool;
    /// Runs `f` on the sysctl tree, exclusively.
    fn sysctl(&self, s: &Sleepable, f: &mut dyn FnMut(&mut Tree));
    /// A console line (attach messages, diagnostics).
    fn log(&self, args: core::fmt::Arguments<'_>);
}
