// SPDX-License-Identifier: BSD-2-Clause
//! A GPU virtual address space (specification 05 section 3): four levels of
//! 4 KiB tables of 512 eight-byte entries, 48-bit addresses, 4 KiB pages.
//!
//! This builds the tables and walks them; where a table lives (video or
//! system memory) and how its words are written is the [`Tables`] the
//! caller gives. It does not program the hub (that is `gmc`).

use alloc::vec::Vec;

/// Entries in a table.
pub const ENTRIES: usize = 512;
/// A page.
pub const PAGE: u64 = 4096;
/// The address space: 48 bits.
pub const VA_BITS: u32 = 48;

/// Entry bits (05 section 3.2).
pub mod bit {
    /// The entry is valid.
    pub const VALID: u64 = 1 << 0;
    /// The address is system memory (a bus address).
    pub const SYSTEM: u64 = 1 << 1;
    /// Access to system memory snoops the processor's caches.
    pub const SNOOPED: u64 = 1 << 2;
    /// Shader instruction fetch allowed.
    pub const EXECUTABLE: u64 = 1 << 4;
    /// Readable.
    pub const READABLE: u64 = 1 << 5;
    /// Writeable.
    pub const WRITEABLE: u64 = 1 << 6;
    /// Partially resident: an unmapped access reads zero.
    pub const PRT: u64 = 1 << 51;
    /// In a level-0 directory entry: maps 2 MiB directly, no page table.
    pub const PDE_IS_PTE: u64 = 1 << 54;
}

/// The address bits of an entry: 47:12.
const ADDRESS: u64 = 0x0000_ffff_ffff_f000;

/// Memory type (bits 50:48).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum MemoryType {
    /// Non-coherent, cached: the default.
    Nc = 0,
    /// Write-combined.
    Wc = 1,
    /// Cache-coherent.
    Cc = 2,
    /// Uncached.
    Uc = 3,
}

/// What a mapping allows and where its pages are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flags {
    pub readable: bool,
    pub writeable: bool,
    pub executable: bool,
    /// The pages are in system memory (bus addresses).
    pub system: bool,
    /// Snooped (system memory only).
    pub snooped: bool,
    pub memory_type: MemoryType,
}

impl Flags {
    /// Readable and writeable video memory, not executable.
    pub const VRAM_RW: Self = Self {
        readable: true,
        writeable: true,
        executable: false,
        system: false,
        snooped: false,
        memory_type: MemoryType::Nc,
    };
    /// The same in system memory, snooped.
    pub const SYSTEM_RW: Self = Self {
        readable: true,
        writeable: true,
        executable: false,
        system: true,
        snooped: true,
        memory_type: MemoryType::Nc,
    };

    /// The entry's low bits and memory type, with `VALID`.
    #[must_use]
    pub fn bits(self) -> u64 {
        let mut b = bit::VALID | ((self.memory_type as u64) << 48);
        if self.readable {
            b |= bit::READABLE;
        }
        if self.writeable {
            b |= bit::WRITEABLE;
        }
        if self.executable {
            b |= bit::EXECUTABLE;
        }
        if self.system {
            b |= bit::SYSTEM;
        }
        if self.snooped {
            b |= bit::SNOOPED;
        }
        b
    }
}

/// A page-table entry for the page at `address` (4 KiB aligned; for video
/// memory the physical address, 05 section 2.3).
#[must_use]
pub fn pte(address: u64, flags: Flags) -> u64 {
    (address & ADDRESS) | flags.bits()
}

/// A directory entry pointing at the table at `address`: valid, and system
/// if the table is in system memory.
#[must_use]
pub fn pde(address: u64, system: bool) -> u64 {
    (address & ADDRESS) | bit::VALID | if system { bit::SYSTEM } else { 0 }
}

/// Where the tables are kept: allocation and word access.
pub trait Tables {
    /// A new zeroed 4 KiB table, or `None` if there is no memory. Returns
    /// its handle.
    fn alloc(&mut self) -> Option<usize>;
    /// The address to put in a directory entry that points at table
    /// `handle`, and whether that table is in system memory.
    fn address(&self, handle: usize) -> (u64, bool);
    /// Writes entry `index` of table `handle`.
    fn write(&mut self, handle: usize, index: usize, entry: u64);
    /// Reads it.
    fn read(&self, handle: usize, index: usize) -> u64;
    /// The handle of the table a directory entry points at, if the entry
    /// was made by [`Tables::address`] (for walking back).
    fn table_at(&self, entry: u64) -> Option<usize>;
}

/// Why a mapping failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No memory for a table.
    NoMemory,
    /// An address or length outside the 48-bit space, or not page aligned.
    Range,
    /// A page of the range is already mapped.
    Busy,
}

/// One address space.
#[derive(Debug)]
pub struct Space<T: Tables> {
    tables: T,
    root: usize,
    /// The tables made, to free them.
    made: Vec<usize>,
}

fn index(va: u64, level: u32) -> usize {
    // Level 3 is the root (bits 47:39); level 0 the page table (20:12).
    ((va >> (12 + 9 * level)) & 0x1ff) as usize
}

impl<T: Tables> Space<T> {
    /// An empty space: the root table only.
    ///
    /// # Errors
    ///
    /// [`Error::NoMemory`] if the root cannot be made.
    pub fn new(mut tables: T) -> Result<Self, Error> {
        let root = tables.alloc().ok_or(Error::NoMemory)?;
        Ok(Self {
            tables,
            root,
            made: alloc::vec![root],
        })
    }

    /// The root table's handle (its address goes in the context's
    /// page-table base register).
    #[must_use]
    pub fn root(&self) -> usize {
        self.root
    }

    /// The table memory.
    pub fn tables(&self) -> &T {
        &self.tables
    }

    /// The handles of every table of the space.
    #[must_use]
    pub fn made(&self) -> &[usize] {
        &self.made
    }

    /// The page table (level 0) that holds `va`'s entry, made if `create`.
    fn leaf(&mut self, va: u64, create: bool) -> Result<Option<usize>, Error> {
        let mut table = self.root;
        for level in (1..4).rev() {
            let i = index(va, level);
            let e = self.tables.read(table, i);
            table = if e & bit::VALID != 0 {
                self.tables.table_at(e).ok_or(Error::Range)?
            } else if create {
                let t = self.tables.alloc().ok_or(Error::NoMemory)?;
                self.made.push(t);
                let (addr, system) = self.tables.address(t);
                self.tables.write(table, i, pde(addr, system));
                t
            } else {
                return Ok(None);
            };
        }
        Ok(Some(table))
    }

    /// Maps `pages` pages from `va`; page `n`'s address is `addr(n)`.
    ///
    /// # Errors
    ///
    /// [`Error::Range`] for an unaligned or out-of-space range,
    /// [`Error::Busy`] if a page is mapped already, [`Error::NoMemory`].
    /// A failure leaves the pages mapped so far; the caller unmaps them.
    pub fn map(
        &mut self,
        va: u64,
        pages: u64,
        flags: Flags,
        addr: impl Fn(u64) -> u64,
    ) -> Result<(), Error> {
        let end = pages
            .checked_mul(PAGE)
            .and_then(|n| va.checked_add(n))
            .ok_or(Error::Range)?;
        if !va.is_multiple_of(PAGE) || end > 1 << VA_BITS || pages == 0 {
            return Err(Error::Range);
        }
        for n in 0..pages {
            let v = va + n * PAGE;
            let leaf = self.leaf(v, true)?.ok_or(Error::NoMemory)?;
            let i = index(v, 0);
            if self.tables.read(leaf, i) & bit::VALID != 0 {
                return Err(Error::Busy);
            }
            self.tables.write(leaf, i, pte(addr(n), flags));
        }
        Ok(())
    }

    /// Clears `pages` pages from `va` (pages that are not mapped are left).
    ///
    /// # Errors
    ///
    /// [`Error::Range`] for an unaligned or out-of-space range.
    pub fn unmap(&mut self, va: u64, pages: u64) -> Result<(), Error> {
        let end = pages
            .checked_mul(PAGE)
            .and_then(|n| va.checked_add(n))
            .ok_or(Error::Range)?;
        if !va.is_multiple_of(PAGE) || end > 1 << VA_BITS {
            return Err(Error::Range);
        }
        for n in 0..pages {
            let v = va + n * PAGE;
            if let Some(leaf) = self.leaf(v, false)? {
                self.tables.write(leaf, index(v, 0), 0);
            }
        }
        Ok(())
    }

    /// The entry that maps `va`, if any: a walk as the hardware does.
    #[must_use]
    pub fn translate(&mut self, va: u64) -> Option<u64> {
        let leaf = self.leaf(va, false).ok()??;
        let e = self.tables.read(leaf, index(va, 0));
        (e & bit::VALID != 0).then_some(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tables in a map; table `n` is at "physical" `0x1000_0000 + n * 4096`,
    /// in system memory or not.
    struct Model {
        tables: Vec<[u64; ENTRIES]>,
        system: bool,
        limit: usize,
    }

    impl Model {
        fn new(system: bool) -> Self {
            Self {
                tables: Vec::new(),
                system,
                limit: usize::MAX,
            }
        }
    }

    impl Tables for Model {
        fn alloc(&mut self) -> Option<usize> {
            if self.tables.len() >= self.limit {
                return None;
            }
            self.tables.push([0; ENTRIES]);
            Some(self.tables.len() - 1)
        }
        fn address(&self, h: usize) -> (u64, bool) {
            (0x1000_0000 + h as u64 * PAGE, self.system)
        }
        fn write(&mut self, h: usize, i: usize, e: u64) {
            self.tables[h][i] = e;
        }
        fn read(&self, h: usize, i: usize) -> u64 {
            self.tables[h][i]
        }
        fn table_at(&self, entry: u64) -> Option<usize> {
            let a = entry & ADDRESS;
            let h = usize::try_from((a - 0x1000_0000) / PAGE).ok()?;
            (h < self.tables.len()).then_some(h)
        }
    }

    #[test]
    fn the_specifications_pte_vectors() {
        // A readable, writeable, non-executable video page: flags 0x61.
        assert_eq!(pte(0, Flags::VRAM_RW), 0x61);
        assert_eq!(pte(0x1_8400_1000, Flags::VRAM_RW), 0x1_8400_1061);
        // The same page in system memory, snooped: 0x67.
        assert_eq!(pte(0, Flags::SYSTEM_RW), 0x67);
        let x = Flags {
            executable: true,
            memory_type: MemoryType::Uc,
            ..Flags::VRAM_RW
        };
        assert_eq!(pte(0x2000, x), 0x2000 | 0x71 | (3 << 48));
        // A directory entry: valid, plus system if the table is.
        assert_eq!(pde(0x5000, false), 0x5001);
        assert_eq!(pde(0x5000, true), 0x5003);
    }

    #[test]
    fn a_mapping_is_found_by_the_walk_and_nothing_else_is() {
        let mut s = Space::new(Model::new(false)).unwrap();
        s.map(0x7000_1000, 3, Flags::VRAM_RW, |n| {
            0x18_4000_0000 + n * PAGE
        })
        .unwrap();
        assert_eq!(
            s.translate(0x7000_1000),
            Some(pte(0x18_4000_0000, Flags::VRAM_RW))
        );
        assert_eq!(
            s.translate(0x7000_3000),
            Some(pte(0x18_4000_2000, Flags::VRAM_RW))
        );
        assert_eq!(s.translate(0x7000_4000), None);
        assert_eq!(s.translate(0x7000_0000), None);
        // Four tables: root, PDB1, PDB0, PTB.
        assert_eq!(s.made().len(), 4);
        // A mapping elsewhere in the same 2 MiB shares the page table; one
        // far away makes three more.
        s.map(0x7000_5000, 1, Flags::SYSTEM_RW, |_| 0x9000).unwrap();
        assert_eq!(s.made().len(), 4);
        s.map(0x7fff_ffff_f000, 1, Flags::VRAM_RW, |_| 0x1000)
            .unwrap();
        assert_eq!(s.made().len(), 7);
        assert!(s.translate(0x7fff_ffff_f000).is_some());
    }

    #[test]
    fn the_indexes_are_nine_bits_a_level() {
        let va = (5u64 << 39) | (6 << 30) | (7 << 21) | (8 << 12);
        assert_eq!(
            [index(va, 3), index(va, 2), index(va, 1), index(va, 0)],
            [5, 6, 7, 8]
        );
        let mut s = Space::new(Model::new(true)).unwrap();
        s.map(va, 1, Flags::VRAM_RW, |_| 0xabc000).unwrap();
        // The root's entry 5 points at a system table (the model says so).
        let e = s.tables().read(s.root(), 5);
        assert_eq!(e & 3, 3);
        assert_eq!(s.translate(va), Some(pte(0xabc000, Flags::VRAM_RW)));
    }

    #[test]
    fn unmapping_clears_and_a_double_map_is_refused() {
        let mut s = Space::new(Model::new(false)).unwrap();
        s.map(0x20_0000, 4, Flags::VRAM_RW, |n| n * PAGE).unwrap();
        assert_eq!(s.map(0x20_2000, 1, Flags::VRAM_RW, |_| 0), Err(Error::Busy));
        s.unmap(0x20_1000, 2).unwrap();
        assert!(s.translate(0x20_0000).is_some());
        assert_eq!(s.translate(0x20_1000), None);
        assert_eq!(s.translate(0x20_2000), None);
        assert!(s.translate(0x20_3000).is_some());
        // Unmapping what was never mapped is quietly fine.
        s.unmap(0x4000_0000_0000, 10).unwrap();
        s.map(0x20_1000, 1, Flags::VRAM_RW, |_| 0x77000).unwrap();
    }

    #[test]
    fn bad_ranges_and_exhausted_memory_are_errors() {
        let mut s = Space::new(Model::new(false)).unwrap();
        assert_eq!(s.map(0x1001, 1, Flags::VRAM_RW, |_| 0), Err(Error::Range));
        assert_eq!(s.map(0x1000, 0, Flags::VRAM_RW, |_| 0), Err(Error::Range));
        assert_eq!(
            s.map((1 << 48) - PAGE, 2, Flags::VRAM_RW, |_| 0),
            Err(Error::Range)
        );
        assert_eq!(
            s.map(u64::MAX - 4095, 2, Flags::VRAM_RW, |_| 0),
            Err(Error::Range)
        );
        // Room for the root and one more table only.
        let mut m = Model::new(false);
        m.limit = 2;
        let mut s = Space::new(m).unwrap();
        assert_eq!(
            s.map(0x1000, 1, Flags::VRAM_RW, |_| 0),
            Err(Error::NoMemory)
        );
    }
}
