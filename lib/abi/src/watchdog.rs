// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/fido` (`sys/watchdog.h`): the requests `watchdogd(8)` and
//! `watchdog(8)` send, the timeout encodings they use, and `sbintime_t`
//! conversions (`sys/time.h`).
//!
//! FreeBSD 15 takes timeouts as `sbintime_t` (32.32 fixed-point seconds);
//! FreeBSD 14's requests, which `COMPAT_FREEBSD14` keeps, took a `u_int`
//! with flags and a power of two of nanoseconds (`WD_INTERVAL`). Those
//! older request numbers are defined in sys/dev/watchdog/watchdog.c, not
//! the header, so `abi-check` cannot hold them.
//!
//! Behavior reference: sys/dev/watchdog/watchdog.c, sys/sys/time.h.

use crate::ioctl::{IOC_IN, IOC_OUT};
use crate::layout::{Arches, ConstValue};

/// `_PATH_WATCHDOG`, under `/dev`.
pub const PATH_WATCHDOG: &[u8] = b"fido";

/// `sbintime_t`: seconds in the high 32 bits, a binary fraction in the low.
pub type Sbintime = i64;
/// `SBT_1S`.
pub const SBT_1S: Sbintime = 1 << 32;
/// `SBT_1MS`.
pub const SBT_1MS: Sbintime = SBT_1S / 1000;

const NS_PER_S: i128 = 1_000_000_000;

/// `sbttons`: rounded down (toward minus infinity), as FreeBSD's
/// `__stime64_scale64_floor`.
#[must_use]
pub fn sbt_to_ns(sbt: Sbintime) -> i64 {
    let v = (i128::from(sbt) * NS_PER_S).div_euclid(i128::from(SBT_1S));
    // |sbt| * 10^9 / 2^32 is under 2^63 for any i64 `sbt`.
    i64::try_from(v).unwrap_or(if sbt < 0 { i64::MIN } else { i64::MAX })
}

/// `nstosbt`: rounded up, so `sbt_to_ns(ns_to_sbt(n)) == n`; saturating
/// where FreeBSD's would overflow.
#[must_use]
pub fn ns_to_sbt(ns: i64) -> Sbintime {
    let n = i128::from(ns) * i128::from(SBT_1S);
    let v = -((-n).div_euclid(NS_PER_S));
    i64::try_from(v).unwrap_or(if ns < 0 { i64::MIN } else { i64::MAX })
}

/// `mstosbt`, rounded up as FreeBSD's.
#[must_use]
pub fn ms_to_sbt(ms: i64) -> Sbintime {
    let n = i128::from(ms) * i128::from(SBT_1S);
    let v = -((-n).div_euclid(1000));
    i64::try_from(v).unwrap_or(if ms < 0 { i64::MIN } else { i64::MAX })
}

/// `flsll`: the 1-based index of the highest bit set, 0 for none.
#[must_use]
pub fn flsll(v: i64) -> u32 {
    64 - v.cast_unsigned().leading_zeros()
}

/// The power of two of nanoseconds a timeout is reported as
/// (`wd_last_u`, `hw.watchdog.wd_last_u`): `flsll(sbttons(sbt))`, 0 for
/// none.
#[must_use]
pub fn sbt_to_pow2ns(sbt: Sbintime) -> u32 {
    if sbt == 0 { 0 } else { flsll(sbt_to_ns(sbt)) }
}

/// The timeout a `WD_INTERVAL` value names: 2^`pow2ns` nanoseconds, or
/// none for [`WD_TO_NEVER`]. FreeBSD 16 shifts a 64-bit one (15.1 shifts
/// an `int`, which overflows past `WD_TO_1SEC`).
#[must_use]
pub fn pow2ns_to_sbt(pow2ns: u32) -> Sbintime {
    match pow2ns {
        WD_TO_NEVER => 0,
        // 2^63 ns and up do not fit; the longest there is.
        p if p >= 63 => Sbintime::MAX,
        p => ns_to_sbt(1i64 << p),
    }
}

// Every argument is an `int`, `u_int`, or `sbintime_t`.
#[allow(clippy::cast_possible_truncation, reason = "argument sizes are 4 or 8")]
const fn ioc(dir: u32, num: u32, len: usize) -> u32 {
    dir | ((len as u32) << 16) | (b'W' as u32) << 8 | num
}

const SBT_LEN: usize = size_of::<Sbintime>();
const INT_LEN: usize = size_of::<i32>();

/// `WDIOC_PATPAT`: defined, but no `wd_ioctl` case takes it (`ENOTTY`).
pub const WDIOC_PATPAT: u32 = ioc(IOC_IN, 52, SBT_LEN);
/// `WDIOC_SETTIMEOUT`: arm (or disarm, with 0) and pat.
pub const WDIOC_SETTIMEOUT: u32 = ioc(IOC_IN, 53, SBT_LEN);
/// `WDIOC_GETTIMEOUT`: the last timeout set.
pub const WDIOC_GETTIMEOUT: u32 = ioc(IOC_OUT, 54, SBT_LEN);
/// `WDIOC_GETTIMELEFT`.
pub const WDIOC_GETTIMELEFT: u32 = ioc(IOC_OUT, 55, SBT_LEN);
/// `WDIOC_GETPRETIMEOUT`.
pub const WDIOC_GETPRETIMEOUT: u32 = ioc(IOC_OUT, 56, SBT_LEN);
/// `WDIOC_SETPRETIMEOUT`.
pub const WDIOC_SETPRETIMEOUT: u32 = ioc(IOC_IN, 57, SBT_LEN);
/// `WDIOC_SETPRETIMEOUTACT`: what the pre-timeout does (`WD_SOFT_*`).
pub const WDIOC_SETPRETIMEOUTACT: u32 = ioc(IOC_IN, 48, INT_LEN);
/// `WDIOC_SETSOFT`: the software timer instead of the hardware.
pub const WDIOC_SETSOFT: u32 = ioc(IOC_IN, 49, INT_LEN);
/// `WDIOC_SETSOFTTIMEOUTACT`.
pub const WDIOC_SETSOFTTIMEOUTACT: u32 = ioc(IOC_IN, 50, INT_LEN);
/// `WDIOC_CONTROL`: `WD_CTRL_*`.
pub const WDIOC_CONTROL: u32 = ioc(IOC_IN, 51, INT_LEN);

/// FreeBSD 14's requests (`COMPAT_FREEBSD14`, watchdog.c's `*_14`).
pub const WDIOCPATPAT_14: u32 = ioc(IOC_IN, 42, INT_LEN);
pub const WDIOC_SETTIMEOUT_14: u32 = ioc(IOC_IN, 43, INT_LEN);
pub const WDIOC_GETTIMEOUT_14: u32 = ioc(IOC_OUT, 44, INT_LEN);
pub const WDIOC_GETTIMELEFT_14: u32 = ioc(IOC_OUT, 45, INT_LEN);
pub const WDIOC_GETPRETIMEOUT_14: u32 = ioc(IOC_OUT, 46, INT_LEN);
pub const WDIOC_SETPRETIMEOUT_14: u32 = ioc(IOC_IN, 47, INT_LEN);

/// The `u_int` timeout's flags: arm with the interval, ...
pub const WD_ACTIVE: u32 = 0x800_0000;
/// ... let the kernel pat (not implemented by FreeBSD: `ENOSYS`), ...
pub const WD_PASSIVE: u32 = 0x40_0000;
/// ... or pat with the last timeout.
pub const WD_LASTVAL: u32 = 0x20_0000;
/// The power of two of nanoseconds.
pub const WD_INTERVAL: u32 = 0xff;

pub const WD_TO_NEVER: u32 = 0;
pub const WD_TO_1MS: u32 = 20;
pub const WD_TO_125MS: u32 = 27;
pub const WD_TO_250MS: u32 = 28;
pub const WD_TO_500MS: u32 = 29;
pub const WD_TO_1SEC: u32 = 30;
pub const WD_TO_2SEC: u32 = 31;
pub const WD_TO_4SEC: u32 = 32;
pub const WD_TO_8SEC: u32 = 33;
pub const WD_TO_16SEC: u32 = 34;
pub const WD_TO_32SEC: u32 = 35;
pub const WD_TO_64SEC: u32 = 36;
pub const WD_TO_128SEC: u32 = 37;

/// `WDIOC_CONTROL`'s argument.
pub const WD_CTRL_DISABLE: i32 = 0;
pub const WD_CTRL_ENABLE: i32 = 1;
pub const WD_CTRL_RESET: i32 = 2;

/// What a pre-timeout or software timeout does.
pub const WD_SOFT_PANIC: i32 = 0x01;
pub const WD_SOFT_DDB: i32 = 0x02;
pub const WD_SOFT_LOG: i32 = 0x04;
pub const WD_SOFT_PRINTF: i32 = 0x08;
pub const WD_SOFT_MASK: i32 = 0x0f;

/// What a FreeBSD 14 `WDIOCPATPAT` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyPat {
    /// Pat with this `WD_INTERVAL` value (`WD_TO_NEVER` disarms).
    Interval(u32),
    /// Pat with the last timeout (`WD_LASTVAL`).
    LastValue,
}

/// Why a FreeBSD 14 `WDIOCPATPAT` value is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyPatError {
    /// Unknown bits, both modes, or no mode with an interval (`EINVAL`).
    Invalid,
    /// `WD_PASSIVE`, which FreeBSD never implemented (`ENOSYS`).
    Passive,
}

/// Decodes a FreeBSD 14 `WDIOCPATPAT` value as `wd_ioctl_patpat` and
/// `wdog_kern_pat` do.
///
/// # Errors
///
/// [`LegacyPatError`] for what FreeBSD refuses.
pub fn decode_legacy_pat(u: u32) -> Result<LegacyPat, LegacyPatError> {
    let mode = u & (WD_ACTIVE | WD_PASSIVE);
    if u & !(WD_ACTIVE | WD_PASSIVE | WD_LASTVAL | WD_INTERVAL) != 0
        || mode == WD_ACTIVE | WD_PASSIVE
        || (mode == 0 && (u & WD_INTERVAL != 0 || u & WD_LASTVAL != 0))
    {
        return Err(LegacyPatError::Invalid);
    }
    if mode == WD_PASSIVE {
        return Err(LegacyPatError::Passive);
    }
    decode_kern_pat(u)
}

/// `wdog_kern_pat`'s reading of a `u_int` timeout (no mode bits).
///
/// # Errors
///
/// [`LegacyPatError::Invalid`] for `WD_LASTVAL` with an interval.
pub fn decode_kern_pat(u: u32) -> Result<LegacyPat, LegacyPatError> {
    if u & WD_LASTVAL != 0 {
        if u & WD_INTERVAL != 0 {
            return Err(LegacyPatError::Invalid);
        }
        return Ok(LegacyPat::LastValue);
    }
    Ok(LegacyPat::Interval(u & WD_INTERVAL))
}

/// Whether `act` is a set of `WD_SOFT_*` (`wd_valid_act`).
#[must_use]
pub fn valid_act(act: i32) -> bool {
    act & !WD_SOFT_MASK == 0
}

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/watchdog.h",
        value,
        arches: Arches::All,
    }
}

const fn r(c_name: &'static str, value: u32) -> ConstValue {
    c(c_name, value as i64)
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    r("WDIOC_PATPAT", WDIOC_PATPAT),
    r("WDIOC_SETTIMEOUT", WDIOC_SETTIMEOUT),
    r("WDIOC_GETTIMEOUT", WDIOC_GETTIMEOUT),
    r("WDIOC_GETTIMELEFT", WDIOC_GETTIMELEFT),
    r("WDIOC_GETPRETIMEOUT", WDIOC_GETPRETIMEOUT),
    r("WDIOC_SETPRETIMEOUT", WDIOC_SETPRETIMEOUT),
    r("WDIOC_SETPRETIMEOUTACT", WDIOC_SETPRETIMEOUTACT),
    r("WDIOC_SETSOFT", WDIOC_SETSOFT),
    r("WDIOC_SETSOFTTIMEOUTACT", WDIOC_SETSOFTTIMEOUTACT),
    r("WDIOC_CONTROL", WDIOC_CONTROL),
    r("WD_ACTIVE", WD_ACTIVE),
    r("WD_PASSIVE", WD_PASSIVE),
    r("WD_LASTVAL", WD_LASTVAL),
    r("WD_INTERVAL", WD_INTERVAL),
    r("WD_TO_NEVER", WD_TO_NEVER),
    r("WD_TO_1MS", WD_TO_1MS),
    r("WD_TO_125MS", WD_TO_125MS),
    r("WD_TO_250MS", WD_TO_250MS),
    r("WD_TO_500MS", WD_TO_500MS),
    r("WD_TO_1SEC", WD_TO_1SEC),
    r("WD_TO_2SEC", WD_TO_2SEC),
    r("WD_TO_4SEC", WD_TO_4SEC),
    r("WD_TO_8SEC", WD_TO_8SEC),
    r("WD_TO_16SEC", WD_TO_16SEC),
    r("WD_TO_32SEC", WD_TO_32SEC),
    r("WD_TO_64SEC", WD_TO_64SEC),
    r("WD_TO_128SEC", WD_TO_128SEC),
    c("WD_CTRL_DISABLE", WD_CTRL_DISABLE as i64),
    c("WD_CTRL_ENABLE", WD_CTRL_ENABLE as i64),
    c("WD_CTRL_RESET", WD_CTRL_RESET as i64),
    c("WD_SOFT_PANIC", WD_SOFT_PANIC as i64),
    c("WD_SOFT_DDB", WD_SOFT_DDB as i64),
    c("WD_SOFT_LOG", WD_SOFT_LOG as i64),
    c("WD_SOFT_PRINTF", WD_SOFT_PRINTF as i64),
    c("WD_SOFT_MASK", WD_SOFT_MASK as i64),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_numbers() {
        // `_IOW('W', 53, sbintime_t)` and `_IOW('W', 51, int)`.
        assert_eq!(WDIOC_SETTIMEOUT, 0x8008_5735);
        assert_eq!(WDIOC_GETTIMEOUT, 0x4008_5736);
        assert_eq!(WDIOC_CONTROL, 0x8004_5733);
        assert_eq!(WDIOCPATPAT_14, 0x8004_572a);
    }

    #[test]
    fn sbt_conversions_round_as_freebsd() {
        assert_eq!(sbt_to_ns(SBT_1S), 1_000_000_000);
        assert_eq!(sbt_to_ns(128 * SBT_1S), 128_000_000_000);
        assert_eq!(ns_to_sbt(1_000_000_000), SBT_1S);
        // One nanosecond is 4.29 units: up to 5, which reads back as 1.
        assert_eq!(ns_to_sbt(1), 5);
        assert_eq!(sbt_to_ns(5), 1);
        assert_eq!(sbt_to_ns(4), 0);
        assert_eq!(sbt_to_ns(-1), -1, "floor, not toward zero");
        for n in [1, 999, 1_000_000, 123_456_789_012] {
            assert_eq!(sbt_to_ns(ns_to_sbt(n)), n);
        }
        assert_eq!(ms_to_sbt(1000), SBT_1S);
        assert_eq!(ms_to_sbt(1), 4_294_968);
    }

    #[test]
    fn pow2ns_encoding() {
        assert_eq!(sbt_to_pow2ns(0), 0);
        // 2^30 ns <= 1 s < 2^31: fls is 30.
        assert_eq!(sbt_to_pow2ns(SBT_1S), WD_TO_1SEC);
        // watchdogd's default 128 s: between 2^36 and 2^37 ns.
        assert_eq!(sbt_to_pow2ns(128 * SBT_1S), WD_TO_128SEC);
        assert_eq!(sbt_to_ns(pow2ns_to_sbt(WD_TO_1SEC)), 1 << 30);
        assert_eq!(pow2ns_to_sbt(WD_TO_NEVER), 0);
        assert_eq!(pow2ns_to_sbt(WD_INTERVAL), Sbintime::MAX);
        // A power of two of nanoseconds encodes back to itself plus one
        // (fls of 2^n is n + 1), as FreeBSD's wd_last_u reads.
        assert_eq!(sbt_to_pow2ns(pow2ns_to_sbt(WD_TO_4SEC)), WD_TO_4SEC + 1);
    }

    #[test]
    fn legacy_patpat_decoding() {
        use LegacyPat::*;
        use LegacyPatError::*;
        assert_eq!(
            decode_legacy_pat(WD_ACTIVE | WD_TO_8SEC),
            Ok(Interval(WD_TO_8SEC))
        );
        assert_eq!(decode_legacy_pat(WD_ACTIVE | WD_TO_NEVER), Ok(Interval(0)));
        assert_eq!(
            decode_legacy_pat(0),
            Ok(Interval(0)),
            "no mode, no interval"
        );
        assert_eq!(decode_legacy_pat(WD_ACTIVE | WD_LASTVAL), Ok(LastValue));
        assert_eq!(
            decode_legacy_pat(WD_TO_8SEC),
            Err(Invalid),
            "an interval needs a mode"
        );
        assert_eq!(decode_legacy_pat(WD_LASTVAL), Err(Invalid));
        assert_eq!(decode_legacy_pat(WD_ACTIVE | WD_PASSIVE), Err(Invalid));
        assert_eq!(
            decode_legacy_pat(WD_ACTIVE | 0x100),
            Err(Invalid),
            "unknown bit"
        );
        assert_eq!(decode_legacy_pat(WD_PASSIVE | WD_TO_1SEC), Err(Passive));
        assert_eq!(
            decode_legacy_pat(WD_ACTIVE | WD_LASTVAL | WD_TO_1SEC),
            Err(Invalid),
            "the last value and an interval"
        );
    }

    #[test]
    fn actions() {
        assert!(valid_act(0));
        assert!(valid_act(WD_SOFT_MASK));
        assert!(!valid_act(0x10));
        assert!(!valid_act(-1));
    }
}
