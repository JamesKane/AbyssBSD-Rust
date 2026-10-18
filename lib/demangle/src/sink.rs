// SPDX-License-Identifier: BSD-2-Clause
//! Bounded output sink shared by the demanglers, and the dry-run-then-print
//! driver that keeps a malformed symbol from being half printed.

use core::fmt;

use crate::Error;

/// Longest demangled name we are willing to produce. Backrefs let a short
/// symbol describe an exponentially long name; anything past this falls back
/// to the raw symbol.
pub(crate) const MAX_OUTPUT: usize = 16 * 1024;

/// Counts what is written and forwards it to the real writer, if any.
pub(crate) struct Sink<'o> {
    out: Option<&'o mut dyn fmt::Write>,
    len: usize,
    err: Option<Error>,
}

impl Sink<'_> {
    /// Writes `s`, failing once the total exceeds [`MAX_OUTPUT`].
    pub(crate) fn str(&mut self, s: &str) -> Result<(), Error> {
        self.len = self.len.saturating_add(s.len());
        if self.len > MAX_OUTPUT {
            return Err(Error::TooComplex);
        }
        if let Some(out) = self.out.as_mut() {
            crate::write_sanitized(s, &mut **out)?;
        }
        Ok(())
    }

    /// Writes one character.
    pub(crate) fn char(&mut self, c: char) -> Result<(), Error> {
        let mut buf = [0u8; 4];
        self.str(c.encode_utf8(&mut buf))
    }

    /// Writes formatted output (numbers, escaped characters).
    pub(crate) fn fmt(&mut self, args: fmt::Arguments<'_>) -> Result<(), Error> {
        match fmt::write(self, args) {
            Ok(()) => Ok(()),
            Err(fmt::Error) => Err(self.err.take().unwrap_or(Error::Fmt)),
        }
    }
}

impl fmt::Write for Sink<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.str(s).map_err(|e| {
            self.err = Some(e);
            fmt::Error
        })
    }
}

/// Runs `print` once against a counting-only sink and, if that succeeds,
/// again against `out`. `print` must be deterministic.
pub(crate) fn dry_run_then_print(
    out: &mut dyn fmt::Write,
    print: impl Fn(&mut Sink<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    print(&mut Sink {
        out: None,
        len: 0,
        err: None,
    })?;
    print(&mut Sink {
        out: Some(out),
        len: 0,
        err: None,
    })
}
