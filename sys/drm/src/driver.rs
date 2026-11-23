// SPDX-License-Identifier: BSD-2-Clause
//! What a DRM driver provides to the core.

use abi::errno::Errno;

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

/// A driver's side of a DRM device. The core owns the file, the handle
/// tables and the dispatch; the driver says what it is and what it can
/// do.
pub trait DrmDriver: Send + Sync {
    /// Its identity.
    fn info(&self) -> DriverInfo;

    /// Its display (kernel mode setting: the ioctls that set display modes), if it has one.
    fn kms(&self) -> Option<&crate::kms::Kms> {
        None
    }

    /// Whether it publishes a display.
    fn has_kms(&self) -> bool {
        self.kms().is_some()
    }

    /// Its dumb-buffer limits, or `None` if it has no dumb buffers.
    fn dumb(&self) -> Option<DumbLimits> {
        None
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
}
