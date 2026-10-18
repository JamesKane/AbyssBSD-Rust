// SPDX-License-Identifier: BSD-2-Clause
//! Four-level page tables with 4 KiB granules, for x86_64 and aarch64.
//!
//! The walk is the same on both architectures (9 index bits per level,
//! 4 KiB / 2 MiB / 1 GiB leaves); only the entry encodings differ, and those
//! live behind [`Format`]. [`PageTable`] builds and queries a table through
//! a [`Memory`] implementation that owns the frames, so the same code serves
//! the loader (frames from UEFI, identity mapped), the kernel (frames from
//! the physical allocator, reached through the DMAP), and host tests
//! (frames in a `Vec`).
//!
//! This crate only builds tables. Loading them into the MMU, and TLB
//! maintenance, belong to the architecture code.

#![no_std]
#![forbid(unsafe_code)]

pub mod aarch64;
pub mod x86_64;

use core::marker::PhantomData;

pub use addr::{PhysAddr, VirtAddr};

/// Entries per table.
pub const ENTRIES: usize = 512;

/// One page-table frame.
pub type Frame = [u64; ENTRIES];

/// Leaf sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PageSize {
    /// 4 KiB, a level-1 leaf (x86 PTE, Arm level 3 page).
    Size4K,
    /// 2 MiB, a level-2 leaf (x86 PDE with PS, Arm level 2 block).
    Size2M,
    /// 1 GiB, a level-3 leaf (x86 PDPTE with PS, Arm level 1 block).
    Size1G,
}

impl PageSize {
    /// Size in bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        match self {
            Self::Size4K => 1 << 12,
            Self::Size2M => 1 << 21,
            Self::Size1G => 1 << 30,
        }
    }

    /// The table level a leaf of this size lives in (1 = last level).
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            Self::Size4K => Level(1),
            Self::Size2M => Level(2),
            Self::Size1G => Level(3),
        }
    }

    const fn at_level(level: Level) -> Option<Self> {
        match level.0 {
            1 => Some(Self::Size4K),
            2 => Some(Self::Size2M),
            3 => Some(Self::Size1G),
            _ => None,
        }
    }
}

/// A table level, counted from the leaves: 1 is the last level, 4 the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(u8);

impl Level {
    /// The root level.
    pub const ROOT: Self = Self(4);

    /// The level number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The table index `va` selects at this level.
    #[must_use]
    pub const fn index(self, va: VirtAddr) -> usize {
        let shift = 12 + 9 * (self.0 as u32 - 1);
        ((va.get() >> shift) & 0x1ff) as usize
    }
}

/// Memory type of a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cache {
    /// Normal memory, write-back cacheable.
    WriteBack,
    /// Normal memory, uncached but write-combining (framebuffers).
    WriteCombining,
    /// Device registers: uncached, no speculation, no combining.
    Device,
}

/// Access rights and memory type of a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrs {
    /// Writable.
    pub write: bool,
    /// Executable (at the privilege level that owns it).
    pub exec: bool,
    /// Accessible from user mode.
    pub user: bool,
    /// Shared by every address space (not tagged with an ASID/PCID).
    pub global: bool,
    /// Memory type.
    pub cache: Cache,
}

impl Attrs {
    /// Kernel code.
    pub const KERNEL_RX: Self = Self::kernel(false, true, Cache::WriteBack);
    /// Kernel read-only data.
    pub const KERNEL_RO: Self = Self::kernel(false, false, Cache::WriteBack);
    /// Kernel data.
    pub const KERNEL_RW: Self = Self::kernel(true, false, Cache::WriteBack);
    /// Device registers.
    pub const DEVICE: Self = Self::kernel(true, false, Cache::Device);
    /// A framebuffer.
    pub const FRAMEBUFFER: Self = Self::kernel(true, false, Cache::WriteCombining);
    /// The loader's temporary identity map. The only W+X mapping, gone once
    /// the kernel installs its own tables.
    pub const IDENTITY: Self = Self {
        write: true,
        exec: true,
        user: false,
        global: false,
        cache: Cache::WriteBack,
    };

    const fn kernel(write: bool, exec: bool, cache: Cache) -> Self {
        Self {
            write,
            exec,
            user: false,
            global: true,
            cache,
        }
    }
}

/// An architecture's page-table entry encoding.
pub trait Format {
    /// A pointer to a next-level table.
    fn table(next: PhysAddr) -> u64;
    /// A leaf mapping `pa` with `attrs`.
    fn leaf(pa: PhysAddr, size: PageSize, attrs: Attrs) -> u64;
    /// Whether the entry is valid at all.
    fn is_valid(entry: u64) -> bool;
    /// Whether a valid entry at `level` (above 1) points to a table rather
    /// than being a leaf.
    fn is_table(entry: u64, level: Level) -> bool;
    /// The address a valid entry points to (table or leaf base).
    fn address(entry: u64, level: Level) -> PhysAddr;
    /// Decodes a valid leaf's attributes.
    fn attrs(entry: u64, level: Level) -> Attrs;
    /// Whether `va` is a well-formed address for this architecture.
    fn is_canonical(va: VirtAddr) -> bool;
}

/// Frames that page tables live in.
pub trait Memory {
    /// Allocates a zeroed frame, or `None` when out of memory.
    fn alloc_frame(&mut self) -> Option<PhysAddr>;
    /// The frame at `pa`, which an earlier [`Memory::alloc_frame`] returned.
    fn frame(&mut self, pa: PhysAddr) -> &mut Frame;
}

/// Why a mapping failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Addresses or length are not 4 KiB aligned.
    Misaligned,
    /// The virtual range is not canonical or wraps.
    BadAddress(VirtAddr),
    /// Part of the range is already mapped.
    AlreadyMapped(VirtAddr),
    /// No frame for an intermediate table.
    OutOfMemory,
}

/// A page-table hierarchy rooted at one top-level frame.
#[derive(Debug)]
pub struct PageTable<F: Format> {
    root: PhysAddr,
    max_leaf: PageSize,
    _format: PhantomData<F>,
}

/// A successful lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translation {
    /// The physical address `va` maps to.
    pub pa: PhysAddr,
    /// Size of the leaf that maps it.
    pub size: PageSize,
    /// The leaf's attributes.
    pub attrs: Attrs,
}

impl<F: Format> PageTable<F> {
    /// Allocates an empty root. Mappings use leaves up to `max_leaf`.
    pub fn new(mem: &mut impl Memory, max_leaf: PageSize) -> Result<Self, MapError> {
        let root = mem.alloc_frame().ok_or(MapError::OutOfMemory)?;
        Ok(Self {
            root,
            max_leaf,
            _format: PhantomData,
        })
    }

    /// A handle on an existing hierarchy rooted at `root`, for lookups and
    /// further mappings. The leaf limit is the 4 KiB minimum.
    #[must_use]
    pub const fn from_root(root: PhysAddr) -> Self {
        Self {
            root,
            max_leaf: PageSize::Size4K,
            _format: PhantomData,
        }
    }

    /// The root frame, for loading into the MMU.
    #[must_use]
    pub const fn root(&self) -> PhysAddr {
        self.root
    }

    /// Maps `[va, va + len)` to `[pa, pa + len)`, using the largest leaves
    /// that alignment and `max_leaf` allow.
    ///
    /// On error, mappings made before the failure stay in place.
    pub fn map(
        &mut self,
        mem: &mut impl Memory,
        va: VirtAddr,
        pa: PhysAddr,
        len: u64,
        attrs: Attrs,
    ) -> Result<(), MapError> {
        let small = PageSize::Size4K.bytes();
        if !va.is_aligned(small) || !pa.is_aligned(small) || !len.is_multiple_of(small) {
            return Err(MapError::Misaligned);
        }
        if len == 0 {
            return Ok(());
        }
        let last = va.checked_add(len - 1).ok_or(MapError::BadAddress(va))?;
        if !F::is_canonical(va) || !F::is_canonical(last) || (va.get() ^ last.get()) >> 47 != 0 {
            return Err(MapError::BadAddress(va));
        }
        let mut done = 0;
        while done < len {
            let (v, p) = (va + done, pa + done);
            let size = self.leaf_size(v, p, len - done);
            self.map_one(mem, v, p, size, attrs)?;
            done += size.bytes();
        }
        Ok(())
    }

    fn leaf_size(&self, va: VirtAddr, pa: PhysAddr, left: u64) -> PageSize {
        [PageSize::Size1G, PageSize::Size2M]
            .into_iter()
            .find(|s| {
                *s <= self.max_leaf
                    && va.is_aligned(s.bytes())
                    && pa.is_aligned(s.bytes())
                    && left >= s.bytes()
            })
            .unwrap_or(PageSize::Size4K)
    }

    fn map_one(
        &mut self,
        mem: &mut impl Memory,
        va: VirtAddr,
        pa: PhysAddr,
        size: PageSize,
        attrs: Attrs,
    ) -> Result<(), MapError> {
        let mut table = self.root;
        let mut level = Level::ROOT;
        while level != size.level() {
            let i = level.index(va);
            let e = mem.frame(table)[i];
            table = if !F::is_valid(e) {
                let next = mem.alloc_frame().ok_or(MapError::OutOfMemory)?;
                mem.frame(table)[i] = F::table(next);
                next
            } else if F::is_table(e, level) {
                F::address(e, level)
            } else {
                return Err(MapError::AlreadyMapped(va));
            };
            level = Level(level.0 - 1);
        }
        let i = level.index(va);
        let slot = &mut mem.frame(table)[i];
        if F::is_valid(*slot) {
            return Err(MapError::AlreadyMapped(va));
        }
        *slot = F::leaf(pa, size, attrs);
        Ok(())
    }

    /// Looks up the mapping for `va`.
    pub fn translate(&self, mem: &mut impl Memory, va: VirtAddr) -> Option<Translation> {
        let mut table = self.root;
        let mut level = Level::ROOT;
        loop {
            let e = mem.frame(table)[level.index(va)];
            if !F::is_valid(e) {
                return None;
            }
            if level.0 > 1 && F::is_table(e, level) {
                table = F::address(e, level);
                level = Level(level.0 - 1);
                continue;
            }
            let size = PageSize::at_level(level)?;
            let offset = va.get() & (size.bytes() - 1);
            return Some(Translation {
                pa: F::address(e, level) + offset,
                size,
                attrs: F::attrs(e, level),
            });
        }
    }

    /// Counts the frames the hierarchy uses, root included.
    pub fn frames_used(&self, mem: &mut impl Memory) -> usize {
        Self::count(mem, self.root, Level::ROOT)
    }

    fn count(mem: &mut impl Memory, table: PhysAddr, level: Level) -> usize {
        let entries = *mem.frame(table);
        1 + entries
            .iter()
            .filter(|e| level.0 > 1 && F::is_valid(**e) && F::is_table(**e, level))
            .map(|e| Self::count(mem, F::address(*e, level), Level(level.0 - 1)))
            .sum::<usize>()
    }
}

/// Sign-extension check shared by both 48-bit formats.
const fn is_canonical_48(va: VirtAddr) -> bool {
    let top = va.get() >> 47;
    top == 0 || top == 0x1_ffff
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::boxed::Box;
    use std::vec::Vec;

    use super::*;

    /// Frames in a vector; "physical" addresses are `(index + 1) * 4096` so
    /// that zero is never a valid frame.
    #[derive(Default)]
    pub(crate) struct VecMemory {
        frames: Vec<Box<Frame>>,
        pub(crate) limit: Option<usize>,
    }

    impl Memory for VecMemory {
        fn alloc_frame(&mut self) -> Option<PhysAddr> {
            if self.limit.is_some_and(|l| self.frames.len() >= l) {
                return None;
            }
            self.frames.push(Box::new([0; ENTRIES]));
            Some(PhysAddr::new(self.frames.len() as u64 * 4096))
        }

        fn frame(&mut self, pa: PhysAddr) -> &mut Frame {
            &mut self.frames[usize::try_from(pa.get() / 4096 - 1).unwrap()]
        }
    }

    fn exercise<F: Format>() {
        let mut mem = VecMemory::default();
        let mut pt = PageTable::<F>::new(&mut mem, PageSize::Size1G).unwrap();

        // 4 KiB head, 2 MiB middle, 4 KiB tail.
        let va = VirtAddr::new(0xffff_ffff_801f_f000);
        let pa = PhysAddr::new(0x41ff_f000);
        let len = 0x1000 + (2 << 20) + 0x3000;
        pt.map(&mut mem, va, pa, len, Attrs::KERNEL_RX).unwrap();
        let t = pt.translate(&mut mem, va + 0x123).unwrap();
        assert_eq!(t.pa, pa + 0x123);
        assert_eq!(t.size, PageSize::Size4K);
        assert_eq!(t.attrs, Attrs::KERNEL_RX);
        let t = pt.translate(&mut mem, va + 0x1000 + 0x1_2345).unwrap();
        assert_eq!(t.size, PageSize::Size2M);
        assert_eq!(t.pa, pa + 0x1000 + 0x1_2345);
        assert_eq!(
            pt.translate(&mut mem, va + len - 1).unwrap().pa,
            pa + len - 1
        );
        assert_eq!(pt.translate(&mut mem, va + len), None);

        // 1 GiB leaves for an aligned DMAP-style range.
        let dmap = VirtAddr::new(0xffff_8000_0000_0000);
        pt.map(&mut mem, dmap, PhysAddr::ZERO, 4 << 30, Attrs::KERNEL_RW)
            .unwrap();
        let t = pt.translate(&mut mem, dmap + (3 << 30) + 5).unwrap();
        assert_eq!(t.size, PageSize::Size1G);
        assert_eq!(t.pa, PhysAddr::new((3 << 30) + 5));
        assert_eq!(t.attrs, Attrs::KERNEL_RW);

        // Every memory type and permission round-trips.
        for (i, attrs) in [
            Attrs::KERNEL_RO,
            Attrs::DEVICE,
            Attrs::FRAMEBUFFER,
            Attrs::IDENTITY,
            Attrs {
                write: true,
                exec: false,
                user: true,
                global: false,
                cache: Cache::WriteBack,
            },
            Attrs {
                write: false,
                exec: true,
                user: true,
                global: false,
                cache: Cache::WriteBack,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let v = VirtAddr::new(0x10_0000 + i as u64 * 0x1000);
            pt.map(&mut mem, v, PhysAddr::new(0x5000), 0x1000, attrs)
                .unwrap();
            assert_eq!(pt.translate(&mut mem, v).unwrap().attrs, attrs);
        }

        // Overlaps are refused, at a leaf and under a large leaf.
        assert_eq!(
            pt.map(&mut mem, va, pa, 0x1000, Attrs::KERNEL_RO),
            Err(MapError::AlreadyMapped(va))
        );
        assert!(matches!(
            pt.map(&mut mem, dmap + 0x1000, pa, 0x1000, Attrs::KERNEL_RO),
            Err(MapError::AlreadyMapped(_))
        ));
        // Bad input.
        assert_eq!(
            pt.map(&mut mem, va + 1, pa, 0x1000, Attrs::KERNEL_RO),
            Err(MapError::Misaligned)
        );
        let hole = VirtAddr::new(0x0000_8000_0000_0000);
        assert_eq!(
            pt.map(&mut mem, hole, pa, 0x1000, Attrs::KERNEL_RO),
            Err(MapError::BadAddress(hole))
        );
        let below = VirtAddr::new(0x0000_7fff_ffff_f000);
        assert_eq!(
            pt.map(&mut mem, below, pa, 0x2000, Attrs::KERNEL_RO),
            Err(MapError::BadAddress(below))
        );
    }

    #[test]
    fn x86_64_tables() {
        exercise::<x86_64::X86_64>();
    }

    #[test]
    fn aarch64_tables() {
        exercise::<aarch64::Aarch64>();
    }

    #[test]
    fn max_leaf_is_respected() {
        let mut mem = VecMemory::default();
        let mut pt = PageTable::<x86_64::X86_64>::new(&mut mem, PageSize::Size2M).unwrap();
        pt.map(
            &mut mem,
            VirtAddr::new(1 << 30),
            PhysAddr::new(1 << 30),
            1 << 30,
            Attrs::KERNEL_RW,
        )
        .unwrap();
        let t = pt.translate(&mut mem, VirtAddr::new(1 << 30)).unwrap();
        assert_eq!(t.size, PageSize::Size2M);
        // Root, one PDPT, one PD.
        assert_eq!(pt.frames_used(&mut mem), 3);
    }

    #[test]
    fn out_of_memory_is_reported() {
        let mut mem = VecMemory {
            limit: Some(2),
            ..VecMemory::default()
        };
        let mut pt = PageTable::<aarch64::Aarch64>::new(&mut mem, PageSize::Size1G).unwrap();
        assert_eq!(
            pt.map(
                &mut mem,
                VirtAddr::new(0x1000),
                PhysAddr::new(0x1000),
                0x1000,
                Attrs::KERNEL_RW
            ),
            Err(MapError::OutOfMemory)
        );
    }

    #[test]
    fn indices() {
        let va = VirtAddr::new(0xffff_ffff_8020_3000);
        assert_eq!(Level(4).index(va), 511);
        assert_eq!(Level(3).index(va), 510);
        assert_eq!(Level(2).index(va), 1);
        assert_eq!(Level(1).index(va), 3);
    }
}
