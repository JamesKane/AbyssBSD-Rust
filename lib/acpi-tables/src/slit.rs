// SPDX-License-Identifier: BSD-2-Clause
//! System Locality Information Table (ACPI 6.5 §5.2.17): relative distances
//! between proximity domains.
//!
//! Behavior reference: `sys/dev/acpica/acpi_pxm.c`.

use crate::{Error, HEADER_LEN, Sdt, u64_at};

const COUNT: usize = HEADER_LEN;
const MATRIX: usize = HEADER_LEN + 8;

/// A parsed SLIT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slit<'a> {
    count: usize,
    matrix: &'a [u8],
}

/// Parses a SLIT, checking that the whole `n × n` matrix is present.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table, [`Error::BadLength`] if the
/// matrix does not fit.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Slit<'a>, Error> {
    if t.signature() != *b"SLIT" {
        return Err(Error::BadSignature);
    }
    let bad = Error::BadLength(*b"SLIT");
    let count = u64_at(t.bytes, COUNT).ok_or(bad)?;
    let count = usize::try_from(count).map_err(|_| bad)?;
    let cells = count.checked_mul(count).ok_or(bad)?;
    let matrix = t
        .bytes
        .get(MATRIX..)
        .and_then(|m| m.get(..cells))
        .ok_or(bad)?;
    Ok(Slit { count, matrix })
}

impl Slit<'_> {
    /// The number of system localities.
    #[must_use]
    pub fn locality_count(&self) -> usize {
        self.count
    }

    /// The relative distance from locality `from` to `to`: 10 is local,
    /// 255 unreachable. `None` if either index is out of range.
    #[must_use]
    pub fn distance(&self, from: usize, to: usize) -> Option<u8> {
        if from >= self.count || to >= self.count {
            return None;
        }
        self.matrix
            .get(from.checked_mul(self.count)?.checked_add(to)?)
            .copied()
    }
}
