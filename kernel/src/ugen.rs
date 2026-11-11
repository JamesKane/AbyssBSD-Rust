// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of `ugen` (`sys/usb`): its requests reach the
//! caller's memory through `copyin` and `copyout`, and check the caller's
//! `PRIV_DRIVER`.

use abi::errno::Errno;

struct Kernel;

impl usbus::ugen::Kernel for Kernel {
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        crate::uaccess::copy_in(dst, src).map_err(|_| Errno::EFAULT)
    }

    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        crate::uaccess::copy_out(dst, src).map_err(|_| Errno::EFAULT)
    }

    fn priv_driver(&self) -> Result<(), Errno> {
        crate::fs::priv_check(crate::cred::Priv::Driver)
    }
}

/// Hands `ugen` the kernel's side and makes `hw.usb` and `/dev/usbctl`;
/// before devices attach.
pub(crate) fn init(s: &sched::Sleepable) {
    usbus::ugen::set_kernel(&Kernel);
    usbus::ugen::init(s);
}
