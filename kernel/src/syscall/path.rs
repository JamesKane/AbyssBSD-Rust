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

use super::{SysResult, int, process, with_process};
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
    // Copied straight into the argument returned, not moved there.
    let mut arg = PathArg::Short {
        len: 0,
        bytes: [0; SHORT_PATH],
    };
    let PathArg::Short { len, bytes } = &mut arg else {
        unreachable!("just made short");
    };
    match uaccess::copy_in_str(addr, bytes) {
        Ok(n) => {
            *len = u8::try_from(n).map_err(|_| Errno::ENAMETOOLONG)?;
            Ok(arg)
        }
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
    need: &abi::capsicum::CapRights,
) -> Result<Arc<Vnode>, Errno> {
    let at = At::new(int(fd), *need);
    if path.is_empty() && empty_ok {
        return match at {
            At::Cwd => Ok(Arc::clone(p.cwd.lock().vnode())),
            At::Fd(fd, need) => p
                .files
                .lock(s)
                .get_checked(fd, &need.with(abi::capsicum::CAP_LOOKUP))?
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
    with_process(|p| {
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
            create: flags & O_CREAT != 0,
        };
        let ucred = p.cred();
        let cred = &*ucred;
        let mut created = false;
        // What the directory must allow (`flags_to_rights`); through a
        // strict walk the file inherits its limits (`ni_filecaps`).
        let need = crate::capsicum::rights::of_open(flags);
        let mut inherited = None;
        // Opened from a descriptor with `UF_RESOLVE_BENEATH`: so is this.
        let beneath;
        let v = if path.is_empty() && flags & O_EMPTY_PATH != 0 {
            // The descriptor itself, reopened: its `UF_RESOLVE_BENEATH`
            // carries over as a lookup from it would.
            let v = at_vnode(s, p, fd, &path, walk, true, &need)?;
            beneath = int(fd) != AT_FDCWD && p.files.lock(s).slot(int(fd))?.flags.resolve_beneath;
            v
        } else {
            let (found, caps, b) =
                crate::fs::resolve_caps(s, p, cred, At::new(int(fd), need), &path, walk)?;
            inherited = caps;
            beneath = b;
            match found {
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
                vfs::acl::access(s, &v, cred, want)?;
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
                resolve_beneath: beneath,
            };
            return super::io::install_caps(s, p, file, fdf, inherited);
        }
        let vf = if flags & O_PATH != 0 {
            VnodeFile::path_only(&v)
        } else {
            VnodeFile::open(s, &v, fflags & FWRITE != 0, flags & O_NONBLOCK != 0)?
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
            resolve_beneath: beneath,
        };
        super::io::install_caps(s, p, file, fdf, inherited)
    })
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
    with_process(|p| {
        let path = path(path_ptr)?;
        let walk = Walk {
            follow: flag & AT_SYMLINK_NOFOLLOW == 0,
            beneath: flag & AT_RESOLVE_BENEATH != 0,
            create: false,
        };
        let st = if path.is_empty() && flag & AT_EMPTY_PATH != 0 && int(fd) != AT_FDCWD {
            // The descriptor may be a pipe.
            super::io::file(s, p, fd, &crate::capsicum::rights::FSTAT)?.stat(s)
        } else {
            at_vnode(
                s,
                p,
                fd,
                &path,
                walk,
                flag & AT_EMPTY_PATH != 0,
                &crate::capsicum::rights::FSTAT,
            )?
            .stat(s)
        };
        // Written out from where it was made: not moved out of the result
        // first.
        match &st {
            Ok(st) => UserPtr::<Stat>::new(buf)
                .write(st)
                .map_err(|_| Errno::EFAULT)?,
            Err(e) => return Err(*e),
        }
        Ok(0)
    })
}

/// `chdir(path)`.
pub(super) fn chdir(s: &Sleepable, path_ptr: u64) -> SysResult {
    let p = process()?;
    let path = path(path_ptr)?;
    let v = crate::fs::lookup(s, &p, At::Cwd, &path, Walk::FOLLOW)?;
    if !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    vfs::acl::access(s, &v, &crate::fs::cred(), VEXEC)?;
    let old = core::mem::replace(&mut *p.cwd.lock(), v.use_ref());
    drop(old);
    Ok(0)
}

/// Makes `v` the process's root (`change_dir`, `pwd_chroot`): a
/// directory it may search. A process already in a changed root may not
/// change it again while it has a directory open
/// (`kern.chroot_allow_open_directories` is 1). The working directory
/// stays.
fn change_root(s: &Sleepable, p: &Process, v: &Arc<Vnode>) -> SysResult {
    if !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    vfs::acl::access(s, v, &crate::fs::cred(), VEXEC)?;
    if p.root.lock().is_some() {
        let t = p.files.lock(s);
        let open_dir = (0..t.len())
            .filter_map(|fd| i32::try_from(fd).ok())
            .any(|fd| {
                t.get(fd)
                    .is_ok_and(|f| f.vnode().is_some_and(|v| v.is_dir()))
            });
        if open_dir {
            return Err(Errno::EPERM);
        }
    }
    // The system's root is no changed root (`pwd_rdir == rootvnode`).
    let new = (!Arc::ptr_eq(v, &crate::fs::root())).then(|| v.use_ref());
    let old = core::mem::replace(&mut *p.root.lock(), new);
    drop(old);
    Ok(0)
}

/// `chroot(path)`: with `PRIV_VFS_CHROOT`.
pub(super) fn chroot(s: &Sleepable, path_ptr: u64) -> SysResult {
    let p = process()?;
    p.priv_check(crate::cred::Priv::VfsChroot)?;
    let path = path(path_ptr)?;
    let v = crate::fs::lookup(s, &p, At::Cwd, &path, Walk::FOLLOW)?;
    change_root(s, &p, &v)
}

/// `fchroot(fd)`: with `PRIV_VFS_FCHROOT` and `CAP_FCHROOT`.
pub(super) fn fchroot(s: &Sleepable, fd: u64) -> SysResult {
    let p = process()?;
    p.priv_check(crate::cred::Priv::VfsFchroot)?;
    let f = super::io::file(s, &p, fd, &crate::capsicum::rights::FCHROOT)?;
    let v = f.vnode().ok_or(Errno::ENOTDIR)?;
    change_root(s, &p, v)
}

/// `fchdir(fd)`.
pub(super) fn fchdir(s: &Sleepable, fd: u64) -> SysResult {
    let p = process()?;
    // A directory that must be looked beneath is no working directory.
    if p.files.lock(s).slot(int(fd))?.flags.resolve_beneath {
        return Err(Errno::ENOTCAPABLE);
    }
    let f = super::io::file(s, &p, fd, &crate::capsicum::rights::FCHDIR)?;
    let v = f.vnode().ok_or(Errno::ENOTDIR)?;
    if !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    vfs::acl::access(s, v, &crate::fs::cred(), VEXEC)?;
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
    let mut path = crate::fs::namespace().path_of(s, &cwd, &crate::fs::root_of(&p))?;
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
    let v = crate::fs::lookup(
        s,
        &p,
        At::new(int(fd), crate::capsicum::rights::NO),
        &path,
        Walk::FOLLOW,
    )?;
    let mut full = crate::fs::namespace().path_of(s, &v, &crate::fs::root_of(&p))?;
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
        create: false,
    };
    // Without AT_EACCESS, the lookup and the check both use the real ids
    // (FreeBSD's `kern_accessat`).
    let cred = if flag & AT_EACCESS != 0 {
        p.cred()
    } else {
        Arc::new(p.cred().as_real())
    };
    let v = if path.is_empty() && flag & AT_EMPTY_PATH != 0 {
        at_vnode(
            s,
            &p,
            fd,
            &path,
            walk,
            true,
            &crate::capsicum::rights::FSTAT,
        )?
    } else {
        let at = At::new(int(fd), crate::capsicum::rights::FSTAT);
        match crate::fs::resolve_as(s, &p, &cred, at, &path, walk)? {
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
    vfs::acl::access(s, &v, &cred, want)?;
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
    let v = at_vnode(
        s,
        &p,
        fd,
        &path,
        Walk::NOFOLLOW,
        false,
        &crate::capsicum::rights::NO,
    )?;
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
    let mut st = statfs_mount(s, v.mount())?;
    enforce_statfs(v.mount(), &mut st);
    Ok(st)
}

/// Whether the caller's jail may see mount `m` (`prison_canseemount`): with
/// `enforce_statfs` 0 every one, with 1 those under its root, with 2 only
/// its root's own.
fn can_see_mount(m: &vfs::Mount) -> bool {
    let pr = crate::prison::current();
    let st = pr.settings();
    if pr.is_prison0() || st.enforce_statfs == 0 {
        return true;
    }
    if core::ptr::eq(&**pr.root().mount(), m) {
        return true;
    }
    if st.enforce_statfs == 2 {
        return false;
    }
    let path = pr.path_from(crate::prison::prison0());
    if path == b"/" {
        return true;
    }
    let on = m.on();
    on.starts_with(&path) && matches!(on.get(path.len()), None | Some(&b'/'))
}

/// A jail's view of mount `m`'s statistics (`prison_enforce_statfs`):
/// another's mount is `[restricted]`, the fsid hidden past
/// `enforce_statfs` 1, and the mount point named from the jail's root.
fn enforce_statfs(m: &vfs::Mount, sp: &mut Statfs) {
    let pr = crate::prison::current();
    let st = pr.settings();
    if pr.is_prison0() || st.enforce_statfs == 0 {
        return;
    }
    if !can_see_mount(m) {
        sp.f_fsid = abi::mount::Fsid::default();
        sp.f_mntonname = [0; abi::mount::MNAMELEN];
        name_field(&mut sp.f_mntonname, b"[restricted]");
        return;
    }
    if st.enforce_statfs > 1 {
        sp.f_fsid = abi::mount::Fsid::default();
    }
    if core::ptr::eq(&**pr.root().mount(), m) {
        sp.f_mntonname = [0; abi::mount::MNAMELEN];
        sp.f_mntonname[0] = b'/';
        return;
    }
    let path = pr.path_from(crate::prison::prison0());
    if path == b"/" {
        return;
    }
    let on = m.on();
    let rest = on.get(path.len()..).unwrap_or(&[]).to_vec();
    sp.f_mntonname = [0; abi::mount::MNAMELEN];
    name_field(
        &mut sp.f_mntonname,
        if rest.is_empty() { b"/" } else { &rest },
    );
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
    // Only the mounts the caller's jail may see.
    let mounts: alloc::vec::Vec<_> = crate::fs::namespace()
        .mounts()
        .into_iter()
        .filter(|m| can_see_mount(m))
        .collect();
    if buf == 0 {
        return Ok(mounts.len() as u64);
    }
    let room = usize::try_from(bufsize).unwrap_or(usize::MAX) / size_of::<Statfs>();
    let n = mounts.len().min(room);
    for (i, m) in mounts.iter().take(n).enumerate() {
        let mut st = statfs_mount(s, m)?;
        enforce_statfs(m, &mut st);
        UserPtr::<Statfs>::new(buf + (i * size_of::<Statfs>()) as u64)
            .write(&st)
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
    let f = super::io::current_file(s, fd, &crate::capsicum::rights::FPATHCONF)?;
    match f.vnode() {
        Some(v) => pathconf_of(v, name),
        // A pipe: only its buffer size means anything.
        None if u32::try_from(name) == Ok(_PC_PIPE_BUF) => Ok(512),
        None => Err(Errno::EINVAL),
    }
}

fn pathconf_of(v: &Vnode, name: u64) -> SysResult {
    let name = u32::try_from(name).map_err(|_| Errno::EINVAL)?;
    // On a mount that keeps POSIX.1e ACLs, they are there, at full size
    // (`ufs_pathconf`).
    if vfs::acl::enabled(v) {
        match name {
            abi::fcntl::_PC_ACL_EXTENDED => return Ok(1),
            abi::fcntl::_PC_ACL_PATH_MAX => return Ok(u64::from(abi::acl::ACL_MAX_COUNT)),
            _ => {}
        }
    }
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
