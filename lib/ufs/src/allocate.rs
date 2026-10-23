// SPDX-License-Identifier: BSD-2-Clause
//! Allocation (stage 2 of docs/VFS.md §10.3): blocks, fragments, and
//! inodes, with FreeBSD's policies, so layouts and fsck's view match
//! FreeBSD's.
//!
//! [`Summary`] is the state FreeBSD keeps under `UFS_LOCK`: the totals,
//! each group's counts, and the allocation hints. The caller holds it
//! (behind its own lock) and passes it to every allocation; group headers
//! change through [`BlockIo::modify`] (delayed writes, as FreeBSD's
//! `bdwrite`).
//!
//! Behavior reference: sys/ufs/ffs/ffs_alloc.c (`ffs_alloc`,
//! `ffs_realloccg`, `ffs_valloc`, `ffs_dirpref`, `ffs_blkpref_ufs2`,
//! `ffs_hashalloc`, `ffs_fragextend`, `ffs_alloccg`, `ffs_alloccgblk`,
//! `ffs_nodealloccg`, `ffs_blkfree_cg`, `ffs_freefile`).

use alloc::vec::Vec;

use crate::cg::{self, Cg, Csum};
use crate::inode::Inode;
use crate::sb::Totals;
use crate::softdep::{Deps, Work};
use crate::{BlockIo, Error, Fill, Kind, Mode, NDADDR, NDIRECT, Ufs, idx, idx64, int32};

/// `FS_OPTSPACE`.
const OPTSPACE: u32 = 1;

/// The allocation state of a mounted file system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// `fs_cstotal`.
    pub totals: Totals,
    /// Each group's counts (`fs_csp`).
    pub cs: Vec<Csum>,
    /// Directories made in a row in each group (`fs_contigdirs`).
    contigdirs: Vec<u8>,
    /// Where the last search for a group with free blocks ended.
    cgrotor: u32,
    /// `fs_optim`, which allocation may switch.
    pub optim: u32,
    /// Changed since last written (`fs_fmod`).
    pub modified: bool,
}

/// Whether an allocation may use the space reserved for the superuser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reserve {
    Allowed,
    Denied,
}

impl<B: BlockIo> Ufs<B> {
    /// Reads the summary: the totals from the superblock and each group's
    /// counts from the summary area (FreeBSD's `ffs_mountfs`).
    pub fn load_summary(&self, cx: &B::Cx) -> Result<Summary, Error> {
        let sb = &self.sb;
        let len = usize::try_from(sb.fragroundup(u64::from(sb.cssize)))
            .map_err(|_| Error::Corrupt("fs_cssize"))?;
        let mut cs = Vec::with_capacity(idx(sb.ncg));
        self.io
            .read(cx, sb.csaddr * u64::from(sb.fsize), len, &mut |b| {
                for c in b.as_chunks::<16>().0.iter().take(idx(sb.ncg)) {
                    cs.push(Csum::read(c));
                }
            })?;
        if cs.len() != idx(sb.ncg) {
            return Err(Error::Corrupt("summary area"));
        }
        Ok(Summary {
            totals: sb.totals,
            cs,
            contigdirs: alloc::vec![0; idx(sb.ncg)],
            cgrotor: 0,
            optim: sb.optim,
            modified: false,
        })
    }

    /// The summary counted again from each group's header (FreeBSD's
    /// `softdep_mount`): with soft updates the totals written may lag
    /// behind the groups after a crash, but a group's counts are written
    /// with its maps.
    pub fn summary_from_groups(&self, cx: &B::Cx) -> Result<Summary, Error> {
        let mut sum = self.load_summary(cx)?;
        let mut totals = Totals::default();
        for c in 0..self.sb.ncg {
            let cs = self.with_group(cx, c, |g| g.cs())?;
            totals.ndir += i64::from(cs.ndir);
            totals.nbfree += i64::from(cs.nbfree);
            totals.nifree += i64::from(cs.nifree);
            totals.nffree += i64::from(cs.nffree);
            sum.cs[idx(c)] = cs;
        }
        sum.modified = sum.totals != totals;
        sum.totals = totals;
        Ok(sum)
    }

    /// Reads group `cg`'s header, checked, and calls `f` with it.
    fn with_group<R>(&self, cx: &B::Cx, cg: u32, f: impl FnOnce(&Cg<'_>) -> R) -> Result<R, Error> {
        let sb = &self.sb;
        let hashed = self.io.hashes_groups();
        let mut f = Some(f);
        let mut out = Err(Error::Io);
        self.io.read(
            cx,
            sb.cgtod(cg) * u64::from(sb.fsize),
            idx(sb.cgsize),
            &mut |b| {
                out = cg::check(sb, b, cg, !hashed).map(|()| {
                    let mut copy = b.to_vec();
                    (f.take().expect("called once"))(&Cg::new(sb, &mut copy))
                });
            },
        )?;
        out
    }

    /// Writes the summary area and the superblock, now, with the clean
    /// flag and the time (FreeBSD's `ffs_sbupdate`); then flushes.
    pub fn write_summary(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        clean: bool,
        time: i64,
    ) -> Result<(), Error> {
        let sb = &self.sb;
        let len = usize::try_from(sb.fragroundup(u64::from(sb.cssize)))
            .map_err(|_| Error::Corrupt("fs_cssize"))?;
        self.io.modify(
            cx,
            sb.csaddr * u64::from(sb.fsize),
            len,
            Fill::Read,
            Mode::Sync,
            &mut |b| {
                for (c, s) in b.as_chunks_mut::<16>().0.iter_mut().zip(&sum.cs) {
                    s.write(c);
                }
            },
        )?;
        let bytes = sb.encode(&sum.totals, clean, time, sum.optim);
        self.io.modify(
            cx,
            crate::sb::SBLOCK,
            crate::sb::SBLOCKSIZE,
            Fill::Read,
            Mode::Sync,
            &mut |b| {
                b.copy_from_slice(&bytes);
            },
        )?;
        self.io.flush(cx)?;
        sum.modified = false;
        Ok(())
    }

    /// Changes group `cg`'s header with `f` (a delayed write), after
    /// checking it; its check-hash is kept.
    pub(crate) fn with_cg<R>(
        &self,
        cx: &B::Cx,
        cg: u32,
        f: impl FnOnce(&mut Cg<'_>) -> R,
    ) -> Result<R, Error> {
        let sb = &self.sb;
        let hashed = self.io.hashes_groups();
        let mut f = Some(f);
        let mut out = Err(Error::Io);
        self.io.modify(
            cx,
            sb.cgtod(cg) * u64::from(sb.fsize),
            idx(sb.cgsize),
            Fill::Read,
            Mode::Delayed,
            &mut |b| {
                out = cg::check(sb, b, cg, !hashed).map(|()| {
                    let r = (f.take().expect("called once"))(&mut Cg::new(sb, b));
                    if !hashed {
                        cg::update_ckhash(sb, b);
                    }
                    r
                });
            },
        )?;
        out
    }

    /// Where the next block of file `ino` should go (FreeBSD's
    /// `ffs_blkpref_ufs2`): `indx` is the block's place in its pointer
    /// array (negative for an indirect block of that level), and `prev`
    /// the pointer before it there (0 if none).
    pub fn blkpref(
        &self,
        sum: &mut Summary,
        ino: u32,
        ip: &Inode,
        lbn: u64,
        indx: i64,
        prev: u64,
    ) -> u64 {
        let sb = &self.sb;
        let frag = u64::from(sb.frag);
        let inocg = ino / sb.ipg;
        let nindir = u64::from(sb.nindir);
        if indx < 0 {
            let mut pref = sb.cgdmin(inocg);
            if indx == -1 && lbn < NDADDR + nindir && ip.db[NDIRECT - 1] != 0 {
                pref = ip.db[NDIRECT - 1] + frag;
                if sb.dtog(pref) >= sb.ncg {
                    pref = 0;
                }
            }
            return pref;
        }
        if lbn == NDADDR {
            let pref = ip.ib[0];
            if pref != 0 && pref >= sb.cgdata(inocg) && pref < sb.cgbase(inocg + 1) {
                if sb.dtog(pref + frag) >= sb.ncg {
                    return 0;
                }
                return pref + frag;
            }
        }
        // Not negative here.
        let slot = usize::try_from(indx).unwrap_or(0);
        let prevbn = if slot == 0 || prev >= sb.size {
            0
        } else {
            prev
        };
        if slot % idx(sb.maxbpg) == 0 || prevbn == 0 {
            if ip.kind() == Kind::Directory {
                return sb.cgdmin(inocg);
            }
            if lbn < NDADDR + nindir {
                return sb.cgdata(inocg);
            }
            let startcg = if slot == 0 || prevbn == 0 {
                (u64::from(inocg) + lbn / u64::from(sb.maxbpg)) % u64::from(sb.ncg)
            } else {
                (u64::from(sb.dtog(prevbn)) + 1) % u64::from(sb.ncg)
            };
            // Below `ncg`.
            let startcg = u32::try_from(startcg).unwrap_or(0);
            let avgbfree = sum.totals.nbfree / i64::from(sb.ncg);
            for cg in (startcg..sb.ncg).chain(0..startcg) {
                if i64::from(sum.cs[idx(cg)].nbfree) >= avgbfree {
                    sum.cgrotor = cg;
                    return sb.cgdata(cg);
                }
            }
            return 0;
        }
        if sb.dtog(prevbn + frag) >= sb.ncg {
            return 0;
        }
        prevbn + frag
    }

    /// FreeBSD's `ffs_hashalloc`: `try_cg` in the preferred group, then
    /// quadratic rehash, then every group.
    fn hashalloc(
        &self,
        icg: u32,
        pref: u64,
        mut try_cg: impl FnMut(u32, u64) -> Result<Option<u64>, Error>,
    ) -> Result<Option<u64>, Error> {
        let ncg = self.sb.ncg;
        if let Some(r) = try_cg(icg, pref)? {
            return Ok(Some(r));
        }
        let mut cg = icg;
        let mut i = 1;
        while i < ncg {
            cg += i;
            if cg >= ncg {
                cg -= ncg;
            }
            if let Some(r) = try_cg(cg, 0)? {
                return Ok(Some(r));
            }
            i *= 2;
        }
        let mut cg = (icg + 2) % ncg;
        for _ in 2..ncg {
            if let Some(r) = try_cg(cg, 0)? {
                return Ok(Some(r));
            }
            cg = (cg + 1) % ncg;
        }
        Ok(None)
    }

    /// Allocates a block in a group whose header is `g` (FreeBSD's
    /// `ffs_alloccgblk`); frees the fragments past `size` again.
    fn alloccgblk(
        &self,
        sum: &mut Summary,
        g: &mut Cg<'_>,
        cg: u32,
        bpref: u64,
        size: u64,
    ) -> Option<u64> {
        let sb = &self.sb;
        let frag = sb.frag;
        let bpref = if bpref == 0 {
            sb.cgbase(cg) + u64::from(g.rotor()) + u64::from(frag)
        } else if sb.dtog(bpref) != cg {
            if bpref < sb.cgdata(sb.dtog(bpref)) {
                sb.cgdmin(cg)
            } else {
                sb.cgdata(cg)
            }
        } else {
            bpref
        };
        let want = sb.dtogd(bpref - bpref % u64::from(frag));
        let bno = if want < sb.fpg && want + frag <= sb.fpg && g.is_block(want / frag) {
            want
        } else {
            let found = g.mapsearch(sb.dtogd(bpref), frag, sb.fpg)?;
            if u64::from(found) >= u64::from(sb.dtogd(sb.cgdata(cg))) {
                g.set_rotor(found);
            }
            found
        };
        let blkno = bno / frag;
        g.clr_block(blkno);
        g.clusteracct(blkno, -1);
        g.add_nbfree(-1);
        sum.totals.nbfree -= 1;
        sum.cs[idx(cg)].nbfree -= 1;
        sum.modified = true;
        let frags = sb.numfrags(size);
        if frags != frag {
            for i in frags..frag {
                g.set_frag_free(bno + i, true);
            }
            let n = frag - frags;
            g.add_nffree(int32(n));
            sum.totals.nffree += i64::from(n);
            sum.cs[idx(cg)].nffree += int32(n);
            g.add_frsum(n, 1);
        }
        Some(sb.cgbase(cg) + u64::from(bno))
    }

    /// Allocates `size` bytes (a block, or fragments) in group `cg` for
    /// file `owner` (FreeBSD's `ffs_alloccg`); `rsize` is what the caller
    /// keeps. With soft updates, pointers to it wait for the group.
    #[allow(clippy::too_many_arguments)]
    fn alloccg(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        owner: u32,
        cg: u32,
        bpref: u64,
        size: u64,
        rsize: u64,
    ) -> Result<Option<u64>, Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        if sum.cs[idx(cg)].nbfree == 0 && size == bsize {
            return Ok(None);
        }
        let cg_at = sb.cgtod(cg) * u64::from(sb.fsize);
        self.with_cg(cx, cg, |g| {
            let got = self.alloccg_in(sum, g, cg, bpref, size, rsize);
            if let Some(bno) = got {
                self.soft(cx, |d| d.new_block(bno, 0, rsize, cg_at, owner));
            }
            got
        })
    }

    /// [`Self::alloccg`] with the group's header held.
    fn alloccg_in(
        &self,
        sum: &mut Summary,
        g: &mut Cg<'_>,
        cg: u32,
        bpref: u64,
        size: u64,
        rsize: u64,
    ) -> Option<u64> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        if g.cs().nbfree == 0 && size == bsize {
            return None;
        }
        if size == bsize {
            return self.alloccgblk(sum, g, cg, bpref, rsize);
        }
        let frags = sb.numfrags(size);
        let allocsiz = (frags..sb.frag)
            .find(|&a| g.frsum(a) != 0)
            .unwrap_or(sb.frag);
        if allocsiz == sb.frag {
            if g.cs().nbfree == 0 {
                return None;
            }
            return self.alloccgblk(sum, g, cg, bpref, rsize);
        }
        let start = if bpref != 0 {
            sb.dtogd(bpref)
        } else {
            g.frotor()
        };
        let bno = g.mapsearch(start, allocsiz, sb.fpg)?;
        for i in 0..frags {
            g.set_frag_free(bno + i, false);
        }
        g.add_nffree(-int32(frags));
        g.add_frsum(allocsiz, -1);
        if frags != allocsiz {
            g.add_frsum(allocsiz - frags, 1);
        }
        sum.totals.nffree -= i64::from(frags);
        sum.cs[idx(cg)].nffree -= int32(frags);
        sum.modified = true;
        Some(sb.cgbase(cg) + u64::from(bno))
    }

    /// With soft updates, out of space: writes what is dirty, so frees
    /// waiting for it come due, and does them (FreeBSD's
    /// `softdep_request_cleanup`). Returns whether any were waiting.
    pub(crate) fn reclaim(&self, cx: &B::Cx, sum: &mut Summary) -> bool {
        let Some(pending) = self.soft(cx, |d| d.pending_free() != 0 || !d.is_idle()) else {
            return false;
        };
        if !pending {
            return false;
        }
        let _ = self.io.flush(cx);
        loop {
            let frees = self.soft(cx, Deps::take_frees).unwrap_or_default();
            if frees.is_empty() {
                return true;
            }
            for w in frees {
                let _ = match w {
                    Work::Free(list) => self.apply_free(cx, sum, &list),
                    Work::FreeFile { ino, mode } => self.freefile(cx, sum, ino, mode),
                    Work::Unlink { .. } => Ok(()),
                };
            }
        }
    }

    /// Allocates `size` bytes near `bpref` for file `ino` (FreeBSD's
    /// `ffs_alloc`); the caller adds them to the inode's block count. Out
    /// of space with soft updates, frees due are done first and it tries
    /// again.
    pub fn alloc(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        bpref: u64,
        size: u64,
        reserve: Reserve,
    ) -> Result<u64, Error> {
        match self.alloc_once(cx, sum, ino, bpref, size, reserve) {
            Err(Error::NoSpace) if self.reclaim(cx, sum) => {
                self.alloc_once(cx, sum, ino, bpref, size, reserve)
            }
            r => r,
        }
    }

    fn alloc_once(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        bpref: u64,
        size: u64,
        reserve: Reserve,
    ) -> Result<u64, Error> {
        let sb = &self.sb;
        if size == u64::from(sb.bsize) && sum.totals.nbfree == 0 {
            return Err(Error::NoSpace);
        }
        if reserve == Reserve::Denied
            && sb.freespace(&sum.totals) - i64::from(sb.numfrags(size)) < 0
        {
            return Err(Error::NoSpace);
        }
        let bpref = if bpref >= sb.size { 0 } else { bpref };
        let cg = if bpref == 0 {
            ino / sb.ipg
        } else {
            sb.dtog(bpref)
        };
        self.hashalloc(cg, bpref, |cg, pref| {
            self.alloccg(cx, sum, ino, cg, pref, size, size)
        })?
        .ok_or(Error::NoSpace)
    }

    /// Extends the fragments at `bprev` from `osize` to `nsize` bytes in
    /// place, if the fragments after them are free (FreeBSD's
    /// `ffs_fragextend`).
    #[allow(clippy::too_many_arguments)]
    fn fragextend(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        owner: u32,
        cg: u32,
        bprev: u64,
        osize: u64,
        nsize: u64,
    ) -> Result<bool, Error> {
        let sb = &self.sb;
        if i64::from(sum.cs[idx(cg)].nffree) < i64::from(sb.numfrags(nsize - osize)) {
            return Ok(false);
        }
        let frag = u64::from(sb.frag);
        let frags = sb.numfrags(nsize);
        // Below `fs_frag`.
        let bbase = u32::try_from(bprev % frag).unwrap_or(0);
        if u64::from(bbase) > (bprev + u64::from(frags) - 1) % frag {
            return Ok(false);
        }
        let ofrags = sb.numfrags(osize);
        let cg_at = sb.cgtod(cg) * u64::from(sb.fsize);
        self.with_cg(cx, cg, |g| {
            let bno = sb.dtogd(bprev);
            if (ofrags..frags).any(|i| !g.frag_free(bno + i)) {
                return false;
            }
            let i = (frags..sb.frag - bbase)
                .find(|&i| !g.frag_free(bno + i))
                .unwrap_or(sb.frag - bbase);
            g.add_frsum(i - ofrags, -1);
            if i != frags {
                g.add_frsum(i - frags, 1);
            }
            for i in ofrags..frags {
                g.set_frag_free(bno + i, false);
            }
            let n = frags - ofrags;
            g.add_nffree(-int32(n));
            sum.totals.nffree -= i64::from(n);
            sum.cs[idx(cg)].nffree -= int32(n);
            sum.modified = true;
            self.soft(cx, |d| d.new_block(bprev, osize, nsize, cg_at, owner));
            true
        })
    }

    /// Grows the fragments at `bprev` holding `osize` bytes of a file to
    /// `nsize` bytes (FreeBSD's `ffs_realloccg`): in place if possible,
    /// else somewhere new, where the old contents are copied (through the
    /// metadata cache for `metadata`, else straight on the disk) and the
    /// old fragments freed. Returns where they are now, and old fragments
    /// to free once the new pointer is on the disk (with soft updates;
    /// without, they are freed now); the caller adds `nsize - osize` to
    /// the inode's block count.
    #[allow(clippy::too_many_arguments)]
    pub fn realloccg(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        bprev: u64,
        bpref: u64,
        osize: u64,
        nsize: u64,
        reserve: Reserve,
        metadata: bool,
        carried: bool,
    ) -> Result<(u64, Option<(u64, u64)>), Error> {
        let once = |sum: &mut Summary| {
            self.realloccg_once(
                cx, sum, ino, bprev, bpref, osize, nsize, reserve, metadata, carried,
            )
        };
        match once(sum) {
            Err(Error::NoSpace) if self.reclaim(cx, sum) => once(sum),
            r => r,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn realloccg_once(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        bprev: u64,
        bpref: u64,
        osize: u64,
        nsize: u64,
        reserve: Reserve,
        metadata: bool,
        carried: bool,
    ) -> Result<(u64, Option<(u64, u64)>), Error> {
        let sb = &self.sb;
        if reserve == Reserve::Denied
            && sb.freespace(&sum.totals) - i64::from(sb.numfrags(nsize - osize)) < 0
        {
            return Err(Error::NoSpace);
        }
        let cg = sb.dtog(bprev);
        if self.fragextend(cx, sum, ino, cg, bprev, osize, nsize)? {
            // The fragments taken read as zeros (FreeBSD zeroes them in
            // the buffer). A directory rewrites its block itself, and a
            // caller that `carried` the data clears them in memory.
            if !metadata && !carried {
                let zeros = alloc::vec![0u8; idx64(nsize - osize)];
                self.io
                    .write_data(cx, bprev * u64::from(sb.fsize) + osize, &zeros)?;
            }
            return Ok((bprev, None));
        }
        let bpref = if bpref >= sb.size { 0 } else { bpref };
        let dsize = i64::try_from(sb.dsize).unwrap_or(i64::MAX);
        let minfree = i64::from(sb.minfree);
        let request = if sum.optim == OPTSPACE {
            if minfree > 5 && sum.totals.nffree <= dsize * minfree / 200 {
                sum.optim = 0;
            }
            nsize
        } else {
            if sum.totals.nffree >= dsize * (minfree - 2) / 100 {
                sum.optim = OPTSPACE;
            }
            u64::from(sb.bsize)
        };
        let bno = self
            .hashalloc(cg, bpref, |cg, pref| {
                self.alloccg(cx, sum, ino, cg, pref, request, nsize)
            })?
            .ok_or(Error::NoSpace)?;
        // Move the old contents; the new part reads as zeros.
        let fsize = u64::from(sb.fsize);
        if metadata {
            let mut data = alloc::vec![0u8; idx64(nsize)];
            self.io.read(cx, bprev * fsize, idx64(osize), &mut |b| {
                data[..b.len()].copy_from_slice(b)
            })?;
            self.io.modify(
                cx,
                bno * fsize,
                idx64(nsize),
                Fill::Zero,
                self.ordered(),
                &mut |b| {
                    b.copy_from_slice(&data);
                    self.soft(cx, |d| d.contents(bno, bno * fsize));
                },
            )?;
        } else if !carried {
            let mut data = alloc::vec![0u8; idx64(nsize)];
            self.io
                .read_data(cx, bprev * fsize, &mut data[..idx64(osize)])?;
            self.io.write_data(cx, bno * fsize, &data)?;
        }
        if self.soft {
            // The old fragments stay until the new pointer is on the disk;
            // their cached copy is never written again.
            if metadata {
                self.io.forget(cx, bprev * fsize, idx64(osize));
            }
            return Ok((bno, Some((bprev, osize))));
        }
        self.blkfree(cx, sum, bprev, osize)?;
        Ok((bno, None))
    }

    /// Frees `size` bytes at fragment `bno` (FreeBSD's `ffs_blkfree_cg`),
    /// dropping any cached metadata over them.
    pub fn blkfree(&self, cx: &B::Cx, sum: &mut Summary, bno: u64, size: u64) -> Result<(), Error> {
        let sb = &self.sb;
        if bno >= sb.size {
            return Err(Error::Corrupt("freeing a block past the end"));
        }
        let fsize = u64::from(sb.fsize);
        self.io.forget(cx, bno * fsize, idx64(size));
        let cg = sb.dtog(bno);
        let frag = sb.frag;
        self.with_cg(cx, cg, |g| {
            let cgbno = sb.dtogd(bno);
            if size == u64::from(sb.bsize) {
                let blk = cgbno / frag;
                if !g.is_free_block(blk) {
                    return Err(Error::Corrupt("freeing a free block"));
                }
                g.set_block(blk);
                g.clusteracct(blk, 1);
                g.add_nbfree(1);
                sum.totals.nbfree += 1;
                sum.cs[idx(cg)].nbfree += 1;
            } else {
                let bbase = cgbno - cgbno % frag;
                g.fragacct(g.blkmap(bbase), -1);
                let frags = sb.numfrags(size);
                for i in 0..frags {
                    if g.frag_free(cgbno + i) {
                        return Err(Error::Corrupt("freeing a free fragment"));
                    }
                    g.set_frag_free(cgbno + i, true);
                }
                g.add_nffree(int32(frags));
                sum.totals.nffree += i64::from(frags);
                sum.cs[idx(cg)].nffree += int32(frags);
                g.fragacct(g.blkmap(bbase), 1);
                let blk = bbase / frag;
                if g.is_block(blk) {
                    g.add_nffree(-int32(frag));
                    sum.totals.nffree -= i64::from(frag);
                    sum.cs[idx(cg)].nffree -= int32(frag);
                    g.clusteracct(blk, 1);
                    g.add_nbfree(1);
                    sum.totals.nbfree += 1;
                    sum.cs[idx(cg)].nbfree += 1;
                }
            }
            sum.modified = true;
            Ok(())
        })?
    }

    /// The group to put a new directory under `parent` in (FreeBSD's
    /// `ffs_dirpref`), as an inode number to start from.
    fn dirpref(&self, sum: &Summary, parent_ino: u32, parent: &Inode) -> u32 {
        let sb = &self.sb;
        let ncg = i64::from(sb.ncg);
        let avgifree = sum.totals.nifree / ncg;
        let avgbfree = sum.totals.nbfree / ncg;
        let avgndir = sum.totals.ndir / ncg;
        let depth = parent.dirdepth.min(31);
        let range = ncg / (1i64 << depth);
        let curcg = i64::from(parent_ino / sb.ipg);
        let numdirs = i64::from(parent.nlink).saturating_sub(1).max(0);
        // `fls(numdirs)`; `numdirs` is not negative.
        let power = 64 - i64::from(numdirs.cast_unsigned().leading_zeros());
        let (numerator, denominator) = if power == 0 {
            (1, 1)
        } else {
            ((numdirs & !(1 << (power - 1))) * 2 + 1, 1i64 << power)
        };
        let mut prefcg = curcg - range / 2 + range * numerator / denominator;
        if prefcg < 0 {
            prefcg += ncg;
        }
        if prefcg >= ncg {
            prefcg -= ncg;
        }
        if depth == 0 && parent_ino != crate::ROOT_INO {
            prefcg = curcg;
        }
        let prefcg = u32::try_from(prefcg.clamp(0, ncg - 1)).unwrap_or(0);
        let maxndir = (avgndir + (1 << depth)).min(i64::from(sb.ipg));
        let minifree = (avgifree - avgifree / 4).max(1);
        let minbfree = (avgbfree - avgbfree / 4).max(1);
        let cgsize = i64::from(sb.fsize) * i64::from(sb.fpg);
        let mut dirsize = i64::from(sb.avgfilesize) * i64::from(sb.avgfpdir);
        let curdirsize = if avgndir != 0 {
            (cgsize - avgbfree * i64::from(sb.bsize)) / avgndir
        } else {
            0
        };
        if dirsize < curdirsize {
            dirsize = curdirsize;
        }
        let mut maxcontigdirs = if dirsize <= 0 {
            0
        } else {
            ((avgbfree * i64::from(sb.bsize)) / dirsize).min(255)
        };
        if let Some(per) = sb.ipg.checked_div(sb.avgfpdir) {
            maxcontigdirs = maxcontigdirs.min(i64::from(per));
        }
        let maxcontigdirs = maxcontigdirs.max(1);
        let order = || (prefcg..sb.ncg).chain(0..prefcg);
        for cg in order() {
            let cs = &sum.cs[idx(cg)];
            if i64::from(cs.ndir) < maxndir
                && i64::from(cs.nifree) >= minifree
                && i64::from(cs.nbfree) >= minbfree
                && i64::from(sum.contigdirs[idx(cg)]) < maxcontigdirs
            {
                return sb.ipg * cg;
            }
        }
        for cg in order() {
            if i64::from(sum.cs[idx(cg)].nifree) >= avgifree {
                return sb.ipg * cg;
            }
        }
        0
    }

    /// Allocates an inode for a file of `mode` made in directory
    /// `parent_ino` (FreeBSD's `ffs_valloc`), returning its number; the
    /// caller fills in and writes the inode. `random`, called only then, seeds the generation
    /// numbers of inode blocks initialized on the way.
    pub fn valloc(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        parent_ino: u32,
        parent: &Inode,
        mode: u16,
        random: fn() -> u32,
    ) -> Result<u32, Error> {
        match self.valloc_once(cx, sum, parent_ino, parent, mode, random) {
            Err(Error::NoSpace) if self.reclaim(cx, sum) => {
                self.valloc_once(cx, sum, parent_ino, parent, mode, random)
            }
            r => r,
        }
    }

    fn valloc_once(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        parent_ino: u32,
        parent: &Inode,
        mode: u16,
        random: fn() -> u32,
    ) -> Result<u32, Error> {
        let sb = &self.sb;
        if sum.totals.nifree == 0 {
            return Err(Error::NoSpace);
        }
        let is_dir = mode & 0o170_000 == 0o040_000;
        let mut ipref = if is_dir {
            self.dirpref(sum, parent_ino, parent)
        } else {
            parent_ino
        };
        if ipref >= sb.ncg * sb.ipg {
            ipref = 0;
        }
        let cg = ipref / sb.ipg;
        let cd = &mut sum.contigdirs[idx(cg)];
        if is_dir {
            *cd = cd.saturating_add(1);
        } else {
            *cd = cd.saturating_sub(1);
        }
        self.hashalloc(cg, u64::from(ipref), |cg, pref| {
            // `pref` is an inode number, from a `u32`.
            let pref = u32::try_from(pref).unwrap_or(0);
            self.nodealloccg(cx, sum, cg, pref, is_dir, random)
                .map(|o| o.map(u64::from))
        })?
        .and_then(|i| u32::try_from(i).ok())
        .ok_or(Error::NoSpace)
    }

    /// Allocates an inode in group `cg` (FreeBSD's `ffs_nodealloccg`),
    /// first initializing the next inode block if the inode lies past
    /// those initialized (UFS2 initializes inode blocks lazily).
    fn nodealloccg(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        cg: u32,
        ipref: u32,
        is_dir: bool,
        random: fn() -> u32,
    ) -> Result<Option<u32>, Error> {
        let sb = &self.sb;
        loop {
            if sum.cs[idx(cg)].nifree == 0 {
                return Ok(None);
            }
            // Choose and take it in one visit to the group (FreeBSD's
            // `ffs_nodealloccg`), unless an inode block must be made first.
            let cg_at = sb.cgtod(cg) * u64::from(sb.fsize);
            let choice = self.with_cg(cx, cg, |g| {
                if g.cs().nifree == 0 {
                    return None;
                }
                let local = if ipref != 0 && !g.inode_used(ipref % sb.ipg) {
                    ipref % sb.ipg
                } else {
                    g.find_free_inode(g.irotor(), sb.ipg)?
                };
                if local + sb.inopb > g.initediblk() && g.initediblk() < g.niblk() {
                    return Some(Err(g.initediblk()));
                }
                g.set_irotor(local);
                g.set_inode_used(local, true);
                g.add_nifree(-1);
                if is_dir {
                    g.add_ndir(1);
                }
                self.soft(cx, |d| d.new_inode(cg * sb.ipg + local, cg_at));
                Some(Ok(local))
            })?;
            let local = match choice {
                None => return Ok(None),
                Some(Ok(local)) => local,
                Some(Err(inited)) => {
                    // Zero the next inode block, with fresh generation
                    // numbers, and write it before the group claims it.
                    let (offset, _) = sb.inode_location(cg * sb.ipg + inited);
                    let mut seed = random() | 1;
                    self.io.modify(
                        cx,
                        offset,
                        idx(sb.bsize),
                        Fill::Zero,
                        Mode::Sync,
                        &mut |b| {
                            for ino in b.as_chunks_mut::<{ crate::inode::SIZE }>().0 {
                                seed ^= seed << 13;
                                seed ^= seed >> 17;
                                seed ^= seed << 5;
                                ino[80..84].copy_from_slice(&seed.max(1).to_le_bytes());
                            }
                        },
                    )?;
                    self.with_cg(cx, cg, |g| {
                        if g.initediblk() == inited {
                            g.set_initediblk(inited + sb.inopb);
                        }
                    })?;
                    continue;
                }
            };
            sum.totals.nifree -= 1;
            sum.cs[idx(cg)].nifree -= 1;
            if is_dir {
                sum.totals.ndir += 1;
                sum.cs[idx(cg)].ndir += 1;
            }
            sum.modified = true;
            return Ok(Some(cg * sb.ipg + local));
        }
    }

    /// Frees inode `ino`, which was of `mode` (FreeBSD's `ffs_freefile`).
    pub fn freefile(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        mode: u16,
    ) -> Result<(), Error> {
        let sb = &self.sb;
        if ino >= sb.ipg * sb.ncg {
            return Err(Error::Corrupt("freeing an inode past the end"));
        }
        let cg = ino / sb.ipg;
        let local = ino % sb.ipg;
        let is_dir = mode & 0o170_000 == 0o040_000;
        self.with_cg(cx, cg, |g| {
            if !g.inode_used(local) {
                return Err(Error::Corrupt("freeing a free inode"));
            }
            g.set_inode_used(local, false);
            if local < g.irotor() {
                g.set_irotor(local);
            }
            g.add_nifree(1);
            if is_dir {
                g.add_ndir(-1);
            }
            Ok(())
        })??;
        sum.totals.nifree += 1;
        sum.cs[idx(cg)].nifree += 1;
        if is_dir {
            sum.totals.ndir -= 1;
            sum.cs[idx(cg)].ndir -= 1;
        }
        sum.modified = true;
        Ok(())
    }
}
