// SPDX-License-Identifier: BSD-2-Clause
//! Giving files blocks and taking them back: `balloc` and `truncate`
//! (FreeBSD's `ffs_balloc_ufs2` and `ffs_truncate`, without soft
//! updates), and writing inodes.
//!
//! Without soft updates, write ordering follows FreeBSD's UFS without
//! them: a new indirect block is zeroed on the disk before anything points
//! to it, and a truncated inode is written before its blocks are freed, so
//! a crash leaves at worst leaked blocks for fsck to reclaim.
//!
//! With them, every new pointer is placed with the tracker (waiting for
//! the block's group and contents), and a truncation's frees wait for the
//! inode (and any indirect block kept) to go out with the pointers gone.

use alloc::vec::Vec;

use crate::allocate::{Reserve, Summary};
use crate::inode::Inode;
use crate::softdep::{Slot, Work};
use crate::{
    BlockIo, Error, Fill, Mode, NDADDR, NDIRECT, NIADDR, Ufs, count32, idx, idx64, int64, ix64,
    len64,
};

/// 512-byte units (`DEV_BSIZE`), which `di_blocks` counts.
const DEV_BSIZE: u64 = 512;

impl<B: BlockIo> Ufs<B> {
    /// Writes inode `ino` (FreeBSD's `ffs_update`): copies it into its
    /// block, which goes out as `mode` says. With soft updates the copy is
    /// what its waits wait for.
    pub fn write_inode(&self, cx: &B::Cx, ino: u32, ip: &Inode, mode: Mode) -> Result<(), Error> {
        let sb = &self.sb;
        let (block, within) = sb.inode_location(ino);
        let bytes = ip.encode(sb);
        self.io
            .modify(cx, block, idx(sb.bsize), Fill::Read, mode, &mut |b| {
                b[within..within + crate::inode::SIZE].copy_from_slice(&bytes);
                self.soft(cx, |d| d.inode_copied(ino));
            })
    }

    /// The indirection level (0 single, 1 double, 2 triple) reaching file
    /// block `lbn` (past the direct ones), and the index at each level
    /// from the top (FreeBSD's `ufs_getlbns`).
    fn indir_path(&self, lbn: u64) -> Result<(usize, [usize; NIADDR]), Error> {
        let nindir = u64::from(self.sb.nindir);
        let mut rest = lbn - NDADDR;
        let mut span = 1u64;
        for level in 0..NIADDR {
            span = span.saturating_mul(nindir);
            if rest < span {
                let mut idx = [0; NIADDR];
                let mut per = span;
                for slot in idx.iter_mut().take(level + 1) {
                    per /= nindir;
                    *slot = usize::try_from(rest / per).map_err(|_| Error::TooBig)?;
                    rest %= per;
                }
                return Ok((level, idx));
            }
            rest -= span;
        }
        Err(Error::TooBig)
    }

    /// Entry `i` of the indirect block at fragment `blk`.
    fn ind_get(&self, cx: &B::Cx, blk: u64, i: usize) -> Result<u64, Error> {
        let sb = &self.sb;
        let mut v = 0;
        self.io
            .read(cx, blk * u64::from(sb.fsize), idx(sb.bsize), &mut |b| {
                v = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
            })?;
        if v >= sb.size {
            return Err(Error::Corrupt("block address"));
        }
        Ok(v)
    }

    /// Points entry `i` of the indirect block at fragment `blk` at new
    /// block `v` (a delayed write; with soft updates, the pointer waits).
    fn ind_set(&self, cx: &B::Cx, blk: u64, i: usize, v: u64) -> Result<(), Error> {
        let sb = &self.sb;
        let at = blk * u64::from(sb.fsize);
        self.io
            .modify(cx, at, idx(sb.bsize), Fill::Read, Mode::Delayed, &mut |b| {
                b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
                self.soft(cx, |d| {
                    d.indirect(at);
                    d.place(v, Slot::Indirect { at, index: i }, 0, 0, None);
                });
            })
    }

    /// Places new block `new` at inode `ino`'s slot `index`, with the
    /// tracker.
    fn place_direct(
        &self,
        cx: &B::Cx,
        ino: u32,
        index: usize,
        new: u64,
        old: (u64, u64),
        free_old: Option<(u64, u64)>,
    ) {
        self.soft(cx, |d| {
            d.place(new, Slot::Inode { ino, index }, old.0, old.1, free_old);
        });
    }

    /// A new indirect block, zeroed (on the disk now, before anything
    /// points to it; with soft updates, the pointers to it wait).
    fn new_indirect(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        pref: u64,
        reserve: Reserve,
    ) -> Result<u64, Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let nb = self.alloc(cx, sum, ino, pref, bsize, reserve)?;
        ip.blocks += bsize / DEV_BSIZE;
        let at = nb * u64::from(sb.fsize);
        self.io.modify(
            cx,
            at,
            idx(sb.bsize),
            Fill::Zero,
            self.ordered(),
            &mut |_| {
                self.soft(cx, |d| {
                    d.indirect(at);
                    d.contents(nb, at);
                });
            },
        )?;
        Ok(nb)
    }

    /// Zeroes `len` bytes of new space at byte `at` on the disk.
    fn zero_data(&self, cx: &B::Cx, at: u64, len: u64) -> Result<(), Error> {
        if len == 0 {
            return Ok(());
        }
        let zeros = alloc::vec![0u8; idx64(len)];
        self.io.write_data(cx, at, &zeros)
    }

    /// Gives file `ino` the disk space for bytes `[offset, offset + len)`,
    /// which lie in one block (FreeBSD's `ffs_balloc_ufs2`), returning
    /// that block's fragment address. New space is zeroed on the disk
    /// unless `clear` is false (the caller overwrites all of it, as
    /// FreeBSD's callers without `BA_CLRBUF`). `metadata` says the file is
    /// a directory, whose blocks go through the metadata cache. The caller
    /// sets the size afterwards and writes the inode.
    #[allow(clippy::too_many_arguments)]
    pub fn balloc(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        offset: u64,
        len: u64,
        reserve: Reserve,
        clear: bool,
        metadata: bool,
    ) -> Result<u64, Error> {
        self.balloc_in(
            cx, sum, ino, ip, offset, len, reserve, clear, metadata, None,
        )
    }

    /// As [`Self::balloc`] for a regular file whose caller clears new
    /// space in memory, as FreeBSD clears a buffer (`vfs_bio_clrbuf`)
    /// rather than the disk: a newly allocated block is not zeroed here,
    /// and its byte range in the file goes into `new`. The caller writes
    /// all of it, zeros where it has nothing else, before the pointer to
    /// it may reach the disk (soft updates wait for every byte of new
    /// space). Fragments grown in place or moved are still zeroed past
    /// their old end on the disk, by [`Self::realloccg`].
    #[allow(clippy::too_many_arguments)]
    pub fn balloc_new(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        offset: u64,
        len: u64,
        reserve: Reserve,
        new: &mut Vec<(u64, u64)>,
    ) -> Result<u64, Error> {
        self.balloc_in(
            cx,
            sum,
            ino,
            ip,
            offset,
            len,
            reserve,
            false,
            false,
            Some(new),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn balloc_in(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        offset: u64,
        len: u64,
        reserve: Reserve,
        clear: bool,
        metadata: bool,
        mut new_space: Option<&mut Vec<(u64, u64)>>,
    ) -> Result<u64, Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let lbn = offset / bsize;
        let size = offset % bsize + len;
        if size > bsize || len == 0 {
            return Err(Error::Corrupt("balloc across blocks"));
        }
        if offset + len > sb.maxfilesize {
            return Err(Error::TooBig);
        }
        // A fragment at the old end grows to a whole block first, if the
        // file now reaches past it.
        let lastlbn = ip.size / bsize;
        if lastlbn < NDADDR && lastlbn < lbn {
            let nb = lastlbn;
            let osize = u64::from(sb.block_size(ip.size, nb));
            if osize < bsize && osize > 0 && ip.db[idx64(nb)] != 0 {
                let pref = self.blkpref(sum, ino, ip, nb, int64(nb), prev_db(ip, nb));
                let old = ip.db[idx64(nb)];
                let (new, free_old) =
                    self.realloccg(cx, sum, ino, old, pref, osize, bsize, reserve, metadata)?;
                ip.blocks += (bsize - osize) / DEV_BSIZE;
                ip.db[idx64(nb)] = new;
                ip.size = (nb + 1) * bsize;
                self.place_direct(cx, ino, idx64(nb), new, (old, osize), free_old);
                if metadata {
                    self.soft(cx, |d| d.dir_block(ino, nb, new));
                }
            }
        }
        if lbn < NDADDR {
            let i = idx64(lbn);
            let nb = ip.db[i];
            if nb != 0 && ip.size >= (lbn + 1) * bsize {
                return Ok(nb);
            }
            if nb != 0 {
                let osize = sb.fragroundup(ip.size % bsize);
                let nsize = sb.fragroundup(size);
                if nsize <= osize {
                    return Ok(nb);
                }
                let pref = self.blkpref(sum, ino, ip, lbn, int64(lbn), prev_db(ip, lbn));
                let (new, free_old) =
                    self.realloccg(cx, sum, ino, nb, pref, osize, nsize, reserve, metadata)?;
                ip.blocks += (nsize - osize) / DEV_BSIZE;
                ip.db[i] = new;
                self.place_direct(cx, ino, i, new, (nb, osize), free_old);
                if metadata {
                    self.soft(cx, |d| d.dir_block(ino, lbn, new));
                }
                return Ok(new);
            }
            let nsize = if ip.size < (lbn + 1) * bsize {
                sb.fragroundup(size)
            } else {
                bsize
            };
            let pref = self.blkpref(sum, ino, ip, lbn, int64(lbn), prev_db(ip, lbn));
            let new = self.alloc(cx, sum, ino, pref, nsize, reserve)?;
            ip.blocks += nsize / DEV_BSIZE;
            ip.db[i] = new;
            self.place_direct(cx, ino, i, new, (0, 0), None);
            if metadata {
                self.soft(cx, |d| d.dir_block(ino, lbn, new));
            }
            if clear && !metadata {
                self.zero_data(cx, new * fsize, nsize)?;
            } else if let Some(v) = new_space.as_deref_mut() {
                v.push((lbn * bsize, nsize));
            }
            return Ok(new);
        }
        // Through the indirect blocks, making missing ones on the way; if
        // the data block cannot be had, those go again (FreeBSD's `fail:`).
        let mut fresh = Vec::new();
        let r = self.balloc_indirect(
            cx, sum, ino, ip, lbn, reserve, clear, metadata, &mut fresh, new_space,
        );
        if r.is_err() && !fresh.is_empty() {
            self.unwind_indirects(cx, sum, ino, ip, &fresh);
        }
        r
    }

    /// The indirect part of [`Self::balloc`]: new indirect blocks go into
    /// `fresh`, with where they are pointed to from (an indirect block and
    /// entry, or the inode's root of that level).
    #[allow(clippy::too_many_arguments)]
    fn balloc_indirect(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        lbn: u64,
        reserve: Reserve,
        clear: bool,
        metadata: bool,
        fresh: &mut Vec<(u64, Pointer)>,
        new_space: Option<&mut Vec<(u64, u64)>>,
    ) -> Result<u64, Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let (level, idx) = self.indir_path(lbn)?;
        let mut pref = 0;
        let mut blk = ip.ib[level];
        if blk == 0 {
            let p = self.blkpref(sum, ino, ip, lbn, -(ix64(level)) - 1, 0);
            blk = self.new_indirect(cx, sum, ino, ip, p, reserve)?;
            pref = blk + u64::from(sb.frag);
            ip.ib[level] = blk;
            self.place_direct(cx, ino, NDIRECT + level, blk, (0, 0), None);
            fresh.push((blk, Pointer::Root(level)));
        }
        for (depth, &at) in idx.iter().enumerate().take(level) {
            let next = self.ind_get(cx, blk, at)?;
            if next != 0 {
                blk = next;
                continue;
            }
            if pref == 0 {
                pref = self.blkpref(sum, ino, ip, lbn, ix64(depth) - ix64(level) - 1, 0);
            }
            let nb = self.new_indirect(cx, sum, ino, ip, pref, reserve)?;
            pref = nb + u64::from(sb.frag);
            self.ind_set(cx, blk, idx[depth], nb)?;
            fresh.push((nb, Pointer::Entry(blk, idx[depth])));
            blk = nb;
        }
        let slot = idx[level];
        let nb = self.ind_get(cx, blk, slot)?;
        if nb != 0 {
            return Ok(nb);
        }
        if pref == 0 || (lbn > NDADDR && sb.metaspace != 0) {
            let prev = if slot > 0 {
                self.ind_get(cx, blk, slot - 1)?
            } else {
                0
            };
            pref = self.blkpref(sum, ino, ip, lbn, ix64(slot), prev);
        }
        let new = self.alloc(cx, sum, ino, pref, bsize, reserve)?;
        ip.blocks += bsize / DEV_BSIZE;
        if clear && !metadata {
            self.zero_data(cx, new * fsize, bsize)?;
        } else if let Some(v) = new_space {
            v.push((lbn * bsize, bsize));
        }
        self.ind_set(cx, blk, slot, new)?;
        if metadata {
            self.soft(cx, |d| d.dir_block(ino, lbn, new));
        }
        Ok(new)
    }

    /// Takes back the indirect blocks a failed [`Self::balloc`] made, the
    /// pointers to them first: without soft updates the indirect block
    /// that pointed to one is written at once and the block freed; with
    /// them, the frees wait for those pointers (and the inode) to go out.
    fn unwind_indirects(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        fresh: &[(u64, Pointer)],
    ) {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let list: Vec<(u64, u64)> = fresh.iter().map(|&(nb, _)| (nb, bsize)).collect();
        let id = self.soft(cx, |d| d.defer(Work::Free(list)));
        for &(nb, from) in fresh.iter().rev() {
            match from {
                Pointer::Root(level) => {
                    ip.ib[level] = 0;
                    let slot = Slot::Inode {
                        ino,
                        index: NDIRECT + level,
                    };
                    self.soft(cx, |d| {
                        d.cancel_slot(slot);
                        if let Some(id) = id {
                            d.wait_copy(id, ino);
                        }
                    });
                }
                Pointer::Entry(parent, index) => {
                    let at = parent * fsize;
                    let _ = self.io.modify(
                        cx,
                        at,
                        idx(sb.bsize),
                        Fill::Read,
                        self.ordered(),
                        &mut |b| {
                            b[index * 8..index * 8 + 8].fill(0);
                            self.soft(cx, |d| {
                                d.cancel_slot(Slot::Indirect { at, index });
                                if let Some(id) = id {
                                    d.wait_buffer(id, at);
                                }
                            });
                        },
                    );
                }
            }
            ip.blocks = ip.blocks.saturating_sub(bsize / DEV_BSIZE);
            self.soft(cx, |d| d.forget_buffer(nb * fsize, ino));
            if id.is_none() {
                let _ = self.blkfree(cx, sum, nb, bsize);
            } else {
                self.io.forget(cx, nb * fsize, idx(sb.bsize));
            }
        }
        if let Some(id) = id {
            self.soft(cx, |d| d.arm(id));
        }
    }

    /// Sets file `ino`'s size to `length` (FreeBSD's `ffs_truncate`,
    /// without soft updates). Growing gives the last byte's block space,
    /// zeroed. Shrinking zeroes the rest of a partial last block on the
    /// disk, writes the inode (now) with the pointers past the end gone,
    /// and then frees those blocks, a kept indirect block rewritten before
    /// its children go. The caller writes the inode afterwards.
    #[allow(clippy::too_many_arguments)]
    pub fn truncate(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        length: u64,
        reserve: Reserve,
        metadata: bool,
    ) -> Result<(), Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        if length > sb.maxfilesize {
            return Err(Error::TooBig);
        }
        if length == ip.size {
            return Ok(());
        }
        if length > ip.size {
            let last = length - 1;
            let lbn = last / bsize;
            let old = ip.size;
            let before = self.bmap(cx, ip, lbn)?;
            let addr = self.balloc(cx, sum, ino, ip, last, 1, reserve, before == 0, metadata)?;
            // An existing block that now holds more of the file: what lay
            // past the old end reads as zeros.
            if before != 0 && !metadata && old / bsize == lbn {
                let space = u64::from(sb.block_size(length, lbn));
                let from = old % bsize;
                self.zero_data(cx, addr * fsize + from, space - from)?;
            }
            ip.size = length;
            return Ok(());
        }
        // Shrinking. UFS files never end in a hole: the block that ends up
        // last is allocated if it is not (and zeroed); if partial, the rest
        // of it past the new end is zeroed on the disk, and in the direct
        // blocks its fragments past the end go (FreeBSD's `ffs_truncate`).
        let offset = length % bsize;
        let mut tail_free = None;
        if length > 0 {
            let lbn = (length - 1) / bsize;
            let blkno = self.bmap(cx, ip, lbn)?;
            if blkno == 0 || offset != 0 {
                let addr = self.balloc(cx, sum, ino, ip, length - 1, 1, reserve, true, metadata)?;
                let oldspace = u64::from(sb.block_size(ip.size, lbn));
                if offset != 0 && !metadata {
                    self.zero_data(cx, addr * fsize + offset, oldspace - offset)?;
                }
                if lbn < NDADDR {
                    let newspace = u64::from(sb.block_size(length, lbn));
                    if newspace < oldspace {
                        // A pointer waiting for the block's data waits for
                        // what stays of it.
                        self.soft(cx, |d| d.shrink_block(addr, newspace));
                        tail_free =
                            Some((addr + u64::from(sb.numfrags(newspace)), oldspace - newspace));
                    }
                }
            }
        }
        let old_size = ip.size;
        let olddb = ip.db;
        let oldib = ip.ib;
        // The last block kept, and at each level the last kept index.
        let lastblock = int64(length.div_ceil(bsize)) - 1;
        let nindir = i64::from(sb.nindir);
        let mut lastiblock = [0i64; NIADDR];
        lastiblock[0] = lastblock - ix64(NDIRECT);
        lastiblock[1] = lastiblock[0] - nindir;
        lastiblock[2] = lastiblock[1] - nindir * nindir;
        if self.soft {
            return self.truncate_soft(
                cx,
                ino,
                ip,
                length,
                (lastblock, lastiblock),
                tail_free,
                metadata,
            );
        }
        for i in 0..NDIRECT {
            if ix64(i) > lastblock {
                ip.db[i] = 0;
            }
        }
        for (ib, last) in ip.ib.iter_mut().zip(lastiblock) {
            if last < 0 {
                *ib = 0;
            }
        }
        ip.size = length;
        self.write_inode(cx, ino, ip, Mode::Sync)?;
        let mut released = 0u64;
        for level in (0..NIADDR).rev() {
            let bn = oldib[level];
            if bn == 0 {
                continue;
            }
            released += self.indirtrunc(cx, sum, bn, level, lastiblock[level])?;
            if lastiblock[level] < 0 {
                self.blkfree(cx, sum, bn, bsize)?;
                released += bsize / DEV_BSIZE;
            }
        }
        for i in (0..NDIRECT).rev() {
            if ix64(i) <= lastblock {
                break;
            }
            let bn = olddb[i];
            if bn == 0 {
                continue;
            }
            let space = u64::from(sb.block_size(old_size, len64(i)));
            self.blkfree(cx, sum, bn, space)?;
            released += space / DEV_BSIZE;
        }
        if let Some((at, len)) = tail_free {
            self.blkfree(cx, sum, at, len)?;
            released += len / DEV_BSIZE;
        }
        ip.blocks = ip.blocks.saturating_sub(released);
        Ok(())
    }

    /// The shrinking part of [`Self::truncate`] with soft updates: the
    /// pointers past the end go now, and everything they reached is freed
    /// once the inode, and each indirect block kept, is on the disk without
    /// them (FreeBSD's `softdep_setup_freeblocks`).
    #[allow(clippy::too_many_arguments)]
    fn truncate_soft(
        &self,
        cx: &B::Cx,
        ino: u32,
        ip: &mut Inode,
        length: u64,
        (lastblock, lastiblock): (i64, [i64; NIADDR]),
        tail_free: Option<(u64, u64)>,
        metadata: bool,
    ) -> Result<(), Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        // What the file held, and fragments moved away from that the disk
        // may still show (not in its block count).
        let mut frees = Vec::new();
        let mut moved = Vec::new();
        let mut kept = Vec::new();
        for level in (0..NIADDR).rev() {
            let bn = ip.ib[level];
            if bn == 0 {
                continue;
            }
            let last = lastiblock[level];
            self.collect_indir(
                cx, ino, bn, level, last, metadata, &mut frees, &mut moved, &mut kept,
            )?;
            if lastiblock[level] < 0 {
                frees.push((bn, bsize));
                let slot = Slot::Inode {
                    ino,
                    index: NDIRECT + level,
                };
                self.soft(cx, |d| {
                    moved.extend(d.cancel_slot(slot));
                    moved.extend(d.forget_buffer(bn * fsize, ino));
                });
                self.io.forget(cx, bn * fsize, idx(sb.bsize));
                ip.ib[level] = 0;
            }
        }
        for i in (0..NDIRECT).rev() {
            if ix64(i) <= lastblock {
                break;
            }
            let bn = ip.db[i];
            if bn == 0 {
                continue;
            }
            let space = u64::from(sb.block_size(ip.size, len64(i)));
            frees.push((bn, space));
            self.soft(cx, |d| {
                moved.extend(d.cancel_slot(Slot::Inode { ino, index: i }));
                if metadata {
                    moved.extend(d.forget_buffer(bn * fsize, ino));
                }
            });
            if metadata {
                self.io.forget(cx, bn * fsize, idx64(space));
            }
            ip.db[i] = 0;
        }
        if let Some(tail) = tail_free {
            frees.push(tail);
        }
        let released: u64 = frees.iter().map(|(_, n)| n / DEV_BSIZE).sum();
        frees.extend(moved);
        let Some(id) = self.soft(cx, |d| d.defer(Work::Free(frees))) else {
            return Err(Error::ReadOnly);
        };
        ip.size = length;
        ip.blocks = ip.blocks.saturating_sub(released);
        self.soft(cx, |d| d.wait_copy(id, ino));
        // Indirect blocks kept lose their tails, which fsck must never see
        // under either size: the inode goes out first, now, with the roots
        // over them cleared (on the disk the file then ends in a hole of
        // its indirect blocks, which fsck shortens), then the tails, now.
        // Each root comes back once its indirect block holds a pointer on
        // the disk.
        if !kept.is_empty() {
            let mut cut = ip.clone();
            for (level, last) in lastiblock.iter().enumerate() {
                if *last >= 0 {
                    cut.ib[level] = 0;
                }
            }
            self.write_inode(cx, ino, &cut, Mode::Sync)?;
        }
        // Deepest first: an indirect block left empty on the disk is known
        // as such before the one above it goes out.
        for (bn, keep) in kept.into_iter().rev() {
            let at = bn * fsize;
            self.io
                .modify(cx, at, idx(sb.bsize), Fill::Read, Mode::Sync, &mut |b| {
                    b[keep * 8..].fill(0);
                    self.soft(cx, |d| {
                        d.indirect(at);
                        d.wait_buffer(id, at);
                    });
                })?;
        }
        let r = self.write_inode(cx, ino, ip, Mode::Delayed);
        self.soft(cx, |d| d.arm(id));
        r
    }

    /// With soft updates: collects into `frees` what the indirect block at
    /// `bn` (of `level`) maps past file block `lastbn` of its range (-1:
    /// all of it), forgetting the pointers and the freed indirect blocks
    /// (fragments moved away from by those pointers go into `moved`), and
    /// into `kept` the indirect blocks that keep some (with the first
    /// entry they lose).
    #[allow(clippy::too_many_arguments)]
    fn collect_indir(
        &self,
        cx: &B::Cx,
        ino: u32,
        bn: u64,
        level: usize,
        lastbn: i64,
        metadata: bool,
        frees: &mut Vec<(u64, u64)>,
        moved: &mut Vec<(u64, u64)>,
        kept: &mut Vec<(u64, usize)>,
    ) -> Result<(), Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let nindir = idx(sb.nindir);
        let factor = i64::from(sb.nindir).pow(count32(level));
        let last = if lastbn < 0 { -1 } else { lastbn / factor };
        let mut entries = alloc::vec![0u64; nindir];
        self.io.read(cx, bn * fsize, idx(sb.bsize), &mut |b| {
            for (e, c) in entries.iter_mut().zip(b.as_chunks::<8>().0) {
                *e = u64::from_le_bytes(*c);
            }
        })?;
        // `last` is -1 or more.
        let first_free = usize::try_from(last + 1).unwrap_or(0);
        if last >= 0 {
            kept.push((bn, first_free));
        }
        for (i, &nb) in entries.iter().enumerate().skip(first_free) {
            if nb == 0 {
                continue;
            }
            if nb >= sb.size {
                return Err(Error::Corrupt("block address"));
            }
            if level > 0 {
                self.collect_indir(cx, ino, nb, level - 1, -1, metadata, frees, moved, kept)?;
            }
            frees.push((nb, bsize));
            let at = bn * fsize;
            self.soft(cx, |d| {
                moved.extend(d.cancel_slot(Slot::Indirect { at, index: i }));
                if level > 0 || metadata {
                    moved.extend(d.forget_buffer(nb * fsize, ino));
                }
            });
            if level > 0 || metadata {
                self.io.forget(cx, nb * fsize, idx(sb.bsize));
            }
        }
        if level > 0 && last >= 0 {
            let nb = entries[usize::try_from(last).unwrap_or(0)];
            if nb != 0 {
                let last = lastbn % factor;
                self.collect_indir(cx, ino, nb, level - 1, last, metadata, frees, moved, kept)?;
            }
        }
        Ok(())
    }

    /// Frees what the indirect block at `bn` (of `level`: 0 points at data
    /// blocks) maps past file block `lastbn` of its range (-1: all of it),
    /// rewriting the block first if it keeps some (FreeBSD's
    /// `ffs_indirtrunc`). Returns the 512-byte units released (not the
    /// block itself).
    fn indirtrunc(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        bn: u64,
        level: usize,
        lastbn: i64,
    ) -> Result<u64, Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let nindir = idx(sb.nindir);
        let factor = i64::from(sb.nindir).pow(count32(level));
        let last = if lastbn < 0 { -1 } else { lastbn / factor };
        let mut entries = alloc::vec![0u64; nindir];
        self.io
            .read(cx, bn * u64::from(sb.fsize), idx(sb.bsize), &mut |b| {
                for (e, c) in entries.iter_mut().zip(b.as_chunks::<8>().0) {
                    *e = u64::from_le_bytes(*c);
                }
            })?;
        if last >= 0 {
            // `last` is not negative here.
            let keep = usize::try_from(last + 1).unwrap_or(0);
            self.io.modify(
                cx,
                bn * u64::from(sb.fsize),
                idx(sb.bsize),
                Fill::Read,
                Mode::Sync,
                &mut |b| {
                    b[keep * 8..].fill(0);
                },
            )?;
        }
        let mut released = 0;
        // `last` is -1 or more.
        let first_free = usize::try_from(last + 1).unwrap_or(0);
        for i in (first_free..nindir).rev() {
            let nb = entries[i];
            if nb == 0 {
                continue;
            }
            if nb >= sb.size {
                return Err(Error::Corrupt("block address"));
            }
            if level > 0 {
                released += self.indirtrunc(cx, sum, nb, level - 1, -1)?;
            }
            self.blkfree(cx, sum, nb, bsize)?;
            released += bsize / DEV_BSIZE;
        }
        if level > 0 && last >= 0 {
            let nb = entries[usize::try_from(last).unwrap_or(0)];
            if nb != 0 {
                released += self.indirtrunc(cx, sum, nb, level - 1, lastbn % factor)?;
            }
        }
        Ok(released)
    }
}

/// Where a new indirect block is pointed to from.
#[derive(Clone, Copy, Debug)]
enum Pointer {
    /// The inode's root of that level.
    Root(usize),
    /// An entry of the indirect block at that fragment.
    Entry(u64, usize),
}

/// The direct pointer before `lbn` (for `blkpref`), 0 for the first.
fn prev_db(ip: &Inode, lbn: u64) -> u64 {
    if lbn == 0 { 0 } else { ip.db[idx64(lbn) - 1] }
}
