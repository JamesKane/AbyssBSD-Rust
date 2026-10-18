// SPDX-License-Identifier: BSD-2-Clause
//! Time types. Behavior reference: sys/sys/_timespec.h, sys/sys/_types.h.

/// `time_t`: 64-bit on every architecture AbyssBSD supports.
pub type TimeT = i64;

/// `struct timespec`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timespec {
    /// Seconds.
    pub tv_sec: TimeT,
    /// Nanoseconds, 0 to 999 999 999 (C type `long`).
    pub tv_nsec: i64,
}
