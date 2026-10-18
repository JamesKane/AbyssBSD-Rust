// SPDX-License-Identifier: BSD-2-Clause
//! x86_64 4-level paging entries (Intel SDM Vol. 3A §4.5, AMD APM Vol. 2
//! §5.3).
//!
//! Memory types come from the PAT. [`PAT`] is the layout these entries
//! assume; the kernel entry code writes it to `IA32_PAT` before loading
//! tables built here.

use crate::{Attrs, Cache, Format, Level, PageSize, PhysAddr, VirtAddr};

const PRESENT: u64 = 1 << 0;
const WRITE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const PWT: u64 = 1 << 3;
const PCD: u64 = 1 << 4;
const ACCESSED: u64 = 1 << 5;
const DIRTY: u64 = 1 << 6;
/// Page size in a PDE/PDPTE.
const HUGE: u64 = 1 << 7;
const GLOBAL: u64 = 1 << 8;
const NO_EXEC: u64 = 1 << 63;
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// `IA32_PAT` value: PA0 WB, PA1 WC, PA2 UC-, PA3 UC, repeated in PA4–7.
/// Entries select PA0 (WB), PA1 (WC, via PWT), and PA3 (UC, via PCD|PWT).
pub const PAT: u64 = 0x0007_0106_0007_0106;

/// The x86_64 entry format.
#[derive(Debug)]
pub enum X86_64 {}

impl Format for X86_64 {
    fn table(next: PhysAddr) -> u64 {
        // Rights are decided by the leaf, so tables grant everything.
        (next.get() & ADDR_MASK) | PRESENT | WRITE | USER
    }

    fn leaf(pa: PhysAddr, size: PageSize, a: Attrs) -> u64 {
        let mut e = (pa.get() & ADDR_MASK) | PRESENT | ACCESSED | DIRTY;
        if size != PageSize::Size4K {
            e |= HUGE;
        }
        if a.write {
            e |= WRITE;
        }
        if a.user {
            e |= USER;
        }
        if a.global {
            e |= GLOBAL;
        }
        if !a.exec {
            e |= NO_EXEC;
        }
        e | match a.cache {
            Cache::WriteBack => 0,
            Cache::WriteCombining => PWT,
            Cache::Device => PCD | PWT,
        }
    }

    fn is_valid(entry: u64) -> bool {
        entry & PRESENT != 0
    }

    fn is_table(entry: u64, level: Level) -> bool {
        level.get() == 4 || entry & HUGE == 0
    }

    fn address(entry: u64, level: Level) -> PhysAddr {
        let mask = match level.get() {
            3 if entry & HUGE != 0 => ADDR_MASK & !((1 << 30) - 1),
            2 if entry & HUGE != 0 => ADDR_MASK & !((1 << 21) - 1),
            _ => ADDR_MASK,
        };
        PhysAddr::new(entry & mask)
    }

    fn attrs(entry: u64, _level: Level) -> Attrs {
        Attrs {
            write: entry & WRITE != 0,
            exec: entry & NO_EXEC == 0,
            user: entry & USER != 0,
            global: entry & GLOBAL != 0,
            cache: match entry & (PCD | PWT) {
                0 => Cache::WriteBack,
                PWT => Cache::WriteCombining,
                _ => Cache::Device,
            },
        }
    }

    fn is_canonical(va: VirtAddr) -> bool {
        crate::is_canonical_48(va)
    }
}
