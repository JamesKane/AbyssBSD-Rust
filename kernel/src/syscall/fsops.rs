// SPDX-License-Identifier: BSD-2-Clause
//! System calls that change the file system: `mkdir`, `unlink`, `rmdir`,
//! `rename`, `link`, `symlink`, `mknod`, `mkfifo` (each with its `*at`
//! form), the attribute calls (`chmod`, `chown`, `utimes` and
//! `utimensat`, `chflags`, `truncate`, with their descriptor and
//! no-follow forms), `fsync`, `sync`, and `nmount`/`unmount`. The rules
//! live in the generic layer (`vfs::ops`); these decode arguments, find
//! the files, and apply FreeBSD's per-call details (mode masks, flags,
//! errno choices).
//!
//! Behavior reference: sys/kern/vfs_syscalls.c, sys/kern/vfs_mount.c.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::fcntl::{
    AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_RESOLVE_BENEATH, AT_SYMLINK_FOLLOW,
    AT_SYMLINK_NOFOLLOW, FWRITE,
};
use abi::stat::{ALLPERMS, S_IFBLK, S_IFCHR, S_IFIFO, S_IFMT};
use abi::time::Timespec;
use sched::Sleepable;
use vfs::{Create, NewNode, SetAttr, Vnode};

use super::path::path;
use super::{SysResult, int, process};
use crate::fs::{At, Walk};
use crate::proc::Process;
use crate::uaccess::UserPtr;

const ACCESSPERMS: u16 = 0o777;
const S_IFWHT: u16 = 0o160_000;

fn cwd() -> u64 {
    u64::from(AT_FDCWD.cast_unsigned())
}

/// The process's umask.
fn umask(p: &Process) -> u16 {
    p.umask.load(Ordering::Relaxed)
}

pub(super) fn create_as(mode: u16) -> Create {
    let c = crate::fs::cred();
    Create {
        mode,
        uid: c.uid,
        gid: c.gid,
        time: crate::fs::now(),
    }
}

/// Makes `node` at `path` from `fd` (the `mk*at` calls).
fn make(s: &Sleepable, fd: u64, path_ptr: u64, node: &NewNode, mode: u16) -> SysResult {
    let p = process()?;
    let path = path(path_ptr)?;
    let parent = crate::fs::parent_of(s, &p, At::new(int(fd)), &path, false)?;
    if parent.trailing_slash && *node != NewNode::Directory {
        return Err(Errno::ENOENT);
    }
    vfs::ops::create(
        s,
        &parent.dir,
        &parent.name,
        node,
        &create_as(mode),
        &crate::fs::cred(),
        true,
    )?;
    Ok(0)
}

/// `mkdirat(fd, path, mode)`.
pub(super) fn mkdirat(s: &Sleepable, fd: u64, path: u64, mode: u64) -> SysResult {
    let p = process()?;
    let mode = (u16::try_from(mode & 0o7777).unwrap_or(0) & ACCESSPERMS) & !umask(&p);
    make(s, fd, path, &NewNode::Directory, mode)
}

/// `mkdir(path, mode)`.
pub(super) fn mkdir(s: &Sleepable, path: u64, mode: u64) -> SysResult {
    mkdirat(s, cwd(), path, mode)
}

/// `mkfifoat(fd, path, mode)`.
pub(super) fn mkfifoat(s: &Sleepable, fd: u64, path: u64, mode: u64) -> SysResult {
    let p = process()?;
    let mode = (u16::try_from(mode & 0o7777).unwrap_or(0) & ALLPERMS) & !umask(&p);
    make(s, fd, path, &NewNode::Fifo, mode)
}

/// `mkfifo(path, mode)`.
pub(super) fn mkfifo(s: &Sleepable, path: u64, mode: u64) -> SysResult {
    mkfifoat(s, cwd(), path, mode)
}

/// `mknodat(fd, path, mode, dev)`: a FIFO, or (the superuser only) a
/// character device node, which outside devfs cannot be opened.
pub(super) fn mknodat(s: &Sleepable, fd: u64, path: u64, mode: u64, dev: u64) -> SysResult {
    let p = process()?;
    let m = u16::try_from(mode & 0o177_777).unwrap_or(0);
    let perm = (m & ALLPERMS) & !umask(&p);
    // Device nodes are the superuser's to make (FreeBSD's
    // `PRIV_VFS_MKNOD_DEV`), with a number (`VNOVAL` is none).
    let device = |node: NewNode| {
        if !p.cred().is_root() {
            Err(Errno::EPERM)
        } else if dev == u64::MAX {
            Err(Errno::EINVAL)
        } else {
            make(s, fd, path, &node, perm)
        }
    };
    match m & S_IFMT {
        S_IFCHR => device(NewNode::CharDevice(dev)),
        S_IFBLK => device(NewNode::BlockDevice(dev)),
        S_IFIFO => make(s, fd, path, &NewNode::Fifo, perm),
        // No union mounts to need whiteouts.
        S_IFWHT => Err(Errno::EOPNOTSUPP),
        _ => Err(Errno::EINVAL),
    }
}

/// `symlinkat(target, fd, path)`.
pub(super) fn symlinkat(s: &Sleepable, target_ptr: u64, fd: u64, path_ptr: u64) -> SysResult {
    let p = process()?;
    let target = path(target_ptr)?;
    let mode = ACCESSPERMS & !umask(&p);
    make(s, fd, path_ptr, &NewNode::Symlink(target.to_vec()), mode)
}

/// `symlink(target, path)`.
pub(super) fn symlink(s: &Sleepable, target: u64, path: u64) -> SysResult {
    symlinkat(s, target, cwd(), path)
}

/// `funlinkat(dfd, path, fd, flag)` and `unlinkat(fd, path, flag)` (with
/// `fd` -1): `AT_REMOVEDIR` makes it `rmdir`.
pub(super) fn funlinkat(s: &Sleepable, dfd: u64, path_ptr: u64, fd: u64, flag: u64) -> SysResult {
    let flag = u32::try_from(flag).map_err(|_| Errno::EINVAL)?;
    if flag & !(AT_REMOVEDIR | AT_RESOLVE_BENEATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let path = path(path_ptr)?;
    let parent = crate::fs::parent_of(
        s,
        &p,
        At::new(int(dfd)),
        &path,
        flag & AT_RESOLVE_BENEATH != 0,
    )?;
    // With a descriptor, it must still be the file the name names.
    if int(fd) != -1 {
        let f = super::io::file(s, &p, fd)?;
        let named = vfs::namei::Namespace::child(s, &parent.dir, &parent.name)?;
        if !f.vnode().is_some_and(|v| Arc::ptr_eq(v, &named)) {
            return Err(Errno::EDEADLK);
        }
    }
    vfs::ops::remove(
        s,
        &parent.dir,
        &parent.name,
        &crate::fs::cred(),
        flag & AT_REMOVEDIR != 0,
    )?;
    Ok(0)
}

/// `unlinkat(fd, path, flag)`.
pub(super) fn unlinkat(s: &Sleepable, fd: u64, path: u64, flag: u64) -> SysResult {
    funlinkat(s, fd, path, u64::MAX, flag)
}

/// `unlink(path)`.
pub(super) fn unlink(s: &Sleepable, path: u64) -> SysResult {
    funlinkat(s, cwd(), path, u64::MAX, 0)
}

/// `rmdir(path)`.
pub(super) fn rmdir(s: &Sleepable, path: u64) -> SysResult {
    funlinkat(s, cwd(), path, u64::MAX, u64::from(AT_REMOVEDIR))
}

/// `renameat(fromfd, from, tofd, to)`.
pub(super) fn renameat(
    s: &Sleepable,
    fromfd: u64,
    from_ptr: u64,
    tofd: u64,
    to_ptr: u64,
) -> SysResult {
    let p = process()?;
    let from = path(from_ptr)?;
    let to = path(to_ptr)?;
    let f = crate::fs::parent_of(s, &p, At::new(int(fromfd)), &from, false)?;
    let t = crate::fs::parent_of(s, &p, At::new(int(tofd)), &to, false)?;
    vfs::ops::rename(s, &f.dir, &f.name, &t.dir, &t.name, &crate::fs::cred())?;
    Ok(0)
}

/// `rename(from, to)`.
pub(super) fn rename(s: &Sleepable, from: u64, to: u64) -> SysResult {
    renameat(s, cwd(), from, cwd(), to)
}

/// `linkat(fd1, path1, fd2, path2, flag)`.
pub(super) fn linkat(
    s: &Sleepable,
    fd1: u64,
    path1: u64,
    fd2: u64,
    path2: u64,
    flag: u64,
) -> SysResult {
    let flag = u32::try_from(flag).map_err(|_| Errno::EINVAL)?;
    if flag & !(AT_SYMLINK_FOLLOW | AT_RESOLVE_BENEATH | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let from = path(path1)?;
    let walk = Walk {
        follow: flag & AT_SYMLINK_FOLLOW != 0,
        beneath: flag & AT_RESOLVE_BENEATH != 0,
        create: false,
    };
    let target = super::path::at_vnode(s, &p, fd1, &from, walk, flag & AT_EMPTY_PATH != 0)?;
    let to = path(path2)?;
    let t = crate::fs::parent_of(s, &p, At::new(int(fd2)), &to, false)?;
    vfs::ops::link(s, &target, &t.dir, &t.name, &crate::fs::cred())?;
    Ok(0)
}

/// `link(path1, path2)`.
pub(super) fn link(s: &Sleepable, path1: u64, path2: u64) -> SysResult {
    linkat(s, cwd(), path1, cwd(), path2, 0)
}

/// The file an attribute call names: a path from `fd` (following the
/// last link unless `nofollow`), or with `AT_EMPTY_PATH` the descriptor's
/// file.
fn target(
    s: &Sleepable,
    p: &Process,
    fd: u64,
    path_ptr: u64,
    flag: u32,
) -> Result<Arc<Vnode>, Errno> {
    let path = path(path_ptr)?;
    let walk = Walk {
        follow: flag & AT_SYMLINK_NOFOLLOW == 0,
        beneath: flag & AT_RESOLVE_BENEATH != 0,
        create: false,
    };
    super::path::at_vnode(s, p, fd, &path, walk, flag & AT_EMPTY_PATH != 0)
}

fn check_flags(flag: u64) -> Result<u32, Errno> {
    let flag = u32::try_from(flag).map_err(|_| Errno::EINVAL)?;
    if flag & !(AT_SYMLINK_NOFOLLOW | AT_RESOLVE_BENEATH | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(flag)
}

/// The vnode open on `fd`.
fn fd_vnode(s: &Sleepable, fd: u64) -> Result<Arc<Vnode>, Errno> {
    let f = super::io::current_file(s, fd)?;
    f.vnode().cloned().ok_or(Errno::EINVAL)
}

fn setattr(s: &Sleepable, v: &Vnode, a: &SetAttr) -> SysResult {
    vfs::ops::setattr(s, v, a, &crate::fs::cred(), false)?;
    Ok(0)
}

fn mode_of(mode: u64) -> SetAttr {
    SetAttr {
        mode: Some(u16::try_from(mode & u64::from(ALLPERMS)).unwrap_or(0)),
        ..SetAttr::default()
    }
}

/// `fchmodat(fd, path, mode, flag)`.
pub(super) fn fchmodat(s: &Sleepable, fd: u64, path: u64, mode: u64, flag: u64) -> SysResult {
    let flag = check_flags(flag)?;
    let p = process()?;
    setattr(s, &*target(s, &p, fd, path, flag)?, &mode_of(mode))
}

/// `chmod(path, mode)`, and `lchmod` (`nofollow`).
pub(super) fn chmod(s: &Sleepable, path: u64, mode: u64, nofollow: bool) -> SysResult {
    let flag = if nofollow { AT_SYMLINK_NOFOLLOW } else { 0 };
    fchmodat(s, cwd(), path, mode, u64::from(flag))
}

/// `fchmod(fd, mode)`.
pub(super) fn fchmod(s: &Sleepable, fd: u64, mode: u64) -> SysResult {
    let f = super::io::current_file(s, fd)?;
    if let crate::fs::file::FileKind::Shm(m) = f.kind() {
        #[allow(clippy::cast_possible_truncation, reason = "a mode_t")]
        m.chmod(s, &crate::fs::cred(), mode as u16)?;
        return Ok(0);
    }
    // A socket's mode is what `bind` gives its node (`uipc_chmod`).
    if let crate::fs::file::FileKind::Socket(so) = f.kind() {
        so.so.chmod(int(mode).cast_unsigned())?;
        return Ok(0);
    }
    setattr(s, &*fd_vnode(s, fd)?, &mode_of(mode))
}

/// A `uid_t`/`gid_t` argument: -1 leaves it.
fn id(a: u64) -> Option<u32> {
    let v = int(a);
    (v != -1).then_some(v.cast_unsigned())
}

fn owner_of(uid: u64, gid: u64) -> SetAttr {
    SetAttr {
        uid: id(uid),
        gid: id(gid),
        ..SetAttr::default()
    }
}

/// `fchownat(fd, path, uid, gid, flag)`.
pub(super) fn fchownat(
    s: &Sleepable,
    fd: u64,
    path: u64,
    uid: u64,
    gid: u64,
    flag: u64,
) -> SysResult {
    let flag = check_flags(flag)?;
    let p = process()?;
    setattr(s, &*target(s, &p, fd, path, flag)?, &owner_of(uid, gid))
}

/// `chown(path, uid, gid)`, and `lchown` (`nofollow`).
pub(super) fn chown(s: &Sleepable, path: u64, uid: u64, gid: u64, nofollow: bool) -> SysResult {
    let flag = if nofollow { AT_SYMLINK_NOFOLLOW } else { 0 };
    fchownat(s, cwd(), path, uid, gid, u64::from(flag))
}

/// `fchown(fd, uid, gid)`.
pub(super) fn fchown(s: &Sleepable, fd: u64, uid: u64, gid: u64) -> SysResult {
    let f = super::io::current_file(s, fd)?;
    if let crate::fs::file::FileKind::Shm(m) = f.kind() {
        m.chown(s, &crate::fs::cred(), id(uid), id(gid))?;
        return Ok(0);
    }
    setattr(s, &*fd_vnode(s, fd)?, &owner_of(uid, gid))
}

/// `UTIME_NOW` and `UTIME_OMIT` (`tv_nsec` values).
const UTIME_NOW: i64 = -1;
const UTIME_OMIT: i64 = -2;

/// The times a `utimensat` array asks for, and whether every one given
/// is "now" (which write access permits).
fn times_of(ptr: u64) -> Result<(SetAttr, bool), Errno> {
    let now = crate::fs::now();
    if ptr == 0 {
        return Ok((
            SetAttr {
                atime: Some(now),
                mtime: Some(now),
                ..SetAttr::default()
            },
            true,
        ));
    }
    let ts = UserPtr::<[Timespec; 2]>::new(ptr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let mut a = SetAttr::default();
    let mut all_now = true;
    for (i, t) in ts.iter().enumerate() {
        let v = match t.tv_nsec {
            UTIME_OMIT => None,
            UTIME_NOW => Some(now),
            n if (0..1_000_000_000).contains(&n) => {
                all_now = false;
                Some(*t)
            }
            _ => return Err(Errno::EINVAL),
        };
        if i == 0 {
            a.atime = v;
        } else {
            a.mtime = v;
        }
    }
    Ok((a, all_now))
}

fn set_times(s: &Sleepable, v: &Vnode, times: u64) -> SysResult {
    let (a, now) = times_of(times)?;
    if a.atime.is_none() && a.mtime.is_none() {
        return Ok(0);
    }
    vfs::ops::setattr(s, v, &a, &crate::fs::cred(), now)?;
    Ok(0)
}

/// `utimensat(fd, path, times, flag)`.
pub(super) fn utimensat(s: &Sleepable, fd: u64, path: u64, times: u64, flag: u64) -> SysResult {
    let flag = check_flags(flag)?;
    let p = process()?;
    set_times(s, &*target(s, &p, fd, path, flag)?, times)
}

/// `futimens(fd, times)`.
pub(super) fn futimens(s: &Sleepable, fd: u64, times: u64) -> SysResult {
    set_times(s, &*fd_vnode(s, fd)?, times)
}

/// The `struct timeval[2]` of `utimes` and friends, as a `utimensat`
/// change (null: now).
fn timevals(ptr: u64) -> Result<(SetAttr, bool), Errno> {
    if ptr == 0 {
        return times_of(0);
    }
    let tv = UserPtr::<[[i64; 2]; 2]>::new(ptr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let mut a = SetAttr::default();
    for (i, [sec, usec]) in tv.iter().enumerate() {
        if !(0..1_000_000).contains(usec) {
            return Err(Errno::EINVAL);
        }
        let t = Timespec {
            tv_sec: *sec,
            tv_nsec: usec * 1000,
        };
        if i == 0 {
            a.atime = Some(t);
        } else {
            a.mtime = Some(t);
        }
    }
    Ok((a, false))
}

/// `utimes(path, tv)`, `lutimes` (`nofollow`).
pub(super) fn utimes(s: &Sleepable, path: u64, tv: u64, nofollow: bool) -> SysResult {
    let p = process()?;
    let flag = if nofollow { AT_SYMLINK_NOFOLLOW } else { 0 };
    let v = target(s, &p, cwd(), path, flag)?;
    let (a, now) = timevals(tv)?;
    vfs::ops::setattr(s, &v, &a, &crate::fs::cred(), now)?;
    Ok(0)
}

/// `futimes(fd, tv)`.
pub(super) fn futimes(s: &Sleepable, fd: u64, tv: u64) -> SysResult {
    let v = fd_vnode(s, fd)?;
    let (a, now) = timevals(tv)?;
    vfs::ops::setattr(s, &v, &a, &crate::fs::cred(), now)?;
    Ok(0)
}

/// `futimesat(fd, path, tv)`.
pub(super) fn futimesat(s: &Sleepable, fd: u64, path: u64, tv: u64) -> SysResult {
    let p = process()?;
    let v = target(s, &p, fd, path, 0)?;
    let (a, now) = timevals(tv)?;
    vfs::ops::setattr(s, &v, &a, &crate::fs::cred(), now)?;
    Ok(0)
}

fn flags_of(flags: u64) -> Result<SetAttr, Errno> {
    Ok(SetAttr {
        flags: Some(u32::try_from(flags).map_err(|_| Errno::EOPNOTSUPP)?),
        ..SetAttr::default()
    })
}

/// `chflagsat(fd, path, flags, atflag)`.
pub(super) fn chflagsat(s: &Sleepable, fd: u64, path: u64, flags: u64, atflag: u64) -> SysResult {
    let atflag = check_flags(atflag)?;
    let p = process()?;
    setattr(s, &*target(s, &p, fd, path, atflag)?, &flags_of(flags)?)
}

/// `chflags(path, flags)`, and `lchflags` (`nofollow`).
pub(super) fn chflags(s: &Sleepable, path: u64, flags: u64, nofollow: bool) -> SysResult {
    let flag = if nofollow { AT_SYMLINK_NOFOLLOW } else { 0 };
    chflagsat(s, cwd(), path, flags, u64::from(flag))
}

/// `revoke(path)` (`sys_revoke`): every file open on the device node at
/// `path` is dead, and its device forgets them. The node's owner or the
/// superuser only.
pub(super) fn revoke(s: &Sleepable, path: u64) -> SysResult {
    let p = process()?;
    let v = target(s, &p, cwd(), path, 0)?;
    if v.kind() != vfs::Kind::CharDevice {
        return Err(Errno::EINVAL);
    }
    let a = v.attr(s)?;
    if crate::fs::cred().uid != a.uid && !crate::fs::cred_is_root() {
        return Err(Errno::EPERM);
    }
    crate::fs::devfs::revoke(s, &v)?;
    Ok(0)
}

/// `fchflags(fd, flags)`.
pub(super) fn fchflags(s: &Sleepable, fd: u64, flags: u64) -> SysResult {
    setattr(s, &*fd_vnode(s, fd)?, &flags_of(flags)?)
}

fn size_of_len(len: u64) -> Result<SetAttr, Errno> {
    if len.cast_signed() < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(SetAttr {
        size: Some(len),
        ..SetAttr::default()
    })
}

/// `truncate(path, length)`.
pub(super) fn truncate(s: &Sleepable, path: u64, len: u64) -> SysResult {
    let p = process()?;
    let v = target(s, &p, cwd(), path, 0)?;
    setattr(s, &v, &size_of_len(len)?)
}

/// `ftruncate(fd, length)`: the file must be open for writing.
pub(super) fn ftruncate(s: &Sleepable, fd: u64, len: u64) -> SysResult {
    let f = super::io::current_file(s, fd)?;
    if let crate::fs::file::FileKind::Shm(m) = f.kind() {
        if f.flags() & FWRITE == 0 {
            return Err(Errno::EINVAL);
        }
        let len = u64::try_from(len.cast_signed()).map_err(|_| Errno::EINVAL)?;
        m.truncate(s, len)?;
        return Ok(0);
    }
    let v = f.vnode().ok_or(Errno::EINVAL)?;
    if f.flags() & FWRITE == 0 {
        return Err(Errno::EINVAL);
    }
    if v.kind() != vfs::Kind::Regular {
        return Err(Errno::EINVAL);
    }
    // Open for writing is permission enough (FreeBSD checks the open).
    vfs::ops::setattr(s, v, &size_of_len(len)?, &vfs::Cred::root(), false)?;
    Ok(0)
}

/// `fsync(fd)` and `fdatasync(fd)`: the file's dirty pages and its
/// metadata go to the disk.
pub(super) fn fsync(s: &Sleepable, fd: u64) -> SysResult {
    let v = fd_vnode(s, fd)?;
    v.fsync(s)?;
    Ok(0)
}

/// `sync()`: starts writing every file system's dirty data.
pub(super) fn sync(s: &Sleepable) -> SysResult {
    for m in crate::fs::namespace().mounts() {
        let _ = m.fs().sync(s, false);
    }
    Ok(0)
}

/// An `nmount` option: its name and value.
type MountOption = (Vec<u8>, Vec<u8>);

/// The `nmount` options in an iovec of name and value pairs.
fn options(iov: u64, iovcnt: u64) -> Result<Vec<MountOption>, Errno> {
    let n = usize::try_from(iovcnt).map_err(|_| Errno::EINVAL)?;
    if n % 2 != 0 || n > 128 {
        return Err(Errno::EINVAL);
    }
    let mut out = Vec::new();
    for i in (0..n).step_by(2) {
        let read = |j: usize| -> Result<Vec<u8>, Errno> {
            let at = iov + (j * size_of::<abi::uio::Iovec>()) as u64;
            let e = UserPtr::<abi::uio::Iovec>::new(at)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            let len = usize::try_from(e.iov_len).map_err(|_| Errno::EINVAL)?;
            if len > 1024 {
                return Err(Errno::EINVAL);
            }
            let mut b = alloc::vec![0u8; len];
            if len > 0 {
                crate::uaccess::copy_in(&mut b, e.iov_base).map_err(|_| Errno::EFAULT)?;
            }
            // Strings carry their NUL.
            if b.last() == Some(&0) {
                b.pop();
            }
            Ok(b)
        };
        out.push((read(i)?, read(i + 1)?));
    }
    Ok(out)
}

/// `nmount(iov, iovcnt, flags)`: so far, `tmpfs` with `mode`, `uid`, and
/// `gid`; `ro`/`rdonly` is honored.
pub(super) fn nmount(s: &Sleepable, iov: u64, iovcnt: u64, flags: u64) -> SysResult {
    let opts = options(iov, iovcnt)?;
    let get = |name: &[u8]| {
        opts.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    };
    let fstype = get(b"fstype").ok_or(Errno::EINVAL)?;
    let fspath = get(b"fspath").ok_or(Errno::EINVAL)?;
    // "ro" and "noro" (or "rw") as FreeBSD's `vfs_donmount` reads them;
    // without either an update goes read-write.
    let ro = get(b"ro").is_some() || get(b"rdonly").is_some();
    let flags = if ro {
        flags | abi::mount::MNT_RDONLY
    } else if get(b"noro").is_some() || get(b"rw").is_some() {
        flags & !abi::mount::MNT_RDONLY
    } else {
        flags
    };
    let p = process()?;
    let covered = crate::fs::lookup(s, &p, At::Cwd, fspath, Walk::FOLLOW)?;
    if !covered.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    if flags & abi::mount::MNT_UPDATE != 0 || get(b"update").is_some() {
        let force = flags & abi::mount::MNT_FORCE != 0 || get(b"force").is_some();
        return update(s, &covered, fstype, flags, force);
    }
    let num = |name: &[u8], default: u32| -> Result<u32, Errno> {
        get(name).map_or(Ok(default), |v| {
            let t = core::str::from_utf8(v).map_err(|_| Errno::EINVAL)?;
            let (digits, radix) = t
                .strip_prefix('0')
                .filter(|r| !r.is_empty())
                .map_or((t, 10), |r| (r, 8));
            u32::from_str_radix(digits, radix).map_err(|_| Errno::EINVAL)
        })
    };
    let m = match fstype {
        b"tmpfs" => {
            let a = covered.attr(s)?;
            let mode = num(b"mode", u32::from(a.mode))?;
            let uid = num(b"uid", a.uid)?;
            let gid = num(b"gid", a.gid)?;
            vfs::Mount::new(
                vfs::MountInfo {
                    type_name: "tmpfs",
                    flags: abi::mount::MNT_LOCAL | flags,
                    from: b"tmpfs".to_vec(),
                    io_size: 4096,
                },
                Box::new(vfs::tmpfs::TmpFs::new(
                    u16::try_from(mode & 0o7777).unwrap_or(0o755),
                    uid,
                    gid,
                    crate::mm::space::page_ops(),
                )),
            )
        }
        b"ufs" => {
            let from = get(b"from").ok_or(Errno::EINVAL)?;
            let disk = disk_at(s, &p, from)?;
            let read_only = flags & abi::mount::MNT_RDONLY != 0;
            let force = flags & abi::mount::MNT_FORCE != 0 || get(b"force").is_some();
            let fs = crate::fs::ufs::UfsFs::mount(s, disk, read_only, force)?;
            // Soft updates follow the file system's flag, as on FreeBSD.
            let softdep = if fs.soft_updates() {
                abi::mount::MNT_SOFTDEP
            } else {
                0
            };
            vfs::Mount::new(
                vfs::MountInfo {
                    type_name: "ufs",
                    flags: abi::mount::MNT_LOCAL | (flags & !abi::mount::MNT_SOFTDEP) | softdep,
                    from: from.strip_suffix(b"\0").unwrap_or(from).to_vec(),
                    io_size: fs.block_size(),
                },
                Box::new(fs),
            )
        }
        b"msdosfs" => {
            let from = get(b"from").ok_or(Errno::EINVAL)?;
            let disk = disk_at(s, &p, from)?;
            let read_only = flags & abi::mount::MNT_RDONLY != 0;
            // mount_msdosfs passes these in decimal (FreeBSD's
            // `vfs_scanopt("%d")`).
            let dec = |name: &[u8], default: u32| -> Result<u32, Errno> {
                get(name).map_or(Ok(default), |v| {
                    let v = v.strip_suffix(b"\0").unwrap_or(v);
                    core::str::from_utf8(v)
                        .ok()
                        .and_then(|t| t.parse::<i64>().ok())
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or(Errno::EINVAL)
                })
            };
            let d = crate::fs::msdosfs::Opts::default();
            let perm = |v: u32| u16::try_from(v & 0o7777).unwrap_or(0);
            let opts = crate::fs::msdosfs::Opts {
                uid: dec(b"uid", d.uid)?,
                gid: dec(b"gid", d.gid)?,
                mask: perm(dec(b"mask", u32::from(d.mask))?),
                dirmask: perm(dec(b"dirmask", u32::from(d.dirmask))?),
                shortname: [&b"shortname"[..], b"shortnames", b"nowin95"]
                    .iter()
                    .any(|o| get(o).is_some()),
            };
            let fs = crate::fs::msdosfs::MsdosFs::mount(s, disk, read_only, opts)?;
            vfs::Mount::new(
                vfs::MountInfo {
                    type_name: "msdosfs",
                    flags: abi::mount::MNT_LOCAL | flags,
                    from: from.strip_suffix(b"\0").unwrap_or(from).to_vec(),
                    io_size: fs.cluster_size(),
                },
                Box::new(fs),
            )
        }
        _ => return Err(Errno::ENODEV),
    };
    let on = crate::fs::namespace().path_of(s, &covered, &crate::fs::root())?;
    crate::fs::namespace().mount(s, covered, m, &on)?;
    Ok(0)
}

/// `nmount` with `MNT_UPDATE`: the mount whose root `v` is goes
/// read-only or read-write (FreeBSD's `vfs_domount_update`). Soft updates
/// show (`MNT_SOFTDEP`) only while a UFS mount is read-write.
fn update(s: &Sleepable, v: &Arc<vfs::Vnode>, fstype: &[u8], flags: u64, force: bool) -> SysResult {
    use abi::mount::{MNT_RDONLY, MNT_SOFTDEP};
    if !v.is_mount_root() {
        return Err(Errno::EINVAL);
    }
    let m = v.mount();
    if fstype.strip_suffix(b"\0").unwrap_or(fstype) != m.type_name().as_bytes() {
        return Err(Errno::EOPNOTSUPP);
    }
    m.update(s, MNT_RDONLY, flags, force)?;
    if let Some(fs) = m.fs().as_any().downcast_ref::<crate::fs::ufs::UfsFs>() {
        let softdep = if fs.soft_updates() { MNT_SOFTDEP } else { 0 };
        m.update(s, MNT_SOFTDEP, softdep, false)?;
    }
    Ok(0)
}

/// The disk device node at `path` (a mount's `from`): its name in devfs
/// is the disk's.
fn disk_at(
    s: &Sleepable,
    p: &crate::proc::Process,
    path: &[u8],
) -> Result<Arc<storage::Disk>, Errno> {
    let path = path.strip_suffix(b"\0").unwrap_or(path);
    let v = crate::fs::lookup(s, p, At::Cwd, path, Walk::FOLLOW)?;
    if v.kind() != vfs::Kind::CharDevice {
        return Err(Errno::ENOTBLK);
    }
    // Its name under /dev: `vtbd0p2`, or an alias such as `gpt/rootfs0`.
    let name = match path.strip_prefix(b"/dev/") {
        Some(rest) => rest,
        None => path.rsplit(|&c| c == b'/').next().unwrap_or(path),
    };
    let name = core::str::from_utf8(name).map_err(|_| Errno::ENXIO)?;
    storage::find(name).ok_or(Errno::ENXIO)
}

/// `unmount(path, flags)`.
/// With `MNT_BYFSID` the path is `FSID:<val0>:<val1>`, a mount's
/// `f_fsid` (what `umount(8)` passes).
pub(super) fn unmount(s: &Sleepable, path_ptr: u64, flags: u64) -> SysResult {
    let p = process()?;
    let path = path(path_ptr)?;
    let force = flags & abi::mount::MNT_FORCE != 0;
    let m = if flags & abi::mount::MNT_BYFSID != 0 {
        let want = parse_fsid(&path).ok_or(Errno::EINVAL)?;
        crate::fs::namespace()
            .mounts()
            .into_iter()
            .find(|m| super::path::fsid(m) == want)
            .ok_or(Errno::ENOENT)?
    } else {
        let v = crate::fs::lookup(s, &p, At::Cwd, &path, Walk::FOLLOW)?;
        if !v.is_mount_root() {
            return Err(Errno::EINVAL);
        }
        Arc::clone(v.mount())
    };
    crate::fs::namespace().unmount(s, &m, force)?;
    Ok(0)
}

/// `FSID:<a>:<b>` (FreeBSD's `sscanf("FSID:%d:%d")`).
fn parse_fsid(path: &[u8]) -> Option<[i32; 2]> {
    let rest = core::str::from_utf8(path.strip_prefix(b"FSID:")?).ok()?;
    let (a, b) = rest.split_once(':')?;
    Some([a.parse().ok()?, b.parse().ok()?])
}

/// The ACL and extended-attribute calls (`__acl_*`, `extattr_*`): the
/// file must exist, then, as on a FreeBSD file system mounted without
/// them, `EOPNOTSUPP`.
pub(super) fn no_acls(s: &Sleepable, by_fd: bool, arg: u64, nofollow: bool) -> SysResult {
    let p = process()?;
    if by_fd {
        let _ = fd_vnode(s, arg)?;
    } else {
        let flag = if nofollow { AT_SYMLINK_NOFOLLOW } else { 0 };
        let _ = target(s, &p, cwd(), arg, flag)?;
    }
    Err(Errno::EOPNOTSUPP)
}
