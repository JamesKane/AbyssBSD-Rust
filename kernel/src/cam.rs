// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of CAM's user interfaces (`sys/cam`) and the NVMe
//! control devices: their requests reach the caller's memory through
//! `copyin` and `copyout`, and check the caller's securelevel.

use abi::errno::Errno;

/// User memory and the caller's credentials.
struct Host;

impl cam::Host for Host {
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        crate::uaccess::copy_in(dst, src).map_err(|_| Errno::EFAULT)
    }

    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        crate::uaccess::copy_out(dst, src).map_err(|_| Errno::EFAULT)
    }

    fn securelevel_gt(&self, level: i32) -> Result<(), Errno> {
        crate::fs::cred().securelevel_gt(level)
    }
}

/// Hands CAM the copies and makes `/dev/xpt0`; after devfs, before
/// devices attach.
pub(crate) fn init() {
    cam::set_host(&Host);
    cam::init();
}
