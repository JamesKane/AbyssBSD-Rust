// SPDX-License-Identifier: BSD-2-Clause
//! What a Direct Rendering Manager driver provides to the core.

use alloc::sync::Arc;

use abi::errno::Errno;
use sched::Sleepable;

use crate::card::Open;
use crate::gem::{Gem, Placement};

/// What `DRM_IOCTL_VERSION` reports. Mesa's loader picks its driver by
/// `name`, so it is the Linux driver's name (`virtio_gpu`, `panthor`),
/// not the kernel module's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverInfo {
    pub name: &'static str,
    pub desc: &'static str,
    pub date: &'static str,
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
}

/// Limits of the dumb buffers a driver's display can scan out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DumbLimits {
    /// The pitch (bytes per line) is a multiple of this: a power of two.
    pub pitch_align: u32,
    /// The largest buffer, in bytes.
    pub max_size: u64,
    /// `DRM_CAP_DUMB_PREFERRED_DEPTH`.
    pub preferred_depth: u32,
    /// `DRM_CAP_DUMB_PREFER_SHADOW`: userland should draw into its own
    /// memory and copy, because the buffer is slow to write.
    pub prefer_shadow: bool,
}

impl Default for DumbLimits {
    fn default() -> Self {
        Self {
            pitch_align: 64,
            max_size: 1 << 30,
            preferred_depth: 24,
            prefer_shadow: false,
        }
    }
}

/// What a driver's own request may use of the file that made it (the
/// render interface of a GPU driver: its buffers, its user pointers, its
/// waits).
pub struct IoctlCx<'a> {
    pub(crate) file: &'a Open,
    pub(crate) s: &'a Sleepable,
}

impl core::fmt::Debug for IoctlCx<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "IoctlCx(file {})", self.file.id())
    }
}

impl IoctlCx<'_> {
    /// The file's identity (the same until it closes: what a driver keys its
    /// per-file state by).
    #[must_use]
    pub fn file_id(&self) -> u64 {
        self.file.id()
    }

    /// The requesting thread, which may sleep on a fence.
    #[must_use]
    pub fn sleepable(&self) -> &Sleepable {
        self.s
    }

    /// Reads the caller's memory at `src` into `dst`.
    ///
    /// # Errors
    ///
    /// `ENXIO` before the kernel's environment is set, `EFAULT` for a bad
    /// address.
    pub fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        crate::env().ok_or(Errno::ENXIO)?.user.copy_in(dst, src)
    }

    /// Writes `src` to the caller's memory at `dst`.
    ///
    /// # Errors
    ///
    /// As [`IoctlCx::copy_in`].
    pub fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        crate::env().ok_or(Errno::ENXIO)?.user.copy_out(dst, src)
    }

    /// The buffer `handle` names in this file.
    #[must_use]
    pub fn gem(&self, handle: u32) -> Option<Arc<Gem>> {
        self.file.gem(handle)
    }

    /// Makes a buffer of at least `size` bytes of system memory with the
    /// placement `placement`, names it in this file, and returns the handle.
    ///
    /// # Errors
    ///
    /// `EINVAL` for size 0, `ENOMEM`, `ENOSPC` for too many handles.
    pub fn new_gem(&self, size: u64, placement: Placement) -> Result<(u32, Arc<Gem>), Errno> {
        let env = crate::env().ok_or(Errno::ENXIO)?;
        let pages = size.div_ceil(addr::PAGE_SIZE);
        let offset = self.file.card.alloc_offset(pages)?;
        let gem = Gem::new(env.pages, size, offset, placement)?;
        let handle = self.file.handles.lock().insert(Arc::clone(&gem))?;
        Ok((handle, gem))
    }

    /// Drops `handle` from this file.
    pub fn close_handle(&self, handle: u32) -> Option<Arc<Gem>> {
        self.file.handles.lock().remove(handle)
    }
}

/// A driver's side of a Direct Rendering Manager device. The core owns the file, the handle
/// tables and the dispatch; the driver says what it is and what it can
/// do.
pub trait DrmDriver: Send + Sync {
    /// Its identity.
    fn info(&self) -> DriverInfo;

    /// Its display (kernel mode setting: the ioctls that set display modes), if it has one.
    fn kms(&self) -> Option<&crate::kms::Kms> {
        None
    }

    /// Whether it is the firmware's framebuffer standing in for a display
    /// driver (docs/GRAPHICS.md §7.5): a native driver whose display shows
    /// the same screen retires it with [`crate::retire_firmware_cards`].
    fn firmware_framebuffer(&self) -> bool {
        false
    }

    /// Shows the console again, because the kernel has panicked: puts the
    /// display back on the surface the console draws on. Called with the
    /// other processors stopped and interrupts off, so it must take no lock
    /// and wait for nothing; a driver that scans out its own buffers
    /// overrides it.
    fn panic_restore(&self) {}

    /// Whether it publishes a display.
    fn has_kms(&self) -> bool {
        self.kms().is_some()
    }

    /// Its dumb-buffer limits, or `None` if it has no dumb buffers.
    fn dumb(&self) -> Option<DumbLimits> {
        None
    }

    /// Where its dumb buffers live and how they are cached. A chip whose
    /// display snoops the processor's caches keeps the default; one that
    /// does not says so, and the core calls [`DrmDriver::sync_for_device`]
    /// before it shows one.
    fn dumb_placement(&self) -> Placement {
        Placement::SYSTEM_COHERENT
    }

    /// Cleans the processor's caches for `range` (bytes) of `gem`, so a
    /// device that does not snoop reads what was written. Called for
    /// buffers whose placement is not coherent, before a commit shows
    /// them.
    fn sync_for_device(&self, gem: &Gem, range: core::ops::Range<u64>) {
        let _ = (gem, range);
    }

    /// Invalidates the processor's caches for `range` of `gem`, so the
    /// processor reads what a device wrote.
    fn sync_for_cpu(&self, gem: &Gem, range: core::ops::Range<u64>) {
        let _ = (gem, range);
    }

    /// The value of capability `cap` beyond the core's (`GET_CAP`):
    /// `None` is unknown (`EINVAL`).
    fn cap(&self, cap: u64) -> Option<u64> {
        let _ = cap;
        None
    }

    /// Its own request `nr` (`DRM_COMMAND_BASE` to `DRM_COMMAND_END`),
    /// with the argument sized as the request says; the value it returns
    /// is the call's result.
    ///
    /// # Errors
    ///
    /// `ENOTTY` for a request it lacks.
    fn ioctl(&self, nr: u8, arg: &mut [u8]) -> Result<usize, Errno> {
        let _ = (nr, arg);
        Err(Errno::ENOTTY)
    }

    /// Its own request `nr` with the file that made it: the one a driver with
    /// per-file state (an address space, contexts) overrides. By default it is
    /// [`DrmDriver::ioctl`].
    ///
    /// # Errors
    ///
    /// `ENOTTY` for a request it lacks.
    fn ioctl_cx(&self, cx: &IoctlCx<'_>, nr: u8, arg: &mut [u8]) -> Result<usize, Errno> {
        let _ = cx;
        self.ioctl(nr, arg)
    }

    /// A file of this card closed (its last reference went): the driver
    /// frees what it kept for `file`.
    fn file_closed(&self, file: u64) {
        let _ = file;
    }
}
