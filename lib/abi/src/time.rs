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

impl Timespec {
    /// Nanoseconds since the epoch as a timespec; seconds past `i64::MAX`
    /// saturate.
    #[must_use]
    #[inline]
    pub fn from_ns(ns: u64) -> Self {
        Self {
            tv_sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
            tv_nsec: (ns % 1_000_000_000).cast_signed(),
        }
    }

    /// Whether it is a valid interval or time: seconds not negative and
    /// nanoseconds under one second (FreeBSD's `itimespecfix` test).
    #[must_use]
    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.tv_sec >= 0 && self.tv_nsec >= 0 && self.tv_nsec < 1_000_000_000
    }
}

/// `struct timeval`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeval {
    pub tv_sec: TimeT,
    /// Microseconds (C type `suseconds_t`, a `long`).
    pub tv_usec: i64,
}

impl Timeval {
    /// Microseconds since the epoch as a timeval; seconds past `i64::MAX`
    /// saturate.
    #[must_use]
    #[inline]
    pub fn from_us(us: u64) -> Self {
        Self {
            tv_sec: i64::try_from(us / 1_000_000).unwrap_or(i64::MAX),
            tv_usec: (us % 1_000_000).cast_signed(),
        }
    }

    /// Whether it is valid: seconds not negative, microseconds under one
    /// second.
    #[must_use]
    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.tv_sec >= 0 && self.tv_usec >= 0 && self.tv_usec < 1_000_000
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespec_from_ns_splits_and_saturates() {
        assert_eq!(
            Timespec::from_ns(3_000_000_007),
            Timespec {
                tv_sec: 3,
                tv_nsec: 7
            }
        );
        assert_eq!(Timespec::from_ns(u64::MAX).tv_nsec, 709_551_615);
    }

    #[test]
    fn validity_matches_the_range_checks() {
        let ts = |tv_sec, tv_nsec| Timespec { tv_sec, tv_nsec };
        assert!(ts(0, 0).is_valid() && ts(5, 999_999_999).is_valid());
        assert!(!ts(-1, 0).is_valid() && !ts(0, -1).is_valid() && !ts(0, 1_000_000_000).is_valid());
        let tv = |tv_sec, tv_usec| Timeval { tv_sec, tv_usec };
        assert!(tv(1, 999_999).is_valid());
        assert!(!tv(-1, 0).is_valid() && !tv(0, 1_000_000).is_valid());
        assert_eq!(Timeval::from_us(1_500_000), tv(1, 500_000));
    }
}
