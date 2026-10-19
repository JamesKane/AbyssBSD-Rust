// SPDX-License-Identifier: BSD-2-Clause
//! Scatter/gather I/O (`sys/uio.h`).

/// `struct iovec`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Iovec {
    pub iov_base: u64,
    pub iov_len: u64,
}

/// The most `iovec`s one call takes (`IOV_MAX`, `UIO_MAXIOV`).
pub const IOV_MAX: usize = 1024;
