// SPDX-License-Identifier: BSD-2-Clause
//! The real-time clock class: calendar time from battery-backed clocks
//! (`atrtc`, `efirtc`), which the kernel reads once to know the time of
//! day.
//!
//! Clocks register themselves; [`read`] asks them in registration order
//! and takes the first answer, as FreeBSD's `inittodr` takes the
//! registered clock's.
//!
//! Behavior reference: sys/kern/subr_rtc.c, sys/kern/subr_clock.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use sync::SpinLock;

/// A calendar time, UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    /// The year (`2026`).
    pub year: u16,
    /// 1 to 12.
    pub month: u8,
    /// 1 to 31.
    pub day: u8,
    /// 0 to 23.
    pub hour: u8,
    /// 0 to 59.
    pub minute: u8,
    /// 0 to 59 (60 for a leap second).
    pub second: u8,
    /// Within the second.
    pub nanosecond: u32,
}

/// Days from 1970-01-01 to `y`-`m`-`d` (proleptic Gregorian), the
/// "days from civil" algorithm.
fn days_from_civil(y: i64, m: u8, d: u8) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl DateTime {
    /// Whether every field is in range (a clock read mid-update, or a dead
    /// battery, reads garbage).
    #[must_use]
    pub fn valid(&self) -> bool {
        let leap = self.year.is_multiple_of(4)
            && (!self.year.is_multiple_of(100) || self.year.is_multiple_of(400));
        let days = match self.month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return false,
        };
        (1970..=9999).contains(&self.year)
            && (1..=days).contains(&self.day)
            && self.hour < 24
            && self.minute < 60
            && self.second <= 60
            && self.nanosecond < 1_000_000_000
    }

    /// The calendar time `secs` seconds after the Unix epoch (the
    /// "civil from days" algorithm).
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "each field is in range by construction"
    )]
    pub fn from_unix(secs: u64, nanosecond: u32) -> Self {
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
        Self {
            year,
            month,
            day,
            hour: (rem / 3600) as u8,
            minute: (rem / 60 % 60) as u8,
            second: (rem % 60) as u8,
            nanosecond,
        }
    }

    /// Seconds since the Unix epoch, if the time is valid.
    #[must_use]
    pub fn unix_seconds(&self) -> Option<u64> {
        if !self.valid() {
            return None;
        }
        let days = days_from_civil(i64::from(self.year), self.month, self.day);
        let secs = days * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second);
        u64::try_from(secs).ok()
    }
}

impl core::fmt::Display for DateTime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// A battery-backed clock.
pub trait RealTimeClock: Send + Sync {
    /// Its name (`atrtc0`).
    fn name(&self) -> &str;
    /// The current time, or `None` if it cannot be read.
    fn read(&self) -> Option<DateTime>;
}

static CLOCKS: SpinLock<Vec<Arc<dyn RealTimeClock>>> = SpinLock::new(Vec::new());

/// Registers `clock`.
pub fn register(clock: Arc<dyn RealTimeClock>) {
    CLOCKS.lock().push(clock);
}

/// Unregisters `clock` (its driver is detaching).
pub fn unregister(clock: &Arc<dyn RealTimeClock>) {
    CLOCKS.lock().retain(|c| !Arc::ptr_eq(c, clock));
}

/// The first registered clock's valid answer, with its name.
#[must_use]
pub fn read() -> Option<(alloc::string::String, DateTime)> {
    let clocks = CLOCKS.lock().clone();
    clocks.iter().find_map(|c| {
        c.read()
            .filter(DateTime::valid)
            .map(|t| (c.name().into(), t))
    })
}

/// Every registered clock's answer (diagnostics and tests).
#[must_use]
pub fn read_all() -> Vec<(alloc::string::String, Option<DateTime>)> {
    let clocks = CLOCKS.lock().clone();
    clocks.iter().map(|c| (c.name().into(), c.read())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> DateTime {
        DateTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            nanosecond: 0,
        }
    }

    #[test]
    fn unix_time() {
        assert_eq!(t(1970, 1, 1, 0, 0, 0).unix_seconds(), Some(0));
        assert_eq!(t(2000, 3, 1, 0, 0, 0).unix_seconds(), Some(951_868_800));
        assert_eq!(
            t(2026, 10, 18, 12, 34, 56).unix_seconds(),
            Some(1_792_326_896)
        );
        assert_eq!(
            t(2024, 2, 29, 23, 59, 59).unix_seconds(),
            Some(1_709_251_199)
        );
    }

    #[test]
    fn round_trips() {
        for secs in [0, 951_868_800, 1_709_251_199, 1_792_326_896, 4_102_444_799] {
            assert_eq!(
                DateTime::from_unix(secs, 0).unix_seconds(),
                Some(secs),
                "{secs}"
            );
        }
        assert_eq!(
            DateTime::from_unix(1_792_326_896, 5),
            DateTime {
                nanosecond: 5,
                ..t(2026, 10, 18, 12, 34, 56)
            }
        );
    }

    #[test]
    fn invalid_times() {
        assert_eq!(
            t(2023, 2, 29, 0, 0, 0).unix_seconds(),
            None,
            "not a leap year"
        );
        assert_eq!(t(2026, 13, 1, 0, 0, 0).unix_seconds(), None);
        assert_eq!(t(2026, 4, 31, 0, 0, 0).unix_seconds(), None);
        assert_eq!(t(1969, 12, 31, 23, 59, 59).unix_seconds(), None);
        assert_eq!(t(2026, 1, 1, 24, 0, 0).unix_seconds(), None);
        assert_eq!(
            t(2100, 2, 29, 0, 0, 0).unix_seconds(),
            None,
            "1900-style century"
        );
        assert!(t(2000, 2, 29, 0, 0, 0).valid(), "but 2000 was a leap year");
    }
}
