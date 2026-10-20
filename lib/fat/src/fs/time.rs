// SPDX-License-Identifier: BSD-2-Clause
//! FAT times: a date (years since 1980, month, day), a time in 2-second
//! steps, and hundredths of a second, in local time. FreeBSD's msdosfs
//! uses the kernel's offset from UTC (`utc_offset()`), which is 0 on a
//! system whose clock is in UTC; so is it here.
//!
//! Behavior reference: sys/kern/subr_fattime.c (`timespec2fattime`,
//! `fattime2timespec`).

/// Seconds in a day.
const DAY: i64 = 86_400;
/// Days from 1970-01-01 to 1980-01-01.
const T1980: i64 = 3652;

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A time as FAT keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FatTime {
    pub date: u16,
    pub time: u16,
    /// Hundredths of a second past the 2-second step (0..200).
    pub hundredths: u8,
}

/// `sec`.`nsec` since the epoch as a FAT time (FreeBSD's
/// `timespec2fattime`): a date before 1980 is 1980-01-01, the time of day
/// kept.
#[must_use]
pub fn to_fat(sec: i64, nsec: u32) -> FatTime {
    let t = sec;
    let hundredths = u8::try_from((sec & 1) * 100 + i64::from(nsec / 10_000_000)).unwrap_or(0);
    let tod = t.rem_euclid(DAY);
    let time = u16::try_from(((tod % 60) / 2) | (((tod / 60) % 60) << 5) | ((tod / 3600) << 11))
        .unwrap_or(0);
    let days = t.div_euclid(DAY);
    let date = if days < T1980 {
        0x0021
    } else {
        let (y, m, d) = civil_from_days(days);
        u16::try_from((((y - 1980) & 0x7f) << 9) | (m << 5) | d).unwrap_or(0x21)
    };
    FatTime {
        date,
        time,
        hundredths,
    }
}

/// A FAT time as seconds and nanoseconds since the epoch (FreeBSD's
/// `fattime2timespec`).
#[must_use]
pub fn from_fat(t: FatTime) -> (i64, u32) {
    let dt = i64::from(t.time);
    let mut sec = (dt & 0x1f) << 1;
    sec += ((dt & 0x7e0) >> 5) * 60;
    sec += ((dt & 0xf800) >> 11) * 3600;
    sec += i64::from(t.hundredths / 100);
    let nsec = u32::from(t.hundredths % 100) * 10_000_000;
    let dd = i64::from(t.date);
    let (y, m, d) = (
        1980 + (dd >> 9),
        ((dd >> 5) & 0xf).max(1),
        (dd & 0x1f).max(1),
    );
    sec += days_from_civil(y, m, d) * DAY;
    (sec, nsec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_round_trip_in_two_second_steps() {
        // 2026-10-19 12:34:57.250 UTC.
        let t = to_fat(1_792_413_297, 250_000_000);
        assert_eq!(t.date, ((2026 - 1980) << 9) | (10 << 5) | 19);
        assert_eq!(t.time, (12 << 11) | (34 << 5) | 28);
        assert_eq!(t.hundredths, 125);
        assert_eq!(from_fat(t), (1_792_413_297, 250_000_000));
        // Without hundredths, the odd second is lost.
        let plain = FatTime { hundredths: 0, ..t };
        assert_eq!(from_fat(plain).0, 1_792_413_296);
        // Before 1980: the first day, the time of day kept.
        let early = to_fat(86_400 * 10 + 3600, 0);
        assert_eq!(early.date, 0x21);
        assert_eq!(early.time, 1 << 11);
        // A leap day, and 2100 (not a leap year).
        let leap = to_fat(days_from_civil(2024, 2, 29) * DAY, 0);
        assert_eq!(leap.date, ((2024 - 1980) << 9) | (2 << 5) | 29);
        let day = days_from_civil(2100, 3, 1) * DAY;
        assert_eq!(from_fat(to_fat(day, 0)).0, day);
    }
}
