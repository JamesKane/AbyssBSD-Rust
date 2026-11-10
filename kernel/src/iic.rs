// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of `iic` (`sys/iicbus`): its requests reach the
//! caller's memory through `copyin` and `copyout`.

use abi::errno::Errno;

/// User memory, for `I2CRDWR`'s messages and buffers.
struct UserCopies;

impl iicbus::iic::UserMemory for UserCopies {
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        crate::uaccess::copy_in(dst, src).map_err(|_| Errno::EFAULT)
    }

    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        crate::uaccess::copy_out(dst, src).map_err(|_| Errno::EFAULT)
    }
}

/// Hands `iic` the copies; before devices attach.
pub(crate) fn init() {
    iicbus::set_user_memory(&UserCopies);
}
