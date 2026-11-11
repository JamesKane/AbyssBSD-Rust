// SPDX-License-Identifier: BSD-2-Clause
//! BCD calendar times, as RTC chips keep them (FreeBSD's `struct
//! bcd_clocktime`), and their conversions.
//!
//! Behavior reference: sys/kern/subr_clock.c (`clock_bcd_to_ts`,
//! `clock_ts_to_bcd`, `clock_print_bcd`).

use crate::DateTime;

/// A calendar time in BCD.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bcd {
    /// The year: two BCD digits, or four with the century above them.
    pub year: u16,
    /// 0x01 to 0x12.
    pub mon: u8,
    /// 0x01 to 0x31.
    pub day: u8,
    /// 0x00 to 0x23.
    pub hour: u8,
    /// 0x00 to 0x59.
    pub min: u8,
    /// 0x00 to 0x59.
    pub sec: u8,
    /// The day of the week, 0 (Sunday) to 6, in binary.
    pub dow: u8,
    /// Within the second, in binary.
    pub nsec: u32,
}

/// Whether `v` is two valid BCD digits (`validbcd`).
#[must_use]
pub const fn valid(v: u8) -> bool {
    v & 0x0f < 10 && v >> 4 < 10
}

/// Two BCD digits as their value; `valid(v)` must hold.
#[must_use]
pub const fn from_bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0x0f)
}

/// `v` (below 100) as two BCD digits.
#[must_use]
pub const fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

/// Days from 1970-01-01 to `t`'s date.
fn weekday(t: &DateTime) -> u8 {
    // 1970-01-01 was a Thursday.
    t.unix_seconds()
        .map_or(0, |s| u8::try_from((s / 86_400 + 4) % 7).unwrap_or(0))
}

impl Bcd {
    /// `clock_bcd_to_ts`: the calendar time, if every field is valid BCD
    /// and in range. Two-digit years pivot on 70, as FreeBSD's
    /// `clock_ct_to_ts` does: 00-69 are 20xx, 70-99 19xx.
    #[must_use]
    pub fn to_datetime(&self) -> Option<DateTime> {
        let [byear, bcent] = self.year.to_le_bytes();
        if ![
            bcent, byear, self.mon, self.day, self.hour, self.min, self.sec,
        ]
        .iter()
        .all(|&v| valid(v))
        {
            return None;
        }
        let mut year = u16::from(from_bcd(byear)) + u16::from(from_bcd(bcent)) * 100;
        if year < 70 {
            year += 2000;
        } else if year < 200 {
            year += 1900;
        }
        let t = DateTime {
            year,
            month: from_bcd(self.mon),
            day: from_bcd(self.day),
            hour: from_bcd(self.hour),
            minute: from_bcd(self.min),
            second: from_bcd(self.sec),
            nanosecond: self.nsec,
        };
        // `clock_ct_to_ts` takes no leap second.
        (t.valid() && t.second < 60).then_some(t)
    }

    /// `clock_ts_to_bcd`: `t` in BCD, the century above the year's two
    /// digits, with its day of the week.
    #[must_use]
    pub fn from_datetime(t: &DateTime) -> Self {
        let y = t.year % 10_000;
        let lo = u8::try_from(y % 100).unwrap_or(0);
        let hi = u8::try_from(y / 100).unwrap_or(0);
        Self {
            year: u16::from(to_bcd(lo)) | (u16::from(to_bcd(hi)) << 8),
            mon: to_bcd(t.month),
            day: to_bcd(t.day),
            hour: to_bcd(t.hour),
            min: to_bcd(t.minute),
            sec: to_bcd(t.second),
            dow: weekday(t),
            nsec: t.nanosecond,
        }
    }
}

impl core::fmt::Display for Bcd {
    /// `clock_print_bcd` with nine digits of the second.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:04x}-{:02x}-{:02x} {:02x}:{:02x}:{:02x}.{:09}",
            self.year, self.mon, self.day, self.hour, self.min, self.sec, self.nsec
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let b = Bcd {
            year: 0x26,
            mon: 0x11,
            day: 0x10,
            hour: 0x12,
            min: 0x34,
            sec: 0x56,
            dow: 0,
            nsec: 0,
        };
        let t = b.to_datetime().unwrap();
        assert_eq!(
            (t.year, t.month, t.day, t.hour, t.minute, t.second),
            (2026, 11, 10, 12, 34, 56)
        );
        let back = Bcd::from_datetime(&t);
        // 2026-11-10 is a Tuesday; the century rides above the year.
        assert_eq!(
            back,
            Bcd {
                year: 0x2026,
                dow: 2,
                ..b
            }
        );
        assert_eq!(alloc::format!("{back}"), "2026-11-10 12:34:56.000000000");
        // Years pivot on 70.
        assert_eq!(Bcd { year: 0x70, ..b }.to_datetime().unwrap().year, 1970);
        assert_eq!(Bcd { year: 0x69, ..b }.to_datetime().unwrap().year, 2069);
        // Bad BCD, and values out of range, are refused.
        for bad in [
            Bcd { sec: 0x5a, ..b },
            Bcd { sec: 0x60, ..b },
            Bcd { mon: 0x13, ..b },
            Bcd { day: 0x31, ..b },
            Bcd { hour: 0x24, ..b },
            Bcd { year: 0x2a, ..b },
        ] {
            assert_eq!(bad.to_datetime(), None, "{bad:?}");
        }
        assert_eq!(Bcd::from_datetime(&DateTime::from_unix(0, 0)).dow, 4);
    }
}
