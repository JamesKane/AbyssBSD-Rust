// SPDX-License-Identifier: BSD-2-Clause
//! Base address registers: sizing and decoding.
//!
//! A BAR is sized by writing all-ones with the function's decoding of that
//! space turned off, reading back which address bits stick, and restoring
//! it. The result is checked before anything trusts it.

use devtree::strings::PciAddress;

use crate::config::ConfigSpace;
use crate::header::{BAR0, CMD_IO, CMD_MEMORY, COMMAND};

/// What a BAR decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    /// Memory, 32-bit address.
    Memory32,
    /// Memory, 64-bit address (the BAR and the next one).
    Memory64,
    /// x86 I/O ports.
    Io,
}

/// A sized, implemented BAR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    /// The BAR number (0-5); a 64-bit BAR also uses the next.
    pub index: u8,
    /// What it decodes.
    pub kind: BarKind,
    /// The address firmware assigned.
    pub base: u64,
    /// The size, a power of two.
    pub size: u64,
    /// Memory that may be mapped write-combining.
    pub prefetchable: bool,
}

/// Why a BAR is not usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarError {
    /// A 64-bit BAR in the last slot (no upper half).
    Truncated,
    /// The memory type field is reserved.
    ReservedType,
    /// The size read back is not a power of two.
    BadSize,
    /// Firmware left it unassigned (address 0).
    Unassigned,
    /// Base plus size overflows.
    Overflow,
}

/// Sizes BAR `index` of `a` (of `count` in its header). Returns the
/// BAR, `Ok(None)` if it is not implemented, and how many BAR slots it
/// took (2 for a 64-bit BAR).
pub fn read(
    cfg: &dyn ConfigSpace,
    a: PciAddress,
    index: u8,
    count: u8,
) -> (Result<Option<Bar>, BarError>, u8) {
    let reg = BAR0 + 4 * u16::from(index);
    let lo = cfg.read32(a, reg);
    let io = lo & 1 != 0;
    let wide = !io && (lo >> 1) & 3 == 2;
    if !io && (lo >> 1) & 3 == 3 {
        return (Err(BarError::ReservedType), 1);
    }
    if wide && index + 1 >= count {
        return (Err(BarError::Truncated), 1);
    }
    let hi = if wide { cfg.read32(a, reg + 4) } else { 0 };

    let cmd = cfg.read16(a, COMMAND);
    let off = if io { CMD_IO } else { CMD_MEMORY };
    cfg.write16(a, COMMAND, cmd & !off);
    cfg.write32(a, reg, u32::MAX);
    if wide {
        cfg.write32(a, reg + 4, u32::MAX);
    }
    let test_lo = cfg.read32(a, reg);
    let test_hi = if wide { cfg.read32(a, reg + 4) } else { 0 };
    cfg.write32(a, reg, lo);
    if wide {
        cfg.write32(a, reg + 4, hi);
    }
    cfg.write16(a, COMMAND, cmd);

    let slots = if wide { 2 } else { 1 };
    let flags: u32 = if io { 0x3 } else { 0xf };
    let bits = test_lo & !flags;
    if bits == 0 && (!wide || test_hi == 0) {
        return (Ok(None), slots);
    }
    // The low address bits that read back as zero give the size, in the
    // BAR's own width. I/O BARs may decode only 16 bits, the upper ones
    // reading back as zero.
    let size = if wide {
        (!((u64::from(test_hi) << 32) | u64::from(bits))).wrapping_add(1)
    } else if io {
        u64::from((!(bits | 0xffff_0000)).wrapping_add(1))
    } else {
        u64::from((!bits).wrapping_add(1))
    };
    if size == 0 || !size.is_power_of_two() {
        return (Err(BarError::BadSize), slots);
    }
    let base = ((u64::from(hi) << 32) | u64::from(lo)) & !u64::from(flags);
    let base = if io { base & 0xffff_ffff } else { base };
    if base == 0 {
        return (Err(BarError::Unassigned), slots);
    }
    if base.checked_add(size).is_none() {
        return (Err(BarError::Overflow), slots);
    }
    let bar = Bar {
        index,
        kind: if io {
            BarKind::Io
        } else if wide {
            BarKind::Memory64
        } else {
            BarKind::Memory32
        },
        base,
        size,
        prefetchable: !io && lo & 8 != 0,
    };
    (Ok(Some(bar)), slots)
}

/// Sizes every BAR of a function with `count` BAR slots.
pub fn read_all(
    cfg: &dyn ConfigSpace,
    a: PciAddress,
    count: u8,
) -> alloc::vec::Vec<Result<Bar, (u8, BarError)>> {
    let mut out = alloc::vec::Vec::new();
    let mut i = 0;
    while i < count {
        let (r, slots) = read(cfg, a, i, count);
        match r {
            Ok(Some(b)) => out.push(Ok(b)),
            Ok(None) => {}
            Err(e) => out.push(Err((i, e))),
        }
        i += slots;
    }
    out
}
