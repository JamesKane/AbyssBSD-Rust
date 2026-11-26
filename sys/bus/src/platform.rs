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
    /// The physical range of the loader's framebuffer that the kernel's
    /// console draws on, if there is one (it is among the sensitive ranges).
    fn console_framebuffer(&self) -> Option<(u64, u64)> {
        None
    }
    /// Has the kernel's console draw from now on on a display driver's
    /// framebuffer: `width` by `height` pixels of XRGB8888, `stride`
    /// pixels a scan line, in one run of memory at `pa` the driver never
    /// frees and shows on its display. Whether it took (the default: no
    /// console to give it to).
    fn console_retarget(
        &self,
        s: &Sleepable,
        pa: u64,
        width: u32,
        height: u32,
        stride: u32,
    ) -> bool {
        let _ = (s, pa, width, height, stride);
        false
    }
    /// Whether `[pa, pa + len)` overlaps a sensitive range other than the
    /// console's framebuffer.
    fn sensitive_beyond_console(&self, pa: u64, len: u64) -> bool {
        self.sensitive(pa, len)
    }
    /// [`Platform::map_device`], write-combining: for a prefetchable
    /// aperture (video memory). The default maps it uncached.
    fn map_device_wc(&self, pa: u64, len: u64) -> Option<usize> {
        self.map_device(pa, len)
    }
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
    /// The bytes of the first valid ACPI table with signature `sig`, if the
    /// firmware gave one (a driver reads the video BIOS table this way).
    fn acpi_table(&self, sig: [u8; 4]) -> Option<alloc::vec::Vec<u8>> {
        let _ = sig;
        None
    }
    /// A console line (attach messages, diagnostics).
    fn log(&self, args: core::fmt::Arguments<'_>);
}
