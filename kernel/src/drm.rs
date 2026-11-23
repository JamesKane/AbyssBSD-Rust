// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of the Direct Rendering Manager core (`sys/drm`): the caller's memory
//! for requests that carry pointers, the page pool buffers draw on, and
//! the console for what the core refuses.

use abi::errno::Errno;

/// User memory, for `VERSION`'s name buffers.
struct UserCopies;

impl drm::UserMemory for UserCopies {
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        crate::uaccess::copy_in(dst, src).map_err(|_| Errno::EFAULT)
    }

    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        crate::uaccess::copy_out(dst, src).map_err(|_| Errno::EFAULT)
    }
}

/// Gives the core its environment; before a display driver registers.
pub(crate) fn init() {
    drm::init(drm::Env {
        user: &UserCopies,
        pages: crate::mm::space::page_ops(),
        log: |args| kprintln!("{args}"),
    });
}
