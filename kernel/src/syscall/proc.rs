// SPDX-License-Identifier: BSD-2-Clause
//! Processes: `fork`, `vfork`, `pdfork` and its descriptors, `execve`,
//! `wait4`, process groups and sessions, and credentials (everything runs
//! as root until P6).

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::procdesc::{PD_ALLOWED_AT_FORK, PD_CLOEXEC, PD_DAEMON};
use abi::resource::{Rusage, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WTRAPPED, WUNTRACED};
use sched::Sleepable;

use super::{SysResult, int, process};
use crate::arch::trap::TrapFrame;
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
        let mut buf = alloc::vec![0u8; (*budget).min(256 * 1024)];
        let n = uaccess::copy_in_str(p, &mut buf).map_err(|e| match e {
            uaccess::StrError::Fault => Errno::EFAULT,
            uaccess::StrError::TooLong => Errno::E2BIG,
        })?;
        *budget = budget.checked_sub(n + 1).ok_or(Errno::E2BIG)?;
        buf.truncate(n);
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

/// `getuid` and the rest: root.
pub(super) fn root() -> SysResult {
    Ok(0)
}

/// `getgroups(gidsetlen, gidset)`: group 0 (wheel).
pub(super) fn getgroups(len: u64, set: u64) -> SysResult {
    match int(len) {
        0 => Ok(1),
        n if n < 0 => Err(Errno::EINVAL),
        _ => {
            UserPtr::<u32>::new(set)
                .write(&0)
                .map_err(|_| Errno::EFAULT)?;
            Ok(1)
        }
    }
}

/// `getresuid(ruid, euid, suid)` and `getresgid`: all 0.
pub(super) fn getres(a: u64, b: u64, c: u64) -> SysResult {
    for p in [a, b, c] {
        if p != 0 {
            UserPtr::<u32>::new(p)
                .write(&0)
                .map_err(|_| Errno::EFAULT)?;
        }
    }
    Ok(0)
}

/// `setuid` and friends: root may become root.
pub(super) fn set_id(id: u64) -> SysResult {
    if int(id) == 0 {
        Ok(0)
    } else {
        Err(Errno::EPERM)
    }
}
