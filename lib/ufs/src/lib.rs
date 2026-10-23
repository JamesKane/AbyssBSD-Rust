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

mod allocate;
mod balloc;
pub mod cg;
pub mod dir;
pub mod dirhash;
mod inode;
pub mod ops;
pub mod sb;
pub mod softdep;

#[cfg(test)]
mod soft_tests;
#[cfg(test)]
mod tests;

pub use allocate::{Reserve, Summary};
pub use balloc::{Moved, NewSpace};
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
    /// The device cannot write (`EROFS`).
    ReadOnly,
    /// No blocks or inodes left (`ENOSPC`).
    NoSpace,
    /// Past the largest file (`EFBIG`).
    TooBig,
    /// A directory operation's name exists already (`EEXIST`).
    Exists,
    /// A directory operation's name is not there (`ENOENT`).
    NotFound,
    /// A directory to remove still has entries (`ENOTEMPTY`).
    NotEmpty,
    /// A name longer than 255 bytes (`ENAMETOOLONG`).
    NameTooLong,
    /// A link count would pass `LINK_MAX` (`EMLINK`).
    TooManyLinks,
}

/// How a changed block goes to the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Now, before returning (`bwrite`): where order matters.
    Sync,
    /// Later (`bdwrite`).
    Delayed,
}

/// What a changed block starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// Its contents on the disk.
    Read,
    /// Zeros: the block is new and overwritten whole.
    Zero,
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

    /// Changes the `len` metadata bytes at `offset` with `f`, starting
    /// from `fill`, and writes them as `mode` says. Offsets and lengths of
    /// one block never change between calls, as with [`Self::read`].
    fn modify(
        &self,
        _cx: &Self::Cx,
        _offset: u64,
        _len: usize,
        _fill: Fill,
        _mode: Mode,
        _f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), Error> {
        Err(Error::ReadOnly)
    }

    /// Reads file data at `offset` straight from the device, past any
    /// metadata cache.
    fn read_data(&self, cx: &Self::Cx, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.read(cx, offset, buf.len(), &mut |b| buf.copy_from_slice(b))
    }

    /// Writes file data at `offset` straight to the device, now.
    fn write_data(&self, _cx: &Self::Cx, _offset: u64, _data: &[u8]) -> Result<(), Error> {
        Err(Error::ReadOnly)
    }

    /// Blocks at `offset` were freed: cached metadata over them must never
    /// be written (the blocks may hold file data next).
    fn forget(&self, _cx: &Self::Cx, _offset: u64, _len: usize) {}

    /// Writes everything delayed, then makes it durable.
    fn flush(&self, _cx: &Self::Cx) -> Result<(), Error> {
        Ok(())
    }

    /// Runs `f` with the device's soft-updates tracker locked, if it keeps
    /// one (returning false if not). `f` never calls the device.
    fn deps(&self, _cx: &Self::Cx, _f: &mut dyn FnMut(&mut softdep::Deps)) -> bool {
        false
    }

    /// Whether the device keeps cylinder groups' check-hashes itself: it
    /// checks them ([`cg::verify`]) as it reads groups from the disk and
    /// sets them ([`cg::update_ckhash`]) as it writes them, as FreeBSD's
    /// buffer cache does. Otherwise each change of a group recomputes it.
    fn hashes_groups(&self) -> bool {
        false
    }
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

    fn modify(
        &self,
        cx: &T::Cx,
        offset: u64,
        len: usize,
        fill: Fill,
        mode: Mode,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), Error> {
        (**self).modify(cx, offset, len, fill, mode, f)
    }

    fn read_data(&self, cx: &T::Cx, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        (**self).read_data(cx, offset, buf)
    }

    fn write_data(&self, cx: &T::Cx, offset: u64, data: &[u8]) -> Result<(), Error> {
        (**self).write_data(cx, offset, data)
    }

    fn forget(&self, cx: &T::Cx, offset: u64, len: usize) {
        (**self).forget(cx, offset, len);
    }

    fn flush(&self, cx: &T::Cx) -> Result<(), Error> {
        (**self).flush(cx)
    }

    fn deps(&self, cx: &T::Cx, f: &mut dyn FnMut(&mut softdep::Deps)) -> bool {
        (**self).deps(cx, f)
    }

    fn hashes_groups(&self) -> bool {
        (**self).hashes_groups()
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

// Conversions between the integer widths the format mixes. Every value
// they see is bounded by the format (block sizes, group sizes, counts in
// a block), so on the 32- and 64-bit targets built for none of them can
// truncate; they saturate rather than wrap if one ever did.

/// Bytes of a directory block [`Ufs::read_dir`] copies at a time: a page
/// of chunks.
const READ_DIR_WINDOW: usize = 4096;

/// A `u32` as an index.
pub(crate) fn idx(x: u32) -> usize {
    usize::try_from(x).unwrap_or(usize::MAX)
}

/// A `u64` offset or length within a block, as an index.
pub(crate) fn idx64(x: u64) -> usize {
    usize::try_from(x).unwrap_or(usize::MAX)
}

/// A length as a `u64`.
pub(crate) fn len64(x: usize) -> u64 {
    u64::try_from(x).unwrap_or(u64::MAX)
}

/// A `u64` count or block number as an `i64`.
pub(crate) fn int64(x: u64) -> i64 {
    i64::try_from(x).unwrap_or(i64::MAX)
}

/// An index as an `i64`.
pub(crate) fn ix64(x: usize) -> i64 {
    i64::try_from(x).unwrap_or(i64::MAX)
}

/// A small `u32` count (fragments in a block) as an `i32`.
pub(crate) fn int32(x: u32) -> i32 {
    i32::try_from(x).unwrap_or(i32::MAX)
}

/// A count or index as a `u32`.
pub(crate) fn count32(x: usize) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

/// The inode number of every file system's root.
pub const ROOT_INO: u32 = 2;
/// Direct block pointers in an inode.
/// Where a file block's pointer lies ([`Ufs::bmap`]).
enum Leaf {
    /// In the inode: the address.
    Direct(u64),
    /// Entry `index` of the indirect block at fragment `block`.
    Indirect { block: u64, index: usize },
    /// Under an indirect block not allocated.
    Hole,
}

pub const NDADDR: u64 = 12;
/// [`NDADDR`] as an index.
pub(crate) const NDIRECT: usize = 12;
/// Indirect block pointers in an inode (single, double, triple).
pub const NIADDR: usize = 3;

/// A mounted UFS2 file system: its superblock and the device under it.
#[derive(Debug)]
pub struct Ufs<B> {
    io: B,
    sb: Superblock,
    /// Writing with soft updates, through the device's tracker.
    soft: bool,
}

impl<B: BlockIo> Ufs<B> {
    /// Finds and checks the superblock (FreeBSD's `ffs_sbget`); the device
    /// has `sector`-byte sectors.
    pub fn open(cx: &B::Cx, io: B, sector: u32) -> Result<Self, Error> {
        let sb = Superblock::find(cx, &io, sector)?;
        Ok(Self {
            io,
            sb,
            soft: false,
        })
    }

    /// Writes with soft updates from now on, if the device keeps a tracker
    /// (which must start empty); returns whether it does.
    pub fn use_soft_updates(&mut self, cx: &B::Cx) -> bool {
        self.soft = self.io.deps(cx, &mut |_| {});
        self.soft
    }

    /// Changes the superblock's `fs_flags` (written with the summary).
    pub fn set_flags(&mut self, flags: u32) {
        self.sb.flags = flags;
    }

    /// Whether writes go with soft updates.
    #[must_use]
    pub fn soft_updates(&self) -> bool {
        self.soft
    }

    /// Runs `f` with the tracker, with soft updates on.
    pub(crate) fn soft<R>(&self, cx: &B::Cx, f: impl FnOnce(&mut softdep::Deps) -> R) -> Option<R> {
        if !self.soft {
            return None;
        }
        let mut f = Some(f);
        let mut out = None;
        self.io.deps(cx, &mut |d| {
            if let Some(f) = f.take() {
                out = Some(f(d));
            }
        });
        out
    }

    /// How a write whose order matters goes: now without soft updates,
    /// delayed (with dependencies) with them.
    pub(crate) fn ordered(&self) -> Mode {
        if self.soft { Mode::Delayed } else { Mode::Sync }
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
    /// With soft updates, its links dropped but not yet on the disk are
    /// gone from [`Inode::effnlink`].
    pub fn inode(&self, cx: &B::Cx, ino: u32) -> Result<Inode, Error> {
        let sb = &self.sb;
        if ino >= sb.ncg.saturating_mul(sb.ipg) {
            return Err(Error::Corrupt("inode number"));
        }
        let (block, within) = sb.inode_location(ino);
        let mut out = Err(Error::Io);
        self.io.read(cx, block, idx(sb.bsize), &mut |b| {
            out = Inode::parse(&b[within..within + inode::SIZE], sb);
        })?;
        let mut ip = out?;
        if let Some(delta) = self.soft(cx, |d| d.nlink_delta(ino)) {
            let delta = u16::try_from(delta).unwrap_or(u16::MAX);
            ip.effnlink = ip.nlink.saturating_sub(delta);
        }
        Ok(ip)
    }

    /// The fragment address of file block `lbn` (0 for a hole), through
    /// the indirect blocks (FreeBSD's `ufs_bmaparray`).
    pub fn bmap(&self, cx: &B::Cx, ip: &Inode, lbn: u64) -> Result<u64, Error> {
        match self.bmap_leaf(cx, ip, lbn)? {
            Leaf::Direct(addr) => Ok(addr),
            Leaf::Hole => Ok(0),
            Leaf::Indirect { block, index } => self.indirect(cx, block, index),
        }
    }

    /// Where file block `lbn`'s pointer lies: in the inode, or in which
    /// indirect block (walked down to it).
    fn bmap_leaf(&self, cx: &B::Cx, ip: &Inode, lbn: u64) -> Result<Leaf, Error> {
        if let Some(&addr) = usize::try_from(lbn).ok().and_then(|i| ip.db.get(i)) {
            return Ok(Leaf::Direct(addr));
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
                for step in 0..=level {
                    if addr == 0 {
                        return Ok(Leaf::Hole);
                    }
                    per /= nindir;
                    let index =
                        usize::try_from(rest / per).map_err(|_| Error::Corrupt("block number"))?;
                    rest %= per;
                    if step == level {
                        return Ok(Leaf::Indirect { block: addr, index });
                    }
                    addr = self.indirect(cx, addr, index)?;
                }
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
        self.io
            .read(cx, addr * u64::from(sb.fsize), idx(sb.bsize), &mut |b| {
                let at = index * 8;
                out = u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]));
            })?;
        if out >= sb.size {
            return Err(Error::Corrupt("block address"));
        }
        Ok(out)
    }

    /// File block `lbn`'s fragment address and the number of blocks from
    /// it on that lie contiguously on the disk, up to `max` (FreeBSD's
    /// `VOP_BMAP` with `runp`, for clustering). A hole is `(0, 1)`. As in
    /// FreeBSD, a run past the inode's pointers ends with the indirect
    /// block holding them, which is read once.
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
        let sb = &self.sb;
        let frag = u64::from(sb.frag);
        let limit = max.min(ip.size.div_ceil(u64::from(sb.bsize)).saturating_sub(lbn));
        // Only whole blocks run on; a fragment ends the file.
        let whole = |n: u64| sb.block_size(ip.size, n) == sb.bsize;
        let mut run = 1;
        while run < limit && lbn + run < NDADDR {
            if !whole(lbn + run - 1) || ip.db[idx64(lbn + run)] != first + run * frag {
                return Ok((first, run));
            }
            run += 1;
        }
        if run >= limit || !whole(lbn + run - 1) {
            return Ok((first, run));
        }
        let Leaf::Indirect { block, index } = self.bmap_leaf(cx, ip, lbn + run)? else {
            return Ok((first, run));
        };
        if block >= sb.size {
            return Err(Error::Corrupt("indirect block address"));
        }
        self.io
            .read(cx, block * u64::from(sb.fsize), idx(sb.bsize), &mut |b| {
                for at in (index * 8..b.len()).step_by(8) {
                    if run >= limit || !whole(lbn + run - 1) {
                        break;
                    }
                    let v = u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]));
                    if v != first + run * frag {
                        break;
                    }
                    run += 1;
                }
            })?;
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
            let size = idx(sb.block_size(ip.size, lbn));
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
            at += len64(n);
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
        let chunk = len64(dir::DIRBLKSIZ);
        // A window of the block at a time, copied out so `f` may use the
        // disk: a reader that takes a few entries (`getdirentries` fills
        // a page) copies a few chunks, not the whole block.
        let mut window = Vec::with_capacity(READ_DIR_WINDOW);
        // From the start of the chunk holding `start`: entries never cross
        // one, so skipping those before `start` resynchronizes.
        let mut lbn = start / bsize;
        while lbn * bsize < dir.size {
            let size = idx(sb.block_size(dir.size, lbn));
            let base = lbn * bsize;
            let addr = match self.bmap(cx, dir, lbn)? {
                0 => return Err(Error::Corrupt("hole in a directory")),
                addr => addr * u64::from(sb.fsize),
            };
            let limit = usize::try_from((dir.size - base).min(len64(size))).unwrap_or(size);
            let mut from = usize::try_from(start.saturating_sub(base) / chunk * chunk).unwrap_or(0);
            while from < limit {
                let to = (from + READ_DIR_WINDOW).min(limit);
                window.clear();
                self.io.read(cx, addr, size, &mut |b| {
                    window.extend_from_slice(&b[from..to]);
                })?;
                let mut more = true;
                dir::entries(&window, base + len64(from), 0, &mut |e| {
                    if e.offset >= start && more {
                        more = f(e);
                    }
                    more
                })?;
                if !more {
                    return Ok(());
                }
                from = to;
            }
            lbn += 1;
        }
        Ok(())
    }

    /// The inode `name` names in directory `dir` (FreeBSD's `ufs_lookup`).
    pub fn lookup(&self, cx: &B::Cx, dir: &Inode, name: &[u8]) -> Result<Option<u32>, Error> {
        if let Some(h) = &dir.dirhash {
            return Ok(h.names.get(name).map(|&(ino, _)| ino));
        }
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
