// SPDX-License-Identifier: BSD-2-Clause
//! 8.3 short names and volume labels.

use core::fmt;

/// Why a name was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    /// The name is empty, or the part before the dot is.
    Empty,
    /// More than eight characters before the dot.
    BaseTooLong,
    /// More than three characters after the dot.
    ExtensionTooLong,
    /// More than eleven characters in a volume label.
    LabelTooLong,
    /// A character not allowed in short names.
    InvalidChar(char),
}

/// A validated 8.3 name in its on-disk form: eleven bytes, upper case, space
/// padded, without the dot.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShortName([u8; 11]);

impl ShortName {
    /// Validates and converts a name such as `bootx64.efi`. Lower case is
    /// folded to upper case, as DOS does.
    pub fn new(name: &str) -> Result<Self, NameError> {
        if name == "." || name == ".." {
            return Err(NameError::InvalidChar('.'));
        }
        let (base, ext) = match name.rsplit_once('.') {
            Some((b, e)) => (b, e),
            None => (name, ""),
        };
        if base.is_empty() {
            return Err(NameError::Empty);
        }
        if base.len() > 8 {
            return Err(NameError::BaseTooLong);
        }
        if ext.len() > 3 {
            return Err(NameError::ExtensionTooLong);
        }
        let mut out = [b' '; 11];
        for (slot, c) in out[..8].iter_mut().zip(base.chars()) {
            *slot = short_char(c)?;
        }
        for (slot, c) in out[8..].iter_mut().zip(ext.chars()) {
            *slot = short_char(c)?;
        }
        Ok(Self(out))
    }

    /// Builds a name from its on-disk form without validation, for reading.
    pub(crate) const fn from_raw(raw: [u8; 11]) -> Self {
        Self(raw)
    }

    /// The on-disk form.
    pub const fn as_bytes(&self) -> &[u8; 11] {
        &self.0
    }
}

fn short_char(c: char) -> Result<u8, NameError> {
    let c = c.to_ascii_uppercase();
    let ok = c.is_ascii_uppercase() || c.is_ascii_digit() || "!#$%&'()-@^_`{}~".contains(c);
    if ok {
        // Only ASCII passes the check above.
        Ok(c as u8)
    } else {
        Err(NameError::InvalidChar(c))
    }
}

impl fmt::Display for ShortName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let base = self.0[..8].trim_ascii_end();
        let ext = self.0[8..].trim_ascii_end();
        for &b in base {
            fmt::Write::write_char(f, char::from(b))?;
        }
        if !ext.is_empty() {
            fmt::Write::write_char(f, '.')?;
            for &b in ext {
                fmt::Write::write_char(f, char::from(b))?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for ShortName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShortName({self})")
    }
}

/// A volume label: up to eleven short-name characters, space padded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VolumeLabel([u8; 11]);

impl VolumeLabel {
    /// Validates a label such as `ABYSS_ESP`.
    pub fn new(label: &str) -> Result<Self, NameError> {
        if label.is_empty() {
            return Err(NameError::Empty);
        }
        if label.len() > 11 {
            return Err(NameError::LabelTooLong);
        }
        let mut out = [b' '; 11];
        for (slot, c) in out.iter_mut().zip(label.chars()) {
            *slot = short_char(c)?;
        }
        Ok(Self(out))
    }

    /// The on-disk form.
    pub const fn as_bytes(&self) -> &[u8; 11] {
        &self.0
    }
}

impl fmt::Debug for VolumeLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = core::str::from_utf8(self.0.trim_ascii_end()).unwrap_or("?");
        write!(f, "VolumeLabel({text})")
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::ToString;

    #[test]
    fn folds_and_pads() {
        let n = ShortName::new("bootx64.efi").unwrap();
        assert_eq!(n.as_bytes(), b"BOOTX64 EFI");
        assert_eq!(n.to_string(), "BOOTX64.EFI");
        assert_eq!(ShortName::new("EFI").unwrap().as_bytes(), b"EFI        ");
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(ShortName::new(""), Err(NameError::Empty));
        assert_eq!(ShortName::new(".efi"), Err(NameError::Empty));
        assert_eq!(ShortName::new("a.long"), Err(NameError::ExtensionTooLong));
        assert_eq!(ShortName::new("a b"), Err(NameError::InvalidChar(' ')));
        assert_eq!(ShortName::new(".."), Err(NameError::InvalidChar('.')));
        assert_eq!(
            VolumeLabel::new("TWELVE_CHARS"),
            Err(NameError::LabelTooLong)
        );
    }
}
