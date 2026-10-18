// SPDX-License-Identifier: BSD-2-Clause
//! Names (ACPI 6.5 §5.3, §20.2.2): four-character name segments, the
//! relative or absolute name strings AML encodes, and absolute paths.
//!
//! Behavior reference: `sys/contrib/dev/acpica/components/namespace/nsaccess.c`
//! (observable search rules only).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::Error;

/// One four-character name segment, padded with `_` (`_SB_`, `PCI0`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameSeg(pub [u8; 4]);

impl NameSeg {
    /// The root's segment, which has no name of its own.
    pub const ROOT: Self = Self(*b"\\___");

    /// Validates a segment read from AML: a lead character `A`–`Z` or `_`,
    /// then three of `A`–`Z`, `0`–`9`, `_`.
    #[must_use]
    pub fn from_aml(b: [u8; 4]) -> Option<Self> {
        if !is_lead_char(b[0]) || !b[1..].iter().all(|&c| is_name_char(c)) {
            return None;
        }
        Some(Self(b))
    }

    /// Parses an ASL-style segment of one to four characters, padding with
    /// `_` (`"_SB"` → `_SB_`).
    #[must_use]
    pub fn from_str_padded(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.is_empty() || b.len() > 4 {
            return None;
        }
        let mut seg = [b'_'; 4];
        seg[..b.len()].copy_from_slice(b);
        Self::from_aml(seg)
    }

    /// The segment as text, with its padding.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Segments are validated ASCII except ROOT, which is ASCII too.
        core::str::from_utf8(&self.0).unwrap_or("????")
    }
}

impl fmt::Debug for NameSeg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for NameSeg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// True for a character that may start a name segment.
#[must_use]
pub const fn is_lead_char(c: u8) -> bool {
    c.is_ascii_uppercase() || c == b'_'
}

const fn is_name_char(c: u8) -> bool {
    is_lead_char(c) || c.is_ascii_digit()
}

/// True for a byte that begins a `NameString` in AML: a lead character,
/// the root or parent prefix, or a dual or multi name prefix.
#[must_use]
pub const fn starts_name_string(c: u8) -> bool {
    is_lead_char(c) || matches!(c, b'\\' | b'^' | 0x2e | 0x2f)
}

/// Where a name string starts its resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// From the root (`\`).
    Root,
    /// From the current scope after going up this many levels (`^^`).
    /// Zero levels with one segment uses the upward search rule.
    Relative(u16),
}

/// A name as AML encodes it: an anchor and zero or more segments. Zero
/// segments is the null name (or a bare prefix such as `\`).
#[derive(Clone, PartialEq, Eq)]
pub struct NameString {
    /// Where resolution starts.
    pub anchor: Anchor,
    /// The segments, outermost first.
    pub segs: Vec<NameSeg>,
}

impl NameString {
    /// True for a single segment without a prefix: the only form that
    /// searches parent scopes (ACPI 6.5 §5.3).
    #[must_use]
    pub fn uses_search_rules(&self) -> bool {
        self.anchor == Anchor::Relative(0) && self.segs.len() == 1
    }

    /// Parses ASL text such as `\_SB.PCI0`, `^^FOO`, or `_STA`. Segments
    /// shorter than four characters are padded with `_`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidName`] for text that is not a valid name.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let (anchor, rest) = if let Some(r) = s.strip_prefix('\\') {
            (Anchor::Root, r)
        } else {
            let up = s.bytes().take_while(|&c| c == b'^').count();
            let up16 = u16::try_from(up).map_err(|_| Error::InvalidName)?;
            (Anchor::Relative(up16), &s[up..])
        };
        let mut segs = Vec::new();
        if !rest.is_empty() {
            for part in rest.split('.') {
                segs.push(NameSeg::from_str_padded(part).ok_or(Error::InvalidName)?);
            }
        }
        Ok(Self { anchor, segs })
    }
}

impl fmt::Debug for NameString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for NameString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.anchor {
            Anchor::Root => f.write_str("\\")?,
            Anchor::Relative(n) => {
                for _ in 0..n {
                    f.write_str("^")?;
                }
            }
        }
        for (i, s) in self.segs.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(s.as_str())?;
        }
        Ok(())
    }
}

/// An absolute path in the namespace: the segments from the root down.
/// The root itself is the empty path, displayed as `\`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct AmlPath(pub Vec<NameSeg>);

impl AmlPath {
    /// Parses an absolute ASL path such as `\_SB.PCI0` (the leading `\` is
    /// optional).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidName`] for text that is not an absolute name.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let s = s.strip_prefix('\\').unwrap_or(s);
        if s.starts_with('^') {
            return Err(Error::InvalidName);
        }
        let n = NameString::parse(s)?;
        Ok(Self(n.segs))
    }

    /// The last segment, or `None` for the root.
    #[must_use]
    pub fn last(&self) -> Option<NameSeg> {
        self.0.last().copied()
    }

    /// The path as text (`\_SB_.PCI0`).
    #[must_use]
    pub fn to_text(&self) -> String {
        alloc::format!("{self}")
    }
}

impl fmt::Debug for AmlPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for AmlPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\\")?;
        for (i, s) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(s.as_str())?;
        }
        Ok(())
    }
}
