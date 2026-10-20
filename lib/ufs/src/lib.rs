// SPDX-License-Identifier: BSD-2-Clause
//! UFS2, FreeBSD's native file system (docs/VFS.md §10.3): the on-disk
//! format and an engine over [`BlockIo`].
//!
//! This is the read side (stage 1): the superblock and its checks,
//! inodes, block maps through the indirect blocks, directories, symbolic
//! links, holes, and `statfs`, with the superblock and inode check-hashes
//! FreeBSD verifies. The engine is synchronous; in the kernel `BlockIo` is
//! the buffer cache, on the host an image, and in the loader the firmware's
//! disk. File data in the kernel goes through the page cache: the engine
//! only maps file offsets to disk blocks ([`Ufs::bmap`]).
//!
//! UFS1 is not supported: FreeBSD has made UFS2 for twenty years.
//!
//! Behavior reference: sys/ufs/ffs/fs.h, sys/ufs/ufs/dinode.h,
//! sys/ufs/ufs/dir.h, sys/ufs/ffs/ffs_subr.c (`validate_sblock`, the
//! check-hashes), sys/ufs/ufs/ufs_bmap.c, sys/ufs/ufs/ufs_lookup.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod dir;
mod inode;
pub mod sb;

#[cfg(test)]
mod tests;

pub use dir::DirEntry;
pub use inode::{Inode, Kind, Timestamp};
pub use sb::{Stats, Superblock};

use alloc::vec::Vec;

/// What went wrong reading a file system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device could not read.
    Io,
    /// No UFS2 superblock where one belongs.
    NotUfs2,
    /// A structure failed its checks (what, for the log).
    Corrupt(&'static str),
    /// A check-hash did not match (FreeBSD's `EINTEGRITY`).
    Integrity(&'static str),
}

/// Where the bytes come from.
pub trait BlockIo {
    /// What every call needs (the kernel's `Sleepable`; `()` elsewhere).
    type Cx: ?Sized;
    /// Calls `f` with the `len` bytes at byte `offset`; both are multiples
    /// of the device's sector size.
    fn read(
        &self,
        cx: &Self::Cx,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), Error>;
}

impl<T: BlockIo + ?Sized> BlockIo for &T {
    type Cx = T::Cx;

    fn read(
        &self,
        cx: &T::Cx,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), Error> {
        (**self).read(cx, offset, len, f)
    }
}

/// An image in memory.
impl BlockIo for [u8] {
    type Cx = ();

    fn read(&self, _: &(), offset: u64, len: usize, f: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
        let start = usize::try_from(offset).map_err(|_| Error::Io)?;
        let end = start.checked_add(len).ok_or(Error::Io)?;
        f(self.get(start..end).ok_or(Error::Io)?);
        Ok(())
    }
}

/// The inode number of every file system's root.
pub const ROOT_INO: u32 = 2;
/// Direct block pointers in an inode.
pub const NDADDR: u64 = 12;
/// Indirect block pointers in an inode (single, double, triple).
pub const NIADDR: usize = 3;

/// A mounted UFS2 file system: its superblock and the device under it.
#[derive(Debug)]
pub struct Ufs<B> {
    io: B,
    sb: Superblock,
}

impl<B: BlockIo> Ufs<B> {
    /// Finds and checks the superblock (FreeBSD's `ffs_sbget`); the device
    /// has `sector`-byte sectors.
    pub fn open(cx: &B::Cx, io: B, sector: u32) -> Result<Self, Error> {
        let sb = Superblock::find(cx, &io, sector)?;
        Ok(Self { io, sb })
    }

    #[must_use]
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    #[must_use]
    pub fn io(&self) -> &B {
        &self.io
    }

    /// The counts `statfs` reports (FreeBSD's `ffs_statfs`).
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.sb.stats()
    }

    /// Inode `ino`, checked (an unallocated one has [`Inode::mode`] 0).
    pub fn inode(&self, cx: &B::Cx, ino: u32) -> Result<Inode, Error> {
        let sb = &self.sb;
        if ino >= sb.ncg.saturating_mul(sb.ipg) {
            return Err(Error::Corrupt("inode number"));
        }
        let (block, within) = sb.inode_location(ino);
        let mut out = Err(Error::Io);
        self.io.read(cx, block, sb.bsize as usize, &mut |b| {
            out = Inode::parse(&b[within..within + inode::SIZE], sb);
        })?;
        out
    }

    /// The fragment address of file block `lbn` (0 for a hole), through
    /// the indirect blocks (FreeBSD's `ufs_bmaparray`).
    pub fn bmap(&self, cx: &B::Cx, ip: &Inode, lbn: u64) -> Result<u64, Error> {
        if let Some(&addr) = usize::try_from(lbn).ok().and_then(|i| ip.db.get(i)) {
            return Ok(addr);
        }
        let nindir = u64::from(self.sb.nindir);
        let mut rest = lbn - NDADDR;
        let mut span = 1u64;
        for level in 0..NIADDR {
            span = span.saturating_mul(nindir);
            if rest < span {
                let mut addr = ip.ib[level];
                // Walk down: the index at each level, most significant first.
                let mut per = span;
                for _ in 0..=level {
                    if addr == 0 {
                        return Ok(0);
                    }
                    per /= nindir;
                    let index =
                        usize::try_from(rest / per).map_err(|_| Error::Corrupt("block number"))?;
                    rest %= per;
                    addr = self.indirect(cx, addr, index)?;
                }
                return Ok(addr);
            }
            rest -= span;
        }
        Err(Error::Corrupt("block number past triple indirect"))
    }

    /// Entry `index` of the indirect block at fragment `addr`.
    fn indirect(&self, cx: &B::Cx, addr: u64, index: usize) -> Result<u64, Error> {
        let sb = &self.sb;
        if addr >= sb.size {
            return Err(Error::Corrupt("indirect block address"));
        }
        let mut out = 0;
        self.io.read(
            cx,
            addr * u64::from(sb.fsize),
            sb.bsize as usize,
            &mut |b| {
                let at = index * 8;
                out = u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]));
            },
        )?;
        if out >= sb.size {
            return Err(Error::Corrupt("block address"));
        }
        Ok(out)
    }

    /// File block `lbn`'s fragment address and the number of blocks from
    /// it on that lie contiguously on the disk, up to `max` (FreeBSD's
    /// `VOP_BMAP` with `runp`, for clustering). A hole is `(0, 1)`.
    pub fn bmap_run(
        &self,
        cx: &B::Cx,
        ip: &Inode,
        lbn: u64,
        max: u64,
    ) -> Result<(u64, u64), Error> {
        let first = self.bmap(cx, ip, lbn)?;
        if first == 0 {
            return Ok((0, 1));
        }
        let frag = u64::from(self.sb.frag);
        let last = ip.size.div_ceil(u64::from(self.sb.bsize));
        let mut run = 1;
        while run < max && lbn + run < last {
            // Only whole blocks run on; a fragment ends the file.
            if self.sb.block_size(ip.size, lbn + run - 1) != self.sb.bsize {
                break;
            }
            if self.bmap(cx, ip, lbn + run)? != first + run * frag {
                break;
            }
            run += 1;
        }
        Ok((first, run))
    }

    /// Reads from `offset` into `buf`, up to the end of the file; holes
    /// read as zeros. Returns the bytes read.
    pub fn read(
        &self,
        cx: &B::Cx,
        ip: &Inode,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let mut done = 0;
        let mut at = offset;
        while done < buf.len() && at < ip.size {
            let lbn = at / bsize;
            let within = usize::try_from(at % bsize).unwrap_or(0);
            let size = sb.block_size(ip.size, lbn) as usize;
            let n = (size - within)
                .min(buf.len() - done)
                .min(usize::try_from(ip.size - at).unwrap_or(usize::MAX));
            let out = &mut buf[done..done + n];
            match self.bmap(cx, ip, lbn)? {
                0 => out.fill(0),
                addr => self
                    .io
                    .read(cx, addr * u64::from(sb.fsize), size, &mut |b| {
                        out.copy_from_slice(&b[within..within + n]);
                    })?,
            }
            done += n;
            at += n as u64;
        }
        Ok(done)
    }

    /// Calls `f` with each entry of directory `dir` from byte `start` on
    /// (an entry's offset, or 0), until it returns false (FreeBSD's
    /// `ufs_readdir`). Each entry carries the offset of the next.
    pub fn read_dir(
        &self,
        cx: &B::Cx,
        dir: &Inode,
        start: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Error> {
        if dir.kind() != Kind::Directory {
            return Err(Error::Corrupt("not a directory"));
        }
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        // From the start of the chunk holding `start`: entries never cross
        // one, so skipping those before `start` resynchronizes.
        let mut lbn = start / bsize;
        while lbn * bsize < dir.size {
            let size = sb.block_size(dir.size, lbn) as usize;
            let base = lbn * bsize;
            let mut block = Vec::new();
            match self.bmap(cx, dir, lbn)? {
                0 => return Err(Error::Corrupt("hole in a directory")),
                addr => self
                    .io
                    .read(cx, addr * u64::from(sb.fsize), size, &mut |b| {
                        block = b.to_vec();
                    })?,
            }
            let skip = start.saturating_sub(base) / dir::DIRBLKSIZ as u64 * dir::DIRBLKSIZ as u64;
            let limit = usize::try_from((dir.size - base).min(size as u64)).unwrap_or(size);
            let mut more = true;
            dir::entries(
                &block[..limit],
                base,
                usize::try_from(skip).unwrap_or(0),
                &mut |e| {
                    if e.offset >= start && more {
                        more = f(e);
                    }
                    more
                },
            )?;
            if !more {
                return Ok(());
            }
            lbn += 1;
        }
        Ok(())
    }

    /// The inode `name` names in directory `dir` (FreeBSD's `ufs_lookup`).
    pub fn lookup(&self, cx: &B::Cx, dir: &Inode, name: &[u8]) -> Result<Option<u32>, Error> {
        let mut found = None;
        self.read_dir(cx, dir, 0, &mut |e| {
            if e.name == name {
                found = Some(e.ino);
            }
            found.is_none()
        })?;
        Ok(found)
    }

    /// The inode at `path` (components separated by `/`), from the root;
    /// no symbolic links are followed. For tools and the loader.
    pub fn namei(&self, cx: &B::Cx, path: &[u8]) -> Result<Option<(u32, Inode)>, Error> {
        let mut ino = ROOT_INO;
        let mut ip = self.inode(cx, ino)?;
        for name in path.split(|&c| c == b'/').filter(|n| !n.is_empty()) {
            if ip.kind() != Kind::Directory {
                return Ok(None);
            }
            let Some(next) = self.lookup(cx, &ip, name)? else {
                return Ok(None);
            };
            ino = next;
            ip = self.inode(cx, ino)?;
        }
        Ok(Some((ino, ip)))
    }

    /// A symbolic link's target: in the inode when short, else in its
    /// blocks (FreeBSD's `ufs_readlink`).
    pub fn readlink(&self, cx: &B::Cx, ip: &Inode) -> Result<Vec<u8>, Error> {
        if ip.kind() != Kind::Symlink {
            return Err(Error::Corrupt("not a symbolic link"));
        }
        if ip.size < u64::from(self.sb.maxsymlinklen) {
            return Ok(ip.shortlink());
        }
        if ip.size > u64::from(self.sb.bsize) * NDADDR {
            return Err(Error::Corrupt("symbolic link size"));
        }
        let mut out = alloc::vec![0; usize::try_from(ip.size).map_err(|_| Error::Corrupt("symbolic link size"))?];
        let n = self.read(cx, ip, 0, &mut out)?;
        out.truncate(n);
        Ok(out)
    }
}
