// SPDX-License-Identifier: BSD-2-Clause
//! Descriptors: reading and writing (`read`, `write`, their vector and
//! positioned forms), `lseek`, `close` and `close_range`, `dup`, `dup2`,
//! `fcntl`, `ioctl`, `pipe2`, `fstat`, `fstatfs`, `getdirentries`, and
//! `copy_file_range`.
//!
//! Behavior reference: sys/kern/sys_generic.c, sys/kern/kern_descrip.c,
//! sys/kern/sys_pipe.c, sys/kern/vfs_syscalls.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::capsicum::CapRights;
use abi::errno::Errno;
use abi::fcntl::*;
use abi::ioctl::{
    FIOASYNC, FIOCLEX, FIONBIO, FIONCLEX, IOC_IN, IOC_OUT, IOC_VOID, IOCPARM_MAX, param_len,
};
use abi::mount::Statfs;
use abi::stat::Stat;
use sched::Sleepable;

use super::{SysResult, int, process, with_process};
use crate::capsicum::rights as R;
use crate::fs::fdtable::{FdFlags, Slot};
use crate::fs::file::{File, FileKind};
use crate::fs::pipe;
use crate::fs::uio::UserIo;
use crate::proc::Process;
use crate::uaccess::{self, Plain, UserPtr};

// SAFETY: integers and timespecs, padding explicit (`abi-check`).
unsafe impl Plain for Stat {}
// SAFETY: integers and byte arrays, no padding (`abi-check`).
unsafe impl Plain for Statfs {}

/// The most one transfer moves (`IOSIZE_MAX`, `SSIZE_MAX`).
const IOSIZE_MAX: u64 = i64::MAX as u64;

/// The descriptor limit (`RLIMIT_NOFILE`'s soft limit).
pub(super) fn max_files(p: &Process) -> usize {
    let lim = p.limits.lock()[abi::resource::rlimit::NOFILE as usize].rlim_cur;
    usize::try_from(lim).unwrap_or(usize::MAX)
}

/// The file open on `fd`, if the descriptor holds the rights in `need`
/// (`ENOTCAPABLE` if not).
pub(super) fn file(
    s: &Sleepable,
    p: &Process,
    fd: u64,
    need: &CapRights,
) -> Result<Arc<File>, Errno> {
    p.files.lock(s).get_checked(int(fd), need)
}

/// The file open on `fd` in the current process (borrowed, so no
/// reference to the process is taken), with the rights in `need`.
pub(super) fn current_file(s: &Sleepable, fd: u64, need: &CapRights) -> Result<Arc<File>, Errno> {
    Process::with_current(|p| file(s, p, fd, need)).unwrap_or(Err(Errno::EINVAL))
}

/// Runs `f` on the file open on `fd` in the current process. A process
/// with one thread lends the file straight from its table, without the
/// table's lock or a reference: each would be a locked instruction, and
/// `read` and `write` would pay for them on every call. With several
/// threads, one could close `fd` meanwhile, so the file is referenced.
///
/// `f` must not touch the descriptor table (it is a transfer).
fn with_file<R>(
    s: &Sleepable,
    fd: u64,
    need: &CapRights,
    f: impl FnOnce(&File) -> Result<R, Errno>,
) -> Result<R, Errno> {
    Process::with_current(|p| {
        if p.sole_user_of_files() {
            // SAFETY: only the process's threads reach its table, this is
            // the only one, and it does nothing else with the table until
            // `f` returns, which the borrow cannot outlive.
            let table = unsafe { p.files.get_unlocked() };
            f(&table.checked(int(fd), need)?.file)
        } else {
            let file = file(s, p, fd, need)?;
            f(&file)
        }
    })
    .unwrap_or(Err(Errno::EINVAL))
}

/// Installs `file` at the lowest free descriptor.
pub(super) fn install(s: &Sleepable, p: &Process, file: Arc<File>, flags: FdFlags) -> SysResult {
    install_caps(s, p, file, flags, None)
}

/// [`install`] with Capsicum limits.
pub(super) fn install_caps(
    s: &Sleepable,
    p: &Process,
    file: Arc<File>,
    flags: FdFlags,
    caps: Option<crate::fs::fdtable::FileCaps>,
) -> SysResult {
    let limit = max_files(p);
    let fd = p.files.lock(s).alloc_with(file, flags, caps, 0, limit)?;
    Ok(u64::from(fd.unsigned_abs()))
}

/// A transfer's count: `EPIPE` also sends `SIGPIPE` (FreeBSD's
/// `dofilewrite`; sockets will decide for themselves).
fn wrote(r: Result<usize, Errno>) -> SysResult {
    match r {
        Ok(n) => Ok(n as u64),
        Err(Errno::EPIPE) => {
            let _ = crate::signal::post_thread(
                &super::thread()?,
                crate::signal::user_info(abi::signal::SIGPIPE, 0, abi::signal::code::SI_KERNEL),
            );
            Err(Errno::EPIPE)
        }
        Err(e) => Err(e),
    }
}

fn check_len(n: u64) -> Result<(), Errno> {
    if n > IOSIZE_MAX {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}

/// `read(fd, buf, nbyte)`.
pub(super) fn read(s: &Sleepable, fd: u64, buf: u64, nbyte: u64) -> SysResult {
    check_len(nbyte)?;
    let mut uio = UserIo::one(buf, nbyte)?;
    with_file(s, fd, &R::READ, |f| f.read(s, &mut uio)).map(|n| n as u64)
}

/// `readv(fd, iovp, iovcnt)`.
pub(super) fn readv(s: &Sleepable, fd: u64, iov: u64, iovcnt: u64) -> SysResult {
    let mut uio = UserIo::vector(iov, iovcnt)?;
    with_file(s, fd, &R::READ, |f| f.read(s, &mut uio)).map(|n| n as u64)
}

/// `pread(fd, buf, nbyte, offset)`.
pub(super) fn pread(s: &Sleepable, fd: u64, buf: u64, nbyte: u64, offset: u64) -> SysResult {
    check_len(nbyte)?;
    let mut uio = UserIo::one(buf, nbyte)?;
    with_file(s, fd, &R::PREAD, |f| {
        f.pread(s, &mut uio, offset.cast_signed())
    })
    .map(|n| n as u64)
}

/// `preadv(fd, iovp, iovcnt, offset)`.
pub(super) fn preadv(s: &Sleepable, fd: u64, iov: u64, iovcnt: u64, offset: u64) -> SysResult {
    let mut uio = UserIo::vector(iov, iovcnt)?;
    with_file(s, fd, &R::PREAD, |f| {
        f.pread(s, &mut uio, offset.cast_signed())
    })
    .map(|n| n as u64)
}

/// `write(fd, buf, nbyte)`.
pub(super) fn write(s: &Sleepable, fd: u64, buf: u64, nbyte: u64) -> SysResult {
    check_len(nbyte)?;
    let mut uio = UserIo::one(buf, nbyte)?;
    wrote(with_file(s, fd, &R::WRITE, |f| f.write(s, &mut uio)))
}

/// `writev(fd, iovp, iovcnt)`.
pub(super) fn writev(s: &Sleepable, fd: u64, iov: u64, iovcnt: u64) -> SysResult {
    let mut uio = UserIo::vector(iov, iovcnt)?;
    wrote(with_file(s, fd, &R::WRITE, |f| f.write(s, &mut uio)))
}

/// `pwrite(fd, buf, nbyte, offset)`.
pub(super) fn pwrite(s: &Sleepable, fd: u64, buf: u64, nbyte: u64, offset: u64) -> SysResult {
    check_len(nbyte)?;
    let mut uio = UserIo::one(buf, nbyte)?;
    wrote(with_file(s, fd, &R::PWRITE, |f| {
        f.pwrite(s, &mut uio, offset.cast_signed())
    }))
}

/// `pwritev(fd, iovp, iovcnt, offset)`.
pub(super) fn pwritev(s: &Sleepable, fd: u64, iov: u64, iovcnt: u64, offset: u64) -> SysResult {
    let mut uio = UserIo::vector(iov, iovcnt)?;
    wrote(with_file(s, fd, &R::PWRITE, |f| {
        f.pwrite(s, &mut uio, offset.cast_signed())
    }))
}

/// `lseek(fd, offset, whence)`.
pub(super) fn lseek(s: &Sleepable, fd: u64, offset: u64, whence: u64) -> SysResult {
    let f = current_file(s, fd, &R::SEEK)?;
    let whence = u32::try_from(whence).map_err(|_| Errno::EINVAL)?;
    f.seek(s, offset.cast_signed(), whence)
        .map(i64::cast_unsigned)
}

/// `close(fd)`.
pub(super) fn close(s: &Sleepable, fd: u64) -> SysResult {
    with_process(|p| {
        let closed = p.files.lock(s).close(int(fd))?;
        crate::fs::closed(p.pid(), [&closed]);
        drop(closed);
        Ok(0)
    })
}

/// `close_range(lowfd, highfd, flags)` (libc's `closefrom` too).
pub(super) fn close_range(s: &Sleepable, lo: u64, hi: u64, flags: u64) -> SysResult {
    let flags = u32::try_from(flags).map_err(|_| Errno::EINVAL)?;
    if flags & !(CLOSE_RANGE_CLOEXEC | CLOSE_RANGE_CLOFORK) != 0 {
        return Err(Errno::EINVAL);
    }
    // `u_int`s.
    let (lo, hi) = (lo & 0xffff_ffff, hi & 0xffff_ffff);
    if lo > hi {
        return Err(Errno::EINVAL);
    }
    let to = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
    let p = process()?;
    let closed = p.files.lock(s).close_range(
        to(lo),
        to(hi),
        flags & CLOSE_RANGE_CLOEXEC != 0,
        flags & CLOSE_RANGE_CLOFORK != 0,
    );
    crate::fs::closed(p.pid(), &closed);
    drop(closed);
    Ok(0)
}

/// How a duplicate is made (FreeBSD's `kern_dup` modes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dup {
    /// The lowest free descriptor (`dup`).
    Lowest,
    /// The lowest free one at or above the target (`F_DUPFD`).
    AtLeast,
    /// Exactly the target, replacing what is there (`dup2`).
    Fixed,
}

/// Duplicates `old` as `mode` says, with descriptor flags `flags`.
fn dup_fd(s: &Sleepable, mode: Dup, flags: FdFlags, old: i32, new: i32) -> SysResult {
    let p = process()?;
    let limit = max_files(&p);
    let out_of_range = if mode == Dup::AtLeast {
        Errno::EINVAL
    } else {
        Errno::EBADF
    };
    if mode != Dup::Lowest {
        let n = usize::try_from(new).map_err(|_| out_of_range)?;
        if n >= limit {
            return Err(out_of_range);
        }
    }
    let mut t = p.files.lock(s);
    // The copy keeps the original's Capsicum limits (`fdcopy`'s
    // `filecaps_copy`).
    // The copy keeps `UF_RESOLVE_BENEATH` too (`kern_dup`).
    let (file, caps, flags) = {
        let slot = t.slot(old)?;
        let flags = FdFlags {
            resolve_beneath: slot.flags.resolve_beneath,
            ..flags
        };
        (Arc::clone(&slot.file), slot.caps.as_deref().cloned(), flags)
    };
    let fd = match mode {
        Dup::Lowest => t.alloc_with(file, flags, caps, 0, limit)?,
        Dup::AtLeast => {
            t.alloc_with(file, flags, caps, usize::try_from(new).unwrap_or(0), limit)?
        }
        Dup::Fixed if old == new => {
            if flags.cloexec {
                let mut f = t.slot(old)?.flags;
                f.cloexec = true;
                t.set_flags(old, f)?;
            }
            new
        }
        Dup::Fixed => {
            let replaced = t.install_with(new, file, flags, caps, limit)?;
            drop(t);
            crate::fs::closed(p.pid(), replaced.iter());
            drop(replaced);
            new
        }
    };
    Ok(u64::from(fd.unsigned_abs()))
}

/// `dup(fd)`.
pub(super) fn dup(s: &Sleepable, fd: u64) -> SysResult {
    dup_fd(s, Dup::Lowest, FdFlags::default(), int(fd), 0)
}

/// `dup2(from, to)`.
pub(super) fn dup2(s: &Sleepable, from: u64, to: u64) -> SysResult {
    dup_fd(s, Dup::Fixed, FdFlags::default(), int(from), int(to))
}

/// `F_GETFD`'s bits.
fn fd_bits(f: FdFlags) -> u32 {
    (if f.cloexec { FD_CLOEXEC } else { 0 })
        | (if f.clofork { FD_CLOFORK } else { 0 })
        | if f.resolve_beneath {
            FD_RESOLVE_BENEATH
        } else {
            0
        }
}

/// `O_*` flags from a file's flags (`OFLAGS`).
fn oflags(fflags: u32) -> u32 {
    // FreeBSD's unsigned `x - 1`.
    if fflags & O_EXEC != 0 {
        fflags
    } else {
        fflags.wrapping_sub(1)
    }
}

/// The file open on `fd` for `fcntl` command `cmd` (`fget_fcntl`):
/// `CAP_FCNTL` and the command in the descriptor's `fcntl` limits.
fn fcntl_file(s: &Sleepable, fd: i32, cmd: u32) -> Result<Arc<File>, Errno> {
    with_process(|p| {
        let t = p.files.lock(s);
        let slot = t.checked(fd, &R::FCNTL)?;
        if slot
            .caps
            .as_ref()
            .is_some_and(|c| c.fcntls & (1 << cmd) == 0)
        {
            return Err(Errno::ENOTCAPABLE);
        }
        Ok(Arc::clone(&slot.file))
    })
}

/// `fcntl(fd, cmd, arg)`.
pub(super) fn fcntl(s: &Sleepable, fd: u64, cmd: u64, arg: u64) -> SysResult {
    let fd = int(fd);
    let cmd = u32::try_from(cmd).map_err(|_| Errno::EINVAL)?;
    let cloexec = FdFlags {
        cloexec: true,
        clofork: false,
        resolve_beneath: false,
    };
    let clofork = FdFlags {
        cloexec: false,
        clofork: true,
        resolve_beneath: false,
    };
    match cmd {
        F_DUPFD => dup_fd(s, Dup::AtLeast, FdFlags::default(), fd, int(arg)),
        F_DUPFD_CLOEXEC => dup_fd(s, Dup::AtLeast, cloexec, fd, int(arg)),
        F_DUPFD_CLOFORK => dup_fd(s, Dup::AtLeast, clofork, fd, int(arg)),
        F_DUP2FD => dup_fd(s, Dup::Fixed, FdFlags::default(), fd, int(arg)),
        F_DUP2FD_CLOEXEC => dup_fd(s, Dup::Fixed, cloexec, fd, int(arg)),
        F_GETFD => {
            let p = process()?;
            let t = p.files.lock(s);
            Ok(u64::from(fd_bits(t.slot(fd)?.flags)))
        }
        F_SETFD => {
            let p = process()?;
            let bits = u32::try_from(arg & 0xffff_ffff).unwrap_or(0);
            let mut t = p.files.lock(s);
            // `UF_RESOLVE_BENEATH` may be set, never cleared.
            let had = t.slot(fd)?.flags.resolve_beneath;
            t.set_flags(
                fd,
                FdFlags {
                    cloexec: bits & FD_CLOEXEC != 0,
                    clofork: bits & FD_CLOFORK != 0,
                    resolve_beneath: had || bits & FD_RESOLVE_BENEATH != 0,
                },
            )?;
            Ok(0)
        }
        F_GETFL => {
            let f = fcntl_file(s, fd, F_GETFL)?;
            Ok(u64::from(oflags(f.flags())))
        }
        F_SETFL => {
            let f = fcntl_file(s, fd, F_SETFL)?;
            let bits = u32::try_from(arg & 0xffff_ffff).unwrap_or(0) & !O_ACCMODE;
            // Non-blocking and async are the file's own business on
            // FreeBSD (`FIONBIO`, `FIOASYNC`); the files here only read
            // the flag.
            f.set_flags(FCNTLFLAGS, bits);
            Ok(0)
        }
        F_ISUNIONSTACK => {
            let f = current_file(s, u64::from(fd.cast_unsigned()), &R::NO)?;
            // No union mounts.
            f.vnode().map(|_| 0).ok_or(Errno::EBADF)
        }
        F_READAHEAD | F_RDAHEAD => {
            let f = current_file(s, u64::from(fd.cast_unsigned()), &R::NO)?;
            // A hint, for regular files.
            match f.vnode() {
                Some(_) => Ok(0),
                None => Err(Errno::EBADF),
            }
        }
        F_GETLK | F_SETLK | F_SETLKW => record_lock(s, fd, cmd, arg),
        // Only shared memory has seals.
        abi::fcntl::F_KINFO => {
            crate::capsicum::check_capmode()?;
            kinfo(s, fd, arg)
        }
        F_ADD_SEALS | F_GET_SEALS => {
            let f = current_file(s, u64::from(fd.cast_unsigned()), &R::NO)?;
            let crate::fs::file::FileKind::Shm(m) = f.kind() else {
                return Err(Errno::EINVAL);
            };
            if cmd == F_GET_SEALS {
                return Ok(u64::from(m.seals(s)));
            }
            m.add_seals(s, int(arg).cast_unsigned())?;
            Ok(0)
        }
        c if c & ((1 << F_DUP3FD_SHIFT) - 1) == F_DUP3FD => {
            let flags = c >> F_DUP3FD_SHIFT;
            if flags & !(FD_CLOEXEC | FD_CLOFORK) != 0 {
                return Err(Errno::EINVAL);
            }
            let flags = FdFlags {
                cloexec: flags & FD_CLOEXEC != 0,
                clofork: flags & FD_CLOFORK != 0,
                resolve_beneath: false,
            };
            dup_fd(s, Dup::Fixed, flags, fd, int(arg))
        }
        // A command FreeBSD has (from `F_DUPFD` to `F_DUPFD_CLOFORK`).
        c if c <= F_DUPFD_CLOFORK => {
            crate::firstuse::report(crate::firstuse::Kind::Fcntl, format_args!("{c}"));
            Err(Errno::EINVAL)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// The byte range a `struct flock` names, `[start, end]` with `end`
/// inclusive (`u64::MAX`: to the end of the file, however far it grows).
fn lock_range(s: &Sleepable, f: &File, fl: &Flock) -> Result<(u64, u64), Errno> {
    let v = f.vnode().ok_or(Errno::EBADF)?;
    let base: i64 = match u32::try_from(fl.l_whence).map_err(|_| Errno::EINVAL)? {
        SEEK_SET => 0,
        SEEK_CUR => i64::try_from(*f.offset(s)).map_err(|_| Errno::EOVERFLOW)?,
        SEEK_END => i64::try_from(v.size(s)?).map_err(|_| Errno::EOVERFLOW)?,
        _ => return Err(Errno::EINVAL),
    };
    let start = base.checked_add(fl.l_start).ok_or(Errno::EOVERFLOW)?;
    let (start, end) = match fl.l_len {
        0 => (start, u64::MAX),
        len if len > 0 => {
            let end = start.checked_add(len - 1).ok_or(Errno::EOVERFLOW)?;
            (start, end.cast_unsigned())
        }
        // A negative length runs back from the start.
        len => {
            let first = start.checked_add(len).ok_or(Errno::EINVAL)?;
            (first, (start - 1).cast_unsigned())
        }
    };
    let start = u64::try_from(start).map_err(|_| Errno::EINVAL)?;
    Ok((start, end))
}

/// `fcntl`'s `F_GETLK`, `F_SETLK`, and `F_SETLKW`: the calling process's
/// POSIX locks (FreeBSD's `kern_fcntl` and `lf_advlock`).
fn record_lock(s: &Sleepable, fd: i32, cmd: u32, arg: u64) -> SysResult {
    use vfs::lockf::{Lock, LockType, Owner};
    let p = process()?;
    let f = file(s, &p, u64::from(fd.cast_unsigned()), &R::FLOCK)?;
    let v = f.vnode().ok_or(Errno::EBADF)?.clone();
    let ptr = UserPtr::<Flock>::new(arg);
    let mut fl = ptr.read().map_err(|_| Errno::EFAULT)?;
    let (start, end) = lock_range(s, &f, &fl)?;
    let owner = Owner::Process(p.pid());
    let ty = match fl.l_type {
        F_RDLCK => Some(LockType::Shared),
        F_WRLCK => Some(LockType::Exclusive),
        F_UNLCK => None,
        _ => return Err(Errno::EINVAL),
    };
    if cmd == F_GETLK {
        let Some(ty) = ty else {
            return Err(Errno::EINVAL);
        };
        match v.lockf().conflict(owner, ty, start, end) {
            None => fl.l_type = F_UNLCK,
            Some(c) => {
                fl.l_type = match c.ty {
                    LockType::Shared => F_RDLCK,
                    LockType::Exclusive => F_WRLCK,
                };
                fl.l_whence = 0;
                fl.l_start = c.start.cast_signed();
                fl.l_len = if c.end == u64::MAX {
                    0
                } else {
                    (c.end - c.start + 1).cast_signed()
                };
                fl.l_pid = match c.owner {
                    Owner::Process(pid) => pid,
                    Owner::File(_) => -1,
                };
                fl.l_sysid = 0;
            }
        }
        ptr.write(&fl).map_err(|_| Errno::EFAULT)?;
        return Ok(0);
    }
    match ty {
        None => v.lockf().unlock(owner, start, end),
        Some(ty) => {
            // A lock needs the matching access to the file.
            let need = match ty {
                LockType::Shared => FREAD,
                LockType::Exclusive => FWRITE,
            };
            if f.flags() & need == 0 {
                return Err(Errno::EBADF);
            }
            let req = Lock {
                owner,
                pid: p.pid(),
                ty,
                start,
                end,
            };
            v.lockf().lock(s, req, cmd == F_SETLKW)?;
        }
    }
    Ok(0)
}

/// `flock(fd, how)`: a lock on the whole file, owned by the open file.
pub(super) fn flock(s: &Sleepable, fd: u64, how: u64) -> SysResult {
    use vfs::lockf::{Lock, LockType};
    let how = u32::try_from(how).map_err(|_| Errno::EINVAL)?;
    let f = current_file(s, fd, &R::FLOCK)?;
    let FileKind::Vnode(vf) = f.kind() else {
        return Err(Errno::EOPNOTSUPP);
    };
    let owner = vf.lock_owner();
    let lockf = vf.vnode.lockf();
    if how & LOCK_UN != 0 {
        lockf.unlock(owner, 0, u64::MAX);
        return Ok(0);
    }
    let ty = match how & (LOCK_SH | LOCK_EX) {
        LOCK_SH => LockType::Shared,
        LOCK_EX => LockType::Exclusive,
        _ => return Err(Errno::EINVAL),
    };
    // Changing a held lock's type is a release and a new request, as on
    // FreeBSD (not atomic).
    lockf.unlock(owner, 0, u64::MAX);
    let req = Lock {
        owner,
        pid: 0,
        ty,
        start: 0,
        end: u64::MAX,
    };
    lockf.lock(s, req, how & LOCK_NB == 0)?;
    Ok(0)
}

/// `ioctl(fd, com, data)`.
pub(super) fn ioctl(s: &Sleepable, fd: u64, com: u64, data: u64) -> SysResult {
    let p = process()?;
    // `u_long`, but only 32 bits are a request.
    let com = u32::try_from(com & 0xffff_ffff).unwrap_or(0);
    // The keymap requests pass a pointer to a structure too big for the
    // argument, which the driver copies itself.
    let indirect = abi::kbio::indirect(com);
    let size = match indirect {
        Some(i) => u32::try_from(i.len).unwrap_or(u32::MAX),
        None => param_len(com),
    };
    let known_dir = com & (IOC_VOID | IOC_IN | IOC_OUT) != 0;
    if indirect.is_none()
        && (!known_dir || size > IOCPARM_MAX || (com & IOC_VOID != 0 && size != 0 && size != 4))
    {
        return Err(Errno::ENOTTY);
    }
    let fdi = int(fd);
    // Only the requests the descriptor is limited to (`cap_ioctl_check`;
    // without `CAP_IOCTL` the list is empty).
    let f = {
        let t = p.files.lock(s);
        let slot = t.checked(fdi, &crate::capsicum::rights::IOCTL)?;
        if let Some(l) = slot.caps.as_ref().and_then(|c| c.ioctls.as_ref())
            && !l.iter().any(|&x| x & 0xffff_ffff == u64::from(com))
        {
            return Err(Errno::ENOTCAPABLE);
        }
        Arc::clone(&slot.file)
    };
    match com {
        FIOCLEX | FIONCLEX => {
            let mut t = p.files.lock(s);
            let mut f = t.slot(fdi)?.flags;
            f.cloexec = com == FIOCLEX;
            t.set_flags(fdi, f)?;
            return Ok(0);
        }
        _ => {}
    }
    if f.flags() & (FREAD | FWRITE) == 0 {
        return Err(Errno::EBADF);
    }
    let len = usize::try_from(size).unwrap_or(0);
    let mut arg = alloc::vec![0u8; len];
    let copy_in = indirect.map_or(com & IOC_IN != 0, |i| i.copy_in);
    let copy_out = indirect.map_or(com & IOC_OUT != 0, |i| i.copy_out);
    if copy_in && len > 0 {
        uaccess::copy_in(&mut arg, data).map_err(|_| Errno::EFAULT)?;
    } else if indirect.is_none() && com & IOC_VOID != 0 && len == 4 {
        // An `int` passed by value.
        arg.copy_from_slice(&int(data).to_ne_bytes());
    }
    let ret = match com {
        FIONBIO | FIOASYNC => {
            let on = arg.len() == 4 && i32::from_ne_bytes([arg[0], arg[1], arg[2], arg[3]]) != 0;
            let bit = if com == FIONBIO { O_NONBLOCK } else { O_ASYNC };
            f.set_flags(bit, if on { bit } else { 0 });
            0
        }
        _ => f.ioctl(s, com, &mut arg)?,
    };
    if copy_out && len > 0 {
        uaccess::copy_out(data, &arg).map_err(|_| Errno::EFAULT)?;
    }
    Ok(u64::try_from(ret).unwrap_or(0))
}

/// `posix_openpt(flags)` (`sys_posix_openpt`): a new pseudo-terminal's
/// master, read and write.
pub(super) fn posix_openpt(s: &Sleepable, flags: u64) -> SysResult {
    use abi::fcntl::{O_ACCMODE, O_CLOEXEC, O_NOCTTY, O_RDWR};
    let p = process()?;
    let flags = u32::try_from(flags).map_err(|_| Errno::EINVAL)?;
    // POSIX leaves other flags unspecified; FreeBSD refuses them.
    if flags & !(O_RDWR | O_NOCTTY | O_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let master = Arc::new(crate::tty::pts::alloc()?);
    let fflags = (flags & O_ACCMODE) + 1;
    let file = File::new(FileKind::Pts(master), fflags & (FREAD | FWRITE));
    install(
        s,
        &p,
        file,
        FdFlags {
            cloexec: flags & O_CLOEXEC != 0,
            clofork: false,
            resolve_beneath: false,
        },
    )
}

/// `pipe2(fildes, flags)`.
pub(super) fn pipe2(s: &Sleepable, fildes: u64, flags: u64) -> SysResult {
    let flags = u32::try_from(flags).map_err(|_| Errno::EINVAL)?;
    if flags & !(O_CLOEXEC | O_CLOFORK | O_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let (a, b) = pipe::pair();
    let status = FREAD | FWRITE | (flags & O_NONBLOCK);
    let fdf = FdFlags {
        cloexec: flags & O_CLOEXEC != 0,
        clofork: flags & O_CLOFORK != 0,
        resolve_beneath: false,
    };
    let limit = max_files(&p);
    let fds = {
        let mut t = p.files.lock(s);
        let fa = t.alloc(File::new(FileKind::Pipe(a), status), fdf, 0, limit)?;
        match t.alloc(File::new(FileKind::Pipe(b), status), fdf, 0, limit) {
            Ok(fb) => [fa, fb],
            Err(e) => {
                let closed = t.close(fa);
                drop(t);
                drop(closed);
                return Err(e);
            }
        }
    };
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&fds[0].to_ne_bytes());
    out[4..].copy_from_slice(&fds[1].to_ne_bytes());
    if uaccess::copy_out(fildes, &out).is_err() {
        let closed: Vec<Slot> = {
            let mut t = p.files.lock(s);
            fds.iter().filter_map(|&fd| t.close(fd).ok()).collect()
        };
        drop(closed);
        return Err(Errno::EFAULT);
    }
    Ok(0)
}

/// `fstat(fd, sb)`.
pub(super) fn fstat(s: &Sleepable, fd: u64, sb: u64) -> SysResult {
    let f = current_file(s, fd, &R::FSTAT)?;
    UserPtr::<Stat>::new(sb)
        .write(&f.stat(s)?)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `fstatfs(fd, buf)`.
pub(super) fn fstatfs(s: &Sleepable, fd: u64, buf: u64) -> SysResult {
    let f = current_file(s, fd, &R::FSTATFS)?;
    let v = f.vnode().ok_or(Errno::EINVAL)?;
    let st = super::path::statfs_of(s, v)?;
    UserPtr::<Statfs>::new(buf)
        .write(&st)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `getdirentries(fd, buf, nbytes, basep)`: FreeBSD's `dirent` records
/// from the directory's offset (a cookie the file system gave), which
/// advances; `*basep` gets where this read started.
pub(super) fn getdirentries(
    s: &Sleepable,
    fd: u64,
    buf: u64,
    nbytes: u64,
    basep: u64,
) -> SysResult {
    check_len(nbytes)?;
    let f = current_file(s, fd, &R::READ)?;
    // Not a file system object (`getvnode`), then not readable, then not
    // a directory (`kern_getdirentries`).
    let v = f.vnode().ok_or(Errno::EINVAL)?;
    if f.flags() & FREAD == 0 {
        return Err(Errno::EBADF);
    }
    if !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    let room = usize::try_from(nbytes).unwrap_or(usize::MAX).min(64 * 1024);
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve(room).map_err(|_| Errno::ENOMEM)?;
    let mut off = f.offset(s);
    let base = *off;
    let next = vfs::ops::getdirentries(s, v, base, room, &mut out)?;
    uaccess::copy_out(buf, &out).map_err(|_| Errno::EFAULT)?;
    *off = next;
    drop(off);
    if basep != 0 {
        UserPtr::<i64>::new(basep)
            .write(&base.cast_signed())
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(out.len() as u64)
}

/// `copy_file_range(infd, inoffp, outfd, outoffp, len, flags)`: FreeBSD's
/// checks, in its order, then the copy. Programs fall back to `read` and
/// `write` on `EINVAL`, `EBADF`, or `EISDIR` (`cat` does, for devices).
pub(super) fn copy_file_range(
    s: &Sleepable,
    infd: u64,
    inoffp: u64,
    outfd: u64,
    outoffp: u64,
    len: u64,
    flags: u64,
) -> SysResult {
    let flags = u32::try_from(flags).map_err(|_| Errno::EINVAL)?;
    if flags & !COPY_FILE_RANGE_CLONE != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let input = file(s, &p, infd, &R::READ)?;
    if input.flags() & FREAD == 0 {
        return Err(Errno::EBADF);
    }
    let invp = input.vnode().ok_or(Errno::EINVAL)?;
    let output = file(s, &p, outfd, &R::WRITE)?;
    if output.flags() & FWRITE == 0 {
        return Err(Errno::EBADF);
    }
    let outvp = output.vnode().ok_or(Errno::EINVAL)?;
    if inoffp == 0 && outoffp == 0 && Arc::ptr_eq(&input, &output) {
        return Err(Errno::EINVAL);
    }
    if output.flags() & O_APPEND != 0 {
        return Err(Errno::EBADF);
    }
    if len == 0 {
        return Ok(0);
    }
    let offset = |ptr: u64| -> Result<(), Errno> {
        if ptr != 0 {
            let v = UserPtr::<i64>::new(ptr).read().map_err(|_| Errno::EFAULT)?;
            if v < 0 {
                return Err(Errno::EINVAL);
            }
        }
        Ok(())
    };
    offset(inoffp)?;
    offset(outoffp)?;
    if invp.is_dir() || outvp.is_dir() {
        return Err(Errno::EISDIR);
    }
    if invp.kind() != vfs::Kind::Regular || outvp.kind() != vfs::Kind::Regular {
        return Err(Errno::EINVAL);
    }
    // The generic copy (FreeBSD's `vn_generic_copy_file_range`): through a
    // kernel buffer, a chunk at a time, from and to the offsets given or
    // the files' own, which then advance.
    let read_off = |ptr: u64, f: &File| -> Result<u64, Errno> {
        if ptr == 0 {
            Ok(*f.offset(s))
        } else {
            UserPtr::<i64>::new(ptr)
                .read()
                .map(i64::cast_unsigned)
                .map_err(|_| Errno::EFAULT)
        }
    };
    let mut inoff = read_off(inoffp, &input)?;
    let mut outoff = read_off(outoffp, &output)?;
    if Arc::ptr_eq(invp, outvp)
        && inoff < outoff.saturating_add(len)
        && outoff < inoff.saturating_add(len)
    {
        return Err(Errno::EINVAL);
    }
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    while done < len {
        let want = usize::try_from(len - done)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let mut r = vfs::ReadBuf::new(&mut buf[..want]);
        let n = invp.read_at(s, inoff, &mut r)?;
        if n == 0 {
            break;
        }
        let w = outvp.write_at(s, outoff, &mut vfs::WriteBuf::new(&buf[..n]))?;
        inoff += w as u64;
        outoff += w as u64;
        done += w as u64;
        if w < n {
            break;
        }
    }
    let write_off = |ptr: u64, f: &File, v: u64| -> Result<(), Errno> {
        if ptr == 0 {
            *f.offset(s) = v;
            Ok(())
        } else {
            UserPtr::<i64>::new(ptr)
                .write(&v.cast_signed())
                .map_err(|_| Errno::EFAULT)
        }
    };
    write_off(inoffp, &input, inoff)?;
    write_off(outoffp, &output, outoff)?;
    Ok(done)
}

/// `fcntl(F_KINFO)`: the descriptor's `struct kinfo_file`, the caller
/// having set its `kf_structsize`. Only its type, descriptor, flags, and
/// offset are filled yet; the per-type parts and the path come with
/// `kern.proc.filedesc` (P6.8).
fn kinfo(s: &Sleepable, fd: i32, arg: u64) -> SysResult {
    /// `KINFO_FILE_SIZE`.
    const SIZE: usize = 1392;
    let size = UserPtr::<i32>::new(arg).read().map_err(|_| Errno::EFAULT)?;
    if usize::try_from(size).ok() != Some(SIZE) {
        return Err(Errno::EINVAL);
    }
    let f = current_file(s, u64::from(fd.cast_unsigned()), &R::NO)?;
    let ty: i32 = match f.kind() {
        FileKind::Vnode(v) if v.device.is_some() => 12,
        FileKind::Vnode(_) => 1,
        FileKind::Socket(_) => 2,
        FileKind::Pipe(_) => 3,
        FileKind::Fifo(_) => 4,
        FileKind::Kqueue(_) => 5,
        FileKind::Shm(_) => 8,
        FileKind::Process(_) => 11,
        FileKind::Jail(_) => 16,
        FileKind::Eventfd(_) => 13,
        FileKind::Timerfd(_) => 14,
        FileKind::Pts(_) => 10,
    };
    let fl = f.flags();
    let mut flags = 0i32;
    for (have, kf) in [
        (abi::fcntl::FREAD, 0x01),
        (abi::fcntl::FWRITE, 0x02),
        (abi::fcntl::O_APPEND, 0x04),
        (abi::fcntl::O_NONBLOCK, 0x20),
    ] {
        if fl & have != 0 {
            flags |= kf;
        }
    }
    let mut kf = [0u8; SIZE];
    let off = i64::try_from(f.offset_hint()).unwrap_or(i64::MAX);
    kf[0..4].copy_from_slice(&i32::try_from(SIZE).unwrap_or(0).to_ne_bytes());
    kf[4..8].copy_from_slice(&ty.to_ne_bytes());
    kf[8..12].copy_from_slice(&fd.to_ne_bytes());
    kf[12..16].copy_from_slice(
        &i32::try_from(Arc::strong_count(&f) - 1)
            .unwrap_or(0)
            .to_ne_bytes(),
    );
    kf[16..20].copy_from_slice(&flags.to_ne_bytes());
    kf[24..32].copy_from_slice(&off.to_ne_bytes());
    uaccess::copy_out(arg, &kf).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}
