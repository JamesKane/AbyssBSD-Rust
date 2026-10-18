// SPDX-License-Identifier: BSD-2-Clause
//! GUIDs in the UEFI / GPT on-disk encoding.
//!
//! UEFI stores the first three fields little-endian and the last two as raw
//! bytes, which differs from the big-endian order of the textual form. This
//! type keeps the fields separate so the encoding is explicit.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// A 128-bit globally unique identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// Why a textual GUID was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The string is not 36 characters long.
    Length,
    /// A hyphen is missing from one of the four group separators.
    Separator,
    /// A character other than a hex digit appears in a group.
    Digit,
}

impl Guid {
    /// The all-zero GUID, which GPT uses to mark an unused partition entry.
    pub const NIL: Self = Self::from_fields(0, 0, 0, [0; 8]);

    /// Builds a GUID from its four fields.
    pub const fn from_fields(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }

    /// Parses the canonical `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` form.
    ///
    /// Usable in constant context, so well-known GUIDs can be written as text
    /// and still checked at compile time.
    pub const fn parse(s: &str) -> Result<Self, ParseError> {
        let b = s.as_bytes();
        if b.len() != 36 {
            return Err(ParseError::Length);
        }
        if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
            return Err(ParseError::Separator);
        }
        let mut raw = [0u8; 16];
        let mut out = 0;
        let mut i = 0;
        while i < 36 {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                i += 1;
                continue;
            }
            let hi = match hex(b[i]) {
                Some(v) => v,
                None => return Err(ParseError::Digit),
            };
            let lo = match hex(b[i + 1]) {
                Some(v) => v,
                None => return Err(ParseError::Digit),
            };
            raw[out] = (hi << 4) | lo;
            out += 1;
            i += 2;
        }
        Ok(Self::from_be_bytes(raw))
    }

    /// Like [`Guid::parse`], but fails compilation when used in a `const`
    /// with a malformed literal.
    pub const fn must_parse(s: &str) -> Self {
        match Self::parse(s) {
            Ok(g) => g,
            Err(_) => panic!("malformed GUID literal"),
        }
    }

    /// Interprets 16 bytes in textual (big-endian, RFC 9562) order.
    pub const fn from_be_bytes(b: [u8; 16]) -> Self {
        Self {
            data1: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            data2: u16::from_be_bytes([b[4], b[5]]),
            data3: u16::from_be_bytes([b[6], b[7]]),
            data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        }
    }

    /// Decodes the UEFI / GPT on-disk (mixed-endian) encoding.
    pub const fn from_uefi_bytes(b: [u8; 16]) -> Self {
        Self {
            data1: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            data2: u16::from_le_bytes([b[4], b[5]]),
            data3: u16::from_le_bytes([b[6], b[7]]),
            data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        }
    }

    /// Encodes in the UEFI / GPT on-disk (mixed-endian) encoding.
    pub const fn to_uefi_bytes(self) -> [u8; 16] {
        let a = self.data1.to_le_bytes();
        let b = self.data2.to_le_bytes();
        let c = self.data3.to_le_bytes();
        let d = self.data4;
        [
            a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5],
            d[6], d[7],
        ]
    }

    /// Makes an RFC 9562 version 8 (vendor-defined) GUID from 16 bytes of
    /// caller-chosen data, such as a hash of a stable name. Used for
    /// reproducible disk images.
    pub const fn v8_from_bytes(mut b: [u8; 16]) -> Self {
        b[6] = (b[6] & 0x0f) | 0x80;
        b[8] = (b[8] & 0x3f) | 0x80;
        Self::from_be_bytes(b)
    }
}

const fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = &self.data4;
        write!(
            f,
            "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Guid({self})")
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::ToString;

    const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

    #[test]
    fn parse_and_display_round_trip() {
        let g = Guid::must_parse(ESP);
        assert_eq!(g.to_string(), ESP);
        assert_eq!(Guid::parse(&ESP.to_lowercase()), Ok(g));
    }

    #[test]
    fn uefi_encoding_is_mixed_endian() {
        let g = Guid::must_parse(ESP);
        let bytes = g.to_uefi_bytes();
        assert_eq!(
            bytes,
            [
                0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
                0xC9, 0x3B
            ]
        );
        assert_eq!(Guid::from_uefi_bytes(bytes), g);
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(Guid::parse("short"), Err(ParseError::Length));
        assert_eq!(
            Guid::parse("C12A7328_F81F-11D2-BA4B-00A0C93EC93B"),
            Err(ParseError::Separator)
        );
        assert_eq!(
            Guid::parse("G12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
            Err(ParseError::Digit)
        );
    }

    #[test]
    fn v8_sets_version_and_variant() {
        let g = Guid::v8_from_bytes([0xff; 16]);
        let s = g.to_string();
        assert_eq!(&s[14..15], "8");
        assert_eq!(s.as_bytes()[19], b'B');
    }
}
