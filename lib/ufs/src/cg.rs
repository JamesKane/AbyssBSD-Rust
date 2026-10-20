// SPDX-License-Identifier: BSD-2-Clause
//! Cylinder groups (`struct cg`): the header, the inode and fragment
//! bitmaps, the cluster map and summary, and the check-hash, with the
//! accounting FreeBSD keeps exactly (fsck rebuilds and compares it).
//!
//! Behavior reference: sys/ufs/ffs/fs.h (`struct cg`), sys/ufs/ffs/
//! ffs_subr.c (`ffs_isblock` and its relatives, `ffs_fragacct`,
//! `ffs_clusteracct`), sys/ufs/ffs/ffs_alloc.c (`ffs_mapsearch`,
//! `ffs_ckhash_cg`).

use crate::sb::{CK_CYLGRP, Superblock};
use crate::{Error, count32, idx};
use crc::crc32c;

/// `CG_MAGIC`.
pub const MAGIC: u32 = 0x0009_0255;

/// Field offsets in `struct cg` (checked against FreeBSD 15.1's headers).
mod off {
    pub(super) const MAGIC: usize = 4;
    pub(super) const CGX: usize = 12;
    pub(super) const CS: usize = 24;
    pub(super) const ROTOR: usize = 40;
    pub(super) const FROTOR: usize = 44;
    pub(super) const IROTOR: usize = 48;
    pub(super) const FRSUM: usize = 52;
    pub(super) const IUSEDOFF: usize = 92;
    pub(super) const FREEOFF: usize = 96;
    pub(super) const CLUSTERSUMOFF: usize = 104;
    pub(super) const CLUSTEROFF: usize = 108;
    pub(super) const NCLUSTERBLKS: usize = 112;
    pub(super) const NIBLK: usize = 116;
    pub(super) const INITEDIBLK: usize = 120;
    pub(super) const CKHASH: usize = 132;
}

/// A group's summary counts (`struct csum`), as the group header and the
/// superblock's summary array both keep them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Csum {
    pub ndir: i32,
    pub nbfree: i32,
    pub nifree: i32,
    pub nffree: i32,
}

impl Csum {
    /// Decodes the 16 bytes at `b`.
    #[must_use]
    pub fn read(b: &[u8]) -> Self {
        let i = |at: usize| i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        Self {
            ndir: i(0),
            nbfree: i(4),
            nifree: i(8),
            nffree: i(12),
        }
    }

    /// Encodes into the 16 bytes at `b`.
    pub fn write(&self, b: &mut [u8]) {
        b[0..4].copy_from_slice(&self.ndir.to_le_bytes());
        b[4..8].copy_from_slice(&self.nbfree.to_le_bytes());
        b[8..12].copy_from_slice(&self.nifree.to_le_bytes());
        b[12..16].copy_from_slice(&self.nffree.to_le_bytes());
    }
}

/// A cylinder group's bytes, decoded in place.
pub(crate) struct Cg<'a> {
    b: &'a mut [u8],
    frag: u32,
    contigsumsize: u32,
}

/// Checks a group read at `cg`: its magic, its index, and its check-hash
/// if the file system keeps them (FreeBSD's `ffs_getcg`).
pub(crate) fn verify(sb: &Superblock, b: &[u8], cg: u32) -> Result<(), Error> {
    if b.len() < 168 {
        return Err(Error::Corrupt("cylinder group size"));
    }
    let u = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    if u(off::MAGIC) != MAGIC || u(off::CGX) != cg {
        return Err(Error::Corrupt("cylinder group header"));
    }
    if sb.metackhash & CK_CYLGRP != 0 && ckhash(b) != u(off::CKHASH) {
        return Err(Error::Integrity("cylinder group check-hash"));
    }
    // The maps must lie inside.
    let len = b.len();
    let fits = |at: usize, bytes: usize| at.checked_add(bytes).is_some_and(|e| e <= len);
    let ok = fits(idx(u(off::IUSEDOFF)), idx(sb.ipg).div_ceil(8))
        && fits(idx(u(off::FREEOFF)), idx(sb.fpg).div_ceil(8))
        && (sb.contigsumsize == 0
            || (fits(idx(u(off::CLUSTERSUMOFF)), 4 * (idx(sb.contigsumsize) + 1))
                && fits(
                    idx(u(off::CLUSTEROFF)),
                    idx(u(off::NCLUSTERBLKS)).div_ceil(8),
                )));
    if ok {
        Ok(())
    } else {
        Err(Error::Corrupt("cylinder group maps"))
    }
}

/// The check-hash of group bytes `b`, with the hash field taken as zero
/// (FreeBSD's `ffs_ckhash_cg`).
fn ckhash(b: &[u8]) -> u32 {
    let crc = crc32c(!0, &b[..off::CKHASH]);
    let crc = crc32c(crc, &[0; 4]);
    crc32c(crc, &b[off::CKHASH + 4..])
}

/// Recomputes a changed group's check-hash, if the file system keeps them.
pub(crate) fn update_ckhash(sb: &Superblock, b: &mut [u8]) {
    if sb.metackhash & CK_CYLGRP != 0 {
        let h = ckhash(b);
        b[off::CKHASH..off::CKHASH + 4].copy_from_slice(&h.to_le_bytes());
    }
}

impl<'a> Cg<'a> {
    pub(crate) fn new(sb: &Superblock, b: &'a mut [u8]) -> Self {
        Self {
            b,
            frag: sb.frag,
            contigsumsize: sb.contigsumsize,
        }
    }

    fn u32(&self, at: usize) -> u32 {
        u32::from_le_bytes([self.b[at], self.b[at + 1], self.b[at + 2], self.b[at + 3]])
    }

    fn set_u32(&mut self, at: usize, v: u32) {
        self.b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn add_i32(&mut self, at: usize, d: i32) {
        let v = self.u32(at).cast_signed();
        self.set_u32(at, v.wrapping_add(d).cast_unsigned());
    }

    pub(crate) fn cs(&self) -> Csum {
        Csum::read(&self.b[off::CS..off::CS + 16])
    }

    pub(crate) fn add_nbfree(&mut self, d: i32) {
        self.add_i32(off::CS + 4, d);
    }

    pub(crate) fn add_nifree(&mut self, d: i32) {
        self.add_i32(off::CS + 8, d);
    }

    pub(crate) fn add_nffree(&mut self, d: i32) {
        self.add_i32(off::CS + 12, d);
    }

    pub(crate) fn add_ndir(&mut self, d: i32) {
        self.add_i32(off::CS, d);
    }

    pub(crate) fn rotor(&self) -> u32 {
        self.u32(off::ROTOR)
    }

    pub(crate) fn set_rotor(&mut self, v: u32) {
        self.set_u32(off::ROTOR, v);
    }

    pub(crate) fn set_frotor(&mut self, v: u32) {
        self.set_u32(off::FROTOR, v);
    }

    pub(crate) fn frotor(&self) -> u32 {
        self.u32(off::FROTOR)
    }

    pub(crate) fn irotor(&self) -> u32 {
        self.u32(off::IROTOR)
    }

    pub(crate) fn set_irotor(&mut self, v: u32) {
        self.set_u32(off::IROTOR, v);
    }

    pub(crate) fn frsum(&self, i: u32) -> i32 {
        self.u32(off::FRSUM + 4 * idx(i)).cast_signed()
    }

    pub(crate) fn add_frsum(&mut self, i: u32, d: i32) {
        self.add_i32(off::FRSUM + 4 * idx(i), d);
    }

    pub(crate) fn niblk(&self) -> u32 {
        self.u32(off::NIBLK)
    }

    pub(crate) fn initediblk(&self) -> u32 {
        self.u32(off::INITEDIBLK)
    }

    pub(crate) fn set_initediblk(&mut self, v: u32) {
        self.set_u32(off::INITEDIBLK, v);
    }

    // Bitmaps.

    fn bit(&self, map: usize, i: u32) -> bool {
        self.b[map + idx(i / 8)] & (1 << (i % 8)) != 0
    }

    fn set_bit(&mut self, map: usize, i: u32, on: bool) {
        let byte = &mut self.b[map + idx(i / 8)];
        if on {
            *byte |= 1 << (i % 8);
        } else {
            *byte &= !(1 << (i % 8));
        }
    }

    fn freeoff(&self) -> usize {
        idx(self.u32(off::FREEOFF))
    }

    fn iusedoff(&self) -> usize {
        idx(self.u32(off::IUSEDOFF))
    }

    /// Whether fragment `f` (in the group) is free.
    pub(crate) fn frag_free(&self, f: u32) -> bool {
        self.bit(self.freeoff(), f)
    }

    pub(crate) fn set_frag_free(&mut self, f: u32, free: bool) {
        let m = self.freeoff();
        self.set_bit(m, f, free);
    }

    pub(crate) fn inode_used(&self, i: u32) -> bool {
        self.bit(self.iusedoff(), i)
    }

    pub(crate) fn set_inode_used(&mut self, i: u32, used: bool) {
        let m = self.iusedoff();
        self.set_bit(m, i, used);
    }

    /// The first free inode at or after `from` (a byte's worth at a time,
    /// as FreeBSD's `memcchr` scan), wrapping to the start.
    pub(crate) fn find_free_inode(&self, from: u32, ipg: u32) -> Option<u32> {
        let map = self.iusedoff();
        let bytes = ipg.div_ceil(8);
        let start = from / 8;
        (start..bytes)
            .chain(0..=start.min(bytes.saturating_sub(1)))
            .find(|&i| self.b[map + idx(i)] != 0xff)
            .map(|i| i * 8 + (!self.b[map + idx(i)]).trailing_zeros())
            .filter(|&i| i < ipg)
    }

    // Whole blocks, by block number in the group (`ffs_isblock` and
    // relatives).

    /// The free-fragment bits of the block starting at fragment `bbase`
    /// (`blkmap`).
    pub(crate) fn blkmap(&self, bbase: u32) -> u32 {
        let byte = u32::from(self.b[self.freeoff() + idx(bbase / 8)]);
        (byte >> (bbase % 8)) & (0xff >> (8 - self.frag))
    }

    pub(crate) fn is_block(&self, h: u32) -> bool {
        self.blkmap(h * self.frag) == (0xff >> (8 - self.frag))
    }

    pub(crate) fn is_free_block(&self, h: u32) -> bool {
        self.blkmap(h * self.frag) == 0
    }

    fn set_block_bits(&mut self, h: u32, free: bool) {
        for f in h * self.frag..(h + 1) * self.frag {
            self.set_frag_free(f, free);
        }
    }

    pub(crate) fn clr_block(&mut self, h: u32) {
        self.set_block_bits(h, false);
    }

    pub(crate) fn set_block(&mut self, h: u32) {
        self.set_block_bits(h, true);
    }

    /// Adds `cnt` to the fragment summary for each maximal run of free
    /// fragments, shorter than a block, in a block's map `fragmap`
    /// (`ffs_fragacct`).
    pub(crate) fn fragacct(&mut self, fragmap: u32, cnt: i32) {
        let mut run = 0;
        for i in 0..=self.frag {
            if i < self.frag && fragmap & (1 << i) != 0 {
                run += 1;
            } else {
                if run > 0 && run < self.frag {
                    self.add_frsum(run, cnt);
                }
                run = 0;
            }
        }
    }

    /// Updates the cluster map and summary for block `blkno` being freed
    /// (`cnt` 1) or allocated (-1) (`ffs_clusteracct`).
    pub(crate) fn clusteracct(&mut self, blkno: u32, cnt: i32) {
        if self.contigsumsize == 0 {
            return;
        }
        let map = idx(self.u32(off::CLUSTEROFF));
        let sum = idx(self.u32(off::CLUSTERSUMOFF));
        let nblks = self.u32(off::NCLUSTERBLKS);
        self.set_bit(map, blkno, cnt > 0);
        let max = self.contigsumsize;
        let end = (blkno + 1 + max).min(nblks);
        let forw = (blkno + 1..end).take_while(|&i| self.bit(map, i)).count();
        let forw = count32(forw);
        let back = (blkno.saturating_sub(max)..blkno)
            .rev()
            .take_while(|&i| self.bit(map, i))
            .count();
        let back = count32(back);
        let total = (back + forw + 1).min(max);
        self.add_i32(sum + 4 * idx(total), cnt);
        if back > 0 {
            self.add_i32(sum + 4 * idx(back), -cnt);
        }
        if forw > 0 {
            self.add_i32(sum + 4 * idx(forw), -cnt);
        }
    }

    /// Finds a run of exactly `allocsiz` free fragments, bounded by used
    /// ones or the block's edge, from the byte holding fragment `start` on,
    /// wrapping (FreeBSD's `ffs_mapsearch`); sets the fragment rotor.
    pub(crate) fn mapsearch(&mut self, start: u32, allocsiz: u32, fpg: u32) -> Option<u32> {
        let bytes = fpg.div_ceil(8);
        let first = (start / 8).min(bytes.saturating_sub(1));
        let order = (first..bytes).chain(0..=first);
        for byte in order {
            let base = byte * 8;
            // The blocks this byte covers (one, or several small ones).
            let mut bno = base;
            while bno < base + 8 {
                if bno + self.frag <= fpg {
                    let map = self.blkmap(bno);
                    if let Some(pos) = exact_run(map, self.frag, allocsiz) {
                        self.set_frotor(base);
                        return Some(bno + pos);
                    }
                }
                bno += self.frag;
            }
        }
        None
    }
}

/// Where a run of exactly `len` set bits, bounded by clear bits or the
/// edges, starts among the low `frag` bits of `map`.
fn exact_run(map: u32, frag: u32, len: u32) -> Option<u32> {
    let mut pos = 0;
    while pos < frag {
        if map & (1 << pos) == 0 {
            pos += 1;
            continue;
        }
        let run = count32((pos..frag).take_while(|&i| map & (1 << i) != 0).count());
        if run == len {
            return Some(pos);
        }
        pos += run;
    }
    None
}
