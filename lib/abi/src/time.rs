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

/// `struct timeval`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeval {
    pub tv_sec: TimeT,
    /// Microseconds (C type `suseconds_t`, a `long`).
    pub tv_usec: i64,
}

/// Clock IDs (`sys/_clock_id.h`).
pub mod clock {
    pub const REALTIME: u32 = 0;
    pub const VIRTUAL: u32 = 1;
    pub const PROF: u32 = 2;
    pub const MONOTONIC: u32 = 4;
    pub const UPTIME: u32 = 5;
    pub const UPTIME_PRECISE: u32 = 7;
    pub const UPTIME_FAST: u32 = 8;
    pub const REALTIME_PRECISE: u32 = 9;
    pub const REALTIME_FAST: u32 = 10;
    pub const MONOTONIC_PRECISE: u32 = 11;
    pub const MONOTONIC_FAST: u32 = 12;
    pub const SECOND: u32 = 13;
    pub const THREAD_CPUTIME_ID: u32 = 14;
    pub const PROCESS_CPUTIME_ID: u32 = 15;
    pub const TAI: u32 = 16;
}

pub(crate) const CONSTANTS: &[crate::layout::ConstValue] = &[
    c("CLOCK_REALTIME", clock::REALTIME),
    c("CLOCK_VIRTUAL", clock::VIRTUAL),
    c("CLOCK_PROF", clock::PROF),
    c("CLOCK_MONOTONIC", clock::MONOTONIC),
    c("CLOCK_UPTIME", clock::UPTIME),
    c("CLOCK_UPTIME_PRECISE", clock::UPTIME_PRECISE),
    c("CLOCK_UPTIME_FAST", clock::UPTIME_FAST),
    c("CLOCK_REALTIME_PRECISE", clock::REALTIME_PRECISE),
    c("CLOCK_REALTIME_FAST", clock::REALTIME_FAST),
    c("CLOCK_MONOTONIC_PRECISE", clock::MONOTONIC_PRECISE),
    c("CLOCK_MONOTONIC_FAST", clock::MONOTONIC_FAST),
    c("CLOCK_SECOND", clock::SECOND),
    c("CLOCK_THREAD_CPUTIME_ID", clock::THREAD_CPUTIME_ID),
    c("CLOCK_PROCESS_CPUTIME_ID", clock::PROCESS_CPUTIME_ID),
    c("CLOCK_TAI", clock::TAI),
];

const fn c(c_name: &'static str, value: u32) -> crate::layout::ConstValue {
    crate::layout::ConstValue {
        c_name,
        header: "time.h",
        value: value as i64,
        arches: crate::layout::Arches::All,
    }
}
