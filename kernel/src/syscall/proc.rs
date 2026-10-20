// SPDX-License-Identifier: BSD-2-Clause
//! Processes: `fork`, `vfork`, `pdfork` and its descriptors, `execve`,
//! `wait4`, process groups and sessions, and credentials.

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::procdesc::{PD_ALLOWED_AT_FORK, PD_CLOEXEC, PD_DAEMON};
use abi::resource::{Rusage, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WTRAPPED, WUNTRACED};
use sched::Sleepable;

use super::{SysResult, int, process};
use crate::arch::trap::TrapFrame;
use crate::cred::Ids;
use crate::uaccess::{self, Plain, UserPtr};

// SAFETY: integers and timevals only, no padding.
unsafe impl Plain for Rusage {}

/// `MAXPATHLEN`.
const MAXPATHLEN: usize = 1024;
/// FreeBSD's `ARG_MAX`.
const ARG_MAX: usize = 2 * 256 * 1024;

/// `fork()` and `vfork()`.
pub(super) fn fork(s: &Sleepable, frame: &mut TrapFrame, vfork: bool) -> SysResult {
    let pid = process()?.fork(s, frame, vfork)?;
    // The parent's second value is 0 (the child's is 1).
    #[cfg(target_arch = "x86_64")]
    {
        frame.rdx = 0;
    }
    Ok(u64::from(pid.unsigned_abs()))
}

/// `pdfork(fdp, flags)`: the child's pid, its descriptor written to `fdp`.
pub(super) fn pdfork(s: &Sleepable, frame: &mut TrapFrame, fdp: u64, flags: u64) -> SysResult {
    let flags = u32::try_from(flags).map_err(|_| Errno::EINVAL)?;
    if flags & !PD_ALLOWED_AT_FORK != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let limit = super::io::max_files(&p);
    let (pid, fd) = p.pdfork(
        s,
        frame,
        flags & PD_DAEMON != 0,
        flags & PD_CLOEXEC != 0,
        limit,
    )?;
    #[cfg(target_arch = "x86_64")]
    {
        frame.rdx = 0;
    }
    // The child runs already; a bad pointer only fails the call.
    UserPtr::<i32>::new(fdp)
        .write(&fd)
        .map_err(|_| Errno::EFAULT)?;
    Ok(u64::from(pid.unsigned_abs()))
}

/// The process behind descriptor `fd` (`EBADF` if it is not a process
/// descriptor).
fn described(s: &Sleepable, fd: u64) -> Result<alloc::sync::Arc<crate::proc::Process>, Errno> {
    let f = super::io::current_file(s, fd)?;
    match f.kind() {
        crate::fs::file::FileKind::Process(pd) => Ok(alloc::sync::Arc::clone(pd.process())),
        _ => Err(Errno::EBADF),
    }
}

/// `pdgetpid(fd, pidp)`.
pub(super) fn pdgetpid(s: &Sleepable, fd: u64, pidp: u64) -> SysResult {
    let pid = described(s, fd)?.pid();
    UserPtr::<i32>::new(pidp)
        .write(&pid)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `pdkill(fd, signum)`.
pub(super) fn pdkill(s: &Sleepable, fd: u64, signum: u64) -> SysResult {
    let sig = u32::try_from(signum).map_err(|_| Errno::EINVAL)?;
    if sig == 0 || sig > abi::signal::SIG_MAXSIG {
        return Err(Errno::EINVAL);
    }
    let target = described(s, fd)?;
    let from = process()?.pid();
    let _ = crate::signal::post(
        &target,
        crate::signal::user_info(sig, from, abi::signal::code::SI_USER),
    );
    Ok(0)
}

fn path(addr: u64) -> Result<Vec<u8>, Errno> {
    let mut buf = alloc::vec![0u8; MAXPATHLEN];
    let n = uaccess::copy_in_str(addr, &mut buf).map_err(|e| match e {
        uaccess::StrError::Fault => Errno::EFAULT,
        uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    })?;
    buf.truncate(n);
    Ok(buf)
}

/// The string at `addr`, at most `limit` bytes with its NUL. The buffer
/// starts small and doubles: arguments are short, and a buffer sized for
/// the largest possible one costs its zeroing every time.
fn string(addr: u64, limit: usize) -> Result<Vec<u8>, Errno> {
    let mut size = 256.min(limit.max(1));
    loop {
        let mut buf = alloc::vec![0u8; size];
        match uaccess::copy_in_str(addr, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(buf);
            }
            Err(uaccess::StrError::Fault) => return Err(Errno::EFAULT),
            Err(uaccess::StrError::TooLong) if size < limit => size = (size * 2).min(limit),
            Err(uaccess::StrError::TooLong) => return Err(Errno::E2BIG),
        }
    }
}

/// A NULL-terminated array of string pointers, its strings counted
/// against `budget`.
fn strings(addr: u64, budget: &mut usize) -> Result<Vec<Vec<u8>>, Errno> {
    let mut out = Vec::new();
    if addr == 0 {
        return Ok(out);
    }
    for i in 0.. {
        let p = UserPtr::<u64>::new(addr + i * 8)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        if p == 0 {
            break;
        }
        let buf = string(p, *budget)?;
        *budget = budget.checked_sub(buf.len() + 1).ok_or(Errno::E2BIG)?;
        out.push(buf);
    }
    Ok(out)
}

/// `execve(fname, argv, envv)`. On success the frame starts the new
/// program (`None`: nothing to return).
pub(super) fn execve(
    s: &Sleepable,
    frame: &mut TrapFrame,
    fname: u64,
    argv: u64,
    envv: u64,
) -> Result<Option<u64>, Errno> {
    let path = path(fname)?;
    let mut budget = ARG_MAX;
    let argv = strings(argv, &mut budget)?;
    let envv = strings(envv, &mut budget)?;
    let argv: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    let envv: Vec<&[u8]> = envv.iter().map(Vec::as_slice).collect();
    process()?.exec(s, frame, &path, &argv, &envv)?;
    Ok(None)
}

/// `wait4(pid, status, options, rusage)`.
pub(super) fn wait4(s: &Sleepable, pid: u64, status: u64, options: u64, rusage: u64) -> SysResult {
    let options = u32::try_from(options).map_err(|_| Errno::EINVAL)?;
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOWAIT | WEXITED | WTRAPPED) != 0 {
        return Err(Errno::EINVAL);
    }
    let Some((child, st)) = process()?.wait_child(s, int(pid), options)? else {
        return Ok(0);
    };
    if status != 0 {
        UserPtr::<i32>::new(status)
            .write(&st)
            .map_err(|_| Errno::EFAULT)?;
    }
    if rusage != 0 {
        // Resource accounting comes later; FreeBSD's is zero for a child
        // that used nothing measurable.
        UserPtr::<Rusage>::new(rusage)
            .write(&Rusage::default())
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(u64::from(child.unsigned_abs()))
}

pub(super) fn getppid() -> SysResult {
    Ok(u64::from(process()?.ppid().unsigned_abs()))
}

pub(super) fn getpgrp() -> SysResult {
    Ok(u64::from(process()?.pgid().unsigned_abs()))
}

/// `getpgid(pid)` and `getsid(pid)`: of this process or another.
pub(super) fn getpgid(pid: u64, session: bool) -> SysResult {
    let me = process()?;
    let p = match int(pid) {
        0 => me,
        pid => crate::proc::find(pid).ok_or(Errno::ESRCH)?,
    };
    let v = if session { p.sid() } else { p.pgid() };
    Ok(u64::from(v.unsigned_abs()))
}

pub(super) fn setsid() -> SysResult {
    Ok(u64::from(process()?.setsid()?.unsigned_abs()))
}

pub(super) fn setpgid(pid: u64, pgid: u64) -> SysResult {
    process()?.setpgid(int(pid), int(pgid))?;
    Ok(0)
}

/// Which id a `get*id` call returns.
#[derive(Clone, Copy, Debug)]
pub(super) enum Id {
    Uid,
    Euid,
    Gid,
    Egid,
}

/// `getuid()`, `geteuid()`, `getgid()`, `getegid()`.
pub(super) fn get_id(which: Id) -> SysResult {
    let c = process()?.cred();
    Ok(u64::from(match which {
        Id::Uid => c.ruid,
        Id::Euid => c.euid(),
        Id::Gid => c.rgid,
        Id::Egid => c.egid(),
    }))
}

/// `getgroups(gidsetlen, gidset)`: the supplementary groups.
pub(super) fn getgroups(len: u64, set: u64) -> SysResult {
    let c = process()?.cred();
    let groups = c.groups();
    let n = u64::try_from(groups.len()).map_err(|_| Errno::EINVAL)?;
    match int(len) {
        0 => Ok(n),
        l if usize::try_from(l).is_ok_and(|l| l >= groups.len()) => {
            let bytes: Vec<u8> = groups.iter().flat_map(|g| g.to_ne_bytes()).collect();
            uaccess::copy_out(set, &bytes).map_err(|_| Errno::EFAULT)?;
            Ok(n)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `setgroups(ngroups, gidset)`.
pub(super) fn setgroups(len: u64, set: u64) -> SysResult {
    let n = usize::try_from(int(len)).map_err(|_| Errno::EINVAL)?;
    if n > crate::cred::NGROUPS_MAX {
        return Err(Errno::EINVAL);
    }
    let mut raw = alloc::vec![0u8; n * 4];
    uaccess::copy_in(&mut raw, set).map_err(|_| Errno::EFAULT)?;
    let groups = crate::cred::groups_from(&raw);
    process()?.change_cred(|c| c.setgroups(&groups))?;
    Ok(0)
}

/// `getresuid(ruid, euid, suid)`, or `getresgid` with `gids`.
pub(super) fn getres(a: u64, b: u64, c: u64, gids: bool) -> SysResult {
    let cr = process()?.cred();
    let ids = if gids {
        [cr.rgid, cr.egid(), cr.svgid]
    } else {
        [cr.ruid, cr.euid(), cr.svuid]
    };
    let mut error = Ok(0);
    for (p, id) in [a, b, c].into_iter().zip(ids) {
        if p != 0 && UserPtr::<u32>::new(p).write(&id).is_err() {
            error = Err(Errno::EFAULT);
        }
    }
    error
}

/// Which id a `set*id` call sets.
#[derive(Clone, Copy, Debug)]
pub(super) enum SetId {
    Uid,
    Euid,
    Gid,
    Egid,
}

/// `setuid(uid)`, `seteuid`, `setgid`, `setegid`.
pub(super) fn set_id(which: SetId, id: u64) -> SysResult {
    let id = id_arg(id).ok_or(Errno::EINVAL)?;
    process()?.change_cred(|c| match which {
        SetId::Uid => c.setuid(id),
        SetId::Euid => c.seteuid(id),
        SetId::Gid => c.setgid(id),
        SetId::Egid => c.setegid(id),
    })?;
    Ok(0)
}

/// `setreuid(ruid, euid)`, or `setregid` with `gids`.
pub(super) fn setre(r: u64, e: u64, gids: bool) -> SysResult {
    let (r, e) = (id_arg(r), id_arg(e));
    process()?.change_cred(|c| {
        if gids {
            c.setregid(r, e)
        } else {
            c.setreuid(r, e)
        }
    })?;
    Ok(0)
}

/// `setresuid(ruid, euid, suid)`, or `setresgid` with `gids`.
pub(super) fn setres(r: u64, e: u64, sv: u64, gids: bool) -> SysResult {
    let ids = Ids {
        real: id_arg(r),
        effective: id_arg(e),
        saved: id_arg(sv),
    };
    process()?.change_cred(|c| {
        if gids {
            c.setresgid(ids)
        } else {
            c.setresuid(ids)
        }
    })?;
    Ok(0)
}

/// A `uid_t` or `gid_t` argument: `None` for -1, "leave it".
fn id_arg(a: u64) -> Option<u32> {
    let id = u32::try_from(a & 0xffff_ffff).ok()?;
    (id != u32::MAX).then_some(id)
}
