// SPDX-License-Identifier: BSD-2-Clause
//! Paths: `open`/`openat`, `fstatat`, `chdir`/`fchdir`, `__getcwd`,
//! `access` and its relatives, `readlink`/`readlinkat`, `statfs`,
//! `getfsstat`, and `umask`.
//!
//! Every file system is read-only except devfs, so nothing can be created
//! or truncated: those fail with `EROFS`, as FreeBSD's do on a read-only
//! mount. Everyone is root, so permission bits only matter for execute.
//!
//! Behavior reference: sys/kern/vfs_syscalls.c, sys/kern/vfs_vnops.c
//! (`vn_open_cred`), sys/kern/vfs_cache.c (`kern___getcwd`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::*;
use abi::mount::{MNT_NOWAIT, MNT_WAIT, Statfs};
use abi::stat::Stat;
use sched::Sleepable;
use vfs::namei::Found;
use vfs::{Vnode, VnodeKind};

use super::io::install;
use super::{SysResult, int, process};
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};
use crate::fs::{At, Walk};
use crate::proc::Process;
use crate::uaccess::{self, UserPtr};

/// A path argument.
pub(super) fn path(addr: u64) -> Result<Vec<u8>, Errno> {
    let mut buf = alloc::vec![0u8; PATH_MAX];
    let n = uaccess::copy_in_str(addr, &mut buf).map_err(|e| match e {
        uaccess::StrError::Fault => Errno::EFAULT,
        uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    })?;
    buf.truncate(n);
    Ok(buf)
}

/// The vnode an `*at` call names: `path` from `fd`, or with an empty path
/// and `AT_EMPTY_PATH` (`O_EMPTY_PATH`), the file on `fd` itself.
fn at_vnode(
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
            At::Cwd => Ok(Arc::clone(&p.cwd.lock())),
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

/// `openat(fd, path, flags, mode)`.
pub(super) fn openat(s: &Sleepable, fd: u64, path_ptr: u64, flags: u64, _mode: u64) -> SysResult {
    let p = process()?;
    let flags = u32::try_from(flags & 0xffff_ffff).unwrap_or(0);
    let path = path(path_ptr)?;
    if flags & O_ACCMODE == O_ACCMODE || (flags & O_EXEC != 0 && flags & O_ACCMODE != 0) {
        return Err(Errno::EINVAL);
    }
    let walk = Walk {
        follow: flags & O_NOFOLLOW == 0,
        beneath: flags & O_RESOLVE_BENEATH != 0,
    };
    let v = if path.is_empty() && flags & O_EMPTY_PATH != 0 {
        at_vnode(s, &p, fd, &path, walk, true)?
    } else {
        match crate::fs::resolve(s, &p, At::new(int(fd)), &path, walk)? {
            Found::Vnode(v) => {
                if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
                    return Err(Errno::EEXIST);
                }
                v
            }
            // Nothing can be created yet: every file system that holds
            // files is read-only.
            Found::Missing { .. } if flags & O_CREAT != 0 => return Err(Errno::EROFS),
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
    if let VnodeKind::Symlink(_) = v.kind()
        && flags & O_PATH == 0
    {
        // Only reached with `O_NOFOLLOW`.
        return Err(Errno::EMLINK);
    }
    if flags & O_DIRECTORY != 0 && !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    // A device may open as another (`/dev/tty`).
    let v = match v.kind() {
        VnodeKind::CharDevice(d) if flags & O_PATH == 0 => d.ops.open()?.unwrap_or(v),
        _ => v,
    };
    if flags & O_PATH == 0 {
        let writes = fflags & FWRITE != 0 || flags & O_TRUNC != 0;
        match v.kind() {
            VnodeKind::Directory(_) if writes => return Err(Errno::EISDIR),
            VnodeKind::Regular(_) | VnodeKind::Directory(_) if writes && v.fs().read_only() => {
                return Err(Errno::EROFS);
            }
            VnodeKind::Regular(_) if flags & O_EXEC != 0 && v.meta().perm & 0o111 == 0 => {
                return Err(Errno::EACCES);
            }
            _ => {}
        }
    }
    let keep = FREAD | FWRITE | O_EXEC | FCNTLFLAGS;
    let file = File::new(FileKind::Vnode(v), fflags & keep);
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
        super::io::file(s, &p, fd)?.stat(s)
    } else {
        at_vnode(s, &p, fd, &path, walk, flag & AT_EMPTY_PATH != 0)?.stat()
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
    *p.cwd.lock() = v;
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
    *p.cwd.lock() = Arc::clone(v);
    Ok(0)
}

/// `__getcwd(buf, buflen)`.
pub(super) fn getcwd(buf: u64, buflen: u64) -> SysResult {
    let p = process()?;
    let len = usize::try_from(buflen).unwrap_or(usize::MAX).min(PATH_MAX);
    if len < 2 {
        return Err(Errno::EINVAL);
    }
    let cwd = Arc::clone(&p.cwd.lock());
    let mut path = crate::fs::namespace().path_of(&cwd, &crate::fs::root())?;
    if path.len() + 1 > len {
        return Err(Errno::ERANGE);
    }
    path.push(0);
    uaccess::copy_out(buf, &path).map_err(|_| Errno::EFAULT)?;
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
    let v = at_vnode(s, &p, fd, &path, walk, flag & AT_EMPTY_PATH != 0)?;
    // Root may read and write anything, and execute anything with an
    // execute bit (or search any directory).
    if amode & W_OK != 0 && v.fs().read_only() && !matches!(v.kind(), VnodeKind::CharDevice(_)) {
        return Err(Errno::EROFS);
    }
    if amode & X_OK != 0 && !v.is_dir() && v.meta().perm & 0o111 == 0 {
        return Err(Errno::EACCES);
    }
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
    let VnodeKind::Symlink(target) = v.kind() else {
        return Err(Errno::EINVAL);
    };
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
pub(super) fn statfs_of(v: &Vnode) -> Result<Statfs, Errno> {
    let ns = crate::fs::namespace();
    let m = ns.mount_of(v).ok_or(Errno::ENOENT)?;
    Ok(statfs_mount(m))
}

fn statfs_mount(m: &vfs::namei::Mount) -> Statfs {
    let fs = m.root.fs();
    let mut st = Statfs {
        f_type: fs.type_num,
        f_flags: fs.flags,
        f_bsize: u64::from(fs.io_size),
        f_iosize: u64::from(fs.io_size),
        f_namemax: u32::try_from(NAME_MAX).unwrap_or(u32::MAX),
        f_fsid: abi::mount::Fsid {
            val: [
                i32::from_ne_bytes(
                    u32::try_from(fs.id & 0xffff_ffff)
                        .unwrap_or(0)
                        .to_ne_bytes(),
                ),
                i32::try_from(fs.type_num).unwrap_or(0),
            ],
        },
        ..Statfs::default()
    };
    name_field(&mut st.f_fstypename, fs.type_name.as_bytes());
    name_field(&mut st.f_mntfromname, &fs.from);
    name_field(&mut st.f_mntonname, &m.on);
    st
}

/// `statfs(path, buf)`.
pub(super) fn statfs(s: &Sleepable, path_ptr: u64, buf: u64) -> SysResult {
    let p = process()?;
    let path = path(path_ptr)?;
    let v = crate::fs::lookup(s, &p, At::Cwd, &path, Walk::FOLLOW)?;
    let st = statfs_of(&v)?;
    UserPtr::<Statfs>::new(buf)
        .write(&st)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `getfsstat(buf, bufsize, mode)`: every mount, as many as fit; returns
/// how many there are (with no buffer) or how many were written.
pub(super) fn getfsstat(buf: u64, bufsize: u64, mode: u64) -> SysResult {
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
            .write(&statfs_mount(m))
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(n as u64)
}

/// `umask(newmask)`: returns the old mask.
pub(super) fn umask(newmask: u64) -> SysResult {
    let p = process()?;
    let mask = u16::try_from(newmask & u64::from(abi::stat::ALLPERMS)).unwrap_or(0);
    let old = p.umask.swap(mask, core::sync::atomic::Ordering::Relaxed);
    Ok(u64::from(old))
}
