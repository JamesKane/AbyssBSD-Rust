// SPDX-License-Identifier: BSD-2-Clause
//! The SCMI clock protocol's replies, as the Sky1's AML hands them back
//! (Arm System Control and Management Interface, DEN0056, §4.6): the
//! payload of the shared-memory channel, little-endian 32-bit words, the
//! first a status.
//!
//! The AML methods return a lone word of their own when the channel never
//! comes free (6) or firmware does not answer (11); a real reply is the
//! whole payload buffer.

use alloc::vec::Vec;

use bus::provider::{Hz, ProviderError};

/// `CLOCK_DESCRIBE_RATES` (message 0x4).
pub const DESCRIBE_RATES: u8 = 0x4;
/// `CLOCK_RATE_SET` (message 0x5).
pub const RATE_SET: u8 = 0x5;
/// `CLOCK_RATE_GET` (message 0x6).
pub const RATE_GET: u8 = 0x6;
/// `CLOCK_CONFIG_SET` (message 0x7).
pub const CONFIG_SET: u8 = 0x7;

/// The AML's own failures, distinct from any SCMI status (which are 0 or
/// negative): the channel stayed busy, or firmware never answered.
const AML_BUSY: i32 = 6;
const AML_TIMEOUT: i32 = 11;

/// The reply's words.
fn words(reply: &[u8]) -> Vec<u32> {
    reply
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| u32::from_le_bytes(*w))
        .collect()
}

/// The words after the status, if the status says success.
///
/// # Errors
///
/// The status as a [`ProviderError`]: `NOT_SUPPORTED` (-1) is
/// [`ProviderError::Unsupported`], `INVALID_PARAMETERS` (-2) and
/// `NOT_FOUND` (-4) are [`ProviderError::NoSuchLine`], `OUT_OF_RANGE` (-5)
/// is [`ProviderError::OutOfRange`], the AML's own failures are
/// [`ProviderError::Timeout`], and anything else is
/// [`ProviderError::Firmware`].
pub fn payload(reply: &[u8]) -> Result<Vec<u32>, ProviderError> {
    let w = words(reply);
    #[allow(clippy::cast_possible_wrap, reason = "SCMI statuses are signed")]
    let status = *w.first().ok_or(ProviderError::Firmware(0))? as i32;
    match status {
        0 => Ok(w[1..].to_vec()),
        -1 => Err(ProviderError::Unsupported),
        -2 | -4 => Err(ProviderError::NoSuchLine),
        -5 => Err(ProviderError::OutOfRange),
        AML_BUSY | AML_TIMEOUT => Err(ProviderError::Timeout),
        s => Err(ProviderError::Firmware(s)),
    }
}

/// A `CLOCK_RATE_GET` reply's rate.
///
/// # Errors
///
/// As [`payload`], or [`ProviderError::Firmware`] for a short reply.
pub fn rate(reply: &[u8]) -> Result<Hz, ProviderError> {
    match payload(reply)?[..] {
        [lo, hi, ..] => Ok(Hz(u64::from(hi) << 32 | u64::from(lo))),
        _ => Err(ProviderError::Firmware(0)),
    }
}

/// What a clock can run at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rates {
    /// These rates, ascending.
    Discrete(Vec<u64>),
    /// Every `step` from `min` to `max`.
    Range {
        /// The lowest.
        min: u64,
        /// The highest.
        max: u64,
        /// The spacing (0 for a single rate).
        step: u64,
    },
}

/// One `CLOCK_DESCRIBE_RATES` reply: its rates, and how many follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Described {
    /// The rates in this reply.
    pub rates: Rates,
    /// How many more rates later requests return.
    pub remaining: u32,
}

/// A `CLOCK_DESCRIBE_RATES` reply.
///
/// # Errors
///
/// As [`payload`], or [`ProviderError::Firmware`] for a reply shorter than
/// it says.
pub fn described(reply: &[u8]) -> Result<Described, ProviderError> {
    let w = payload(reply)?;
    let short = ProviderError::Firmware(0);
    let flags = *w.first().ok_or(short)?;
    let returned = (flags & 0xfff) as usize;
    let triplet = flags & 1 << 12 != 0;
    let remaining = flags >> 16;
    let mut rates = Vec::with_capacity(returned);
    for k in 0..returned {
        let (lo, hi) = (
            w.get(1 + 2 * k).ok_or(short)?,
            w.get(2 + 2 * k).ok_or(short)?,
        );
        rates.push(u64::from(*hi) << 32 | u64::from(*lo));
    }
    let rates = if triplet {
        let [min, max, step] = rates[..] else {
            return Err(short);
        };
        Rates::Range { min, max, step }
    } else {
        Rates::Discrete(rates)
    };
    Ok(Described { rates, remaining })
}

/// The rate `CLOCK_RATE_SET` gives for `want` with its rounding flag
/// clear (round down): the highest rate not above it.
///
/// # Errors
///
/// [`ProviderError::OutOfRange`] if every rate is above `want`.
pub fn round_down(rates: &Rates, want: u64) -> Result<u64, ProviderError> {
    match rates {
        Rates::Discrete(list) => list
            .iter()
            .copied()
            .filter(|&r| r <= want)
            .max()
            .ok_or(ProviderError::OutOfRange),
        &Rates::Range { min, max, step } => {
            if want < min {
                return Err(ProviderError::OutOfRange);
            }
            let top = want.min(max);
            Ok(match step {
                0 => min,
                s => min + (top - min) / s * s,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(w: &[u32]) -> Vec<u8> {
        let mut r: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        // The AML hands back the whole 96-byte payload.
        r.resize(0x60, 0);
        r
    }

    #[test]
    fn statuses() {
        assert_eq!(payload(&reply(&[0, 7])).unwrap()[0], 7);
        let e = |s: i32| payload(&reply(&[s.cast_unsigned()])).unwrap_err();
        assert_eq!(e(-1), ProviderError::Unsupported);
        assert_eq!(e(-4), ProviderError::NoSuchLine);
        assert_eq!(e(-5), ProviderError::OutOfRange);
        assert_eq!(e(-8), ProviderError::Firmware(-8));
        // The AML's own four-byte failures.
        assert_eq!(payload(&[6, 0, 0, 0]), Err(ProviderError::Timeout));
        assert_eq!(payload(&[0x0b, 0, 0, 0]), Err(ProviderError::Timeout));
        assert_eq!(payload(&[]), Err(ProviderError::Firmware(0)));
    }

    #[test]
    fn rates() {
        assert_eq!(rate(&reply(&[0, 100_000_000, 0])), Ok(Hz(100_000_000)));
        assert_eq!(rate(&reply(&[0, 0, 1])), Ok(Hz(1 << 32)));
        let d = described(&reply(&[0, 3 | 2 << 16, 10, 0, 20, 0, 40, 0])).unwrap();
        assert_eq!(d.rates, Rates::Discrete(alloc::vec![10, 20, 40]));
        assert_eq!(d.remaining, 2);
        let t = described(&reply(&[0, 3 | 1 << 12, 100, 0, 1000, 0, 100, 0])).unwrap();
        assert_eq!(
            t.rates,
            Rates::Range {
                min: 100,
                max: 1000,
                step: 100
            }
        );
        // Says more than it holds.
        assert!(described(&[0, 0, 0, 0, 40, 0, 0, 0]).is_err());
    }

    #[test]
    fn rounding_is_down() {
        let d = Rates::Discrete(alloc::vec![10, 20, 40]);
        assert_eq!(round_down(&d, 39), Ok(20));
        assert_eq!(round_down(&d, 40), Ok(40));
        assert_eq!(round_down(&d, 9), Err(ProviderError::OutOfRange));
        let r = Rates::Range {
            min: 100,
            max: 1000,
            step: 100,
        };
        assert_eq!(round_down(&r, 950), Ok(900));
        assert_eq!(round_down(&r, 5000), Ok(1000));
        assert_eq!(round_down(&r, 99), Err(ProviderError::OutOfRange));
    }
}
