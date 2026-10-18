// SPDX-License-Identifier: BSD-2-Clause
//! The legacy Rust mangling: an Itanium-style nested name
//! `_ZN{len ident}E`, whose last element is usually the hash `h<16 hex>`.
//!
//! Identifier characters outside `[A-Za-z0-9_]` are escaped as `$..$`
//! sequences (`$LT$` for `<`, `$u20$` for a space, ...) and `::` inside an
//! element (as in `<a::B as c::D>`) is written `..`.

use crate::Error;
use crate::sink::{Sink, dry_run_then_print};
use core::fmt;

/// Demangles `sym`, the part of a legacy symbol after `_ZN`.
pub(crate) fn demangle(sym: &[u8], out: &mut dyn fmt::Write) -> Result<(), Error> {
    // Count the elements and find where the name ends.
    let mut count = 0usize;
    let mut last: &[u8] = &[];
    let mut rest = sym;
    loop {
        match rest.split_first() {
            Some((b'E', tail)) => {
                rest = tail;
                break;
            }
            Some(_) => {
                let (elem, tail) = element(rest)?;
                last = elem;
                rest = tail;
                count = count.saturating_add(1);
            }
            None => return Err(Error::Invalid),
        }
    }
    // Only a vendor suffix (`.llvm.1234`) may follow the name.
    if count == 0 || !(rest.is_empty() || rest.first() == Some(&b'.')) {
        return Err(Error::Invalid);
    }
    let shown = if count > 1 && is_hash(last) {
        count - 1
    } else {
        count
    };

    dry_run_then_print(out, |sink| {
        let mut rest = sym;
        for i in 0..shown {
            let (elem, tail) = element(rest)?;
            rest = tail;
            if i > 0 {
                sink.str("::")?;
            }
            print_element(elem, sink)?;
        }
        Ok(())
    })
}

/// Splits one `decimal-length bytes` element off the front of `s`.
fn element(s: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    let digits = s.iter().take_while(|b| b.is_ascii_digit()).count();
    let (num, rest) = s.split_at_checked(digits).ok_or(Error::Invalid)?;
    if num.first().is_none_or(|&d| d == b'0') {
        return Err(Error::Invalid);
    }
    let len = num.iter().try_fold(0usize, |acc, &d| {
        acc.checked_mul(10)?.checked_add(usize::from(d - b'0'))
    });
    let len = len.ok_or(Error::Invalid)?;
    rest.split_at_checked(len).ok_or(Error::Invalid)
}

/// `h` followed by 16 hex digits.
fn is_hash(elem: &[u8]) -> bool {
    match elem.split_first() {
        Some((b'h', hex)) => hex.len() == 16 && hex.iter().all(u8::is_ascii_hexdigit),
        _ => false,
    }
}

fn print_element(elem: &[u8], sink: &mut Sink<'_>) -> Result<(), Error> {
    // A leading `_` protects an element that starts with an escape.
    let mut rest = match elem {
        [b'_', b'$', ..] => elem.get(1..).ok_or(Error::Invalid)?,
        _ => elem,
    };
    while let Some(&b) = rest.first() {
        match b {
            b'.' => {
                if rest.get(1) == Some(&b'.') {
                    sink.str("::")?;
                    rest = rest.get(2..).ok_or(Error::Invalid)?;
                } else {
                    sink.str(".")?;
                    rest = rest.get(1..).ok_or(Error::Invalid)?;
                }
            }
            b'$' => {
                let body = rest.get(1..).ok_or(Error::Invalid)?;
                let close = body.iter().position(|&c| c == b'$').ok_or(Error::Invalid)?;
                let (esc, tail) = body.split_at_checked(close).ok_or(Error::Invalid)?;
                sink.char(unescape(esc).ok_or(Error::Invalid)?)?;
                rest = tail.get(1..).ok_or(Error::Invalid)?;
            }
            _ => {
                let run = rest
                    .iter()
                    .position(|&c| c == b'.' || c == b'$')
                    .unwrap_or(rest.len());
                let (text, tail) = rest.split_at_checked(run).ok_or(Error::Invalid)?;
                sink.str(core::str::from_utf8(text).map_err(|_| Error::Invalid)?)?;
                rest = tail;
            }
        }
    }
    Ok(())
}

/// The character a `$..$` escape body stands for.
fn unescape(esc: &[u8]) -> Option<char> {
    Some(match esc {
        b"SP" => '@',
        b"BP" => '*',
        b"RF" => '&',
        b"LT" => '<',
        b"GT" => '>',
        b"LP" => '(',
        b"RP" => ')',
        b"C" => ',',
        [b'u', hex @ ..] if !hex.is_empty() && hex.len() <= 6 => {
            let v = hex.iter().try_fold(0u32, |acc, &d| {
                let d = char::from(d).to_digit(16)?;
                Some(acc * 16 + d)
            })?;
            return char::from_u32(v);
        }
        _ => return None,
    })
}
