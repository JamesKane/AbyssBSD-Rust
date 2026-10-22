// SPDX-License-Identifier: BSD-2-Clause
//! Paths: `open`/`openat`, `fstatat`, `chdir`/`fchdir`, `__getcwd`,
//! `access` and its relatives, `readlink`/`readlinkat`, `statfs`,
//! `getfsstat`, and `umask`. The calls that change the file system are in
//! `fsops`.
//!
//! Behavior reference: sys/kern/vfs_syscalls.c, sys/kern/vfs_vnops.c
//! (`vn_open_cred`), sys/kern/vfs_cache.c (`kern___getcwd`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::*;
use abi::mount::{MNT_NOWAIT, MNT_WAIT, Statfs};
use abi::stat::{APPEND, Stat};
use sched::Sleepable;
use vfs::namei::Found;
use vfs::ops::{VEXEC, VREAD, VWRITE};
use vfs::{Kind, NewNode, SetAttr, Vnode};

use super::io::install;
use super::{SysResult, int, process};
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind, VnodeFile};
use crate::fs::{At, Walk};
use crate::proc::Process;
use crate::uaccess::{self, UserPtr};

/// Paths this long or shorter (most of them) are copied in on the stack.
const SHORT_PATH: usize = 128;

/// A path argument: a short one inline, so most cost no allocation.
pub(super) enum PathArg {
    Short { len: u8, bytes: [u8; SHORT_PATH] },
    Long(Vec<u8>),
}

impl core::ops::Deref for PathArg {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Short { len, bytes } => &bytes[..usize::from(*len)],
            Self::Long(v) => v,
        }
    }
}

/// A path argument.
pub(super) fn path(addr: u64) -> Result<PathArg, Errno> {
    let errno = |e| match e {
        uaccess::StrError::Fault => Errno::EFAULT,
        uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    };
    let mut bytes = [0; SHORT_PATH];
    match uaccess::copy_in_str(addr, &mut bytes) {
        Ok(n) => Ok(PathArg::Short {
            len: u8::try_from(n).map_err(|_| Errno::ENAMETOOLONG)?,
            bytes,
        }),
        Err(uaccess::StrError::TooLong) => uaccess::copy_in_str_vec(addr, PATH_MAX)
            .map(PathArg::Long)
            .map_err(errno),
        Err(e) => Err(errno(e)),
    }
}

/// The vnode an `*at` call names: `path` from `fd`, or with an empty path
/// and `AT_EMPTY_PATH` (`O_EMPTY_PATH`), the file on `fd` itself.
pub(super) fn at_vnode(
    s: &Sleepable,
    p: &Process,
    fd: u64,
    path: &[u8],
    walk: Walk,
    empty_ok: bool,
) -> Result<Arc<Vnode>, Errno> {
    let at = At::new(int(fd));
    if path.is_empty() && empty_ok {
        return match at {
            At::Cwd => Ok(Arc::clone(p.cwd.lock().vnode())),
            At::Fd(fd) => p
                .files
                .lock(s)
                .get(fd)?
                .vnode()
                .cloned()
                .ok_or(Errno::EBADF),
        };
    }
    crate::fs::lookup(s, p, at, path, walk)
}

/// Whether writing a node of this kind writes its file system, which a
/// read-only mount refuses; devices, FIFOs, and sockets are written
/// elsewhere (FreeBSD's `ufs_access`).
fn stored_here(kind: Kind) -> bool {
    matches!(kind, Kind::Regular | Kind::Directory | Kind::Symlink)
}

/// `openat(fd, path, flags, mode)` (FreeBSD's `vn_open_cred`).
pub(super) fn openat(s: &Sleepable, fd: u64, path_ptr: u64, flags: u64, mode: u64) -> SysResult {
    let p = process()?;
    let flags = u32::try_from(flags & 0xffff_ffff).unwrap_or(0);
    let path = path(path_ptr)?;
    if flags & O_ACCMODE == O_ACCMODE || (flags & O_EXEC != 0 && flags & O_ACCMODE != 0) {
        return Err(Errno::EINVAL);
    }
    // O_CREAT with O_EXCL does not follow a final symbolic link: the link
    // itself exists.
    let exclusive = flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL;
    let walk = Walk {
        follow: flags & O_NOFOLLOW == 0 && !exclusive,
        beneath: flags & O_RESOLVE_BENEATH != 0,
    };
    let ucred = p.cred();
    let cred = &*ucred;
    let mut created = false;
    let v = if path.is_empty() && flags & O_EMPTY_PATH != 0 {
        at_vnode(s, &p, fd, &path, walk, true)?
    } else {
        match crate::fs::resolve_as(s, &p, cred, At::new(int(fd)), &path, walk)? {
            Found::Vnode(v) => {
                if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
                    return Err(Errno::EEXIST);
                }
                // O_CREAT names a file: an existing directory is not one,
                // even to read.
                if flags & O_CREAT != 0 && v.is_dir() {
                    return Err(Errno::EISDIR);
                }
                v
            }
            Found::Missing { dir, name } if flags & O_CREAT != 0 => {
                // A new file cannot end in a slash.
                if path.last() == Some(&b'/') {
                    return Err(Errno::ENOENT);
                }
                let mask = p.umask.load(core::sync::atomic::Ordering::Relaxed);
                let cmode = (u16::try_from(mode & 0o7777).unwrap_or(0) & !mask)
                    & abi::stat::ALLPERMS
                    & !abi::stat::S_ISVTX;
                let c = vfs::Create {
                    mode: cmode,
                    uid: cred.uid,
                    gid: cred.gid,
                    time: crate::fs::now(),
                };
                let (v, made) = vfs::ops::create(
                    s,
                    &dir,
                    &name,
                    &NewNode::Regular,
                    &c,
                    cred,
                    flags & O_EXCL != 0,
                )?;
                created = made;
                v
            }
            Found::Missing { .. } => return Err(Errno::ENOENT),
        }
    };
    // `O_PATH` opens a handle for `*at`, `fstat`, and `fchdir` only.
    let fflags = if flags & O_PATH != 0 {
        0
    } else if flags & O_EXEC != 0 {
        flags
    } else {
        flags + 1
    };
    if v.kind() == Kind::Symlink && flags & O_PATH == 0 {
        // Only reached with `O_NOFOLLOW`.
        return Err(Errno::EMLINK);
    }
    // A socket is reached through `connect`, not `open`.
    if v.kind() == Kind::Socket && flags & O_PATH == 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if flags & O_DIRECTORY != 0 && !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    if flags & O_PATH == 0 {
        let writes = fflags & FWRITE != 0 || flags & O_TRUNC != 0;
        if v.is_dir() && writes {
            return Err(Errno::EISDIR);
        }
        if writes && stored_here(v.kind()) && v.mount().read_only() {
            return Err(Errno::EROFS);
        }
        // A file just made needs no check: its creator may open it.
        if !created {
            let mut want = 0;
            if fflags & FREAD != 0 {
                want |= VREAD;
            }
            if writes {
                want |= VWRITE;
            }
            if flags & O_EXEC != 0 {
                want |= VEXEC;
            }
            vfs::ops::access(&v.attr(s)?, v.kind(), cred, want)?;
        }
        // An append-only file is written only at its end (FreeBSD's
        // `ufs_open`).
        if fflags & FWRITE != 0 && flags & O_APPEND == 0 && v.attr(s)?.flags & APPEND != 0 {
            return Err(Errno::EPERM);
        }
        if flags & O_TRUNC != 0 && v.kind() == Kind::Regular && !created {
            let zero = SetAttr {
                size: Some(0),
                ..SetAttr::default()
            };
            vfs::ops::setattr(s, &v, &zero, cred, false)?;
        }
    }
    // A device may open as another (`/dev/tty`).
    let v = match v.kind() {
        Kind::CharDevice if flags & O_PATH == 0 => v.device(s)?.ops.open()?.unwrap_or(v),
        _ => v,
    };
    let keep = FREAD | FWRITE | O_EXEC | FCNTLFLAGS;
    if v.kind() == Kind::Fifo && flags & O_PATH == 0 {
        let f = crate::fs::fifo::FifoFile::open(
            s,
            &v,
            fflags & FREAD != 0,
            fflags & FWRITE != 0,
            flags & O_NONBLOCK != 0,
        )?;
        let file = File::new(FileKind::Fifo(f), fflags & keep);
        let fdf = FdFlags {
            cloexec: flags & O_CLOEXEC != 0,
            clofork: flags & O_CLOFORK != 0,
        };
        return install(s, &p, file, fdf);
    }
    let vf = if flags & O_PATH != 0 {
        VnodeFile::path_only(&v)
    } else {
        VnodeFile::open(s, &v, fflags & FWRITE != 0)?
    };
    let file = File::new(FileKind::Vnode(vf), fflags & keep);
    // O_SHLOCK and O_EXLOCK take an `flock` lock as the file opens.
    if flags & (O_SHLOCK | O_EXLOCK) != 0
        && flags & O_PATH == 0
        && let FileKind::Vnode(vf) = file.kind()
    {
        let ty = if flags & O_EXLOCK != 0 {
            vfs::lockf::LockType::Exclusive
        } else {
            vfs::lockf::LockType::Shared
        };
        let req = vfs::lockf::Lock {
            owner: vf.lock_owner(),
            pid: 0,
            ty,
            start: 0,
            end: u64::MAX,
        };
        vf.vnode.lockf().lock(s, req, flags & O_NONBLOCK == 0)?;
    }
    let fdf = FdFlags {
        cloexec: flags & O_CLOEXEC != 0,
        clofork: flags & O_CLOFORK != 0,
    };
    install(s, &p, file, fdf)
}

/// `open(path, flags, mode)`.
pub(super) fn open(s: &Sleepable, path: u64, flags: u64, mode: u64) -> SysResult {
    openat(s, u64::from(AT_FDCWD.cast_unsigned()), path, flags, mode)
}

/// `fstatat(fd, path, buf, flag)` (`stat` and `lstat` in libc).
pub(super) fn fstatat(s: &Sleepable, fd: u64, path_ptr: u64, buf: u64, flag: u64) -> SysResult {
    let flag = u32::try_from(flag).map_err(|_| Errno::EINVAL)?;
    if flag & !(AT_SYMLINK_NOFOLLOW | AT_RESOLVE_BENEATH | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let path = path(path_ptr)?;
    let walk = Walk {
        follow: flag & AT_SYMLINK_NOFOLLOW == 0,
        beneath: flag & AT_RESOLVE_BENEATH != 0,
    };
    let st = if path.is_empty() && flag & AT_EMPTY_PATH != 0 && At::new(int(fd)) != At::Cwd {
        // The descriptor may be a pipe.
        super::io::file(s, &p, fd)?.stat(s)?
    } else {
        at_vnode(s, &p, fd, &path, walk, flag & AT_EMPTY_PATH != 0)?.stat(s)?
    };
    UserPtr::<Stat>::new(buf)
        .write(&st)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `chdir(path)`.
pub(super) fn chdir(s: &Sleepable, path_ptr: u64) -> SysResult {
    let p = process()?;
    let path = path(path_ptr)?;
    let v = crate::fs::lookup(s, &p, At::Cwd, &path, Walk::FOLLOW)?;
    if !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    vfs::ops::access(&v.attr(s)?, v.kind(), &crate::fs::cred(), VEXEC)?;
    let old = core::mem::replace(&mut *p.cwd.lock(), v.use_ref());
    drop(old);
    Ok(0)
}

/// `fchdir(fd)`.
pub(super) fn fchdir(s: &Sleepable, fd: u64) -> SysResult {
    let p = process()?;
    let f = super::io::file(s, &p, fd)?;
    let v = f.vnode().ok_or(Errno::ENOTDIR)?;
    if !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    vfs::ops::access(&v.attr(s)?, v.kind(), &crate::fs::cred(), VEXEC)?;
    let old = core::mem::replace(&mut *p.cwd.lock(), v.use_ref());
    drop(old);
    Ok(0)
}

/// `__getcwd(buf, buflen)`.
pub(super) fn getcwd(s: &Sleepable, buf: u64, buflen: u64) -> SysResult {
    let p = process()?;
    let len = usize::try_from(buflen).unwrap_or(usize::MAX).min(PATH_MAX);
    if len < 2 {
        return Err(Errno::EINVAL);
    }
    let cwd = Arc::clone(p.cwd.lock().vnode());
    let mut path = crate::fs::namespace().path_of(s, &cwd, &crate::fs::root())?;
    if path.len() + 1 > len {
        return Err(Errno::ERANGE);
    }
    path.push(0);
    uaccess::copy_out(buf, &path).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `__realpathat(fd, path, buf, size, flags)`: the absolute path `path`
/// names, symbolic links followed (FreeBSD's `kern___realpathat`, which
/// names a file by the name it was found under).
pub(super) fn realpathat(
    s: &Sleepable,
    fd: u64,
    path_ptr: u64,
    buf: u64,
    size: u64,
    flags: u64,
) -> SysResult {
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let path = path(path_ptr)?;
    let v = crate::fs::lookup(s, &p, At::new(int(fd)), &path, Walk::FOLLOW)?;
    let mut full = crate::fs::namespace().path_of(s, &v, &crate::fs::root())?;
    // FreeBSD's `vn_fullpath_hardlink` says a short buffer is ENOMEM.
    if full.len() + 1 > usize::try_from(size).unwrap_or(usize::MAX) {
        return Err(Errno::ENOMEM);
    }
    full.push(0);
    uaccess::copy_out(buf, &full).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `faccessat(fd, path, amode, flag)`.
pub(super) fn faccessat(s: &Sleepable, fd: u64, path_ptr: u64, amode: u64, flag: u64) -> SysResult {
    let flag = u32::try_from(flag).map_err(|_| Errno::EINVAL)?;
    if flag & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_RESOLVE_BENEATH | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let amode = u32::try_from(amode).map_err(|_| Errno::EINVAL)?;
    if amode & !(R_OK | W_OK | X_OK) != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let path = path(path_ptr)?;
    let walk = Walk {
        follow: flag & AT_SYMLINK_NOFOLLOW == 0,
        beneath: flag & AT_RESOLVE_BENEATH != 0,
    };
    // Without AT_EACCESS, the lookup and the check both use the real ids
    // (FreeBSD's `kern_accessat`).
    let cred = if flag & AT_EACCESS != 0 {
        p.cred()
    } else {
        Arc::new(p.cred().as_real())
    };
    let v = if path.is_empty() && flag & AT_EMPTY_PATH != 0 {
        at_vnode(s, &p, fd, &path, walk, true)?
    } else {
        match crate::fs::resolve_as(s, &p, &cred, At::new(int(fd)), &path, walk)? {
            Found::Vnode(v) => v,
            Found::Missing { .. } => return Err(Errno::ENOENT),
        }
    };
    if amode & W_OK != 0 && stored_here(v.kind()) && v.mount().read_only() {
        return Err(Errno::EROFS);
    }
    let mut want = 0;
    if amode & R_OK != 0 {
        want |= VREAD;
    }
    if amode & W_OK != 0 {
        want |= VWRITE;
    }
    if amode & X_OK != 0 {
        want |= VEXEC;
    }
    vfs::ops::access(&v.attr(s)?, v.kind(), &cred, want)?;
    Ok(0)
}

/// `access(path, amode)`; `eaccess` too (with `effective`).
pub(super) fn access(s: &Sleepable, path: u64, amode: u64, effective: bool) -> SysResult {
    let flag = if effective { AT_EACCESS } else { 0 };
    faccessat(
        s,
        u64::from(AT_FDCWD.cast_unsigned()),
        path,
        amode,
        u64::from(flag),
    )
}

/// `readlinkat(fd, path, buf, bufsize)`.
pub(super) fn readlinkat(
    s: &Sleepable,
    fd: u64,
    path_ptr: u64,
    buf: u64,
    bufsize: u64,
) -> SysResult {
    if bufsize > i64::MAX as u64 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let path = path(path_ptr)?;
    let v = at_vnode(s, &p, fd, &path, Walk::NOFOLLOW, false)?;
    let target = v.readlink(s)?;
    let n = target
        .len()
        .min(usize::try_from(bufsize).unwrap_or(usize::MAX));
    uaccess::copy_out(buf, &target[..n]).map_err(|_| Errno::EFAULT)?;
    Ok(n as u64)
}

/// `readlink(path, buf, count)`.
pub(super) fn readlink(s: &Sleepable, path: u64, buf: u64, count: u64) -> SysResult {
    readlinkat(s, u64::from(AT_FDCWD.cast_unsigned()), path, buf, count)
}

/// Copies `src` into a NUL-padded name field (truncating, NUL kept).
fn name_field(dst: &mut [u8], src: &[u8]) {
    let n = src.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&src[..n]);
}

/// `struct statfs` for the file system holding `v`.
pub(super) fn statfs_of(s: &Sleepable, v: &Vnode) -> Result<Statfs, Errno> {
    statfs_mount(s, v.mount())
}

fn statfs_mount(s: &Sleepable, m: &vfs::Mount) -> Result<Statfs, Errno> {
    let c = m.statfs(s)?;
    let info = m.info();
    let mut st = Statfs {
        f_type: m.type_num,
        f_flags: m.flags(),
        f_bsize: c.bsize,
        f_iosize: c.iosize,
        f_blocks: c.blocks,
        f_bfree: c.bfree,
        f_bavail: c.bavail,
        f_files: c.files,
        f_ffree: c.ffree,
        f_namemax: u32::try_from(NAME_MAX).unwrap_or(u32::MAX),
        f_fsid: abi::mount::Fsid { val: fsid(m) },
        ..Statfs::default()
    };
    name_field(&mut st.f_fstypename, info.type_name.as_bytes());
    name_field(&mut st.f_mntfromname, &info.from);
    name_field(&mut st.f_mntonname, &m.on());
    Ok(st)
}

/// `statfs(path, buf)`.
pub(super) fn statfs(s: &Sleepable, path_ptr: u64, buf: u64) -> SysResult {
    let p = process()?;
    let path = path(path_ptr)?;
    let v = crate::fs::lookup(s, &p, At::Cwd, &path, Walk::FOLLOW)?;
    let st = statfs_of(s, &v)?;
    UserPtr::<Statfs>::new(buf)
        .write(&st)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `getfsstat(buf, bufsize, mode)`: every mount, as many as fit; returns
/// how many there are (with no buffer) or how many were written.
pub(super) fn getfsstat(s: &Sleepable, buf: u64, bufsize: u64, mode: u64) -> SysResult {
    let mode = u32::try_from(mode).map_err(|_| Errno::EINVAL)?;
    if mode != MNT_WAIT && mode != MNT_NOWAIT {
        return Err(Errno::EINVAL);
    }
    let mounts = crate::fs::namespace().mounts();
    if buf == 0 {
        return Ok(mounts.len() as u64);
    }
    let room = usize::try_from(bufsize).unwrap_or(usize::MAX) / size_of::<Statfs>();
    let n = mounts.len().min(room);
    for (i, m) in mounts.iter().take(n).enumerate() {
        UserPtr::<Statfs>::new(buf + (i * size_of::<Statfs>()) as u64)
            .write(&statfs_mount(s, m)?)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(n as u64)
}

/// `pathconf(path, name)`, `lpathconf` (`nofollow`).
pub(super) fn pathconf(s: &Sleepable, path_ptr: u64, name: u64, nofollow: bool) -> SysResult {
    let p = process()?;
    let path = path(path_ptr)?;
    let walk = if nofollow {
        Walk::NOFOLLOW
    } else {
        Walk::FOLLOW
    };
    let v = crate::fs::lookup(s, &p, At::Cwd, &path, walk)?;
    pathconf_of(&v, name)
}

/// `fpathconf(fd, name)`.
pub(super) fn fpathconf(s: &Sleepable, fd: u64, name: u64) -> SysResult {
    let f = super::io::current_file(s, fd)?;
    match f.vnode() {
        Some(v) => pathconf_of(v, name),
        // A pipe: only its buffer size means anything.
        None if u32::try_from(name) == Ok(_PC_PIPE_BUF) => Ok(512),
        None => Err(Errno::EINVAL),
    }
}

fn pathconf_of(v: &Vnode, name: u64) -> SysResult {
    let name = u32::try_from(name).map_err(|_| Errno::EINVAL)?;
    let r = v.mount().fs().pathconf(v.kind(), name)?;
    Ok(r.cast_unsigned())
}

/// `umask(newmask)`: returns the old mask.
pub(super) fn umask(newmask: u64) -> SysResult {
    let p = process()?;
    let mask = u16::try_from(newmask & u64::from(abi::stat::ALLPERMS)).unwrap_or(0);
    let old = p.umask.swap(mask, core::sync::atomic::Ordering::Relaxed);
    Ok(u64::from(old))
}

/// Mount `m`'s `f_fsid`: its id's low half and its type number.
pub(super) fn fsid(m: &vfs::Mount) -> [i32; 2] {
    [
        i32::from_ne_bytes(u32::try_from(m.id & 0xffff_ffff).unwrap_or(0).to_ne_bytes()),
        i32::try_from(m.type_num).unwrap_or(0),
    ]
}
