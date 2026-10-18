// SPDX-License-Identifier: BSD-2-Clause
//! A bounds-checked cursor over AML bytes (ACPI 6.5 §20.2): fixed-width
//! data, package lengths, and name strings.
//!
//! Every read is checked against the cursor's end, which is the end of the
//! innermost enclosing package, so a corrupt length can never reach past
//! the scope it belongs to.
//!
//! Behavior reference: ACPI 6.5 §20.2.4 (`PkgLength`), §20.2.2 (names).

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::Error;
use crate::name::{Anchor, NameSeg, NameString};

const DUAL_NAME_PREFIX: u8 = 0x2e;
const MULTI_NAME_PREFIX: u8 = 0x2f;

/// A position in a table's AML, limited to `pos..end`.
#[derive(Clone)]
pub(crate) struct Cursor {
    data: Arc<[u8]>,
    pub(crate) pos: usize,
    pub(crate) end: usize,
}

impl core::fmt::Debug for Cursor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Cursor({:#x}..{:#x})", self.pos, self.end)
    }
}

impl Cursor {
    /// A cursor over `data[pos..end]`; `end` is clamped to the data.
    pub(crate) fn new(data: Arc<[u8]>, pos: usize, end: usize) -> Self {
        let end = end.min(data.len());
        Self { data, pos, end }
    }

    /// A cursor over `pos..end` of the same data.
    pub(crate) fn sub(&self, pos: usize, end: usize) -> Self {
        Self::new(self.data.clone(), pos, end)
    }

    /// The underlying bytes.
    pub(crate) fn data(&self) -> &Arc<[u8]> {
        &self.data
    }

    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.end
    }

    pub(crate) fn remaining(&self) -> usize {
        self.end.saturating_sub(self.pos)
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        if self.pos < self.end {
            self.data.get(self.pos).copied()
        } else {
            None
        }
    }

    pub(crate) fn peek_at(&self, ahead: usize) -> Option<u8> {
        let p = self.pos.checked_add(ahead)?;
        if p < self.end {
            self.data.get(p).copied()
        } else {
            None
        }
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Error> {
        let b = self.peek().ok_or(Error::UnexpectedEnd)?;
        self.pos += 1;
        Ok(b)
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&[u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::UnexpectedEnd)?;
        if end > self.end {
            return Err(Error::UnexpectedEnd);
        }
        let s = self.data.get(self.pos..end).ok_or(Error::UnexpectedEnd)?;
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut a = [0; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Reads a `PkgLength` and returns the absolute end offset of the
    /// package it measures. The length counts its own encoding, so the
    /// package starts where the length starts. The end must lie inside the
    /// cursor's bounds.
    pub(crate) fn pkg_end(&mut self) -> Result<usize, Error> {
        let start = self.pos;
        let len = self.pkg_length_value()?;
        let end = start.checked_add(len).ok_or(Error::BadPkgLength)?;
        if end < self.pos || end > self.end {
            return Err(Error::BadPkgLength);
        }
        Ok(end)
    }

    /// Reads a `PkgLength` encoding and returns its raw value (used directly
    /// for field bit widths, which are not self-inclusive).
    pub(crate) fn pkg_length_value(&mut self) -> Result<usize, Error> {
        let lead = self.u8()?;
        let follow = usize::from(lead >> 6);
        if follow == 0 {
            return Ok(usize::from(lead & 0x3f));
        }
        if lead & 0x30 != 0 {
            return Err(Error::BadPkgLength);
        }
        let mut len = usize::from(lead & 0x0f);
        for i in 0..follow {
            len |= usize::from(self.u8()?) << (4 + 8 * i);
        }
        Ok(len)
    }

    /// Reads a `NameString`.
    pub(crate) fn name_string(&mut self) -> Result<NameString, Error> {
        let anchor = match self.peek() {
            Some(b'\\') => {
                self.pos += 1;
                Anchor::Root
            }
            _ => {
                let mut up: u16 = 0;
                while self.peek() == Some(b'^') {
                    self.pos += 1;
                    up = up.checked_add(1).ok_or(Error::InvalidName)?;
                }
                Anchor::Relative(up)
            }
        };
        let count = match self.peek().ok_or(Error::UnexpectedEnd)? {
            0 => {
                self.pos += 1;
                0
            }
            DUAL_NAME_PREFIX => {
                self.pos += 1;
                2
            }
            MULTI_NAME_PREFIX => {
                self.pos += 1;
                usize::from(self.u8()?)
            }
            _ => 1,
        };
        if count.saturating_mul(4) > self.remaining() {
            return Err(Error::UnexpectedEnd);
        }
        let mut segs = Vec::new();
        segs.try_reserve_exact(count).map_err(|_| Error::NoMemory)?;
        for _ in 0..count {
            let seg = NameSeg::from_aml(self.array()?).ok_or(Error::InvalidName)?;
            segs.push(seg);
        }
        Ok(NameString { anchor, segs })
    }

    /// Reads a NUL-terminated ASCII string (the `StringPrefix` already
    /// consumed). The NUL must lie inside the cursor's bounds.
    pub(crate) fn c_string(&mut self) -> Result<Vec<u8>, Error> {
        let rest = self
            .data
            .get(self.pos..self.end)
            .ok_or(Error::UnexpectedEnd)?;
        let n = rest
            .iter()
            .position(|&c| c == 0)
            .ok_or(Error::UnexpectedEnd)?;
        let mut v = Vec::new();
        v.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        v.extend_from_slice(&rest[..n]);
        self.pos += n + 1;
        Ok(v)
    }
}
