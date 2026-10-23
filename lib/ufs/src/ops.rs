// SPDX-License-Identifier: BSD-2-Clause
//! File operations over the engine: making, linking, unlinking, and
//! renaming, with the link counts and the write order FreeBSD's UFS keeps
//! (sys/ufs/ufs/ufs_vnops.c: `ufs_makeinode`, `ufs_mkdir`, `ufs_link`,
//! `ufs_remove`, `ufs_rmdir`, `ufs_rename`).
//!
//! An inode is written before a directory entry names it, and a directory
//! entry is removed before the link count drops, so a crash leaves at
//! worst a count too high or an inode no entry names, which fsck repairs.
//! Without soft updates the first writes go now; with them everything is
//! delayed, the entry waiting for the inode, and the count on the disk
//! dropping as deferred work once the removal is out ([`Inode::effnlink`]
//! drops at once). The caller holds the vnode locks and has checked
//! permissions and names; each operation takes the inodes it changes by
//! `&mut` and leaves them written.

use alloc::vec::Vec;

use crate::dir::{DIRBLKSIZ, dtype_of};
use crate::inode::{Inode, Kind, Timestamp};
use crate::softdep::{Deps, Gone, Id, Removal, Work};
use crate::{BlockIo, Error, Mode, Reserve, Summary, Sums, Ufs, idx64, len64};

/// FreeBSD's `UFS_LINK_MAX`.
pub const LINK_MAX: u16 = 32767;

/// How deferred work reaches in-core inodes and the summary, which the
/// caller keeps behind its locks.
pub trait WorkEnv<B: BlockIo> {
    /// Calls `f` with inode `ino` (the in-core one if any, which stays
    /// current), whether something still uses it, and the summary.
    fn with_inode(
        &mut self,
        ino: u32,
        f: &mut dyn FnMut(&mut Inode, bool, &mut Summary) -> Result<(), Error>,
    ) -> Result<(), Error>;

    /// Calls `f` with the summary.
    fn with_summary(
        &mut self,
        f: &mut dyn FnMut(&mut Summary) -> Result<(), Error>,
    ) -> Result<(), Error>;
}

/// What a new file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewKind<'a> {
    Regular,
    Directory,
    Symlink(&'a [u8]),
    /// A character device node, with its number.
    Device(u64),
    /// A block device node, with its number.
    BlockDevice(u64),
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
            Self::BlockDevice(_) => 0o060_000,
            Self::Fifo => 0o010_000,
            Self::Socket => 0o140_000,
        }
    }
}

/// Who makes a file, and when.
#[derive(Clone, Copy, Debug)]
pub struct Maker {
    /// Permission bits (already masked).
    pub perm: u16,
    pub uid: u32,
    pub gid: u32,
    pub now: Timestamp,
    /// A random number, for the generation numbers of inodes that need a
    /// new one (rarely: a new inode's is its last one plus one).
    pub random: fn() -> u32,
    pub reserve: Reserve,
}

impl<B: BlockIo> Ufs<B> {
    /// Makes a file of `kind` named `name` in directory `dir_ino`
    /// (FreeBSD's `ufs_makeinode` and `ufs_mkdir`), returning its number
    /// and inode, both written. `dir` is written if it grew; its new times
    /// are left for the caller to write later (FreeBSD's `IN_CHANGE`).
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        cx: &B::Cx,
        sum: &mut impl Sums,
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
        let size = dir.size;
        let ino = sum.with_sum(|sum| self.valloc(cx, sum, dir_ino, dir, mode, who.random))?;
        // With soft updates the entry waits for the inode (and for a
        // directory, its first block and its parent's link).
        let entry = self.soft(cx, |d| d.new_entry(ino));
        let made = self.make_inode(cx, sum, ino, dir_ino, dir, mode, kind, who, entry);
        let ip = match made {
            Ok(ip) => ip,
            Err(e) => {
                self.cancel_entry(cx, entry);
                let _ = sum.with_sum(|sum| self.release_new(cx, sum, ino, mode, who.reserve));
                return Err(e);
            }
        };
        let entered = self.dir_enter(
            cx,
            sum,
            dir_ino,
            dir,
            name,
            ino,
            dtype_of(mode),
            who.reserve,
            entry,
        );
        if let Err(e) = entered {
            // Undo: the new inode goes, and so does the parent's link.
            self.cancel_entry(cx, entry);
            let mut ip = ip;
            if is_dir {
                dir.nlink -= 1;
                dir.effnlink -= 1;
                let _ = self.write_inode(cx, dir_ino, dir, self.ordered());
            }
            ip.nlink = 0;
            ip.effnlink = 0;
            let _ = sum.with_sum(|sum| self.free_inode(cx, sum, ino, &mut ip, who.reserve));
            return Err(e);
        }
        dir.mtime = who.now;
        dir.ctime = who.now;
        if dir.size != size {
            self.write_inode(cx, dir_ino, dir, Mode::Delayed)?;
        }
        Ok((ino, ip))
    }

    /// Forgets an entry never made.
    fn cancel_entry(&self, cx: &B::Cx, entry: Option<Id>) {
        if let Some(id) = entry {
            self.soft(cx, |d| d.cancel(id));
        }
    }

    /// Gives back new inode `ino` (of `mode`) after a failed make.
    fn release_new(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        mode: u16,
        reserve: Reserve,
    ) -> Result<(), Error> {
        if !self.soft {
            return self.freefile(cx, sum, ino, mode);
        }
        let mut ip = self.inode(cx, ino)?;
        ip.mode = mode;
        ip.nlink = 0;
        ip.effnlink = 0;
        self.free_inode(cx, sum, ino, &mut ip, reserve)
    }

    /// Fills in, and writes, the new inode `ino` (and for a directory its
    /// first block and its parent's link). With soft updates, `entry` (the
    /// name to come) waits for each.
    #[allow(clippy::too_many_arguments)]
    fn make_inode(
        &self,
        cx: &B::Cx,
        sum: &mut impl Sums,
        ino: u32,
        dir_ino: u32,
        dir: &mut Inode,
        mode: u16,
        kind: NewKind<'_>,
        who: &Maker,
        entry: Option<Id>,
    ) -> Result<Inode, Error> {
        let mut ip = self.inode(cx, ino)?;
        if ip.mode != 0 {
            return Err(Error::Corrupt("allocated a used inode"));
        }
        // A new generation number, never 0 (FreeBSD's `ffs_valloc`).
        let mut generation = ip.generation.wrapping_add(1);
        if generation == 0 {
            generation = (who.random)() | 1;
        }
        let raw = ip.raw;
        ip = Inode {
            mode,
            nlink: 1,
            effnlink: 1,
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
        let wait_copy = |of: u32| {
            if let Some(id) = entry {
                self.soft(cx, |d| d.wait_copy(id, of));
            }
        };
        wait_copy(ino);
        match kind {
            NewKind::Directory => {
                ip.nlink = 2;
                ip.effnlink = 2;
                ip.dirdepth = dir.dirdepth + 1;
                // The parent's link first, then the new directory's block.
                self.write_inode(cx, ino, &ip, self.ordered())?;
                dir.link_up();
                wait_copy(dir_ino);
                self.write_inode(cx, dir_ino, dir, self.ordered())?;
                match sum.with_sum(|sum| self.dir_init(cx, sum, ino, &mut ip, dir_ino, who.reserve))
                {
                    Ok(addr) => {
                        if let Some(id) = entry {
                            self.soft(cx, |d| d.wait_block(id, addr));
                        }
                    }
                    Err(e) => {
                        dir.nlink -= 1;
                        dir.effnlink -= 1;
                        let _ = self.write_inode(cx, dir_ino, dir, self.ordered());
                        let _ = sum.with_sum(|sum| {
                            self.truncate(cx, sum, ino, &mut ip, 0, who.reserve, true)
                        });
                        return Err(e);
                    }
                }
            }
            NewKind::Symlink(target) => {
                if target.len() >= usize::from(u16::MAX) || target.is_empty() {
                    return Err(Error::NameTooLong);
                }
                if len64(target.len()) < u64::from(self.sb.maxsymlinklen) {
                    ip.set_shortlink(target);
                } else {
                    sum.with_sum(|sum| {
                        self.write_data_range(cx, sum, ino, &mut ip, 0, target, who.reserve)
                    })?;
                }
            }
            NewKind::Device(rdev) | NewKind::BlockDevice(rdev) => ip.db[0] = rdev,
            NewKind::Regular | NewKind::Fifo | NewKind::Socket => {}
        }
        self.write_inode(cx, ino, &ip, self.ordered())?;
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
        ip.link_up();
        ip.ctime = now;
        let entry = self.new_entry_after_copy(cx, ino);
        let undo = |ip: &mut Inode| {
            self.cancel_entry(cx, entry);
            ip.nlink -= 1;
            ip.effnlink -= 1;
            let _ = self.write_inode(cx, ino, ip, self.ordered());
        };
        if let Err(e) = self.write_inode(cx, ino, ip, self.ordered()) {
            undo(ip);
            return Err(e);
        }
        let kind = dtype_of(ip.mode);
        if let Err(e) = self.dir_enter(cx, sum, dir_ino, dir, name, ino, kind, reserve, entry) {
            undo(ip);
            return Err(e);
        }
        dir.mtime = now;
        dir.ctime = now;
        self.write_inode(cx, dir_ino, dir, Mode::Delayed)
    }

    /// With soft updates, `entry` (naming a directory) waits for the next
    /// copy of its new parent `dir`, whose link for it must be on the disk
    /// first (fsck counts it against the directory naming it).
    fn wait_parent_link(&self, cx: &B::Cx, entry: Option<Id>, dir: u32) {
        if let Some(id) = entry {
            self.soft(cx, |d| d.wait_copy(id, dir));
        }
    }

    /// With soft updates, a new entry naming `ino` that waits for its next
    /// copy (the caller writes it next).
    fn new_entry_after_copy(&self, cx: &B::Cx, ino: u32) -> Option<Id> {
        self.soft(cx, |d| {
            let id = d.new_entry(ino);
            d.wait_copy(id, ino);
            id
        })
    }

    /// Removes entry `name` (for non-directory `ino`) from directory
    /// `dir_ino` (FreeBSD's `ufs_remove`): the entry goes first, then the
    /// count drops (with soft updates, the count on the disk drops once
    /// the removal is there, and [`Inode::effnlink`] now). At no links,
    /// the caller frees the inode once nothing uses it
    /// ([`Self::free_inode`]).
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
        self.dir_remove(cx, dir_ino, dir, name, false)?;
        // The directory's new times are the caller's to write later.
        dir.mtime = now;
        dir.ctime = now;
        ip.ctime = now;
        ip.effnlink = ip.effnlink.saturating_sub(1);
        if self.soft {
            return self.write_inode(cx, ino, ip, Mode::Delayed);
        }
        ip.nlink = ip.nlink.saturating_sub(1);
        self.write_inode(cx, ino, ip, Mode::Sync)
    }

    /// Removes empty directory `ino`, named `name` in `dir_ino` (FreeBSD's
    /// `ufs_rmdir`): the entry goes, the parent loses the child's `..`,
    /// and the directory's blocks are freed now (with soft updates, the
    /// counts on the disk drop, and the blocks go, once the removal is
    /// there); the caller frees the inode once nothing uses it.
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
        self.dir_remove(cx, dir_ino, dir, name, true)?;
        dir.effnlink = dir.effnlink.saturating_sub(1);
        dir.mtime = now;
        dir.ctime = now;
        ip.effnlink = 0;
        ip.ctime = now;
        ip.dirhash = None;
        if self.soft {
            self.write_inode(cx, dir_ino, dir, Mode::Delayed)?;
            return self.write_inode(cx, ino, ip, Mode::Delayed);
        }
        dir.nlink = dir.nlink.saturating_sub(1);
        self.write_inode(cx, dir_ino, dir, Mode::Sync)?;
        ip.nlink = 0;
        self.truncate(cx, sum, ino, ip, 0, Reserve::Allowed, true)?;
        self.write_inode(cx, ino, ip, Mode::Sync)
    }

    /// Frees inode `ino`, which no entry names and nothing uses: its
    /// blocks, then the inode itself (FreeBSD's `ufs_inactive`). With soft
    /// updates both wait for the inode to be on the disk without them.
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
        if let Some(id) = self.soft(cx, |d| {
            let id = d.defer(Work::FreeFile { ino, mode });
            d.wait_copy(id, ino);
            id
        }) {
            let r = self.write_inode(cx, ino, ip, Mode::Delayed);
            self.soft(cx, |d| d.arm(id));
            return r;
        }
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
    /// no links once nothing uses it. With soft updates the counts on the
    /// disk drop once the entries that held the links are gone there.
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
        let ordered = self.ordered();
        // Linked twice for a while.
        ip.link_up();
        ip.ctime = now;
        let entry = self.new_entry_after_copy(cx, ino);
        // A directory's new name, like its first, waits for its first block
        // to be on the disk (fsck refuses a named empty directory).
        if is_dir && let Some(id) = entry {
            let first = ip.db[0];
            self.soft(cx, |d| d.wait_block(id, first));
        }
        let undo = |this: &Self, ip: &mut Inode| {
            this.cancel_entry(cx, entry);
            ip.nlink -= 1;
            ip.effnlink -= 1;
            let _ = this.write_inode(cx, ino, ip, ordered);
        };
        if let Err(e) = self.write_inode(cx, ino, ip, ordered) {
            undo(self, ip);
            return Err(e);
        }
        let kind = dtype_of(ip.mode);
        // The target directory: the other one, or this one.
        let moved = to_dir.is_some();
        // A directory moved in gets `..` naming its new parent: with soft
        // updates that change waits for the parent's new link.
        let mut dotdot = None;
        {
            let (tdir_ino, tdir): (u32, &mut Inode) = match &mut to_dir {
                Some((i, d)) => (*i, &mut **d),
                None => (from_dir_ino, &mut *from_dir),
            };
            match target {
                Some((tino, tip)) => {
                    let tdir_target = tip.kind() == Kind::Directory;
                    // A directory replaced loses its links, and so does
                    // the target directory the one its `..` held. A
                    // directory moved in takes that link over: without
                    // soft updates in place, as FreeBSD does; with them,
                    // as a link of its own first, so the disk never shows
                    // more `..` entries than links.
                    let gone = if tdir_target {
                        Gone::Dir(tdir_ino)
                    } else {
                        Gone::Link
                    };
                    if is_dir && moved && self.soft {
                        tdir.link_up();
                        dotdot = self.new_entry_after_copy(cx, tdir_ino);
                        self.wait_parent_link(cx, entry, tdir_ino);
                        self.write_inode(cx, tdir_ino, tdir, ordered)?;
                    }
                    let rewritten = self.dir_rewrite(
                        cx,
                        tdir_ino,
                        tdir,
                        to_name,
                        ino,
                        kind,
                        entry,
                        gone,
                        &Removal::default(),
                    );
                    if let Err(e) = rewritten {
                        undo(self, ip);
                        return Err(e);
                    }
                    tip.ctime = now;
                    if tdir_target {
                        tip.effnlink = 0;
                        tip.dirhash = None;
                        if !moved || self.soft {
                            tdir.effnlink = tdir.effnlink.saturating_sub(1);
                        }
                    } else {
                        tip.effnlink = tip.effnlink.saturating_sub(1);
                    }
                    if self.soft {
                        self.write_inode(cx, tino, tip, Mode::Delayed)?;
                    } else {
                        if tdir_target {
                            // Emptied now.
                            tip.nlink = 0;
                            if !moved {
                                tdir.nlink = tdir.nlink.saturating_sub(1);
                            }
                            self.truncate(cx, sum, tino, tip, 0, reserve, true)?;
                        } else {
                            tip.nlink = tip.nlink.saturating_sub(1);
                        }
                        self.write_inode(cx, tino, tip, Mode::Sync)?;
                    }
                }
                None => {
                    if is_dir && moved {
                        if tdir.nlink >= LINK_MAX {
                            undo(self, ip);
                            return Err(Error::TooManyLinks);
                        }
                        tdir.link_up();
                        dotdot = self.new_entry_after_copy(cx, tdir_ino);
                        self.wait_parent_link(cx, entry, tdir_ino);
                        self.write_inode(cx, tdir_ino, tdir, ordered)?;
                    }
                    if let Err(e) =
                        self.dir_enter(cx, sum, tdir_ino, tdir, to_name, ino, kind, reserve, entry)
                    {
                        if is_dir && moved {
                            self.cancel_entry(cx, dotdot);
                            tdir.nlink -= 1;
                            tdir.effnlink -= 1;
                            let _ = self.write_inode(cx, tdir_ino, tdir, ordered);
                        }
                        undo(self, ip);
                        return Err(e);
                    }
                }
            }
            tdir.mtime = now;
            tdir.ctime = now;
            self.write_inode(cx, tdir_ino, tdir, ordered)?;
        }
        // The old name goes (in the target directory if it is the same,
        // whose inode is `from_dir`, already written).
        let (_, removal) = self.dir_remove(cx, from_dir_ino, from_dir, from_name, false)?;
        ip.effnlink -= 1;
        if !self.soft {
            ip.nlink -= 1;
        }
        if is_dir && moved {
            if let Some((tdir_ino, tdir)) = &to_dir {
                ip.dirdepth = tdir.dirdepth + 1;
                // The old parent loses the link `..` held, once neither
                // that nor the old entry is on the disk (fsck would give
                // the directory back to it).
                let parent = *tdir_ino;
                self.dir_rewrite(cx, ino, ip, b"..", parent, 4, dotdot, Gone::Link, &removal)?;
            }
            from_dir.effnlink = from_dir.effnlink.saturating_sub(1);
            if !self.soft {
                from_dir.nlink = from_dir.nlink.saturating_sub(1);
            }
        }
        self.write_inode(cx, ino, ip, ordered)?;
        from_dir.mtime = now;
        from_dir.ctime = now;
        self.write_inode(cx, from_dir_ino, from_dir, ordered)
    }

    // ---- Soft updates' deferred work ----

    /// The work whose waits are over (with soft updates).
    pub fn take_work(&self, cx: &B::Cx) -> Vec<Work> {
        self.soft(cx, Deps::take_work).unwrap_or_default()
    }

    /// Frees fragments whose pointers are gone from the disk.
    pub fn apply_free(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        list: &[(u64, u64)],
    ) -> Result<(), Error> {
        for &(bno, size) in list {
            self.blkfree(cx, sum, bno, size)?;
        }
        Ok(())
    }

    /// Links of inode `ino` (`ip`) whose entry is gone from the disk drop
    /// (FreeBSD's `handle_workitem_remove`). A directory removed is
    /// emptied, and its parent's link drops once that is on the disk. At
    /// no links, with nothing using it (`in_use` false), it is freed.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_unlink(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        gone: Gone,
        in_use: bool,
    ) -> Result<(), Error> {
        let n = gone.links();
        ip.nlink = ip.nlink.saturating_sub(n);
        self.soft(cx, |d| d.nlink_applied(ino, u32::from(n)));
        if ip.effnlink > ip.nlink {
            ip.effnlink = ip.nlink;
        }
        if let Gone::Dir(parent) = gone {
            ip.dirhash = None;
            if ip.mode != 0 {
                self.truncate(cx, sum, ino, ip, 0, Reserve::Allowed, true)?;
            }
            let id = self.soft(cx, |d| {
                let id = d.defer(Work::Unlink {
                    ino: parent,
                    gone: Gone::Link,
                });
                d.wait_copy(id, ino);
                id
            });
            let r = self.write_inode(cx, ino, ip, Mode::Delayed);
            if let Some(id) = id {
                self.soft(cx, |d| d.arm(id));
            }
            r?;
        }
        if ip.nlink == 0 && ip.mode != 0 && !in_use {
            return self.free_inode(cx, sum, ino, ip, Reserve::Allowed);
        }
        self.write_inode(cx, ino, ip, Mode::Delayed)
    }

    /// Does `work` with the inodes and the summary found through `env`.
    pub fn apply(&self, cx: &B::Cx, work: Work, env: &mut dyn WorkEnv<B>) -> Result<(), Error> {
        match work {
            Work::Free(list) => env.with_summary(&mut |sum| self.apply_free(cx, sum, &list)),
            Work::FreeFile { ino, mode } => {
                env.with_summary(&mut |sum| self.freefile(cx, sum, ino, mode))
            }
            Work::Unlink { ino, gone } => env.with_inode(ino, &mut |ip, in_use, sum| {
                self.apply_unlink(cx, sum, ino, ip, gone, in_use)
            }),
        }
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
