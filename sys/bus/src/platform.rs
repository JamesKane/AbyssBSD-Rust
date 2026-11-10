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
    /// Reads an I/O port `width` bytes wide (1, 2, or 4). Platforms without
    /// port I/O never describe I/O-port resources, so are never asked.
    fn io_read(&self, port: u16, width: u8) -> u32;
    /// Writes an I/O port `width` bytes wide.
    fn io_write(&self, port: u16, width: u8, v: u32);
    /// Sleeps for `ns` nanoseconds (bounded waits in attach and stop).
    fn sleep_ns(&self, s: &Sleepable, ns: u64);
    /// Sleeps on `q` until `cond` holds or `ns` nanoseconds pass
    /// (FreeBSD's `msleep` with a timeout); whether `cond` held. `cond`
    /// runs with the queue locked, as for `WaitQueue::wait_until`; whoever
    /// makes it true (an interrupt filter) wakes `q`.
    fn wait_timeout(
        &self,
        s: &Sleepable,
        q: &sched::WaitQueue,
        cond: &mut dyn FnMut() -> bool,
        ns: u64,
    ) -> bool;
    /// Busy-waits for `us` microseconds (FreeBSD's `DELAY`): the short,
    /// polled waits register protocols need, too short to sleep for.
    fn delay_us(&self, us: u64);
    /// A console line (attach messages, diagnostics).
    fn log(&self, args: core::fmt::Arguments<'_>);
}
