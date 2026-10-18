// SPDX-License-Identifier: BSD-2-Clause
//! Shared plumbing: bounds-checked field reads and the walk over the
//! type/length-prefixed structure lists that MADT, SRAT, PPTT, and GTDT use.

use crate::{Error, Gas, u16_at, u32_at, u64_at};

/// Fixed-offset reads from one structure; a read past its end is `err`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fields<'a> {
    pub(crate) b: &'a [u8],
    err: Error,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(b: &'a [u8], err: Error) -> Self {
        Self { b, err }
    }

    pub(crate) fn u8(&self, off: usize) -> Result<u8, Error> {
        self.b.get(off).copied().ok_or(self.err)
    }

    pub(crate) fn u16(&self, off: usize) -> Result<u16, Error> {
        u16_at(self.b, off).ok_or(self.err)
    }

    pub(crate) fn u32(&self, off: usize) -> Result<u32, Error> {
        u32_at(self.b, off).ok_or(self.err)
    }

    pub(crate) fn u64(&self, off: usize) -> Result<u64, Error> {
        u64_at(self.b, off).ok_or(self.err)
    }

    pub(crate) fn gas(&self, off: usize) -> Result<Gas, Error> {
        Gas::parse(self.b, off).map_err(|_| self.err)
    }
}

/// One structure from a list: its offset in the table, type, and bytes
/// (header included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Raw<'a> {
    pub(crate) off: usize,
    pub(crate) kind: u8,
    pub(crate) bytes: &'a [u8],
}

/// How a list's structures state their length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LenField {
    /// A byte at offset 1 (MADT, SRAT, PPTT).
    U8,
    /// A little-endian `u16` at offset 1 (GTDT platform timers).
    U16,
}

/// Walks a structure list from `off` to the end of `bytes`, or for `left`
/// entries if a count is given. A structure whose length is shorter than its
/// header or runs past the table yields one `err` and ends the walk, so a
/// zero length cannot loop.
#[derive(Clone, Debug)]
pub(crate) struct Entries<'a> {
    bytes: &'a [u8],
    off: usize,
    left: Option<u32>,
    len_field: LenField,
    err: Error,
    done: bool,
}

impl<'a> Entries<'a> {
    pub(crate) fn new(
        bytes: &'a [u8],
        off: usize,
        left: Option<u32>,
        len_field: LenField,
        err: Error,
    ) -> Self {
        Self {
            bytes,
            off,
            left,
            len_field,
            err,
            done: false,
        }
    }

    /// An iterator that yields `err` once: one entry expected in no bytes.
    pub(crate) fn failed(err: Error) -> Self {
        Self::new(&[], 0, Some(1), LenField::U8, err)
    }

    fn entry(&self) -> Option<Raw<'a>> {
        let kind = *self.bytes.get(self.off)?;
        let (len, min) = match self.len_field {
            LenField::U8 => (usize::from(*self.bytes.get(self.off.checked_add(1)?)?), 2),
            LenField::U16 => (
                usize::from(u16_at(self.bytes, self.off.checked_add(1)?)?),
                3,
            ),
        };
        if len < min {
            return None;
        }
        let bytes = self.bytes.get(self.off..self.off.checked_add(len)?)?;
        Some(Raw {
            off: self.off,
            kind,
            bytes,
        })
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Raw<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match &mut self.left {
            Some(0) => {
                self.done = true;
                return None;
            }
            Some(n) => *n -= 1,
            None if self.off == self.bytes.len() => {
                self.done = true;
                return None;
            }
            None => {}
        }
        if let Some(raw) = self.entry() {
            self.off += raw.bytes.len();
            Some(Ok(raw))
        } else {
            self.done = true;
            Some(Err(self.err))
        }
    }
}
