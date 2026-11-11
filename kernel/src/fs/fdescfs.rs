// SPDX-License-Identifier: BSD-2-Clause
//! fdescfs (docs/VFS.md §10.6): every open descriptor of the calling
//! process as a file, `/dev/fd/N`, as FreeBSD's fdescfs shows them.
//!
//! The root (number 1) lists the caller's descriptors; descriptor `N` is
//! number `N + 3`, one vnode for every process's `N`, answering for
//! whichever process asks (`fdesc_allocvp`'s `FD_DESC + fd`). Opening it
//! shares the descriptor (`dupfdopen`), through the device open's
//! `dup_fd`, which the generic open path serves as for devfs's
//! `/dev/fd/N`. Its attributes are its own (`fdesc_getattr`): a
//! character device, mode `0555`, root's, timed at boot.
//!
//! The mount options (`fdesc_mount`):
//! - `linrdlnk`: entries say they are symbolic links, which a lookup
//!   does not follow, and `readlink(2)` reads the path of the file the
//!   descriptor holds (`VV_READLINK`), or `anon_inode:[unknown]` for
//!   one that is not a vnode; opening one still shares the descriptor.
//! - `rdlnk`: entries are symbolic links to those paths, followed.
//! - `nodup`: a descriptor holding a vnode looks up as that vnode
//!   itself, so an open is a new open of the file (with its own offset).
//!
//! The descriptor table is locked before vnodes, so lookups, `readdir`,
//! `readlink`, and `setattr` are served without the vnode lock (the
//! file-system hooks of `vfs::FileSystem`), as FreeBSD's fdescfs drops
//! its vnode's lock around them. Access is not checked (`VOP_NULL`):
//! the descriptor an open shares decides. Names can be neither made nor
//! removed.
//!
//! Behavior reference: sys/fs/fdescfs/fdesc_vfsops.c,
//! sys/fs/fdescfs/fdesc_vnops.c (FreeBSD 15.1).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::errno::Errno;
use abi::time::Timespec;
use sched::Sleepable;
use vfs::{
    Attr, CharDevice, Create, Device, DeviceNode, DirEntry, DirectoryNode, FileSystem, FsStats,
    Kind, Made, NewNode, Node, NodeOps, Rename, SetAttr, SymlinkNode, Uio, Vnode,
};

use crate::fs::file::{File, FileKind};

/// The root's number (`FD_ROOT`), and the first descriptor's offset
/// (`FD_DESC`).
const FD_ROOT: u64 = 1;
const FD_DESC: u64 = 3;
/// The size of each directory entry `readdir` counts its offsets in
/// (`UIO_MX`, `_GENERIC_DIRLEN(10)`).
const UIO_MX: u64 = 40;

/// What a mount's options say (FreeBSD's `FMNT_*` flags).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Opts {
    /// `linrdlnk` (`FMNT_LINRDLNKF`).
    pub(crate) linrdlnk: bool,
    /// `rdlnk` (`FMNT_RDLNKF`).
    pub(crate) rdlnk: bool,
    /// `nodup` (`FMNT_NODUP`).
    pub(crate) nodup: bool,
}

impl Opts {
    /// The type an entry has in `stat` and `readdir`.
    fn entry_kind(self) -> Kind {
        if self.linrdlnk || self.rdlnk {
            Kind::Symlink
        } else {
            Kind::CharDevice
        }
    }
}

/// A mounted fdescfs.
pub(crate) struct FdescFs {
    opts: Opts,
}

impl FdescFs {
    pub(crate) fn new(opts: Opts) -> Self {
        Self { opts }
    }
}

/// The time every node shows: the boot time, to the second.
fn boot_time() -> Timespec {
    let ns = crate::time::realtime_base_ns();
    Timespec {
        tv_sec: i64::try_from(ns / 1_000_000_000).unwrap_or(0),
        tv_nsec: 0,
    }
}

/// FreeBSD's `makedev(x, y)`.
fn makedev(x: u64, y: u64) -> u64 {
    ((x & 0xffff_ff00) << 32) | ((x & 0xff) << 8) | ((y & 0xff00) << 24) | (y & 0xffff_00ff)
}

/// The attributes every node shares (`fdesc_getattr`): a birth time
/// never set (`VNOVAL`), and the page size as the block size, as
/// `vop_stdstat` rounds `DEV_BSIZE` up.
fn base_attr(fileid: u64) -> Attr {
    let t = boot_time();
    Attr {
        mode: 0o555,
        uid: 0,
        gid: 0,
        atime: t,
        mtime: t,
        ctime: t,
        birthtime: Timespec {
            tv_sec: -1,
            tv_nsec: -1,
        },
        blksize: 4096,
        fileid,
        ..Attr::default()
    }
}

/// The calling process.
fn me() -> Result<Arc<crate::proc::Process>, Errno> {
    crate::proc::Process::current().ok_or(Errno::EBADF)
}

/// The descriptor a name names (`fdesc_lookup`): digits only, no leading
/// zero but in `0`, no overflow; `ENOENT` otherwise.
fn parse_fd(name: &[u8]) -> Result<u32, Errno> {
    if name.is_empty() || (name[0] == b'0' && name.len() != 1) {
        return Err(Errno::ENOENT);
    }
    let mut fd: u32 = 0;
    for &c in name {
        if !c.is_ascii_digit() {
            return Err(Errno::ENOENT);
        }
        fd = fd
            .checked_mul(10)
            .and_then(|n| n.checked_add(u32::from(c - b'0')))
            .ok_or(Errno::ENOENT)?;
    }
    Ok(fd)
}

/// Runs `f` on process `p`'s descriptor table. The table's lock comes
/// before vnodes' and open files' offsets, which callers here may hold
/// (`getdirentries` holds the directory's offset): it is tried, without
/// an order to keep, and tried again after a yield while busy, as
/// `fdesc_readdir`'s `FILEDESC_SLOCK` is taken under its vnode's lock.
fn with_table<R>(p: &crate::proc::Process, f: impl FnOnce(&crate::fs::fdtable::FdTable) -> R) -> R {
    loop {
        if let Some(t) = p.files.try_lock() {
            return f(&t);
        }
        sched::yield_now();
    }
}

/// The file open on descriptor `fd` of the caller (`fget`).
fn file_of(_s: &Sleepable, fd: u32) -> Result<Arc<File>, Errno> {
    let fd = i32::try_from(fd).map_err(|_| Errno::EBADF)?;
    let p = me()?;
    with_table(&p, |t| t.get(fd))
}

/// The vnode a file holds, if it is a vnode's (`DTYPE_VNODE`).
fn vnode_of(f: &File) -> Option<Arc<Vnode>> {
    match f.kind() {
        FileKind::Vnode(v) => Some(Arc::clone(v.vnode.vnode())),
        _ => None,
    }
}

/// What `readlink` reads for descriptor `fd` (`fdesc_readlink`): the path
/// of the file it holds, from the caller's root, or a name for the rest.
fn link_of(s: &Sleepable, fd: u32) -> Result<Vec<u8>, Errno> {
    let f = file_of(s, fd)?;
    match vnode_of(&f) {
        Some(v) => {
            let p = me()?;
            let root = crate::fs::root_of(&p);
            crate::fs::namespace().path_of(s, &v, &root)
        }
        None => Ok(b"anon_inode:[unknown]".to_vec()),
    }
}

impl FileSystem for FdescFs {
    fn root(&self) -> u64 {
        FD_ROOT
    }

    fn load(&self, _s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        if ino == FD_ROOT {
            return Ok(NodeOps::Directory(Box::new(Root)));
        }
        let fd = ino
            .checked_sub(FD_DESC)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(Errno::ENOENT)?;
        let n = Box::new(Desc {
            fd,
            opts: self.opts,
        });
        Ok(if self.opts.rdlnk {
            NodeOps::Symlink(n)
        } else {
            NodeOps::Device(n)
        })
    }

    fn statfs(&self, s: &Sleepable) -> Result<FsStats, Errno> {
        // `fdesc_statfs`: the descriptors left under the caller's limit.
        let p = me()?;
        let lim =
            p.limits.lock()[usize::try_from(abi::resource::rlimit::NOFILE).unwrap_or(0)].rlim_cur;
        let _ = s;
        let used = with_table(&p, crate::fs::fdtable::FdTable::count);
        Ok(FsStats {
            bsize: 512,
            iosize: 512,
            blocks: 2,
            bfree: 2,
            bavail: 2,
            files: lim.saturating_add(1),
            ffree: i64::try_from(lim.saturating_sub(u64::try_from(used).unwrap_or(0)))
                .unwrap_or(i64::MAX),
            flags_mask: Some(abi::mount::MNT_IGNORE),
            ..FsStats::default()
        })
    }

    fn rename(&self, _s: &Sleepable, _r: Rename<'_>) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn pathconf(&self, kind: Kind, name: u32) -> Result<i64, Errno> {
        use abi::fcntl::{_PC_LINK_MAX, _PC_NAME_MAX, NAME_MAX};
        match name {
            _PC_NAME_MAX => Ok(i64::try_from(NAME_MAX).unwrap_or(255)),
            _PC_LINK_MAX if kind == Kind::Directory => Ok(2),
            _PC_LINK_MAX => Ok(1),
            _ => vfs::mount::std_pathconf(name),
        }
    }

    fn caches_created(&self) -> bool {
        false
    }

    fn name_cache(&self) -> bool {
        false
    }

    fn checks_access(&self) -> bool {
        false
    }

    fn fixed_names(&self) -> bool {
        true
    }

    fn lookup_unlocked(
        &self,
        s: &Sleepable,
        dir: &Arc<Vnode>,
        name: &[u8],
    ) -> Option<Result<Arc<Vnode>, Errno>> {
        if dir.ino() != FD_ROOT {
            return Some(Err(Errno::ENOTDIR));
        }
        Some((|| {
            let fd = parse_fd(name)?;
            let f = file_of(s, fd)?;
            // `nodup`: a vnode's descriptor is that vnode
            // (`fdesc_get_ino_alloc`).
            if self.opts.nodup
                && let Some(v) = vnode_of(&f)
            {
                return Ok(v);
            }
            drop(f);
            dir.mount().vget(s, u64::from(fd) + FD_DESC)
        })())
    }

    fn readdir_unlocked(
        &self,
        _s: &Sleepable,
        dir: &Vnode,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Option<Result<(), Errno>> {
        if dir.ino() != FD_ROOT {
            return Some(Err(Errno::ENOTDIR));
        }
        // `fdesc_readdir`: offsets in whole `UIO_MX` entries; `.` and `..`
        // first, then each open descriptor.
        if !from.is_multiple_of(UIO_MX) {
            return Some(Err(Errno::EINVAL));
        }
        let open: Vec<u32> = match me() {
            Ok(p) => with_table(&p, |t| {
                (0..u32::try_from(t.len()).unwrap_or(0))
                    .filter(|&fd| i32::try_from(fd).is_ok_and(|n| t.slot(n).is_ok()))
                    .collect()
            }),
            Err(e) => return Some(Err(e)),
        };
        let slots = open.last().map_or(0, |&l| u64::from(l) + 1);
        let kind = self.opts.entry_kind();
        let mut name = [0u8; 10];
        let mut i = from / UIO_MX;
        while i < slots + 2 {
            let next = UIO_MX * (i + 1);
            let go = match i {
                0 => f(DirEntry {
                    name: b".",
                    ino: FD_ROOT,
                    kind: Kind::Directory,
                    next,
                }),
                1 => f(DirEntry {
                    name: b"..",
                    ino: FD_ROOT + 1,
                    kind: Kind::Directory,
                    next,
                }),
                _ => {
                    let fd = i - 2;
                    if open
                        .binary_search(&u32::try_from(fd).unwrap_or(u32::MAX))
                        .is_ok()
                    {
                        let len = decimal(fd, &mut name);
                        // FreeBSD numbers entries from their place in the
                        // listing, two past `stat`'s (`fdesc_readdir`).
                        f(DirEntry {
                            name: &name[..len],
                            ino: i + FD_DESC,
                            kind,
                            next,
                        })
                    } else {
                        true
                    }
                }
            };
            if !go {
                break;
            }
            i += 1;
        }
        Some(Ok(()))
    }

    fn readlink_unlocked(&self, s: &Sleepable, v: &Vnode) -> Option<Result<Vec<u8>, Errno>> {
        if v.ino() == FD_ROOT {
            return Some(Err(Errno::EINVAL));
        }
        let fd = u32::try_from(v.ino() - FD_DESC).unwrap_or(u32::MAX);
        if !self.opts.linrdlnk && !self.opts.rdlnk {
            return Some(Err(Errno::EINVAL));
        }
        Some(link_of(s, fd))
    }

    fn setattr_unlocked(
        &self,
        s: &Sleepable,
        v: &Vnode,
        a: &SetAttr,
        cred: &vfs::Cred,
    ) -> Option<Result<(), Errno>> {
        // `fdesc_setattr`: not the root's; a vnode's descriptor's file,
        // with that file system's checks; others change nothing (but
        // flags, `EOPNOTSUPP`).
        if v.ino() == FD_ROOT {
            return Some(Err(Errno::EACCES));
        }
        let fd = i32::try_from(v.ino() - FD_DESC).unwrap_or(i32::MAX);
        Some((|| {
            let p = me()?;
            let f = with_table(&p, |t| {
                t.get_checked(fd, &crate::capsicum::rights::EXTATTR_SET)
            })?;
            drop(p);
            match vnode_of(&f) {
                Some(target) => vfs::ops::setattr(s, &target, a, cred, false),
                None if a.flags.is_some() => Err(Errno::EOPNOTSUPP),
                None => Ok(()),
            }
        })())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `n` in decimal into `buf`; returns its length.
fn decimal(mut n: u64, buf: &mut [u8; 10]) -> usize {
    let mut tmp = [0u8; 20];
    let mut len = 0;
    loop {
        tmp[len] = b'0' + u8::try_from(n % 10).unwrap_or(0);
        len += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let len = len.min(buf.len());
    for i in 0..len {
        buf[i] = tmp[len - 1 - i];
    }
    len
}

/// The root directory (`Froot`). Its entries come from the caller's
/// table ([`FdescFs::readdir_unlocked`]); asked with the lock held, it
/// knows none.
struct Root;

impl Node for Root {
    fn attr(&self, _s: &Sleepable) -> Attr {
        Attr {
            nlink: 2,
            size: 512,
            rdev: u64::MAX,
            ..base_attr(FD_ROOT)
        }
    }

    fn setattr(&mut self, _s: &Sleepable, _a: &SetAttr) -> Result<(), Errno> {
        Err(Errno::EACCES)
    }

    fn need_inactive(&self, _s: &Sleepable) -> bool {
        false
    }

    fn reclaim(self: Box<Self>, _s: &Sleepable) {}

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl DirectoryNode for Root {
    fn lookup(&self, _s: &Sleepable, _name: &[u8]) -> Result<u64, Errno> {
        Err(Errno::ENOENT)
    }

    fn parent(&self, _s: &Sleepable) -> u64 {
        FD_ROOT
    }

    fn create(&mut self, _: &Sleepable, _: &[u8], _: &NewNode, _: &Create) -> Result<Made, Errno> {
        Err(Errno::EROFS)
    }

    fn link(&mut self, _: &Sleepable, _: &[u8], _: u64, _: &mut dyn Node) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn remove(&mut self, _: &Sleepable, _: &[u8], _: &mut dyn Node) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn rmdir(&mut self, _: &Sleepable, _: &[u8], _: &mut dyn DirectoryNode) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn readdir(
        &self,
        _s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        // Only `.` and `..` (an emptiness check, `getcwd`'s scan).
        for (i, name) in [(0u64, &b"."[..]), (1, b"..")] {
            if from <= i * UIO_MX
                && !f(DirEntry {
                    name,
                    ino: FD_ROOT + i,
                    kind: Kind::Directory,
                    next: UIO_MX * (i + 1),
                })
            {
                break;
            }
        }
        Ok(())
    }
}

/// Descriptor `fd`'s node (`Fdesc`).
struct Desc {
    fd: u32,
    opts: Opts,
}

impl Desc {
    fn fileid(&self) -> u64 {
        u64::from(self.fd) + FD_DESC
    }
}

impl Node for Desc {
    fn attr(&self, _s: &Sleepable) -> Attr {
        let id = self.fileid();
        Attr {
            nlink: 1,
            size: 0,
            rdev: makedev(0, id),
            kind: Some(self.opts.entry_kind()),
            ..base_attr(id)
        }
    }

    fn setattr(&mut self, _s: &Sleepable, _a: &SetAttr) -> Result<(), Errno> {
        // Served by `FdescFs::setattr_unlocked`.
        Err(Errno::EOPNOTSUPP)
    }

    fn need_inactive(&self, _s: &Sleepable) -> bool {
        false
    }

    fn reclaim(self: Box<Self>, _s: &Sleepable) {}

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl DeviceNode for Desc {
    fn device(&self, _s: &Sleepable) -> Result<Device, Errno> {
        Ok(Device {
            rdev: makedev(0, self.fileid()),
            ops: Arc::new(DescDev(i32::try_from(self.fd).unwrap_or(i32::MAX))),
        })
    }
}

impl SymlinkNode for Desc {
    fn target(&self, s: &Sleepable) -> Result<Vec<u8>, Errno> {
        // Reached with the vnode locked only from the generic layer's
        // own callers; `readlink` and lookups take the unlocked path.
        let _ = s;
        Err(Errno::EINVAL)
    }
}

/// What opening a descriptor's node reaches: the descriptor, shared
/// (`fdesc_open`'s `ENODEV` for `dupfdopen`).
struct DescDev(i32);

impl CharDevice for DescDev {
    fn dup_fd(&self) -> Option<i32> {
        Some(self.0)
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::EBADF)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::EBADF)
    }
}
