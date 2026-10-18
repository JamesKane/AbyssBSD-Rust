// SPDX-License-Identifier: BSD-2-Clause
//! Rust symbol demangling for kernel panic backtraces.
//!
//! Turns a mangled symbol name, as found in an ELF symbol table, into the
//! human-readable path a backtrace should show. Two manglings are understood:
//!
//! - **v0** (`_R...`, RFC 2603), the default for our toolchain. Output matches
//!   the "alternate" form of the usual Rust demanglers: crate disambiguator
//!   hashes are dropped, so `_RNvCs34uFkGXw8xg_1k5TABLE` prints `k::TABLE`.
//! - **legacy** (`_ZN...E`), the older Itanium-like scheme. The trailing
//!   `h<16 hex digits>` hash element is dropped and `$..$` escapes decoded.
//!
//! Anything else, and anything malformed, is printed verbatim, with bytes that
//! are not UTF-8 shown as `?`. Control characters are shown as `?` in all
//! output. Symbol names are untrusted input: parsing is
//! bounds checked, recursion depth and total work are capped, and nothing here
//! panics or allocates. Before a demangled name is printed it is first parsed
//! in a dry run that writes nothing, so a malformed symbol never produces a
//! half-printed name followed by the raw fallback.

#![no_std]
#![forbid(unsafe_code)]

mod legacy;
mod sink;
mod v0;

#[cfg(test)]
mod tests;

use core::fmt;

/// Why a demangling attempt was abandoned (internal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The symbol does not follow the grammar.
    Invalid,
    /// Valid so far, but deeper, longer or more work than the caps allow.
    TooComplex,
    /// The output sink failed.
    Fmt,
}

impl From<fmt::Error> for Error {
    fn from(_: fmt::Error) -> Self {
        Self::Fmt
    }
}

/// Writes the demangled form of `sym` to `out`.
///
/// v0 and legacy Rust symbols are demangled (see the crate documentation);
/// anything else, or a symbol that fails to parse or exceeds the internal
/// complexity caps, is written raw with non-UTF-8 bytes replaced by `?`.
/// The only error is one returned by `out` itself.
pub fn demangle(sym: &[u8], out: &mut impl fmt::Write) -> fmt::Result {
    let out: &mut dyn fmt::Write = out;
    let attempt = if let Some(rest) = strip_any(sym, &[b"_R", b"__R"]) {
        v0::demangle(rest, out)
    } else if let Some(rest) = strip_any(sym, &[b"_ZN", b"__ZN"]) {
        legacy::demangle(rest, out)
    } else {
        Err(Error::Invalid)
    };
    match attempt {
        Ok(()) => Ok(()),
        Err(Error::Fmt) => Err(fmt::Error),
        Err(Error::Invalid | Error::TooComplex) => write_raw(sym, out),
    }
}

/// [`fmt::Display`] adapter: `format_args!("{}", Demangled(name))` prints
/// what [`demangle`] would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Demangled<'a>(pub &'a [u8]);

impl fmt::Display for Demangled<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        demangle(self.0, f)
    }
}

fn strip_any<'a>(sym: &'a [u8], prefixes: &[&[u8]]) -> Option<&'a [u8]> {
    prefixes.iter().find_map(|p| sym.strip_prefix(*p))
}

/// Writes `bytes` as text, one `?` per byte that is not part of valid UTF-8.
fn write_raw(bytes: &[u8], out: &mut dyn fmt::Write) -> fmt::Result {
    for chunk in bytes.utf8_chunks() {
        write_sanitized(chunk.valid(), out)?;
        for _ in chunk.invalid() {
            out.write_char('?')?;
        }
    }
    Ok(())
}

/// Writes `s` with control characters replaced by `?`, so a hostile symbol
/// cannot inject terminal escape sequences into a console.
pub(crate) fn write_sanitized(s: &str, out: &mut dyn fmt::Write) -> fmt::Result {
    for (i, part) in s.split(char::is_control).enumerate() {
        if i > 0 {
            out.write_char('?')?;
        }
        out.write_str(part)?;
    }
    Ok(())
}
