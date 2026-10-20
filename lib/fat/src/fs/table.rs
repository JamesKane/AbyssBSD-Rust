// SPDX-License-Identifier: BSD-2-Clause
//! The file allocation table: entries, chains, the in-use map, cluster
//! allocation with FreeBSD's policy, the volume's dirty bit, and FSInfo.
//!
//! Behavior reference: sys/fs/msdosfs/msdosfs_fat.c (`fatentry`,
//! `fatchain`, `clusteralloc1`, `chainlength`, `chainalloc`,
//! `freeclusterchain`, `fillinusemap`, `extendfile`, `markvoldirty`),
//! msdosfs_vfsops.c (`msdosfs_fsiflush`).

use alloc::vec::Vec;

use super::{BlockIo, CLUST_FIRST, Error, Fat, FatType, Fill, Mode};

/// The end-of-chain mark FreeBSD writes (`CLUST_EOFE`, masked).
const EOF: u32 = 0xffff_ffff;

/// Allocation state, which the caller locks (FreeBSD's `pm_inusemap`,
/// `pm_freeclustercount`, `pm_nxtfree`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alloc {
    /// One bit per cluster, set when in use (and for 0, 1, and past the
    /// end).
    inuse: Vec<u32>,
    pub free: u32,
    pub next_free: u32,
    /// FSInfo needs writing.
    pub fsinfo_dirty: bool,
}

impl Alloc {
    fn used(&self, cn: u32) -> bool {
        self.inuse[(cn / 32) as usize] & (1 << (cn % 32)) != 0
    }

    fn mark(&mut self, cn: u32, used: bool) {
        let w = &mut self.inuse[(cn / 32) as usize];
        let bit = 1 << (cn % 32);
        if used {
            *w |= bit;
        } else {
            *w &= !bit;
        }
    }
}

impl<B: BlockIo> Fat<B> {
    /// Where the entry of cluster `cn` lies in a FAT: the block's offset
    /// in the FAT, its size, and the entry's offset in it (FreeBSD's
    /// `fatblock`).
    fn fat_block(&self, cn: u32) -> (u64, usize, usize) {
        let g = &self.geo;
        let ofs = match g.fat_type {
            FatType::Fat12 => u64::from(cn) * 3 / 2,
            FatType::Fat16 => u64::from(cn) * 2,
            FatType::Fat32 => u64::from(cn) * 4,
        };
        let fb = u64::from(g.fat_block);
        let bn = ofs / fb * fb;
        let size = fb.min(g.fat_bytes - bn);
        (
            bn,
            usize::try_from(size).unwrap_or(0),
            usize::try_from(ofs % fb).unwrap_or(0),
        )
    }

    fn decode(&self, b: &[u8], bo: usize, cn: u32) -> u32 {
        match self.geo.fat_type {
            FatType::Fat12 => {
                let v = u32::from(u16::from_le_bytes([b[bo], b[bo + 1]]));
                if cn & 1 != 0 { v >> 4 } else { v & 0xfff }
            }
            FatType::Fat16 => u32::from(u16::from_le_bytes([b[bo], b[bo + 1]])),
            FatType::Fat32 => {
                u32::from_le_bytes([b[bo], b[bo + 1], b[bo + 2], b[bo + 3]]) & 0x0fff_ffff
            }
        }
    }

    /// The FAT entry of cluster `cn`.
    pub fn fat_get(&self, cx: &B::Cx, cn: u32) -> Result<u32, Error> {
        let (bn, size, bo) = self.fat_block(cn);
        let at = self.geo.fat_start + u64::from(self.geo.active) * self.geo.fat_bytes + bn;
        let mut v = 0;
        self.io
            .read(cx, at, size, &mut |b| v = self.decode(b, bo, cn))?;
        Ok(v)
    }

    /// Sets the FAT entry of cluster `cn` to `value`, in every copy if the
    /// FATs mirror, else in the active one (a delayed write).
    pub fn fat_set(&self, cx: &B::Cx, cn: u32, value: u32) -> Result<(), Error> {
        let g = &self.geo;
        let (bn, size, bo) = self.fat_block(cn);
        let copies: Vec<u32> = if g.mirror {
            (0..g.fats).collect()
        } else {
            alloc::vec![g.active]
        };
        let value = value & g.mask();
        for i in copies {
            let at = g.fat_start + u64::from(i) * g.fat_bytes + bn;
            self.io.modify(
                cx,
                at,
                size,
                Fill::Read,
                Mode::Delayed,
                &mut |b| match g.fat_type {
                    FatType::Fat12 => {
                        let mut v = u16::from_le_bytes([b[bo], b[bo + 1]]);
                        let x = u16::try_from(value).unwrap_or(0);
                        v = if cn & 1 != 0 {
                            (v & 0x000f) | (x << 4)
                        } else {
                            (v & 0xf000) | x
                        };
                        b[bo..bo + 2].copy_from_slice(&v.to_le_bytes());
                    }
                    FatType::Fat16 => {
                        b[bo..bo + 2]
                            .copy_from_slice(&u16::try_from(value).unwrap_or(0).to_le_bytes());
                    }
                    FatType::Fat32 => {
                        let old = u32::from_le_bytes([b[bo], b[bo + 1], b[bo + 2], b[bo + 3]]);
                        let v = (old & !0x0fff_ffff) | value;
                        b[bo..bo + 4].copy_from_slice(&v.to_le_bytes());
                    }
                },
            )?;
        }
        Ok(())
    }

    /// Builds the in-use map and free count from the FAT (FreeBSD's
    /// `fillinusemap`).
    pub fn load_alloc(&self, cx: &B::Cx) -> Result<Alloc, Error> {
        let g = &self.geo;
        let words = (g.max_cluster as usize + 1).div_ceil(32) + 1;
        let mut a = Alloc {
            inuse: alloc::vec![u32::MAX; words],
            free: 0,
            next_free: g.next_free_hint,
            fsinfo_dirty: false,
        };
        for cn in CLUST_FIRST..=g.max_cluster {
            if self.fat_get(cx, cn)? == 0 {
                a.mark(cn, false);
                a.free += 1;
            }
        }
        Ok(a)
    }

    /// Free clusters from `start` on, up to `count` (FreeBSD's
    /// `chainlength`).
    fn chain_free_len(&self, a: &Alloc, start: u32, count: u32) -> u32 {
        let max = self.geo.max_cluster;
        let mut n = 0;
        while n < count && start + n <= max && !a.used(start + n) {
            n += 1;
        }
        n
    }

    /// Takes clusters `[start, start + count)`, chaining them and ending
    /// the chain with `fill` (FreeBSD's `chainalloc`).
    fn chain_take(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        start: u32,
        count: u32,
        fill: u32,
    ) -> Result<(u32, u32), Error> {
        for cn in start..start + count {
            a.mark(cn, true);
            a.free -= 1;
        }
        a.next_free = start + count;
        if a.next_free > self.geo.max_cluster {
            a.next_free = CLUST_FIRST;
        }
        a.fsinfo_dirty = true;
        for cn in start..start + count {
            let next = if cn + 1 == start + count {
                fill
            } else {
                cn + 1
            };
            self.fat_set(cx, cn, next)?;
        }
        Ok((start, count))
    }

    /// Allocates up to `count` clusters, at `start` if that many are free
    /// there, else the first run that long from the next-free hint, else
    /// the longest run (FreeBSD's `clusteralloc1`). Returns the first and
    /// how many were taken.
    pub fn cluster_alloc(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        start: u32,
        count: u32,
        fill: u32,
    ) -> Result<(u32, u32), Error> {
        let max = self.geo.max_cluster;
        let len = if start != 0 {
            self.chain_free_len(a, start, count)
        } else {
            0
        };
        if start != 0 && len >= count {
            return self.chain_take(cx, a, start, count, fill);
        }
        let (mut found, mut found_len) = (0, 0);
        let hint = a.next_free.clamp(CLUST_FIRST, max);
        for (from, to) in [(hint, max + 1), (0, hint)] {
            let mut cn = from;
            while cn < to {
                if a.used(cn) {
                    cn += 1;
                    continue;
                }
                let l = self.chain_free_len(a, cn, count);
                if l >= count {
                    return self.chain_take(cx, a, cn, count, fill);
                }
                if l > found_len {
                    found = cn;
                    found_len = l;
                }
                cn += l + 1;
            }
        }
        if found_len == 0 {
            return Err(Error::NoSpace);
        }
        if len != 0 {
            self.chain_take(cx, a, start, len, fill)
        } else {
            self.chain_take(cx, a, found, found_len, fill)
        }
    }

    /// Frees the chain starting at `start` (FreeBSD's
    /// `freeclusterchain`).
    pub fn free_chain(&self, cx: &B::Cx, a: &mut Alloc, start: u32) -> Result<(), Error> {
        let g = &self.geo;
        let mut cn = start;
        while cn >= CLUST_FIRST && cn <= g.max_cluster {
            let next = self.fat_get(cx, cn)?;
            self.fat_set(cx, cn, 0)?;
            self.io.forget(cx, g.cluster_offset(cn), g.cluster as usize);
            if a.used(cn) {
                a.mark(cn, false);
                a.free += 1;
            }
            if g.is_eof(next) || next == 0 {
                break;
            }
            cn = next;
        }
        a.fsinfo_dirty = true;
        Ok(())
    }

    /// Adds `count` clusters to the chain ending at `last` (0: a new
    /// chain), contiguous with it where possible, zeroing them if `clear`
    /// (FreeBSD's `extendfile`). Returns the first new cluster.
    pub fn extend(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        last: u32,
        count: u32,
        clear: bool,
    ) -> Result<u32, Error> {
        if count > a.free {
            return Err(Error::NoSpace);
        }
        let g = &self.geo;
        let mut first = 0;
        let mut last = last;
        let mut left = count;
        while left > 0 {
            let hint = if last != 0 { last + 1 } else { 0 };
            let (cn, got) = self.cluster_alloc(cx, a, hint, left, EOF)?;
            if last != 0 {
                self.fat_set(cx, last, cn)?;
            }
            if first == 0 {
                first = cn;
            }
            if clear {
                for c in cn..cn + got {
                    let len = g.cluster as usize;
                    self.io.modify(
                        cx,
                        g.cluster_offset(c),
                        len,
                        Fill::Zero,
                        Mode::Sync,
                        &mut |_| {},
                    )?;
                }
            }
            last = cn + got - 1;
            left -= got;
        }
        Ok(first)
    }

    /// Cuts the chain starting at `start` to its first `keep` clusters
    /// (0: all of it goes; the caller clears the start cluster).
    pub fn truncate_chain(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        start: u32,
        keep: u64,
    ) -> Result<(), Error> {
        if start < CLUST_FIRST {
            return Ok(());
        }
        if keep == 0 {
            return self.free_chain(cx, a, start);
        }
        let Some(last) = self.chain_nth(cx, start, keep - 1)? else {
            return Ok(());
        };
        let next = self.fat_get(cx, last)?;
        if self.geo.is_eof(next) {
            return Ok(());
        }
        self.fat_set(cx, last, EOF)?;
        self.free_chain(cx, a, next)
    }

    /// Sets the volume's clean bit (FAT16 and FAT32; FreeBSD's
    /// `markvoldirty`), in the active FAT, now.
    pub fn set_clean(&self, cx: &B::Cx, clean: bool) -> Result<(), Error> {
        let g = &self.geo;
        if g.fat_type == FatType::Fat12 {
            return Ok(());
        }
        let (bn, size, bo) = self.fat_block(1);
        let at = g.fat_start + u64::from(g.active) * g.fat_bytes + bn;
        self.io
            .modify(cx, at, size, Fill::Read, Mode::Sync, &mut |b| {
                if g.fat_type == FatType::Fat32 {
                    let mut v = u32::from_le_bytes([b[bo], b[bo + 1], b[bo + 2], b[bo + 3]]);
                    v = if clean {
                        v | 0x0800_0000
                    } else {
                        v & 0xf7ff_ffff
                    };
                    b[bo..bo + 4].copy_from_slice(&v.to_le_bytes());
                } else {
                    let mut v = u16::from_le_bytes([b[bo], b[bo + 1]]);
                    v = if clean { v | 0x8000 } else { v & 0x7fff };
                    b[bo..bo + 2].copy_from_slice(&v.to_le_bytes());
                }
            })
    }

    /// Writes FSInfo's free count and next-free hint, if it changed
    /// (FreeBSD's `msdosfs_fsiflush`).
    pub fn write_fsinfo(&self, cx: &B::Cx, a: &mut Alloc) -> Result<(), Error> {
        let Some(at) = self.geo.fsinfo else {
            return Ok(());
        };
        if !a.fsinfo_dirty {
            return Ok(());
        }
        let len = self.geo.bytes_per_sec as usize;
        let (free, next) = (a.free, a.next_free);
        self.io
            .modify(cx, at, len, Fill::Read, Mode::Sync, &mut |b| {
                b[488..492].copy_from_slice(&free.to_le_bytes());
                b[492..496].copy_from_slice(&next.to_le_bytes());
            })?;
        a.fsinfo_dirty = false;
        Ok(())
    }
}
