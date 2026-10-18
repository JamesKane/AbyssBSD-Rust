// SPDX-License-Identifier: BSD-2-Clause
//! Operators on data values that need no interpreter state: comparison,
//! concatenation, explicit conversions, `Mid`, `Match`, and BCD (ACPI 6.5
//! §19.6).
//!
//! Behavior reference: `sys/contrib/dev/acpica/components/executer/exconvrt.c`,
//! `exmisc.c`, `exconcat.c` (observable results: string forms, `Match`
//! element skipping).

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt::Write;

use crate::Error;
use crate::value::{IntWidth, Value, join_bytes, parse_dec_prefix, parse_hex_prefix};

/// Compares two operands with the type of the first (`LEqual`,
/// `LGreater`, `LLess`).
pub(crate) fn compare(a: &Value, b: &Value, w: IntWidth) -> Result<Ordering, Error> {
    Ok(match a {
        Value::Integer(x) => w.mask(*x).cmp(&b.to_integer(w)?),
        Value::String(x) => x.as_bytes().cmp(b.to_aml_string(w)?.as_bytes()),
        Value::Buffer(x) => x.as_slice().cmp(b.to_buffer(w)?.as_slice()),
        _ => return Err(Error::TypeMismatch),
    })
}

/// `Concatenate`: the result has the first operand's type.
pub(crate) fn concat(a: &Value, b: &Value, w: IntWidth) -> Result<Value, Error> {
    Ok(match a {
        Value::Integer(x) => {
            let y = b.to_integer(w)?;
            let mut out = Vec::new();
            out.extend_from_slice(&x.to_le_bytes()[..w.bytes()]);
            out.extend_from_slice(&y.to_le_bytes()[..w.bytes()]);
            Value::Buffer(out)
        }
        Value::String(x) => {
            let mut s = x.clone();
            s.push_str(&b.to_aml_string(w)?);
            Value::String(s)
        }
        Value::Buffer(x) => {
            let mut out = x.clone();
            out.extend_from_slice(&b.to_buffer(w)?);
            Value::Buffer(out)
        }
        _ => return Err(Error::TypeMismatch),
    })
}

/// `ConcatenateResTemplate`: both templates without their end tags, then
/// one end tag with a zero checksum.
pub(crate) fn concat_res(a: &[u8], b: &[u8]) -> Result<Vec<u8>, Error> {
    let a_end = crate::resource::end_tag_offset(a).map_err(|_| Error::TypeMismatch)?;
    let b_end = crate::resource::end_tag_offset(b).map_err(|_| Error::TypeMismatch)?;
    let mut out = Vec::new();
    out.extend_from_slice(&a[..a_end]);
    out.extend_from_slice(&b[..b_end]);
    out.extend_from_slice(&[0x79, 0x00]);
    Ok(out)
}

/// `ToHexString`.
pub(crate) fn to_hex_string(a: &Value, w: IntWidth) -> Result<Value, Error> {
    Ok(Value::String(match a {
        Value::Integer(x) => {
            let mut s = String::new();
            let _ = write!(s, "0x{:X}", w.mask(*x));
            s
        }
        Value::String(s) => s.clone(),
        Value::Buffer(b) => join_bytes(b, ",", true),
        _ => return Err(Error::TypeMismatch),
    }))
}

/// `ToDecimalString`.
pub(crate) fn to_decimal_string(a: &Value, w: IntWidth) -> Result<Value, Error> {
    Ok(Value::String(match a {
        Value::Integer(x) => {
            let mut s = String::new();
            let _ = write!(s, "{}", w.mask(*x));
            s
        }
        Value::String(s) => s.clone(),
        Value::Buffer(b) => join_bytes(b, ",", false),
        _ => return Err(Error::TypeMismatch),
    }))
}

/// `ToInteger`: a string is decimal unless it starts with `0x`.
pub(crate) fn to_integer_explicit(a: &Value, w: IntWidth) -> Result<u64, Error> {
    match a {
        Value::String(s) => {
            let t = s.trim_start().as_bytes();
            if t.starts_with(b"0x") || t.starts_with(b"0X") {
                Ok(w.mask(parse_hex_prefix(t)))
            } else {
                Ok(w.mask(parse_dec_prefix(t)))
            }
        }
        v => v.to_integer(w),
    }
}

/// `Mid`: up to `len` elements of a string or buffer from `idx`.
pub(crate) fn mid(a: &Value, idx: u64, len: u64, w: IntWidth) -> Result<Value, Error> {
    let slice = |n: usize| -> (usize, usize) {
        let start = usize::try_from(idx).unwrap_or(usize::MAX).min(n);
        let l = usize::try_from(len).unwrap_or(usize::MAX);
        (start, start.saturating_add(l).min(n))
    };
    Ok(match a {
        Value::String(s) => {
            let (st, en) = slice(s.len());
            Value::String(crate::value::string_from_bytes(&s.as_bytes()[st..en]))
        }
        Value::Buffer(b) => {
            let (st, en) = slice(b.len());
            Value::Buffer(b[st..en].to_vec())
        }
        Value::Integer(_) => {
            let b = a.to_buffer(w)?;
            let (st, en) = slice(b.len());
            Value::Buffer(b[st..en].to_vec())
        }
        _ => return Err(Error::TypeMismatch),
    })
}

/// One `Match` comparison: the package element against the operand,
/// converted to the element's type. Elements that are not integers,
/// strings, or buffers never match.
pub(crate) fn match_test(
    op: u8,
    elem: &Value,
    operand: &Value,
    w: IntWidth,
) -> Result<bool, Error> {
    if op == 0 {
        return Ok(true);
    }
    if !matches!(
        elem,
        Value::Integer(_) | Value::String(_) | Value::Buffer(_)
    ) {
        return Ok(false);
    }
    let Ok(ord) = compare(elem, operand, w) else {
        return Ok(false);
    };
    Ok(match op {
        1 => ord.is_eq(),
        2 => ord.is_le(),
        3 => ord.is_lt(),
        4 => ord.is_ge(),
        5 => ord.is_gt(),
        _ => return Err(Error::InvalidValue("match opcode")),
    })
}

/// `FromBCD`.
pub(crate) fn from_bcd(mut v: u64) -> u64 {
    let mut out: u64 = 0;
    let mut scale: u64 = 1;
    while v != 0 {
        let digit = v & 0xf;
        out = out.wrapping_add(digit.wrapping_mul(scale));
        scale = scale.wrapping_mul(10);
        v >>= 4;
    }
    out
}

/// `ToBCD`.
pub(crate) fn to_bcd(mut v: u64) -> u64 {
    let mut out: u64 = 0;
    let mut shift = 0;
    while v != 0 && shift < 64 {
        out |= (v % 10) << shift;
        v /= 10;
        shift += 4;
    }
    out
}
