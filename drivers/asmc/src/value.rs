// SPDX-License-Identifier: BSD-2-Clause
//! SMC value encodings, as FreeBSD's `asmc` decodes them.
//!
//! Every SMC value is big-endian except `flt `, which the memory window
//! gives in the host's (little-endian) order: fan speeds on T2 Macs, and
//! the Mac Pro (Late 2013)'s platform limit `MSLT` (the fork's
//! `asmc_plimit_flt`).

/// The signed fixed-point sensor types and their divisors
/// (`asmc_sensor_types`), and `ui16` read as is.
const SENSOR_TYPES: &[(&[u8; 4], i32)] = &[
    (b"sp78", 256),
    (b"sp87", 128),
    (b"sp4b", 2048),
    (b"sp5a", 1024),
    (b"sp69", 512),
    (b"sp96", 64),
    (b"sp2d", 8192),
    (b"ui16", 1),
];

/// Whether a two-byte key of type `ty` is a sensor FreeBSD reads.
#[must_use]
pub fn is_sensor(ty: &[u8; 4]) -> bool {
    SENSOR_TYPES.iter().any(|(t, _)| *t == ty)
}

/// A two-byte sensor value in thousandths of its unit
/// (`asmc_sensor_convert`): millidegrees for `sp78`.
#[must_use]
pub fn sensor_milli(ty: &[u8; 4], raw: [u8; 2]) -> Option<i32> {
    let (_, div) = SENSOR_TYPES.iter().find(|(t, _)| *t == ty)?;
    Some(if *div == 1 {
        i32::from(u16::from_be_bytes(raw))
    } else {
        i32::from(i16::from_be_bytes(raw)) * 1000 / div
    })
}

/// A fan speed from an `fpe2` value: unsigned 14.2 fixed point.
#[must_use]
pub fn fpe2(raw: [u8; 2]) -> u32 {
    u32::from(u16::from_be_bytes(raw) >> 2)
}

/// An `fpe2` value for `rpm`, or `None` if it does not fit (FreeBSD wraps
/// modulo 16384 rpm; spec §04 item 1 asks for a range check).
#[must_use]
pub fn to_fpe2(rpm: u32) -> Option<[u8; 2]> {
    let v = u16::try_from(rpm.checked_mul(4)?).ok()?;
    Some(v.to_be_bytes())
}

/// The integer part of a non-negative IEEE 754 single
/// (`asmc_float_to_u32`): 0 for a negative value or below 1.
#[must_use]
pub fn float_to_u32(bits: u32) -> u32 {
    if bits == 0 || bits >> 31 != 0 {
        return 0;
    }
    let exp = i32::try_from((bits >> 23) & 0xff).unwrap_or(0) - 0x7f;
    let frac = bits & 0x7f_ffff;
    match exp {
        e if e < 0 => 0,
        e if e > 30 => u32::MAX,
        e if e > 23 => (1 << e) | (frac << (e - 23)),
        e => (1 << e) + (frac >> (23 - e)),
    }
}

/// An IEEE 754 single for `v` (`asmc_u32_to_float`), exact up to 2^24.
#[must_use]
pub fn u32_to_float(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let top = (31 - v.leading_zeros()).min(30);
    let exp = 0x7f + top;
    let frac = if top >= 23 {
        (v >> (top - 23)) & 0x7f_ffff
    } else {
        (v << (23 - top)) & 0x7f_ffff
    };
    (exp << 23) | frac
}

/// The platform limit from `MSLT`'s four bytes (little-endian `flt`): the
/// integer level, 0 to 100, clamped to 1000, as the fork's
/// `asmc_plimit_flt`; and whether it is on (any positive value, even
/// below 1).
#[must_use]
pub fn limit(raw: [u8; 4]) -> (u32, bool) {
    let bits = u32::from_le_bytes(raw);
    let on = bits != 0 && bits >> 31 == 0;
    (float_to_u32(bits).min(1000), on)
}

/// A temperature in deciKelvin (`IK`) from millidegrees Celsius.
#[must_use]
pub fn milli_c_to_dk(milli: i32) -> i32 {
    milli / 100 + 2731
}
