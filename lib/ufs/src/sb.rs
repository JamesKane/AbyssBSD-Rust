// SPDX-License-Identifier: BSD-2-Clause
//! The superblock (`struct fs`): where it is, what it says, and FreeBSD's
//! checks of it (`validate_sblock`, the check-hash).

use crate::{BlockIo, Error};
use crc::crc32c;

/// `FS_UFS2_MAGIC`.
pub const MAGIC: u32 = 0x1954_0119;
/// Where the UFS2 superblock lives (`SBLOCK_UFS2`). FreeBSD searches
/// other places too (`SBLOCKSEARCH`), for UFS1 and alternates; only a
/// UFS2 one at its own place counts here.
pub const SBLOCK: u64 = 65536;
/// The most a superblock occupies (`SBLOCKSIZE`).
pub const SBLOCKSIZE: usize = 8192;
const SBLOCKSIZE_U32: u32 = 8192;
/// `sizeof(struct ufs2_dinode)`.
const INODE_SIZE: u32 = 256;
/// The longest symbolic link kept in the inode: the address fields
/// (`(UFS_NDADDR + UFS_NIADDR) * sizeof(ufs2_daddr_t)`).
const MAXSYMLINKLEN: u32 = 120;
/// `sizeof(struct fs)`.
const FS_SIZE: u32 = 1376;
const MINBSIZE: u32 = 4096;
const MAXBSIZE: u32 = 65536;
const MAXFRAG: u32 = 8;
const FS_MAXCONTIG: u32 = 16;

/// `fs_flags` bits.
pub const FS_UNCLEAN: u32 = 0x01;
pub const FS_DOSOFTDEP: u32 = 0x02;
pub const FS_NEEDSFSCK: u32 = 0x04;
pub const FS_SUJ: u32 = 0x08;
pub const FS_ACLS: u32 = 0x10;
pub const FS_MULTILABEL: u32 = 0x20;
pub const FS_GJOURNAL: u32 = 0x40;
pub const FS_NFS4ACLS: u32 = 0x100;
pub const FS_METACKHASH: u32 = 0x200;
pub const FS_TRIM: u32 = 0x400;

/// `fs_metackhash` bits: which structures carry check-hashes.
pub const CK_SUPERBLOCK: u32 = 0x01;
pub const CK_CYLGRP: u32 = 0x02;
pub const CK_INODE: u32 = 0x04;

/// Field offsets in `struct fs` (checked against FreeBSD 15.1's headers).
mod off {
    pub(super) const SBLKNO: usize = 8;
    pub(super) const CBLKNO: usize = 12;
    pub(super) const IBLKNO: usize = 16;
    pub(super) const DBLKNO: usize = 20;
    pub(super) const NCG: usize = 44;
    pub(super) const BSIZE: usize = 48;
    pub(super) const FSIZE: usize = 52;
    pub(super) const FRAG: usize = 56;
    pub(super) const MINFREE: usize = 60;
    pub(super) const MAXCONTIG: usize = 88;
    pub(super) const MAXBPG: usize = 92;
    pub(super) const BMASK: usize = 72;
    pub(super) const BSHIFT: usize = 80;
    pub(super) const FSHIFT: usize = 84;
    pub(super) const FRAGSHIFT: usize = 96;
    pub(super) const SBSIZE: usize = 104;
    pub(super) const NINDIR: usize = 116;
    pub(super) const INOPB: usize = 120;
    pub(super) const OPTIM: usize = 128;
    pub(super) const ID: usize = 144;
    pub(super) const CSSIZE: usize = 156;
    pub(super) const CGSIZE: usize = 160;
    pub(super) const IPG: usize = 184;
    pub(super) const FPG: usize = 188;
    pub(super) const FMOD: usize = 208;
    pub(super) const CLEAN: usize = 209;
    pub(super) const FSMNT: usize = 212;
    pub(super) const VOLNAME: usize = 680;
    pub(super) const MAXBSIZE: usize = 860;
    pub(super) const METASPACE: usize = 880;
    pub(super) const SBLOCKACTUALLOC: usize = 992;
    pub(super) const SBLOCKLOC: usize = 1000;
    pub(super) const CSTOTAL: usize = 1008;
    pub(super) const TIME: usize = 1072;
    pub(super) const SIZE: usize = 1080;
    pub(super) const DSIZE: usize = 1088;
    pub(super) const CSADDR: usize = 1096;
    pub(super) const PENDINGBLOCKS: usize = 1104;
    pub(super) const PENDINGINODES: usize = 1112;
    pub(super) const AVGFILESIZE: usize = 1196;
    pub(super) const AVGFPDIR: usize = 1200;
    pub(super) const CKHASH: usize = 1304;
    pub(super) const METACKHASH: usize = 1308;
    pub(super) const FLAGS: usize = 1312;
    pub(super) const CONTIGSUMSIZE: usize = 1316;
    pub(super) const MAXSYMLINKLEN: usize = 1320;
    pub(super) const MAXFILESIZE: usize = 1328;
    pub(super) const MAGIC: usize = 1372;
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn i64_at(b: &[u8], at: usize) -> i64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    i64::from_le_bytes(x)
}

/// The summary counts (`struct csum_total`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub ndir: i64,
    pub nbfree: i64,
    pub nifree: i64,
    pub nffree: i64,
}

/// What `statfs` reports, in fragments (FreeBSD's `ffs_statfs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `f_bsize`: the fragment size.
    pub fsize: u64,
    /// `f_iosize`: the block size.
    pub iosize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: i64,
    pub files: u64,
    pub ffree: i64,
}

/// A checked UFS2 superblock: the fields the engine uses.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Superblock {
    /// Offsets in fragments from a cylinder group's start: its superblock
    /// copy, its header, its inodes, and its data.
    pub sblkno: u32,
    pub cblkno: u32,
    pub iblkno: u32,
    pub dblkno: u32,
    pub ncg: u32,
    pub bsize: u32,
    pub fsize: u32,
    pub frag: u32,
    pub minfree: u32,
    pub bshift: u32,
    pub fshift: u32,
    pub fragshift: u32,
    pub sbsize: u32,
    /// Block addresses per indirect block, and inodes per block.
    pub nindir: u32,
    pub inopb: u32,
    pub id: [u32; 2],
    pub cssize: u32,
    pub cgsize: u32,
    /// Inodes and fragments per cylinder group.
    pub ipg: u32,
    pub fpg: u32,
    /// `fs_clean`: unmounted cleanly.
    pub clean: bool,
    /// Where it was last mounted, and the volume's name.
    pub fsmnt: [u8; 468],
    pub volname: [u8; 32],
    pub maxbsize: u32,
    pub totals: Totals,
    /// Last written, in seconds since the epoch.
    pub time: i64,
    /// Fragments in the file system, and those holding data.
    pub size: u64,
    pub dsize: u64,
    pub csaddr: u64,
    pub pending_blocks: i64,
    pub pending_inodes: u32,
    pub flags: u32,
    /// Which structures carry check-hashes (0 if the file system was
    /// last mounted by a kernel that does not keep them).
    pub metackhash: u32,
    pub maxsymlinklen: u32,
    pub maxfilesize: u64,
    /// Blocks laid out together, and the most blocks a file puts in one
    /// cylinder group before moving on.
    pub maxcontig: u32,
    pub maxbpg: u32,
    /// `FS_OPTTIME` (0) or `FS_OPTSPACE` (1).
    pub optim: u32,
    /// Fragments at the start of each group's data reserved for
    /// metadata (`fs_metaspace`).
    pub metaspace: u64,
    pub avgfilesize: u32,
    pub avgfpdir: u32,
    /// The largest cluster size the groups' cluster summaries count.
    pub contigsumsize: u32,
    /// The superblock as read ([`SBLOCKSIZE`] bytes), for writing back.
    pub raw: alloc::vec::Vec<u8>,
}

impl Superblock {
    /// Reads the UFS2 superblock and checks it.
    pub fn find<B: BlockIo + ?Sized>(cx: &B::Cx, io: &B, sector: u32) -> Result<Self, Error> {
        let mut r = Err(Error::NotUfs2);
        // A device too small to hold one is not UFS2 either.
        io.read(cx, SBLOCK, SBLOCKSIZE, &mut |b| {
            r = Self::parse(b, SBLOCK, sector)
        })
        .map_err(|_| Error::NotUfs2)?;
        r
    }

    /// Parses and checks the superblock read at byte `at` (FreeBSD's
    /// `readsuper` and `validate_sblock`, for UFS2).
    pub fn parse(b: &[u8], at: u64, sector: u32) -> Result<Self, Error> {
        if b.len() < SBLOCKSIZE || u32_at(b, off::MAGIC) != MAGIC {
            return Err(Error::NotUfs2);
        }
        let u = |o| u32_at(b, o);
        let i = |o| i64_at(b, o);
        let unsigned = |v: i64, what| u64::try_from(v).map_err(|_| Error::Corrupt(what));
        let mut fsmnt = [0; 468];
        fsmnt.copy_from_slice(&b[off::FSMNT..off::FSMNT + 468]);
        let mut volname = [0; 32];
        volname.copy_from_slice(&b[off::VOLNAME..off::VOLNAME + 32]);
        let flags = u(off::FLAGS);
        let mut metackhash = u(off::METACKHASH);
        let sb = Self {
            sblkno: u(off::SBLKNO),
            cblkno: u(off::CBLKNO),
            iblkno: u(off::IBLKNO),
            dblkno: u(off::DBLKNO),
            ncg: u(off::NCG),
            bsize: u(off::BSIZE),
            fsize: u(off::FSIZE),
            frag: u(off::FRAG),
            minfree: u(off::MINFREE),
            bshift: u(off::BSHIFT),
            fshift: u(off::FSHIFT),
            fragshift: u(off::FRAGSHIFT),
            sbsize: u(off::SBSIZE),
            nindir: u(off::NINDIR),
            inopb: u(off::INOPB),
            id: [u(off::ID), u(off::ID + 4)],
            cssize: u(off::CSSIZE),
            cgsize: u(off::CGSIZE),
            ipg: u(off::IPG),
            fpg: u(off::FPG),
            clean: b[off::CLEAN] != 0,
            fsmnt,
            volname,
            maxbsize: u(off::MAXBSIZE),
            totals: Totals {
                ndir: i(off::CSTOTAL),
                nbfree: i(off::CSTOTAL + 8),
                nifree: i(off::CSTOTAL + 16),
                nffree: i(off::CSTOTAL + 24),
            },
            time: i(off::TIME),
            size: unsigned(i(off::SIZE), "fs_size")?,
            dsize: unsigned(i(off::DSIZE), "fs_dsize")?,
            csaddr: unsigned(i(off::CSADDR), "fs_csaddr")?,
            pending_blocks: i(off::PENDINGBLOCKS),
            pending_inodes: u(off::PENDINGINODES),
            flags,
            metackhash,
            maxsymlinklen: u(off::MAXSYMLINKLEN),
            maxfilesize: unsigned(i(off::MAXFILESIZE), "fs_maxfilesize")?,
            maxcontig: u(off::MAXCONTIG),
            maxbpg: u(off::MAXBPG).max(1),
            optim: u(off::OPTIM),
            metaspace: unsigned(i(off::METASPACE), "fs_metaspace")?,
            avgfilesize: u(off::AVGFILESIZE),
            avgfpdir: u(off::AVGFPDIR),
            contigsumsize: u(off::CONTIGSUMSIZE),
            raw: b[..SBLOCKSIZE].to_vec(),
        };
        sb.validate(b, at, sector)?;
        // A file system last mounted by a kernel without check-hashes:
        // they are stale (FreeBSD's `readsuper`).
        if flags & FS_METACKHASH == 0 {
            metackhash = 0;
        }
        if metackhash & CK_SUPERBLOCK != 0 {
            let sbsize = crate::idx(sb.sbsize);
            let mut crc = crc32c(!0, &b[..off::CKHASH]);
            crc = crc32c(crc, &[0; 4]);
            crc = crc32c(crc, &b[off::CKHASH + 4..sbsize]);
            if crc != u(off::CKHASH) {
                return Err(Error::Integrity("superblock check-hash"));
            }
        }
        Ok(Self { metackhash, ..sb })
    }

    /// FreeBSD's `validate_sblock` checks, for a UFS2 superblock.
    fn validate(&self, b: &[u8], at: u64, sector: u32) -> Result<(), Error> {
        let check = |ok: bool, what: &'static str| {
            if ok {
                Ok(())
            } else {
                Err(Error::Corrupt(what))
            }
        };
        let pow2 = |x: u32| x != 0 && x.is_power_of_two();
        let sblock = SBLOCK.cast_signed();
        check(i64_at(b, off::SBLOCKLOC) == sblock, "fs_sblockloc")?;
        let actual = i64_at(b, off::SBLOCKACTUALLOC);
        check(
            at == SBLOCK && (actual == 0 || actual == sblock),
            "superblock location",
        )?;
        check(
            (MINBSIZE..=MAXBSIZE).contains(&self.bsize) && pow2(self.bsize),
            "fs_bsize",
        )?;
        check((1..=MAXFRAG).contains(&self.frag), "fs_frag")?;
        check(
            pow2(self.fsize) && self.fsize >= sector && self.fsize * self.frag == self.bsize,
            "fs_fsize",
        )?;
        check(self.bshift == self.bsize.trailing_zeros(), "fs_bshift")?;
        check(self.fshift == self.fsize.trailing_zeros(), "fs_fshift")?;
        check(self.fragshift == self.frag.trailing_zeros(), "fs_fragshift")?;
        check(u32_at(b, off::BMASK) == !(self.bsize - 1), "fs_bmask")?;
        check(
            (FS_SIZE..=SBLOCKSIZE_U32).contains(&self.sbsize) && self.sbsize.is_multiple_of(sector),
            "fs_sbsize",
        )?;
        check(self.fpg >= 3 * self.frag && self.ncg >= 1, "fs_fpg")?;
        check(self.nindir == self.bsize / 8, "fs_nindir")?;
        check(self.inopb == self.bsize / INODE_SIZE, "fs_inopb")?;
        check(self.ipg >= self.inopb, "fs_ipg")?;
        check(
            self.iblkno < self.fpg
                && u64::from(self.iblkno) + u64::from(self.ipg / self.inopb) * u64::from(self.frag)
                    <= u64::from(self.fpg),
            "fs_iblkno",
        )?;
        check(self.maxsymlinklen == MAXSYMLINKLEN, "fs_maxsymlinklen")?;
        let fpg = u64::from(self.fpg);
        let ncg = u64::from(self.ncg);
        check(
            self.size >= 8 * u64::from(self.frag)
                && self.size > (ncg - 1) * fpg
                && self.size <= ncg * fpg,
            "fs_size",
        )?;
        check(self.dsize <= self.size, "fs_dsize")?;
        let inodes = u64::from(self.ipg) * ncg;
        check(
            u64::try_from(self.totals.nifree).is_ok_and(|n| n <= inodes),
            "cs_nifree",
        )?;
        check(
            u64::try_from(self.totals.ndir).is_ok_and(|n| n <= inodes),
            "cs_ndir",
        )?;
        let maxbsize = if self.maxbsize == 0 {
            self.bsize
        } else {
            self.maxbsize
        };
        check(
            maxbsize >= self.bsize && pow2(maxbsize) && maxbsize <= FS_MAXCONTIG * self.bsize,
            "fs_maxbsize",
        )?;
        check(
            self.csaddr + u64::from(self.cssize).div_ceil(u64::from(self.fsize)) <= self.size,
            "fs_csaddr",
        )?;
        Ok(())
    }

    /// The byte offset of the block holding inode `ino`, and the inode's
    /// offset within it (FreeBSD's `ino_to_fsba` and `ino_to_fsbo`).
    #[must_use]
    pub fn inode_location(&self, ino: u32) -> (u64, usize) {
        let cg = u64::from(ino / self.ipg);
        let within_cg = ino % self.ipg;
        let frag = cg * u64::from(self.fpg)
            + u64::from(self.iblkno)
            + u64::from(within_cg / self.inopb) * u64::from(self.frag);
        let within = crate::idx(within_cg % self.inopb) * crate::inode::SIZE;
        (frag * u64::from(self.fsize), within)
    }

    /// The size of file block `lbn` of a file of `size` bytes: a whole
    /// block, or for the last of the direct blocks, only the fragments it
    /// needs (FreeBSD's `blksize`).
    #[must_use]
    pub fn block_size(&self, size: u64, lbn: u64) -> u32 {
        let bsize = u64::from(self.bsize);
        if lbn >= crate::NDADDR || size >= (lbn + 1) * bsize {
            return self.bsize;
        }
        let tail = size % bsize;
        let fsize = u64::from(self.fsize);
        // `fragroundup`; never less than a fragment.
        u32::try_from((tail.div_ceil(fsize) * fsize).max(fsize)).unwrap_or(self.bsize)
    }

    /// The counts `statfs` reports (FreeBSD's `ffs_statfs`).
    #[must_use]
    pub fn stats(&self) -> Stats {
        let frag = i64::from(self.frag);
        let t = &self.totals;
        // Pending counts are in device blocks (512 bytes).
        let pending = self.pending_blocks / (i64::from(self.fsize) / 512).max(1);
        let free = t.nbfree * frag + t.nffree;
        let dsize = i64::try_from(self.dsize).unwrap_or(i64::MAX);
        Stats {
            fsize: u64::from(self.fsize),
            iosize: u64::from(self.bsize),
            blocks: self.dsize,
            bfree: u64::try_from(free + pending).unwrap_or(0),
            bavail: free - dsize * i64::from(self.minfree) / 100 + pending,
            files: u64::from(self.ncg) * u64::from(self.ipg) - u64::from(crate::ROOT_INO),
            ffree: t.nifree + i64::from(self.pending_inodes),
        }
    }

    /// The superblock to write back: as read, with the summary `totals`,
    /// the clean flag, the time written (`fs_time`), and the check-hash
    /// recomputed (FreeBSD's `ffs_sbupdate`).
    #[must_use]
    pub fn encode(
        &self,
        totals: &Totals,
        clean: bool,
        time: i64,
        optim: u32,
    ) -> alloc::vec::Vec<u8> {
        let mut b = self.raw.clone();
        let put64 =
            |b: &mut [u8], at: usize, v: i64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put64(&mut b, off::CSTOTAL, totals.ndir);
        put64(&mut b, off::CSTOTAL + 8, totals.nbfree);
        put64(&mut b, off::CSTOTAL + 16, totals.nifree);
        put64(&mut b, off::CSTOTAL + 24, totals.nffree);
        put64(&mut b, off::TIME, time);
        b[off::OPTIM..off::OPTIM + 4].copy_from_slice(&optim.to_le_bytes());
        b[off::CLEAN] = u8::from(clean);
        b[off::FMOD] = 0;
        if self.metackhash & CK_SUPERBLOCK != 0 {
            b[off::CKHASH..off::CKHASH + 4].fill(0);
            let crc = crc32c(!0, &b[..crate::idx(self.sbsize)]);
            b[off::CKHASH..off::CKHASH + 4].copy_from_slice(&crc.to_le_bytes());
        }
        b
    }

    // Addresses, in fragments (FreeBSD's fs.h macros, for UFS2).

    /// The first fragment of cylinder group `cg` (`cgbase`, `cgstart`).
    #[must_use]
    pub fn cgbase(&self, cg: u32) -> u64 {
        u64::from(self.fpg) * u64::from(cg)
    }

    /// Where group `cg`'s header lives (`cgtod`).
    #[must_use]
    pub fn cgtod(&self, cg: u32) -> u64 {
        self.cgbase(cg) + u64::from(self.cblkno)
    }

    /// Group `cg`'s first data fragment (`cgdmin`, also `cgmeta`).
    #[must_use]
    pub fn cgdmin(&self, cg: u32) -> u64 {
        self.cgbase(cg) + u64::from(self.dblkno)
    }

    /// Group `cg`'s data zone, past the metadata reserve (`cgdata`).
    #[must_use]
    pub fn cgdata(&self, cg: u32) -> u64 {
        self.cgdmin(cg) + self.metaspace
    }

    /// The group fragment `d` lies in (`dtog`).
    #[must_use]
    pub fn dtog(&self, d: u64) -> u32 {
        u32::try_from(d / u64::from(self.fpg)).unwrap_or(u32::MAX)
    }

    /// Fragment `d`'s place within its group (`dtogd`).
    #[must_use]
    pub fn dtogd(&self, d: u64) -> u32 {
        u32::try_from(d % u64::from(self.fpg)).unwrap_or(0)
    }

    /// Fragments in `bytes` (`numfrags`).
    #[must_use]
    pub fn numfrags(&self, bytes: u64) -> u32 {
        u32::try_from(bytes >> self.fshift).unwrap_or(u32::MAX)
    }

    /// `bytes` rounded up to whole fragments (`fragroundup`).
    #[must_use]
    pub fn fragroundup(&self, bytes: u64) -> u64 {
        bytes.next_multiple_of(u64::from(self.fsize))
    }

    /// Free space counting the reserve (`freespace(fs, minfree)`), in
    /// fragments.
    #[must_use]
    pub fn freespace(&self, totals: &Totals) -> i64 {
        totals.nbfree * i64::from(self.frag) + totals.nffree
            - i64::try_from(self.dsize).unwrap_or(i64::MAX) * i64::from(self.minfree) / 100
    }

    /// The volume label, up to its NUL.
    #[must_use]
    pub fn label(&self) -> &[u8] {
        let end = self.volname.iter().position(|&c| c == 0).unwrap_or(32);
        &self.volname[..end]
    }
}
