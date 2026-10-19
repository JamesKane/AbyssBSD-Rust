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

use abi::dirent::{self, NAME_OFFSET};
use abi::errno::Errno;
use abi::fcntl::*;
use abi::ioctl::{
    FIOASYNC, FIOCLEX, FIONBIO, FIONCLEX, IOC_IN, IOC_OUT, IOC_VOID, IOCPARM_MAX, param_len,
};
use abi::mount::Statfs;
use abi::stat::Stat;
use sched::Sleepable;

use super::{SysResult, int, process};
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

/// The file open on `fd`.
pub(super) fn file(s: &Sleepable, p: &Process, fd: u64) -> Result<Arc<File>, Errno> {
    p.files.lock(s).get(int(fd))
}

/// The file open on `fd` in the current process (borrowed, so no
/// reference to the process is taken).
pub(super) fn current_file(s: &Sleepable, fd: u64) -> Result<Arc<File>, Errno> {
    Process::with_current(|p| file(s, p, fd)).unwrap_or(Err(Errno::EINVAL))
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
    f: impl FnOnce(&File) -> Result<R, Errno>,
) -> Result<R, Errno> {
    Process::with_current(|p| {
        if p.sole_thread() {
            // SAFETY: only the process's threads reach its table, this is
            // the only one, and it does nothing else with the table until
            // `f` returns, which the borrow cannot outlive.
            let table = unsafe { p.files.get_unlocked() };
            f(&table.slot(int(fd))?.file)
        } else {
            let file = file(s, p, fd)?;
            f(&file)
        }
    })
    .unwrap_or(Err(Errno::EINVAL))
}

/// Installs `file` at the lowest free descriptor.
pub(super) fn install(s: &Sleepable, p: &Process, file: Arc<File>, flags: FdFlags) -> SysResult {
    let limit = max_files(p);
    let fd = p.files.lock(s).alloc(file, flags, 0, limit)?;
    Ok(u64::from(fd.unsigned_abs()))
}

/// A transfer's count: `EPIPE` also sends `SIGPIPE` (FreeBSD's
/// `dofilewrite`; sockets will decide for themselves).
fn wrote(r: Result<usize, Errno>) -> SysResult {
    match r {
        Ok(n) => Ok(n as u64),
        Err(Errno::EPIPE) => {
            let _ = crate::signal::post(
                &process()?,
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
    with_file(s, fd, |f| f.read(s, &mut uio)).map(|n| n as u64)
}

/// `readv(fd, iovp, iovcnt)`.
pub(super) fn readv(s: &Sleepable, fd: u64, iov: u64, iovcnt: u64) -> SysResult {
    let mut uio = UserIo::vector(iov, iovcnt)?;
    with_file(s, fd, |f| f.read(s, &mut uio)).map(|n| n as u64)
}

/// `pread(fd, buf, nbyte, offset)`.
pub(super) fn pread(s: &Sleepable, fd: u64, buf: u64, nbyte: u64, offset: u64) -> SysResult {
    check_len(nbyte)?;
    let mut uio = UserIo::one(buf, nbyte)?;
    with_file(s, fd, |f| f.pread(s, &mut uio, offset.cast_signed())).map(|n| n as u64)
}

/// `preadv(fd, iovp, iovcnt, offset)`.
pub(super) fn preadv(s: &Sleepable, fd: u64, iov: u64, iovcnt: u64, offset: u64) -> SysResult {
    let mut uio = UserIo::vector(iov, iovcnt)?;
    with_file(s, fd, |f| f.pread(s, &mut uio, offset.cast_signed())).map(|n| n as u64)
}

/// `write(fd, buf, nbyte)`.
pub(super) fn write(s: &Sleepable, fd: u64, buf: u64, nbyte: u64) -> SysResult {
    check_len(nbyte)?;
    let mut uio = UserIo::one(buf, nbyte)?;
    wrote(with_file(s, fd, |f| f.write(s, &mut uio)))
}

/// `writev(fd, iovp, iovcnt)`.
pub(super) fn writev(s: &Sleepable, fd: u64, iov: u64, iovcnt: u64) -> SysResult {
    let mut uio = UserIo::vector(iov, iovcnt)?;
    wrote(with_file(s, fd, |f| f.write(s, &mut uio)))
}

/// `pwrite(fd, buf, nbyte, offset)`.
pub(super) fn pwrite(s: &Sleepable, fd: u64, buf: u64, nbyte: u64, offset: u64) -> SysResult {
    check_len(nbyte)?;
    let mut uio = UserIo::one(buf, nbyte)?;
    wrote(with_file(s, fd, |f| {
        f.pwrite(s, &mut uio, offset.cast_signed())
    }))
}

/// `pwritev(fd, iovp, iovcnt, offset)`.
pub(super) fn pwritev(s: &Sleepable, fd: u64, iov: u64, iovcnt: u64, offset: u64) -> SysResult {
    let mut uio = UserIo::vector(iov, iovcnt)?;
    wrote(with_file(s, fd, |f| {
        f.pwrite(s, &mut uio, offset.cast_signed())
    }))
}

/// `lseek(fd, offset, whence)`.
pub(super) fn lseek(s: &Sleepable, fd: u64, offset: u64, whence: u64) -> SysResult {
    let f = current_file(s, fd)?;
    let whence = u32::try_from(whence).map_err(|_| Errno::EINVAL)?;
    f.seek(s, offset.cast_signed(), whence)
        .map(i64::cast_unsigned)
}

/// `close(fd)`.
pub(super) fn close(s: &Sleepable, fd: u64) -> SysResult {
    let closed = process()?.files.lock(s).close(int(fd))?;
    drop(closed);
    Ok(0)
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
    let closed = process()?.files.lock(s).close_range(
        to(lo),
        to(hi),
        flags & CLOSE_RANGE_CLOEXEC != 0,
        flags & CLOSE_RANGE_CLOFORK != 0,
    );
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
    let file = t.get(old)?;
    let fd = match mode {
        Dup::Lowest => t.alloc(file, flags, 0, limit)?,
        Dup::AtLeast => t.alloc(file, flags, usize::try_from(new).unwrap_or(0), limit)?,
        Dup::Fixed if old == new => {
            if flags.cloexec {
                let mut f = t.slot(old)?.flags;
                f.cloexec = true;
                t.set_flags(old, f)?;
            }
            new
        }
        Dup::Fixed => {
            let replaced = t.install(new, file, flags, limit)?;
            drop(t);
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
    (if f.cloexec { FD_CLOEXEC } else { 0 }) | if f.clofork { FD_CLOFORK } else { 0 }
}

/// `O_*` flags from a file's flags (`OFLAGS`).
fn oflags(fflags: u32) -> u32 {
    if fflags & O_EXEC != 0 {
        fflags
    } else {
        fflags - 1
    }
}

/// `fcntl(fd, cmd, arg)`.
pub(super) fn fcntl(s: &Sleepable, fd: u64, cmd: u64, arg: u64) -> SysResult {
    let fd = int(fd);
    let cmd = u32::try_from(cmd).map_err(|_| Errno::EINVAL)?;
    let cloexec = FdFlags {
        cloexec: true,
        clofork: false,
    };
    let clofork = FdFlags {
        cloexec: false,
        clofork: true,
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
            p.files.lock(s).set_flags(
                fd,
                FdFlags {
                    cloexec: bits & FD_CLOEXEC != 0,
                    clofork: bits & FD_CLOFORK != 0,
                },
            )?;
            Ok(0)
        }
        F_GETFL => {
            let f = current_file(s, u64::from(fd.cast_unsigned()))?;
            Ok(u64::from(oflags(f.flags())))
        }
        F_SETFL => {
            let f = current_file(s, u64::from(fd.cast_unsigned()))?;
            let bits = u32::try_from(arg & 0xffff_ffff).unwrap_or(0) & !O_ACCMODE;
            // Non-blocking and async are the file's own business on
            // FreeBSD (`FIONBIO`, `FIOASYNC`); the files here only read
            // the flag.
            f.set_flags(FCNTLFLAGS, bits);
            Ok(0)
        }
        F_ISUNIONSTACK => {
            let f = current_file(s, u64::from(fd.cast_unsigned()))?;
            // No union mounts.
            f.vnode().map(|_| 0).ok_or(Errno::EBADF)
        }
        F_READAHEAD | F_RDAHEAD => {
            let f = current_file(s, u64::from(fd.cast_unsigned()))?;
            // A hint, for regular files.
            match f.vnode() {
                Some(_) => Ok(0),
                None => Err(Errno::EBADF),
            }
        }
        F_GETLK | F_SETLK | F_SETLKW => {
            let _ = current_file(s, u64::from(fd.cast_unsigned()))?;
            // Advisory locks come with Phase 5.
            Err(Errno::EOPNOTSUPP)
        }
        c if c & ((1 << F_DUP3FD_SHIFT) - 1) == F_DUP3FD => {
            let flags = c >> F_DUP3FD_SHIFT;
            if flags & !(FD_CLOEXEC | FD_CLOFORK) != 0 {
                return Err(Errno::EINVAL);
            }
            let flags = FdFlags {
                cloexec: flags & FD_CLOEXEC != 0,
                clofork: flags & FD_CLOFORK != 0,
            };
            dup_fd(s, Dup::Fixed, flags, fd, int(arg))
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `ioctl(fd, com, data)`.
pub(super) fn ioctl(s: &Sleepable, fd: u64, com: u64, data: u64) -> SysResult {
    let p = process()?;
    // `u_long`, but only 32 bits are a request.
    let com = u32::try_from(com & 0xffff_ffff).unwrap_or(0);
    let size = param_len(com);
    let known_dir = com & (IOC_VOID | IOC_IN | IOC_OUT) != 0;
    if !known_dir || size > IOCPARM_MAX || (com & IOC_VOID != 0 && size != 0 && size != 4) {
        return Err(Errno::ENOTTY);
    }
    let fdi = int(fd);
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
    let f = file(s, &p, fd)?;
    if f.flags() & (FREAD | FWRITE) == 0 {
        return Err(Errno::EBADF);
    }
    let len = usize::try_from(size).unwrap_or(0);
    let mut arg = alloc::vec![0u8; len];
    if com & IOC_IN != 0 && len > 0 {
        uaccess::copy_in(&mut arg, data).map_err(|_| Errno::EFAULT)?;
    } else if com & IOC_VOID != 0 && len == 4 {
        // An `int` passed by value.
        arg.copy_from_slice(&int(data).to_ne_bytes());
    }
    match com {
        FIONBIO | FIOASYNC => {
            let on = arg.len() == 4 && i32::from_ne_bytes([arg[0], arg[1], arg[2], arg[3]]) != 0;
            let bit = if com == FIONBIO { O_NONBLOCK } else { O_ASYNC };
            f.set_flags(bit, if on { bit } else { 0 });
        }
        _ => f.ioctl(s, com, &mut arg)?,
    }
    if com & IOC_OUT != 0 && len > 0 {
        uaccess::copy_out(data, &arg).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
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
    let f = current_file(s, fd)?;
    UserPtr::<Stat>::new(sb)
        .write(&f.stat(s))
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `fstatfs(fd, buf)`.
pub(super) fn fstatfs(s: &Sleepable, fd: u64, buf: u64) -> SysResult {
    let f = current_file(s, fd)?;
    let v = f.vnode().ok_or(Errno::EINVAL)?;
    let st = super::path::statfs_of(v)?;
    UserPtr::<Statfs>::new(buf)
        .write(&st)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `getdirentries(fd, buf, nbytes, basep)`: the directory's entries from
/// the file's offset, which counts entries (`.` is 0, `..` is 1).
pub(super) fn getdirentries(
    s: &Sleepable,
    fd: u64,
    buf: u64,
    nbytes: u64,
    basep: u64,
) -> SysResult {
    check_len(nbytes)?;
    let f = current_file(s, fd)?;
    if f.flags() & FREAD == 0 {
        return Err(Errno::EBADF);
    }
    let v = f.vnode().ok_or(Errno::EINVAL)?;
    let dir = v.as_dir().ok_or(Errno::EINVAL)?;
    let room = usize::try_from(nbytes).unwrap_or(usize::MAX).min(64 * 1024);
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve(room).map_err(|_| Errno::ENOMEM)?;
    let mut off = f.offset(s);
    let base = *off;
    let mut next = base;
    // One record, if it fits.
    let put = |out: &mut Vec<u8>, ino: u64, dtype: u8, name: &[u8], after: u64| -> bool {
        let len = dirent::reclen(name.len());
        if out.len() + len > room {
            return false;
        }
        let at = out.len();
        out.resize(at + len, 0);
        let r = &mut out[at..];
        r[0..8].copy_from_slice(&ino.to_ne_bytes());
        r[8..16].copy_from_slice(&after.to_ne_bytes());
        r[16..18].copy_from_slice(&u16::try_from(len).unwrap_or(u16::MAX).to_ne_bytes());
        r[18] = dtype;
        r[20..22].copy_from_slice(&u16::try_from(name.len()).unwrap_or(0).to_ne_bytes());
        r[NAME_OFFSET..NAME_OFFSET + name.len()].copy_from_slice(name);
        true
    };
    let ns = crate::fs::namespace();
    if next == 0 && put(&mut out, v.ino(), dirent::DT_DIR, b".", 1) {
        next = 1;
    }
    if next == 1 {
        let parent = ns
            .resolve(
                &vfs::namei::Lookup {
                    start: v,
                    root: &crate::fs::root(),
                    follow: false,
                    beneath: false,
                },
                b"..",
            )
            .ok()
            .and_then(|f| match f {
                vfs::namei::Found::Vnode(p) => Some(p.ino()),
                vfs::namei::Found::Missing { .. } => None,
            })
            .unwrap_or(v.ino());
        if put(&mut out, parent, dirent::DT_DIR, b"..", 2) {
            next = 2;
        }
    }
    if next >= 2 {
        dir.entries(next - 2, &mut |e| {
            if put(&mut out, e.vnode.ino(), e.vnode.dtype(), e.name, next + 1) {
                next += 1;
                true
            } else {
                false
            }
        });
    }
    if out.is_empty() && next == base && more_entries(dir.as_ref(), base) {
        // Not even one record fits.
        return Err(Errno::EINVAL);
    }
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

/// Whether the directory has an entry at or after offset `at`.
fn more_entries(dir: &dyn vfs::Directory, at: u64) -> bool {
    if at < 2 {
        return true;
    }
    let mut any = false;
    dir.entries(at - 2, &mut |_| {
        any = true;
        false
    });
    any
}

/// `copy_file_range(infd, inoffp, outfd, outoffp, len, flags)`: FreeBSD's
/// checks, in its order. Programs fall back to `read` and `write` on
/// `EINVAL`, `EBADF`, or `EISDIR` (`cat` does), which is every case here:
/// no regular file can be open for writing until Phase 5.
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
    let input = file(s, &p, infd)?;
    if input.flags() & FREAD == 0 {
        return Err(Errno::EBADF);
    }
    let invp = input.vnode().ok_or(Errno::EINVAL)?;
    let output = file(s, &p, outfd)?;
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
    match (invp.kind(), outvp.kind()) {
        (vfs::VnodeKind::Regular(_), vfs::VnodeKind::Regular(_)) => Err(Errno::EROFS),
        _ => Err(Errno::EINVAL),
    }
}
