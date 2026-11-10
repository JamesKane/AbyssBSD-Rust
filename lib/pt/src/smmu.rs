// SPDX-License-Identifier: BSD-2-Clause
//! SMMUv3 stage 1 descriptors: the VMSAv8-64 format of [`crate::aarch64`]
//! as an SMMU walks it for a device (Arm IHI 0070 §3.4, §5.4; Arm ARM
//! D8.3), 4 KiB granule, 48-bit input through TTB0 only.
//!
//! What differs from the CPU's tables (docs/IOMMU.md §4.7):
//!
//! - Every leaf is reachable by unprivileged accesses (AP\[1\] set): PCIe
//!   requests are unprivileged unless the STE overrides it, and FreeBSD's
//!   `iommu_pmap` sets `ATTR_S1_AP_USER` on every page.
//! - Every leaf is execute-never and not global (tagged with the context
//!   descriptor's ASID), as FreeBSD's `smmu_pmap_enter` makes them.
//! - The access flag is always set: hardware access-flag updates (HTTU)
//!   are not used.
//! - Memory types index the context descriptor's MAIR ([`MAIR`]):
//!   [`Cache::WriteBack`] is Normal write-back inner-shareable (a coherent
//!   device), [`Cache::WriteCombining`] Normal non-cacheable
//!   outer-shareable (a non-coherent one), [`Cache::Device`] Device-nGnRE
//!   (the MSI doorbell).
//! - Permissions are read-only or read-write. Stage 1 cannot express
//!   write-only, so a buffer the device only writes is mapped read-write,
//!   as FreeBSD's busdma maps it.
//!
//! [`Attrs::user`] and [`Attrs::global`] are therefore ignored when
//! encoding; decoding reports `user: true, global: false`. [`dma`] builds
//! the attributes in that normal form.

use crate::aarch64::{
    ACCESS_FLAG, ADDR_MASK, AP_EL0, AP_RO, ATTR_DEVICE, ATTR_INDEX_SHIFT, ATTR_NON_CACHEABLE,
    ATTR_WRITE_BACK, NOT_GLOBAL, PXN, SH_INNER, TABLE_OR_PAGE, UXN, VALID,
};
use crate::{Attrs, Cache, Format, Level, PageSize, PhysAddr, VirtAddr};

/// Outer shareable (SH = 0b10).
const SH_OUTER: u64 = 0b10 << 8;
const SH_MASK: u64 = 0b11 << 8;

/// The MAIR a context descriptor carries for these descriptors: Attr0
/// Normal write-back read/write-allocate (0xff), Attr1 Device-nGnRE (0x04),
/// Attr2 Normal non-cacheable (0x44). The same layout as the CPU's
/// [`crate::aarch64::MAIR`].
pub const MAIR: u64 = crate::aarch64::MAIR;

/// The input address size: TTB0 covers `[0, 2^48)` (T0SZ 16).
pub const INPUT_BITS: u32 = 48;

/// Attributes for a DMA mapping, in this format's normal form.
#[must_use]
pub const fn dma(write: bool, cache: Cache) -> Attrs {
    Attrs {
        write,
        exec: false,
        user: true,
        global: false,
        cache,
    }
}

/// Whether a valid entry's access flag is set.
#[must_use]
pub const fn access_flag(entry: u64) -> bool {
    entry & ACCESS_FLAG != 0
}

/// Whether a leaf grants unprivileged access (AP\[1\]).
#[must_use]
pub const fn unprivileged(entry: u64) -> bool {
    entry & AP_EL0 != 0
}

/// Whether a leaf is read-only (AP\[2\]).
#[must_use]
pub const fn read_only(entry: u64) -> bool {
    entry & AP_RO != 0
}

/// Whether an entry at the last level is a page. The other valid encoding
/// there (bit 1 clear) is reserved and faults as invalid.
#[must_use]
pub const fn is_page(entry: u64) -> bool {
    entry & (VALID | TABLE_OR_PAGE) == VALID | TABLE_OR_PAGE
}

/// The SMMU stage 1 descriptor format.
#[derive(Debug)]
pub enum Smmu {}

impl Format for Smmu {
    fn table(next: PhysAddr) -> u64 {
        (next.get() & ADDR_MASK) | VALID | TABLE_OR_PAGE
    }

    fn leaf(pa: PhysAddr, size: PageSize, a: Attrs) -> u64 {
        let mut e = (pa.get() & ADDR_MASK) | VALID | ACCESS_FLAG | AP_EL0 | NOT_GLOBAL | UXN | PXN;
        if size == PageSize::Size4K {
            e |= TABLE_OR_PAGE;
        }
        let (index, sh) = match a.cache {
            Cache::WriteBack => (ATTR_WRITE_BACK, SH_INNER),
            Cache::WriteCombining => (ATTR_NON_CACHEABLE, SH_OUTER),
            Cache::Device => (ATTR_DEVICE, SH_OUTER),
        };
        e |= (index << ATTR_INDEX_SHIFT) | sh;
        if !a.write {
            e |= AP_RO;
        }
        if a.exec {
            e &= !(UXN | PXN);
        }
        e
    }

    fn is_valid(entry: u64) -> bool {
        entry & VALID != 0
    }

    fn is_table(entry: u64, level: Level) -> bool {
        crate::aarch64::Aarch64::is_table(entry, level)
    }

    fn address(entry: u64, level: Level) -> PhysAddr {
        crate::aarch64::Aarch64::address(entry, level)
    }

    fn attrs(entry: u64, _level: Level) -> Attrs {
        let cache = match (entry >> ATTR_INDEX_SHIFT) & 0b111 {
            ATTR_WRITE_BACK => Cache::WriteBack,
            ATTR_NON_CACHEABLE => Cache::WriteCombining,
            _ => Cache::Device,
        };
        Attrs {
            write: entry & AP_RO == 0,
            exec: entry & UXN == 0,
            user: true,
            global: false,
            cache,
        }
    }

    fn is_canonical(va: VirtAddr) -> bool {
        va.get() >> INPUT_BITS == 0
    }
}

/// The shareability a leaf carries (SH, bits 9:8): 0b11 inner, 0b10 outer.
#[must_use]
pub const fn shareability(entry: u64) -> u8 {
    // Two bits: the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, reason = "a two-bit field")]
    let sh = ((entry & SH_MASK) >> 8) as u8;
    sh
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;
    use crate::tests::VecMemory;
    use crate::{MapError, PageTable, Translation, UnmapError};

    const ALL: [Attrs; 6] = [
        dma(true, Cache::WriteBack),
        dma(false, Cache::WriteBack),
        dma(true, Cache::WriteCombining),
        dma(false, Cache::WriteCombining),
        dma(true, Cache::Device),
        dma(false, Cache::Device),
    ];

    #[test]
    fn leaf_bits() {
        // A coherent read-write page, bit by bit against IHI 0070 / Arm ARM
        // D8.3: valid|page, AttrIndx 0, AP 0b01, SH 0b11, AF, nG, PXN, UXN.
        let e = Smmu::leaf(
            PhysAddr::new(0x8_1234_5000),
            PageSize::Size4K,
            dma(true, Cache::WriteBack),
        );
        assert_eq!(
            e,
            0x8_1234_5000 | 0b11 | (1 << 6) | (0b11 << 8) | (1 << 10) | (1 << 11) | (3 << 53)
        );
        assert!(is_page(e) && access_flag(e) && unprivileged(e) && !read_only(e));
        assert_eq!(shareability(e), 0b11);

        // Read-only, non-cacheable outer-shareable.
        let e = Smmu::leaf(
            PhysAddr::new(0x1000),
            PageSize::Size4K,
            dma(false, Cache::WriteCombining),
        );
        assert_eq!((e >> 2) & 0b111, 2);
        assert!(read_only(e));
        assert_eq!(shareability(e), 0b10);

        // The doorbell: Device-nGnRE.
        let e = Smmu::leaf(
            PhysAddr::new(0xe06_0000),
            PageSize::Size4K,
            dma(true, Cache::Device),
        );
        assert_eq!((e >> 2) & 0b111, 1);

        // A block has bit 1 clear and is not a page.
        let e = Smmu::leaf(PhysAddr::new(1 << 30), PageSize::Size1G, ALL[0]);
        assert_eq!(e & 0b11, 0b01);
        assert!(!is_page(e));

        // user/global in the request are ignored.
        let odd = Attrs {
            user: false,
            global: true,
            ..dma(true, Cache::WriteBack)
        };
        assert_eq!(
            Smmu::leaf(PhysAddr::new(0x1000), PageSize::Size4K, odd),
            Smmu::leaf(PhysAddr::new(0x1000), PageSize::Size4K, ALL[0])
        );
    }

    #[test]
    fn attrs_round_trip_at_every_size() {
        for a in ALL {
            for size in [PageSize::Size4K, PageSize::Size2M, PageSize::Size1G] {
                let pa = PhysAddr::new(5 * size.bytes());
                let e = Smmu::leaf(pa, size, a);
                assert_eq!(Smmu::attrs(e, size.level()), a);
                assert_eq!(Smmu::address(e, size.level()), pa);
                assert!(!Smmu::is_table(e, size.level()) || size == PageSize::Size4K);
            }
        }
        let t = Smmu::table(PhysAddr::new(0x7000));
        for l in 2..=4 {
            assert!(Smmu::is_table(t, Level(l)));
            assert_eq!(Smmu::address(t, Level(l)), PhysAddr::new(0x7000));
        }
    }

    #[test]
    fn canonical_is_ttb0() {
        assert!(Smmu::is_canonical(VirtAddr::ZERO));
        assert!(Smmu::is_canonical(VirtAddr::new((1 << 48) - 1)));
        assert!(!Smmu::is_canonical(VirtAddr::new(1 << 48)));
        assert!(!Smmu::is_canonical(VirtAddr::new(0xffff_8000_0000_0000)));
    }

    /// Map, walk, and unmap at every level and leaf size.
    #[test]
    fn map_walk_unmap_every_level() {
        let mut mem = VecMemory::default();
        let mut pt = PageTable::<Smmu>::new(&mut mem, PageSize::Size1G).unwrap();
        // A 4 KiB head, 2 MiB blocks, a 1 GiB block, and a 4 KiB tail, in
        // one call: va and pa congruent modulo 1 GiB.
        let va = VirtAddr::new((1 << 30) - 0x20_1000);
        let pa = PhysAddr::new(0x40_0000_0000 + (1 << 30) - 0x20_1000);
        let len = 0x20_1000 + (1 << 30) + 0x3000;
        let a = dma(true, Cache::WriteBack);
        pt.map(&mut mem, va, pa, len, a).unwrap();
        let expect = [
            (0, PageSize::Size4K),
            (0x1000, PageSize::Size2M),
            (0x20_0fff, PageSize::Size2M),
            (0x20_1000, PageSize::Size1G),
            (0x20_1000 + (1 << 30) - 1, PageSize::Size1G),
            (0x20_1000 + (1 << 30), PageSize::Size4K),
            (len - 1, PageSize::Size4K),
        ];
        for (off, size) in expect {
            let t = pt.translate(&mut mem, va + off).unwrap();
            assert_eq!(
                t,
                Translation {
                    pa: pa + off,
                    size,
                    attrs: a
                },
                "offset {off:#x}"
            );
        }
        assert_eq!(pt.translate(&mut mem, va + len), None);
        assert_eq!(pt.translate(&mut mem, va - 0x1000), None);
        assert_eq!(pt.resident(&mut mem, va, len), len / 4096);

        // The top of the 48-bit space, through a fresh root entry.
        let top = VirtAddr::new((1 << 48) - 0x1000);
        pt.map(&mut mem, top, PhysAddr::new(0x9000), 0x1000, ALL[1])
            .unwrap();
        assert_eq!(
            pt.translate(&mut mem, top + 0xabc).unwrap().pa,
            PhysAddr::new(0x9abc)
        );
        assert!(!pt.translate(&mut mem, top).unwrap().attrs.write);
        // `map` refuses one range that crosses bit 47 (the CPU formats'
        // half boundary); one page each side is two calls.
        let mid = VirtAddr::new((1 << 47) - 0x1000);
        assert_eq!(
            pt.map(&mut mem, mid, PhysAddr::ZERO, 0x2000, ALL[0]),
            Err(MapError::BadAddress(mid))
        );
        pt.map(&mut mem, mid, PhysAddr::ZERO, 0x1000, ALL[0])
            .unwrap();
        pt.map(&mut mem, mid + 0x1000, PhysAddr::ZERO, 0x1000, ALL[0])
            .unwrap();
        assert_eq!(
            pt.map(
                &mut mem,
                VirtAddr::new(1 << 48),
                PhysAddr::ZERO,
                0x1000,
                ALL[0]
            ),
            Err(MapError::BadAddress(VirtAddr::new(1 << 48)))
        );

        // Unmapping 4 KiB pages hands back each frame; blocks refuse.
        let mut freed = Vec::new();
        pt.unmap(&mut mem, va, 0x1000, |p| freed.push(p)).unwrap();
        assert_eq!(freed, [pa]);
        assert_eq!(
            pt.unmap(&mut mem, va + 0x1000, 0x1000, |_| {}),
            Err(UnmapError::LargePage(va + 0x1000))
        );
        pt.clear(&mut mem, va, len, |_, _| {});
        // The blocks are left by clear (4 KiB leaves only); the tail went.
        assert!(pt.translate(&mut mem, va + 0x1000).is_some());
        assert_eq!(pt.translate(&mut mem, va + len - 1), None);
    }

    /// Randomized 4 KiB map/unmap against a model (a map from page to
    /// frame), with a fixed seed.
    #[test]
    fn random_pages_against_model() {
        let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut mem = VecMemory::default();
        let mut pt = PageTable::<Smmu>::new(&mut mem, PageSize::Size4K).unwrap();
        let mut model = std::collections::BTreeMap::new();
        for _ in 0..4000 {
            // Clustered addresses so tables are shared and levels reused.
            let page = (next() % 4096) | ((next() % 4) << 27);
            let va = VirtAddr::new(page << 12);
            if next() % 3 == 0 {
                let r = pt.unmap(&mut mem, va, 0x1000, |_| {});
                match model.remove(&page) {
                    Some(_) => assert_eq!(r, Ok(())),
                    None => assert_eq!(r, Err(UnmapError::NotMapped(va))),
                }
            } else {
                let frame = (next() % (1 << 28)) << 12;
                let a = ALL[usize::try_from(next() % 6).unwrap()];
                let r = pt.map(&mut mem, va, PhysAddr::new(frame), 0x1000, a);
                if let std::collections::btree_map::Entry::Vacant(v) = model.entry(page) {
                    assert_eq!(r, Ok(()));
                    v.insert((frame, a));
                } else {
                    assert_eq!(r, Err(MapError::AlreadyMapped(va)));
                }
            }
        }
        for (&page, &(frame, a)) in &model {
            let t = pt.translate(&mut mem, VirtAddr::new(page << 12)).unwrap();
            assert_eq!((t.pa.get(), t.attrs), (frame, a));
        }
        let total = pt.resident(&mut mem, VirtAddr::ZERO, 1 << 48);
        assert_eq!(total, u64::try_from(model.len()).unwrap());
    }
}
