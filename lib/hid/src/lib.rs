// SPDX-License-Identifier: BSD-2-Clause
//! HID report descriptors (HID 1.11 §6.2.2) parsed into typed fields, and
//! values read out of reports. The descriptor comes from the device:
//! parsing is bounded and returns errors, never panics.
//!
//! A [`Descriptor`] is a list of top-level [`Collection`]s (what
//! `hidbus` makes children of) and the [`Field`]s of every report, each
//! with the report ID, kind, bit position, size, count, flags, logical
//! range, and usages it was declared with.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod usage;

#[cfg(test)]
mod tests;

use alloc::vec::Vec;

/// Why a descriptor was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An item runs past the end.
    Truncated,
    /// `End Collection` or `Pop` without a match, or nesting too deep.
    Unbalanced,
    /// More fields, usages, or report bits than any real device needs.
    TooBig,
}

/// A report's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Device to host.
    Input,
    /// Host to device.
    Output,
    /// Either way, on request.
    Feature,
}

/// Main item flags (HID 1.11 §6.2.2.5).
pub mod flags {
    /// Constant (padding).
    pub const CONSTANT: u32 = 1 << 0;
    /// Variable: one value per usage (else an array of usage indices).
    pub const VARIABLE: u32 = 1 << 1;
    /// Relative (a mouse's motion).
    pub const RELATIVE: u32 = 1 << 2;
    /// Null state: out-of-range values mean "no value".
    pub const NULL_STATE: u32 = 1 << 6;
}

/// The usages a field's values stand for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Usages {
    /// Listed one by one (extended: page in the high 16 bits).
    List(Vec<u32>),
    /// A range, inclusive (extended).
    Range(u32, u32),
}

impl Usages {
    /// The usage for value index `i` (variable fields), or for array value
    /// `v - logical_min` (array fields). The last usage repeats, as HID
    /// 1.11 §6.2.2.8 says for a list shorter than the count.
    #[must_use]
    pub fn get(&self, i: u32) -> Option<u32> {
        match self {
            Self::List(l) => {
                let i = usize::try_from(i).ok()?;
                l.get(i).or_else(|| l.last()).copied()
            }
            Self::Range(lo, hi) => lo.checked_add(i).filter(|u| u <= hi),
        }
    }
}

/// One main item: `count` values of `size` bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// Input, output, or feature.
    pub kind: Kind,
    /// Its report ID (0 if the device uses none).
    pub report_id: u8,
    /// Its first bit in the report, after the ID byte.
    pub bit: u32,
    /// Bits per value (1 to 32).
    pub size: u8,
    /// Values.
    pub count: u16,
    /// [`flags`].
    pub flags: u32,
    /// The logical minimum.
    pub logical_min: i32,
    /// The logical maximum.
    pub logical_max: i32,
    /// The usages.
    pub usages: Usages,
    /// Which top-level collection it is in.
    pub collection: usize,
}

impl Field {
    /// Whether it is padding.
    #[must_use]
    pub fn is_constant(&self) -> bool {
        self.flags & flags::CONSTANT != 0
    }

    /// Whether each value has its own usage.
    #[must_use]
    pub fn is_variable(&self) -> bool {
        self.flags & flags::VARIABLE != 0
    }

    /// Whether values are relative.
    #[must_use]
    pub fn is_relative(&self) -> bool {
        self.flags & flags::RELATIVE != 0
    }

    /// Value `i` from `report` (the ID byte, if any, already stripped),
    /// sign-extended if the logical minimum is negative. `None` past the
    /// report's end.
    #[must_use]
    pub fn value(&self, report: &[u8], i: u16) -> Option<i32> {
        if i >= self.count {
            return None;
        }
        let at = self.bit + u32::from(i) * u32::from(self.size);
        let raw = bits(report, at, self.size)?;
        Some(if self.logical_min < 0 {
            let shift = 32 - u32::from(self.size);
            // Sign extension by shifting the value's top bit to bit 31.
            i32::from_ne_bytes((raw << shift).to_ne_bytes()) >> shift
        } else {
            i32::from_ne_bytes(raw.to_ne_bytes())
        })
    }

    /// The usage of value `i` of a variable field.
    #[must_use]
    pub fn usage(&self, i: u16) -> Option<u32> {
        self.usages.get(u32::from(i))
    }
}

/// `size` bits at bit `at` of `b`, little-endian.
fn bits(b: &[u8], at: u32, size: u8) -> Option<u32> {
    if size == 0 || size > 32 {
        return None;
    }
    let end = at.checked_add(u32::from(size))?;
    if usize::try_from(end.div_ceil(8)).ok()? > b.len() {
        return None;
    }
    let mut v = 0u64;
    let first = usize::try_from(at / 8).ok()?;
    let last = usize::try_from((end - 1) / 8).ok()?;
    for (k, byte) in b[first..=last].iter().enumerate() {
        v |= u64::from(*byte) << (8 * k);
    }
    v >>= at % 8;
    let mask = (1u64 << size) - 1;
    u32::try_from(v & mask).ok()
}

/// A top-level collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collection {
    /// Its usage (extended).
    pub usage: u32,
    /// Its type (1 application, 0 physical, 2 logical, …).
    pub kind: u8,
}

/// A parsed report descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Descriptor {
    /// Top-level collections, in order.
    pub collections: Vec<Collection>,
    /// Every main item.
    pub fields: Vec<Field>,
    /// Whether the device numbers its reports.
    pub uses_ids: bool,
}

/// The limits that keep a hostile descriptor small.
const MAX_FIELDS: usize = 1024;
const MAX_USAGES: usize = 1024;
const MAX_DEPTH: usize = 16;
const MAX_REPORT_BITS: u32 = 8 * 4096;

#[derive(Clone, Copy, Default)]
struct Globals {
    page: u16,
    logical_min: i32,
    logical_max: i32,
    size: u8,
    count: u16,
    report_id: u8,
}

/// An item's data, sign-extended or not.
fn data(b: &[u8], signed: bool) -> i64 {
    let mut v: i64 = 0;
    for (k, &x) in b.iter().enumerate() {
        v |= i64::from(x) << (8 * k);
    }
    if signed && !b.is_empty() {
        let bits = 8 * u32::try_from(b.len()).unwrap_or(4);
        let shift = 64 - bits;
        v = (v << shift) >> shift;
    }
    v
}

impl Descriptor {
    /// Parses a report descriptor.
    ///
    /// # Errors
    ///
    /// If it is truncated, unbalanced, or absurdly large.
    pub fn parse(d: &[u8]) -> Result<Self, Error> {
        let mut out = Self::default();
        let mut g = Globals::default();
        let mut stack: Vec<Globals> = Vec::new();
        let mut usages: Vec<u32> = Vec::new();
        let mut range: (Option<u32>, Option<u32>) = (None, None);
        let mut depth = 0usize;
        let mut top = 0usize;
        // Next bit per (kind, report ID).
        let mut next: Vec<(Kind, u8, u32)> = Vec::new();
        let mut at = 0;
        while at < d.len() {
            let prefix = d[at];
            if prefix == 0xfe {
                // A long item: skip it.
                let n = usize::from(*d.get(at + 1).ok_or(Error::Truncated)?);
                at += 3 + n;
                if at > d.len() {
                    return Err(Error::Truncated);
                }
                continue;
            }
            let n = match prefix & 3 {
                3 => 4,
                k => usize::from(k),
            };
            let body = d.get(at + 1..at + 1 + n).ok_or(Error::Truncated)?;
            at += 1 + n;
            let tag = prefix >> 4;
            let u = data(body, false);
            let signed = data(body, true);
            let u32v = u32::try_from(u & 0xffff_ffff).unwrap_or(0);
            match (prefix >> 2) & 3 {
                // Main.
                0 => {
                    let kind = match tag {
                        8 => Some(Kind::Input),
                        9 => Some(Kind::Output),
                        0xb => Some(Kind::Feature),
                        0xa => {
                            depth += 1;
                            if depth > MAX_DEPTH {
                                return Err(Error::Unbalanced);
                            }
                            if depth == 1 {
                                let usage = usages.first().copied().unwrap_or(0);
                                out.collections.push(Collection {
                                    usage,
                                    kind: body.first().copied().unwrap_or(0),
                                });
                                top = out.collections.len() - 1;
                            }
                            None
                        }
                        0xc => {
                            depth = depth.checked_sub(1).ok_or(Error::Unbalanced)?;
                            None
                        }
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        let size = g.size;
                        let total = u32::from(size) * u32::from(g.count);
                        let slot = match next
                            .iter_mut()
                            .find(|(k, id, _)| *k == kind && *id == g.report_id)
                        {
                            Some(s) => s,
                            None => {
                                next.push((kind, g.report_id, 0));
                                next.last_mut().ok_or(Error::TooBig)?
                            }
                        };
                        let bit = slot.2;
                        slot.2 = bit.checked_add(total).ok_or(Error::TooBig)?;
                        if slot.2 > MAX_REPORT_BITS {
                            return Err(Error::TooBig);
                        }
                        let usages = match range {
                            (Some(lo), Some(hi)) if usages.is_empty() => {
                                Usages::Range(lo, hi.max(lo))
                            }
                            _ => Usages::List(core::mem::take(&mut usages)),
                        };
                        if size > 0 && g.count > 0 {
                            if out.fields.len() >= MAX_FIELDS {
                                return Err(Error::TooBig);
                            }
                            out.fields.push(Field {
                                kind,
                                report_id: g.report_id,
                                bit,
                                size,
                                count: g.count,
                                flags: u32v,
                                logical_min: g.logical_min,
                                logical_max: g.logical_max,
                                usages,
                                collection: top,
                            });
                        }
                    }
                    // Main items end the local state.
                    usages.clear();
                    range = (None, None);
                }
                // Global.
                1 => match tag {
                    0 => g.page = u16::try_from(u & 0xffff).unwrap_or(0),
                    1 => g.logical_min = i32::try_from(signed).unwrap_or(0),
                    2 => {
                        // A maximum below the minimum means the device
                        // meant it unsigned.
                        let s = i32::try_from(signed).unwrap_or(0);
                        g.logical_max = if s < g.logical_min {
                            i32::try_from(u).unwrap_or(i32::MAX)
                        } else {
                            s
                        };
                    }
                    7 => g.size = u8::try_from(u.min(32)).unwrap_or(32),
                    8 => {
                        g.report_id = u8::try_from(u & 0xff).unwrap_or(0);
                        out.uses_ids = true;
                    }
                    9 => g.count = u16::try_from(u.min(4096)).unwrap_or(4096),
                    0xa => {
                        if stack.len() >= MAX_DEPTH {
                            return Err(Error::Unbalanced);
                        }
                        stack.push(g);
                    }
                    0xb => g = stack.pop().ok_or(Error::Unbalanced)?,
                    _ => {}
                },
                // Local.
                2 => {
                    let extend = |v: u32| {
                        if n == 4 {
                            v
                        } else {
                            (u32::from(g.page) << 16) | (v & 0xffff)
                        }
                    };
                    match tag {
                        0 => {
                            if usages.len() >= MAX_USAGES {
                                return Err(Error::TooBig);
                            }
                            usages.push(extend(u32v));
                        }
                        1 => range.0 = Some(extend(u32v)),
                        2 => range.1 = Some(extend(u32v)),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err(Error::Unbalanced);
        }
        Ok(out)
    }

    /// The fields of top-level collection `c`.
    pub fn fields_of(&self, c: usize) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(move |f| f.collection == c)
    }

    /// The size in bytes of report `id` of `kind`, the ID byte not
    /// included.
    #[must_use]
    pub fn report_len(&self, kind: Kind, id: u8) -> usize {
        let bits = self
            .fields
            .iter()
            .filter(|f| f.kind == kind && f.report_id == id)
            .map(|f| f.bit + u32::from(f.size) * u32::from(f.count))
            .max()
            .unwrap_or(0);
        usize::try_from(bits.div_ceil(8)).unwrap_or(0)
    }
}

/// The boot protocol keyboard's report descriptor (HID 1.11 Appendix
/// B.1), for devices whose own descriptor is unusable.
pub const BOOT_KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

/// The boot protocol mouse's report descriptor (HID 1.11 Appendix B.2).
pub const BOOT_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
    0xc0, 0xc0,
];
