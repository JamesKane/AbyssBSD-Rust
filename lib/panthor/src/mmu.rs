// SPDX-License-Identifier: BSD-2-Clause
//! The GPU's MMU (specification 04): the AArch64 stage-1 page tables with a 4 KiB granule
//! (§4.6), the attribute words (§4.7, 05 §5.3.3), and the address-space commands (§4.3).
//!
//! The tables are reached through [`Tables`], so they can live in device memory in the driver
//! and in a vector in the tests.

use crate::regs::Regs;
use crate::{Delay, Error};

/// Table memory: 4 KiB tables of 512 descriptors, by the physical address the GPU walks.
pub trait Tables {
    /// A new zeroed table, by its physical address, or `None` when out of memory.
    fn alloc(&mut self) -> Option<u64>;
    fn read(&self, table: u64, index: usize) -> u64;
    fn write(&mut self, table: u64, index: usize, desc: u64);
}

/// How a page is mapped (04 §4.6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Map {
    pub read_only: bool,
    pub no_exec: bool,
    pub cacheable: bool,
}

const VALID_TABLE: u64 = 0b11;
const PAGE_DESC: u64 = 0b11;
const OA_MASK: u64 = 0x0000_FFFF_FFFF_F000;
const AP1: u64 = 1 << 6;
const AP2: u64 = 1 << 7;
const AF: u64 = 1 << 10;
const NG: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

/// A last-level page descriptor for `pa` (04 §4.6.2).
pub fn page(pa: u64, m: Map) -> u64 {
    let (attr, sh) = if m.cacheable { (1, 3) } else { (0, 2) };
    let mut d = PAGE_DESC | attr << 2 | AP1 | sh << 8 | AF | NG | (pa & OA_MASK);
    if m.read_only {
        d |= AP2;
    }
    if m.no_exec {
        d |= PXN | UXN;
    }
    d
}

/// The number of levels for `va_bits`, ending at level 3 (04 §4.6.1).
pub fn levels(va_bits: u8) -> u32 {
    (u32::from(va_bits) - 12).div_ceil(9)
}

/// Maps `[va, va + len)` to `[pa, pa + len)` with 4 KiB pages. `va`, `pa` and `len` are page
/// multiples. An address already mapped is an error (nothing here remaps).
pub fn map(
    t: &mut dyn Tables,
    root: u64,
    va_bits: u8,
    va: u64,
    pa: u64,
    len: u64,
    m: Map,
) -> Result<(), Error> {
    if (va | pa | len) & 0xFFF != 0 {
        return Err(Error::Bad("unaligned mapping"));
    }
    if !(21..=48).contains(&va_bits) || va.checked_add(len).is_none_or(|end| end > 1 << va_bits) {
        return Err(Error::Bad("mapping beyond the address space"));
    }
    let n = levels(va_bits);
    let mut off = 0;
    while off < len {
        let v = va + off;
        let mut table = root;
        for level in (4 - n)..3 {
            let shift = 39 - 9 * level;
            let i = (v >> shift & 0x1FF) as usize;
            let d = t.read(table, i);
            table = if d & 0b11 == VALID_TABLE {
                d & OA_MASK
            } else if d == 0 {
                let next = t.alloc().ok_or(Error::Bad("out of table memory"))?;
                t.write(table, i, next | VALID_TABLE);
                next
            } else {
                return Err(Error::Bad("a block is in the way"));
            };
        }
        let i = (v >> 12 & 0x1FF) as usize;
        if t.read(table, i) != 0 {
            return Err(Error::Bad("already mapped"));
        }
        t.write(table, i, page(pa + off, m));
        off += 0x1000;
    }
    Ok(())
}

/// Clears the page descriptors of `[va, va + len)` (whatever is mapped there; holes are left as
/// they are). Tables stay: they are freed with the address space.
pub fn unmap(t: &mut dyn Tables, root: u64, va_bits: u8, va: u64, len: u64) -> Result<(), Error> {
    if (va | len) & 0xFFF != 0 {
        return Err(Error::Bad("unaligned unmapping"));
    }
    if !(21..=48).contains(&va_bits) || va.checked_add(len).is_none_or(|end| end > 1 << va_bits) {
        return Err(Error::Bad("unmapping beyond the address space"));
    }
    let n = levels(va_bits);
    let mut off = 0;
    while off < len {
        let v = va + off;
        let mut table = Some(root);
        for level in (4 - n)..3 {
            let Some(tb) = table else { break };
            let d = t.read(tb, (v >> (39 - 9 * level) & 0x1FF) as usize);
            table = (d & 0b11 == VALID_TABLE).then_some(d & OA_MASK);
        }
        if let Some(tb) = table {
            t.write(tb, (v >> 12 & 0x1FF) as usize, 0);
        }
        off += 0x1000;
    }
    Ok(())
}

/// Walks `va` to its page descriptor (0 if unmapped), for checks.
pub fn walk(t: &dyn Tables, root: u64, va_bits: u8, va: u64) -> u64 {
    if !(21..=48).contains(&va_bits) {
        return 0;
    }
    let n = levels(va_bits);
    let mut table = root;
    for level in (4 - n)..3 {
        let d = t.read(table, (va >> (39 - 9 * level) & 0x1FF) as usize);
        if d & 0b11 != VALID_TABLE {
            return 0;
        }
        table = d & OA_MASK;
    }
    t.read(table, (va >> 12 & 0x1FF) as usize)
}

/// `AS_MEMATTR` for the page tables' attribute indexes (04 §4.7.2): index 1 write-back, the
/// others non-cacheable. Non-coherent: shareability 0 for write-back.
pub fn memattr(coherent: bool) -> u64 {
    if coherent {
        0x4C4C_4C4C_9C4C_9F4C
    } else {
        0x4C4C_4C4C_8C4C_8F4C
    }
}

/// `AS_TRANSCFG` (04 §4.3.1): AArch64 4 KiB granule, the input size from the VA bits, write-back
/// walks with read-allocate, outer-shareable walks only when coherent.
pub fn transcfg(va_bits: u8, coherent: bool) -> u64 {
    let ina = u64::from(55 - va_bits);
    let sh = if coherent { 2 } else { 0 };
    6 | ina << 6 | 2 << 24 | sh << 28 | 1 << 30
}

/// `AS_LOCKADDR` for `[start, end)` (04 §4.4.2), the base aligned to the whole region.
pub fn lockaddr(start: u64, end: u64) -> u64 {
    let last = end - 1;
    let h = if start == last {
        0
    } else {
        64 - (start ^ last).leading_zeros()
    };
    let w = h.max(15) - 1;
    let base = start & !((1u64 << (w + 1)) - 1);
    base | u64::from(w)
}

// Address-space slot registers (02 §2.7.2).
const AS_BASE: usize = 0x2400;
const AS_STRIDE: usize = 0x40;
const AS_TRANSTAB: usize = 0x00;
const AS_MEMATTR: usize = 0x08;
const AS_LOCKADDR: usize = 0x10;
const AS_COMMAND: usize = 0x18;
pub const AS_FAULTSTATUS: usize = 0x1C;
pub const AS_FAULTADDRESS: usize = 0x20;
const AS_STATUS: usize = 0x28;
const AS_TRANSCFG: usize = 0x30;

pub const CMD_UPDATE: u32 = 1;
pub const CMD_LOCK: u32 = 2;
pub const CMD_UNLOCK: u32 = 3;

/// A slot's register.
pub fn as_reg(slot: usize, reg: usize) -> usize {
    AS_BASE + slot * AS_STRIDE + reg
}

/// Runs `cmd` on `slot` with the command protocol (04 §4.3.3): idle before and after, polled
/// every 10 µs for up to 100 ms each.
pub fn command(r: &dyn Regs, d: &mut dyn Delay, slot: usize, cmd: u32) -> Result<(), Error> {
    let idle = |r: &dyn Regs| r.read32(as_reg(slot, AS_STATUS)) & 1 == 0;
    let wait = |r: &dyn Regs, d: &mut dyn Delay| {
        for _ in 0..10_000 {
            if idle(r) {
                return Ok(());
            }
            d.delay_us(10);
        }
        if idle(r) {
            Ok(())
        } else {
            Err(Error::Timeout("address space busy"))
        }
    };
    wait(r, d)?;
    r.write32(as_reg(slot, AS_COMMAND), cmd);
    wait(r, d)
}

/// Binds a VM to `slot` (04 §4.3.1): its fault interrupt unmasked, the three registers, then
/// `UPDATE`.
pub fn enable(
    r: &dyn Regs,
    d: &mut dyn Delay,
    slot: usize,
    root: u64,
    memattr: u64,
    transcfg: u64,
) -> Result<(), Error> {
    use crate::regs::{INT_CLEAR, INT_MASK, MMU_INT};
    let bit = 1u32 << slot;
    r.write32(MMU_INT + INT_CLEAR, bit);
    let mask = r.read32(MMU_INT + INT_MASK);
    r.write32(MMU_INT + INT_MASK, mask | bit);
    r.write64(as_reg(slot, AS_TRANSTAB), root);
    r.write64(as_reg(slot, AS_MEMATTR), memattr);
    r.write64(as_reg(slot, AS_TRANSCFG), transcfg);
    command(r, d, slot, CMD_UPDATE)
}

/// Unbinds `slot` (04 §4.3.2, full form, after the caller's cache flush): translation off, every
/// access faults, its fault interrupt masked.
pub fn disable(r: &dyn Regs, d: &mut dyn Delay, slot: usize) -> Result<(), Error> {
    use crate::regs::{INT_MASK, MMU_INT};
    let mask = r.read32(MMU_INT + INT_MASK);
    r.write32(MMU_INT + INT_MASK, mask & !(1u32 << slot));
    r.write64(as_reg(slot, AS_TRANSTAB), 0);
    r.write64(as_reg(slot, AS_MEMATTR), 0);
    r.write64(as_reg(slot, AS_TRANSCFG), 1);
    command(r, d, slot, CMD_UPDATE)
}

/// Writes the lock region for `[start, end)` and issues `LOCK` (04 §4.4.1 step 1).
pub fn lock(
    r: &dyn Regs,
    d: &mut dyn Delay,
    slot: usize,
    start: u64,
    end: u64,
) -> Result<(), Error> {
    r.write64(as_reg(slot, AS_LOCKADDR), lockaddr(start, end));
    command(r, d, slot, CMD_LOCK)
}
