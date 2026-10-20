// SPDX-License-Identifier: BSD-2-Clause
//! File operations over the engine: making, linking, unlinking, and
//! renaming, with the link counts and the write order FreeBSD's UFS keeps
//! without soft updates (sys/ufs/ufs/ufs_vnops.c: `ufs_makeinode`,
//! `ufs_mkdir`, `ufs_link`, `ufs_remove`, `ufs_rmdir`, `ufs_rename`).
//!
//! An inode is written (now) before a directory entry names it, and a
//! directory entry is removed before the link count drops, so a crash
//! leaves at worst a count too high or an inode no entry names, which
//! fsck repairs. The caller holds the vnode locks and has checked
//! permissions and names; each operation takes the inodes it changes by
//! `&mut` and leaves them written.

use alloc::vec::Vec;

use crate::dir::{DIRBLKSIZ, dtype_of};
use crate::inode::{Inode, Kind, Timestamp};
use crate::{BlockIo, Error, Mode, Reserve, Summary, Ufs, idx64, len64};

/// FreeBSD's `UFS_LINK_MAX`.
pub const LINK_MAX: u16 = 32767;

/// What a new file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewKind<'a> {
    Regular,
    Directory,
    Symlink(&'a [u8]),
    /// A character (or block) device node, with its number.
    Device(u64),
    Fifo,
    Socket,
}

impl NewKind<'_> {
    const fn ifmt(&self) -> u16 {
        match self {
            Self::Regular => 0o100_000,
            Self::Directory => 0o040_000,
            Self::Symlink(_) => 0o120_000,
            Self::Device(_) => 0o020_000,
            Self::Fifo => 0o010_000,
            Self::Socket => 0o140_000,
        }
    }
}

/// Who makes a file, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Maker {
    /// Permission bits (already masked).
    pub perm: u16,
    pub uid: u32,
    pub gid: u32,
    pub now: Timestamp,
    /// For the generation number.
    pub random: u32,
    pub reserve: Reserve,
}

impl<B: BlockIo> Ufs<B> {
    /// Makes a file of `kind` named `name` in directory `dir_ino`
    /// (FreeBSD's `ufs_makeinode` and `ufs_mkdir`), returning its number
    /// and inode, both written.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        dir_ino: u32,
        dir: &mut Inode,
        name: &[u8],
        kind: NewKind<'_>,
        who: &Maker,
    ) -> Result<(u32, Inode), Error> {
        let is_dir = kind == NewKind::Directory;
        if is_dir && dir.nlink >= LINK_MAX {
            return Err(Error::TooManyLinks);
        }
        let mode = kind.ifmt() | (who.perm & 0o7777);
        let ino = self.valloc(cx, sum, dir_ino, dir, mode, who.random)?;
        let made = self.make_inode(cx, sum, ino, dir_ino, dir, mode, kind, who);
        let ip = match made {
            Ok(ip) => ip,
            Err(e) => {
                let _ = self.freefile(cx, sum, ino, mode);
                return Err(e);
            }
        };
        if let Err(e) = self.dir_enter(
            cx,
            sum,
            dir_ino,
            dir,
            name,
            ino,
            dtype_of(mode),
            who.reserve,
        ) {
            // Undo: the new inode goes, and so does the parent's link.
            let mut ip = ip;
            if is_dir {
                dir.nlink -= 1;
                let _ = self.write_inode(cx, dir_ino, dir, Mode::Sync);
            }
            ip.nlink = 0;
            let _ = self.free_inode(cx, sum, ino, &mut ip, who.reserve);
            return Err(e);
        }
        dir.mtime = who.now;
        dir.ctime = who.now;
        self.write_inode(cx, dir_ino, dir, Mode::Delayed)?;
        Ok((ino, ip))
    }

    /// Fills in, and writes, the new inode `ino` (and for a directory its
    /// first block and its parent's link).
    #[allow(clippy::too_many_arguments)]
    fn make_inode(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        dir_ino: u32,
        dir: &mut Inode,
        mode: u16,
        kind: NewKind<'_>,
        who: &Maker,
    ) -> Result<Inode, Error> {
        let mut ip = self.inode(cx, ino)?;
        if ip.mode != 0 {
            return Err(Error::Corrupt("allocated a used inode"));
        }
        // A new generation number, never 0 (FreeBSD's `ffs_valloc`).
        let mut generation = ip.generation.wrapping_add(1);
        if generation == 0 {
            generation = who.random | 1;
        }
        let raw = ip.raw;
        ip = Inode {
            mode,
            nlink: 1,
            uid: who.uid,
            gid: who.gid,
            blksize: 0,
            size: 0,
            blocks: 0,
            atime: who.now,
            mtime: who.now,
            ctime: who.now,
            birthtime: who.now,
            generation,
            flags: 0,
            extsize: 0,
            db: [0; 12],
            ib: [0; crate::NIADDR],
            dirdepth: 0,
            raw,
            dirhash: None,
        };
        // What the raw bytes held past the decoded fields goes too.
        ip.raw[84..88].fill(0);
        ip.raw[96..112].fill(0);
        ip.raw[232..240].fill(0);
        match kind {
            NewKind::Directory => {
                ip.nlink = 2;
                ip.dirdepth = dir.dirdepth + 1;
                // The parent's link first, then the new directory's block.
                self.write_inode(cx, ino, &ip, Mode::Sync)?;
                dir.nlink += 1;
                self.write_inode(cx, dir_ino, dir, Mode::Sync)?;
                if let Err(e) = self.dir_init(cx, sum, ino, &mut ip, dir_ino, who.reserve) {
                    dir.nlink -= 1;
                    let _ = self.write_inode(cx, dir_ino, dir, Mode::Sync);
                    let _ = self.truncate(cx, sum, ino, &mut ip, 0, who.reserve, true);
                    return Err(e);
                }
            }
            NewKind::Symlink(target) => {
                if target.len() >= usize::from(u16::MAX) || target.is_empty() {
                    return Err(Error::NameTooLong);
                }
                if len64(target.len()) < u64::from(self.sb.maxsymlinklen) {
                    ip.set_shortlink(target);
                } else {
                    self.write_data_range(cx, sum, ino, &mut ip, 0, target, who.reserve)?;
                }
            }
            NewKind::Device(rdev) => ip.db[0] = rdev,
            NewKind::Regular | NewKind::Fifo | NewKind::Socket => {}
        }
        self.write_inode(cx, ino, &ip, Mode::Sync)?;
        Ok(ip)
    }

    /// Writes `data` at `offset` of file `ino`, allocating as it goes,
    /// and grows the size (for long symbolic links and tools; the kernel
    /// writes file data through its page cache).
    #[allow(clippy::too_many_arguments)]
    pub fn write_data_range(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        offset: u64,
        data: &[u8],
        reserve: Reserve,
    ) -> Result<(), Error> {
        let bsize = u64::from(self.sb.bsize);
        let fsize = u64::from(self.sb.fsize);
        let mut done = 0usize;
        while done < data.len() {
            let at = offset + len64(done);
            let within = at % bsize;
            let n = idx64(bsize - within).min(data.len() - done);
            let addr = self.balloc(cx, sum, ino, ip, at, len64(n), reserve, true, false)?;
            if ip.size < at + len64(n) {
                ip.size = at + len64(n);
            }
            // Whole fragments: read what is there, change, write back.
            let first = within - within % fsize;
            let end = (within + len64(n)).next_multiple_of(fsize);
            let mut buf = alloc::vec![0u8; idx64(end - first)];
            self.io.read_data(cx, addr * fsize + first, &mut buf)?;
            let from = idx64(within - first);
            buf[from..from + n].copy_from_slice(&data[done..done + n]);
            self.io.write_data(cx, addr * fsize + first, &buf)?;
            done += n;
        }
        Ok(())
    }

    /// Links existing non-directory `ino` as `name` in directory
    /// `dir_ino` (FreeBSD's `ufs_link`): the count goes up and is written
    /// before the entry is made.
    #[allow(clippy::too_many_arguments)]
    pub fn link(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        dir_ino: u32,
        dir: &mut Inode,
        name: &[u8],
        ino: u32,
        ip: &mut Inode,
        now: Timestamp,
        reserve: Reserve,
    ) -> Result<(), Error> {
        if ip.nlink >= LINK_MAX {
            return Err(Error::TooManyLinks);
        }
        ip.nlink += 1;
        ip.ctime = now;
        self.write_inode(cx, ino, ip, Mode::Sync)?;
        if let Err(e) = self.dir_enter(cx, sum, dir_ino, dir, name, ino, dtype_of(ip.mode), reserve)
        {
            ip.nlink -= 1;
            let _ = self.write_inode(cx, ino, ip, Mode::Sync);
            return Err(e);
        }
        dir.mtime = now;
        dir.ctime = now;
        self.write_inode(cx, dir_ino, dir, Mode::Delayed)
    }

    /// Removes entry `name` (for non-directory `ino`) from directory
    /// `dir_ino` (FreeBSD's `ufs_remove`): the entry goes first, then the
    /// count drops. At no links, the caller frees the inode once nothing
    /// uses it ([`Self::free_inode`]).
    #[allow(clippy::too_many_arguments)]
    pub fn unlink(
        &self,
        cx: &B::Cx,
        dir_ino: u32,
        dir: &mut Inode,
        name: &[u8],
        ino: u32,
        ip: &mut Inode,
        now: Timestamp,
    ) -> Result<(), Error> {
        self.dir_remove(cx, dir, name)?;
        dir.mtime = now;
        dir.ctime = now;
        self.write_inode(cx, dir_ino, dir, Mode::Delayed)?;
        ip.nlink = ip.nlink.saturating_sub(1);
        ip.ctime = now;
        self.write_inode(cx, ino, ip, Mode::Sync)
    }

    /// Removes empty directory `ino`, named `name` in `dir_ino` (FreeBSD's
    /// `ufs_rmdir`): the entry goes, the parent loses the child's `..`,
    /// and the directory's blocks are freed now; the caller frees the
    /// inode once nothing uses it.
    #[allow(clippy::too_many_arguments)]
    pub fn rmdir(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        dir_ino: u32,
        dir: &mut Inode,
        name: &[u8],
        ino: u32,
        ip: &mut Inode,
        now: Timestamp,
    ) -> Result<(), Error> {
        if !self.dir_empty(cx, ip)? {
            return Err(Error::NotEmpty);
        }
        self.dir_remove(cx, dir, name)?;
        dir.nlink = dir.nlink.saturating_sub(1);
        dir.mtime = now;
        dir.ctime = now;
        self.write_inode(cx, dir_ino, dir, Mode::Sync)?;
        ip.nlink = 0;
        ip.ctime = now;
        ip.dirhash = None;
        self.truncate(cx, sum, ino, ip, 0, Reserve::Allowed, true)?;
        self.write_inode(cx, ino, ip, Mode::Sync)
    }

    /// Frees inode `ino`, which no entry names and nothing uses: its
    /// blocks, then the inode itself (FreeBSD's `ufs_inactive`).
    pub fn free_inode(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        reserve: Reserve,
    ) -> Result<(), Error> {
        let mode = ip.mode;
        let dir = ip.kind() == Kind::Directory;
        if matches!(ip.kind(), Kind::Regular | Kind::Directory)
            || (ip.kind() == Kind::Symlink && ip.size >= u64::from(self.sb.maxsymlinklen))
        {
            self.truncate(cx, sum, ino, ip, 0, reserve, dir)?;
        }
        ip.mode = 0;
        ip.size = 0;
        ip.blocks = 0;
        ip.dirhash = None;
        ip.db = [0; 12];
        ip.ib = [0; crate::NIADDR];
        self.write_inode(cx, ino, ip, Mode::Sync)?;
        self.freefile(cx, sum, ino, mode)
    }

    /// Moves `from_name` (inode `ino`) in `from_dir` to `to_name` in
    /// `to_dir` (FreeBSD's `ufs_rename`): the moved inode's count goes up
    /// while it is linked twice; an existing target (`target`, already
    /// checked: same kind, an empty directory) is replaced in place, else
    /// the new entry is made; the old entry goes; a moved directory's `..`
    /// points at its new parent. `to_dir` is `None` for a rename within
    /// `from_dir`. A replaced target's count drops; the caller frees it at
    /// no links once nothing uses it.
    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        from_dir_ino: u32,
        from_dir: &mut Inode,
        from_name: &[u8],
        ino: u32,
        ip: &mut Inode,
        mut to_dir: Option<(u32, &mut Inode)>,
        to_name: &[u8],
        target: Option<(u32, &mut Inode)>,
        now: Timestamp,
        reserve: Reserve,
    ) -> Result<(), Error> {
        let is_dir = ip.kind() == Kind::Directory;
        if ip.nlink >= LINK_MAX {
            return Err(Error::TooManyLinks);
        }
        // Linked twice for a while.
        ip.nlink += 1;
        ip.ctime = now;
        self.write_inode(cx, ino, ip, Mode::Sync)?;
        let kind = dtype_of(ip.mode);
        let undo = |this: &Self, ip: &mut Inode| {
            ip.nlink -= 1;
            let _ = this.write_inode(cx, ino, ip, Mode::Sync);
        };
        // The target directory: the other one, or this one.
        let moved = to_dir.is_some();
        {
            let (tdir_ino, tdir): (u32, &mut Inode) = match &mut to_dir {
                Some((i, d)) => (*i, &mut **d),
                None => (from_dir_ino, &mut *from_dir),
            };
            match target {
                Some((tino, tip)) => {
                    if let Err(e) = self.dir_rewrite(cx, tdir, to_name, ino, kind) {
                        undo(self, ip);
                        return Err(e);
                    }
                    tip.ctime = now;
                    if tip.kind() == Kind::Directory {
                        // The replaced directory's `..` no longer counts in
                        // its parent; it is emptied now.
                        tip.nlink = 0;
                        tip.dirhash = None;
                        tdir.nlink = tdir.nlink.saturating_sub(1);
                        self.truncate(cx, sum, tino, tip, 0, reserve, true)?;
                    } else {
                        tip.nlink = tip.nlink.saturating_sub(1);
                    }
                    self.write_inode(cx, tino, tip, Mode::Sync)?;
                }
                None => {
                    if is_dir && moved {
                        if tdir.nlink >= LINK_MAX {
                            undo(self, ip);
                            return Err(Error::TooManyLinks);
                        }
                        tdir.nlink += 1;
                        self.write_inode(cx, tdir_ino, tdir, Mode::Sync)?;
                    }
                    if let Err(e) =
                        self.dir_enter(cx, sum, tdir_ino, tdir, to_name, ino, kind, reserve)
                    {
                        if is_dir && moved {
                            tdir.nlink -= 1;
                            let _ = self.write_inode(cx, tdir_ino, tdir, Mode::Sync);
                        }
                        undo(self, ip);
                        return Err(e);
                    }
                }
            }
            tdir.mtime = now;
            tdir.ctime = now;
            self.write_inode(cx, tdir_ino, tdir, Mode::Sync)?;
        }
        // The old name goes (in the target directory if it is the same,
        // whose inode is `from_dir`, already written).
        self.dir_remove(cx, from_dir, from_name)?;
        ip.nlink -= 1;
        if is_dir && moved {
            if let Some((tdir_ino, _)) = &to_dir {
                self.dir_rewrite(cx, ip, b"..", *tdir_ino, 4)?;
            }
            from_dir.nlink = from_dir.nlink.saturating_sub(1);
        }
        self.write_inode(cx, ino, ip, Mode::Sync)?;
        from_dir.mtime = now;
        from_dir.ctime = now;
        self.write_inode(cx, from_dir_ino, from_dir, Mode::Sync)
    }

    /// The names in directory `dir`, `.` and `..` left out (for tools).
    pub fn names(&self, cx: &B::Cx, dir: &Inode) -> Result<Vec<(Vec<u8>, u32)>, Error> {
        let mut out = Vec::new();
        self.read_dir(cx, dir, 0, &mut |e| {
            if e.name != b"." && e.name != b".." {
                out.push((e.name.to_vec(), e.ino));
            }
            true
        })?;
        Ok(out)
    }
}

const _: () = assert!(DIRBLKSIZ == 512);
