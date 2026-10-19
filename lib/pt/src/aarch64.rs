// SPDX-License-Identifier: BSD-2-Clause
//! AArch64 VMSAv8-64 stage 1 descriptors, 4 KiB granule, 48-bit VA (Arm ARM
//! D8.3).
//!
//! Memory types are indices into `MAIR_EL1`. [`MAIR`] is the layout these
//! descriptors assume; the kernel entry code programs it before enabling
//! tables built here. The upper and lower halves are separate hierarchies
//! (TTBR1 and TTBR0); both use the same index bits.

use crate::{Attrs, Cache, Format, Level, PageSize, PhysAddr, VirtAddr};

const VALID: u64 = 1 << 0;
/// At levels 0–2: a table rather than a block. At level 3: a page.
const TABLE_OR_PAGE: u64 = 1 << 1;
const ATTR_INDEX_SHIFT: u32 = 2;
/// AP[1]: accessible from EL0.
const AP_EL0: u64 = 1 << 6;
/// AP[2]: read-only.
const AP_RO: u64 = 1 << 7;
const SH_INNER: u64 = 0b11 << 8;
const ACCESS_FLAG: u64 = 1 << 10;
const NOT_GLOBAL: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

/// `MAIR_EL1` attribute indices.
const ATTR_WRITE_BACK: u64 = 0;
const ATTR_DEVICE: u64 = 1;
const ATTR_NON_CACHEABLE: u64 = 2;

/// `MAIR_EL1` value: Attr0 Normal WB RW-allocate (0xff), Attr1
/// Device-nGnRE (0x04), Attr2 Normal non-cacheable (0x44).
pub const MAIR: u64 = 0xff | (0x04 << 8) | (0x44 << 16);

/// The AArch64 descriptor format.
#[derive(Debug)]
pub enum Aarch64 {}

impl Format for Aarch64 {
    fn table(next: PhysAddr) -> u64 {
        (next.get() & ADDR_MASK) | VALID | TABLE_OR_PAGE
    }

    fn leaf(pa: PhysAddr, size: PageSize, a: Attrs) -> u64 {
        let mut e = (pa.get() & ADDR_MASK) | VALID | ACCESS_FLAG | SH_INNER;
        if size == PageSize::Size4K {
            e |= TABLE_OR_PAGE;
        }
        let index = match a.cache {
            Cache::WriteBack => ATTR_WRITE_BACK,
            Cache::Device => ATTR_DEVICE,
            Cache::WriteCombining => ATTR_NON_CACHEABLE,
        };
        e |= index << ATTR_INDEX_SHIFT;
        if !a.write {
            e |= AP_RO;
        }
        if !a.global {
            e |= NOT_GLOBAL;
        }
        let device = a.cache == Cache::Device;
        if a.user {
            // The kernel never executes user pages.
            e |= AP_EL0 | PXN;
            if !a.exec || device {
                e |= UXN;
            }
        } else {
            e |= UXN;
            if !a.exec || device {
                e |= PXN;
            }
        }
        e
    }

    fn is_valid(entry: u64) -> bool {
        entry & VALID != 0
    }

    fn is_table(entry: u64, level: Level) -> bool {
        level.get() > 1 && entry & TABLE_OR_PAGE != 0
    }

    fn address(entry: u64, level: Level) -> PhysAddr {
        let mask = match level.get() {
            3 if entry & TABLE_OR_PAGE == 0 => ADDR_MASK & !((1 << 30) - 1),
            2 if entry & TABLE_OR_PAGE == 0 => ADDR_MASK & !((1 << 21) - 1),
            _ => ADDR_MASK,
        };
        PhysAddr::new(entry & mask)
    }

    fn attrs(entry: u64, _level: Level) -> Attrs {
        let user = entry & AP_EL0 != 0;
        let xn = if user { UXN } else { PXN };
        Attrs {
            write: entry & AP_RO == 0,
            exec: entry & xn == 0,
            user,
            global: entry & NOT_GLOBAL == 0,
            cache: match (entry >> ATTR_INDEX_SHIFT) & 0b111 {
                ATTR_WRITE_BACK => Cache::WriteBack,
                ATTR_NON_CACHEABLE => Cache::WriteCombining,
                _ => Cache::Device,
            },
        }
    }

    fn is_canonical(va: VirtAddr) -> bool {
        // TTBR0 covers all of [0, 2^48) (bit 47 included, unlike x86's
        // sign-extended halves); TTBR1 the top 2^48.
        let top = va.get() >> 48;
        top == 0 || top == 0xffff
    }
}
