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
use crate::cred::{Ids, Priv};
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

/// `rfork(flags)`, in the forms base uses: `RFSPAWN` (`posix_spawn`),
/// `RFPROC | RFFDG` (`fork`), and `RFPROC | RFFDG | RFMEM | RFPPWAIT`
/// (`vfork`), and `RFPROC` (the descriptor table shared); the rest are
/// `EINVAL` for now.
const RFFDG: u32 = 1 << 2;
const RFPROC: u32 = 1 << 4;
const RFMEM: u32 = 1 << 5;
const RFSTOPPED: u32 = 1 << 17;
const RFHIGHPID: u32 = 1 << 18;
const RFPROCDESC: u32 = 1 << 28;
const RFPPWAIT: u32 = 1 << 31;
const RFSPAWN: u32 = 1 << 31;

pub(super) fn rfork(s: &Sleepable, frame: &mut TrapFrame, flags: u64) -> SysResult {
    let flags = int(flags).cast_unsigned();
    if flags & (RFSTOPPED | RFHIGHPID | RFPROCDESC) != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let pid = if flags == RFSPAWN {
        p.spawn_fork(s, frame)?
    } else if flags == RFPROC | RFFDG {
        p.fork(s, frame, false)?
    } else if flags == RFPROC | RFFDG | RFMEM | RFPPWAIT {
        p.fork(s, frame, true)?
    } else if flags == RFPROC {
        p.fork_shared_files(s, frame)?
    } else {
        // Shared descriptor tables, shared memory without waiting, and
        // changes to the caller alone are not here yet.
        return Err(Errno::EINVAL);
    };
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

/// `pdrfork(fdp, pdflags, rfflags)`: `rfork` with `RFPROCDESC`, in the
/// forms `rfork` takes, the child's descriptor written to `fdp`.
pub(super) fn pdrfork(
    s: &Sleepable,
    frame: &mut TrapFrame,
    fdp: u64,
    pdflags: u64,
    rfflags: u64,
) -> SysResult {
    let rf = int(rfflags).cast_unsigned();
    if rf & (RFSTOPPED | RFHIGHPID) != 0 {
        return Err(Errno::EINVAL);
    }
    // (vfork, drop caught signals, share the descriptor table)
    let how = if rf & RFSPAWN != 0 {
        // RFSPAWN must not appear with others.
        if rf != RFSPAWN {
            return Err(Errno::EINVAL);
        }
        (true, true, false)
    } else if rf == RFPROC | RFPROCDESC | RFFDG {
        (false, false, false)
    } else if rf == RFPROC | RFPROCDESC | RFFDG | RFMEM | RFPPWAIT {
        (true, false, false)
    } else if rf == RFPROC | RFPROCDESC {
        (false, false, true)
    } else {
        // Without both RFPROC and RFPROCDESC, or a form `rfork` does not
        // take here either.
        return Err(Errno::EINVAL);
    };
    let pdflags = int(pdflags).cast_unsigned();
    if pdflags & !PD_ALLOWED_AT_FORK != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let limit = super::io::max_files(&p);
    let (pid, fd) = p.pdrfork(
        s,
        frame,
        how,
        (pdflags & PD_DAEMON != 0, pdflags & PD_CLOEXEC != 0, limit),
    )?;
    #[cfg(target_arch = "x86_64")]
    {
        frame.rdx = 0;
    }
    // As `pdfork`: the child runs already; a bad pointer only fails the
    // call.
    UserPtr::<i32>::new(fdp)
        .write(&fd)
        .map_err(|_| Errno::EFAULT)?;
    Ok(u64::from(pid.unsigned_abs()))
}

/// The process behind descriptor `fd` (`EBADF` if it is not a process
/// descriptor).
fn described(
    s: &Sleepable,
    fd: u64,
    need: &abi::capsicum::CapRights,
) -> Result<alloc::sync::Arc<crate::proc::Process>, Errno> {
    let f = super::io::current_file(s, fd, need)?;
    match f.kind() {
        crate::fs::file::FileKind::Process(pd) => Ok(alloc::sync::Arc::clone(pd.process())),
        _ => Err(Errno::EBADF),
    }
}

/// `pdgetpid(fd, pidp)`.
pub(super) fn pdgetpid(s: &Sleepable, fd: u64, pidp: u64) -> SysResult {
    let pid = described(s, fd, &crate::capsicum::rights::PDGETPID)?.pid();
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
    let target = described(s, fd, &crate::capsicum::rights::PDKILL)?;
    // Reaped: the descriptor names no process now (`procdesc_reap`).
    if target.is_zombie() && target.parent().is_none() {
        return Err(Errno::ESRCH);
    }
    let me = process()?;
    me.can_signal(&target, sig)?;
    let _ = crate::signal::post(
        &target,
        crate::signal::user_info(sig, me.pid(), abi::signal::code::SI_USER),
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
    // At least one argument (FreeBSD's `kern_execve`).
    if argv.is_empty() {
        return Err(Errno::EINVAL);
    }
    let argv: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    let envv: Vec<&[u8]> = envv.iter().map(Vec::as_slice).collect();
    process()?.exec(s, frame, crate::proc::Program::Path(&path), &argv, &envv)?;
    Ok(None)
}

/// `fexecve(fd, argv, envv)`: `execve` of the file open as `fd`, which
/// needs `CAP_FEXECVE` and must be open for reading or execution and not
/// for writing, or be an `O_PATH` handle (`fgetvp_exec`).
pub(super) fn fexecve(
    s: &Sleepable,
    frame: &mut TrapFrame,
    fd: u64,
    argv: u64,
    envv: u64,
) -> Result<Option<u64>, Errno> {
    use abi::fcntl::{FREAD, FWRITE, O_EXEC};
    let mut budget = ARG_MAX;
    let argv = strings(argv, &mut budget)?;
    let envv = strings(envv, &mut budget)?;
    if argv.is_empty() {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let f = super::io::file(s, &p, fd, &crate::capsicum::rights::FEXECVE)?;
    let fl = f.flags();
    let path_only = fl & (FREAD | FWRITE | O_EXEC) == 0;
    if !path_only && (fl & (FREAD | O_EXEC) == 0 || fl & FWRITE != 0) {
        return Err(Errno::EBADF);
    }
    let v = alloc::sync::Arc::clone(f.vnode().ok_or(Errno::EINVAL)?);
    drop(f);
    let argv: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    let envv: Vec<&[u8]> = envv.iter().map(Vec::as_slice).collect();
    p.exec(
        s,
        frame,
        crate::proc::Program::File(v, int(fd)),
        &argv,
        &envv,
    )?;
    Ok(None)
}

/// `wait4(pid, status, options, rusage)`.
pub(super) fn wait4(s: &Sleepable, pid: u64, status: u64, options: u64, rusage: u64) -> SysResult {
    let options = u32::try_from(options).map_err(|_| Errno::EINVAL)?;
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOWAIT | WEXITED | WTRAPPED) != 0 {
        return Err(Errno::EINVAL);
    }
    // `kern_wait`: exits and a debugger's stops always.
    let Some(w) = process()?.wait_child(s, int(pid), options | WEXITED | WTRAPPED)? else {
        return Ok(0);
    };
    let (child, st) = (w.pid, w.status);
    if status != 0 {
        UserPtr::<i32>::new(status)
            .write(&st)
            .map_err(|_| Errno::EFAULT)?;
    }
    if rusage != 0 {
        let ru = w
            .usage
            .map_or_else(Rusage::default, |(own, _)| own.rusage());
        UserPtr::<Rusage>::new(rusage)
            .write(&ru)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(u64::from(child.unsigned_abs()))
}

/// `wait6(idtype, id, status, options, wrusage, info)`: `wait4` named by
/// `idtype_t` (`P_PID`, `P_PGID`, `P_ALL`), with the child's `siginfo`.
pub(super) fn wait6(
    s: &Sleepable,
    idtype: u64,
    id: u64,
    status: u64,
    options: u64,
    wrusage: u64,
    info: u64,
) -> SysResult {
    /// `idtype_t`.
    const P_PID: i32 = 0;
    const P_PGID: i32 = 2;
    const P_ALL: i32 = 7;
    let options = u32::try_from(options).map_err(|_| Errno::EINVAL)?;
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOWAIT | WEXITED | WTRAPPED) != 0 {
        return Err(Errno::EINVAL);
    }
    let id = i32::try_from(id.cast_signed()).map_err(|_| Errno::EINVAL)?;
    let pid = match int(idtype) {
        P_PID => id,
        P_PGID if id == 0 => 0,
        P_PGID => -id,
        P_ALL => -1,
        _ => return Err(Errno::EINVAL),
    };
    let Some(w) = process()?.wait_child(s, pid, options)? else {
        if info != 0 {
            UserPtr::<abi::signal::Siginfo>::new(info)
                .write(&abi::signal::Siginfo::default())
                .map_err(|_| Errno::EFAULT)?;
        }
        return Ok(0);
    };
    report(w, status, wrusage, info)?;
    Ok(u64::from(w.pid.unsigned_abs()))
}

/// `pdwait(fd, status, options, wrusage, info)`: `wait6` for the process
/// that descriptor `fd` (`CAP_PDWAIT`) names, by whoever holds it
/// (`kern_pdwait`): `ESRCH` once it is reaped (or, without `WEXITED`,
/// once it exits), `EWOULDBLOCK` with `WNOHANG` and nothing to report.
pub(super) fn pdwait(
    s: &Sleepable,
    fd: u64,
    status: u64,
    options: u64,
    wrusage: u64,
    info: u64,
) -> SysResult {
    let options = u32::try_from(options).map_err(|_| Errno::EINVAL)?;
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOWAIT | WEXITED | WTRAPPED) != 0
        || options & (WEXITED | WUNTRACED | WCONTINUED | WTRAPPED) == 0
    {
        return Err(Errno::EINVAL);
    }
    let me = process()?;
    let f = super::io::file(s, &me, fd, &crate::capsicum::rights::PDWAIT)?;
    let crate::fs::file::FileKind::Process(pd) = f.kind() else {
        return Err(Errno::EINVAL);
    };
    let target = alloc::sync::Arc::clone(pd.process());
    drop(f);
    let parent = target.parent().ok_or(Errno::ESRCH)?;
    me.cred().prison_check(&target.cred())?;
    if options & WEXITED == 0 && target.is_zombie() {
        return Err(Errno::ESRCH);
    }
    let w = match parent.wait_child(s, target.pid(), options) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(Errno::EAGAIN),
        Err(Errno::ECHILD) => return Err(Errno::ESRCH),
        Err(e) => return Err(e),
    };
    report(w, status, wrusage, info)?;
    Ok(0)
}

/// Copies out what a wait found: the status, zero resource use, and the
/// child's `siginfo`.
fn report(w: crate::proc::Waited, status: u64, wrusage: u64, info: u64) -> Result<(), Errno> {
    use abi::signal::code::{
        CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED, CLD_TRAPPED,
    };
    let (child, st) = (w.pid, w.status);
    if status != 0 {
        UserPtr::<i32>::new(status)
            .write(&st)
            .map_err(|_| Errno::EFAULT)?;
    }
    if wrusage != 0 {
        // `struct __wrusage`: the child's and its children's.
        let ru = w.usage.map_or([Rusage::default(); 2], |(own, children)| {
            [own.rusage(), children.rusage()]
        });
        UserPtr::<[Rusage; 2]>::new(wrusage)
            .write(&ru)
            .map_err(|_| Errno::EFAULT)?;
    }
    if info != 0 {
        let (code, value) = if st & 0x7f == 0 {
            (CLD_EXITED, (st >> 8) & 0xff)
        } else if st & 0xff == 0o177 && w.trapped {
            (CLD_TRAPPED, (st >> 8) & 0xff)
        } else if st & 0xff == 0o177 {
            (CLD_STOPPED, (st >> 8) & 0xff)
        } else if st == abi::signal::SIGCONT.cast_signed() {
            (CLD_CONTINUED, st)
        } else if st & 0x80 != 0 {
            (CLD_DUMPED, st & 0x7f)
        } else {
            (CLD_KILLED, st & 0x7f)
        };
        let si = abi::signal::Siginfo {
            si_signo: abi::signal::SIGCHLD.cast_signed(),
            si_code: code,
            si_pid: child,
            si_status: value,
            ..abi::signal::Siginfo::default()
        };
        UserPtr::<abi::signal::Siginfo>::new(info)
            .write(&si)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
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

/// The processes `getpriority`'s and `setpriority`'s `which` and `who`
/// name: one process, a process group, or a user's (0: the caller's).
fn prio_targets(
    which: u64,
    who: u64,
) -> Result<Vec<alloc::sync::Arc<crate::proc::Process>>, Errno> {
    const PRIO_PROCESS: i32 = 0;
    const PRIO_PGRP: i32 = 1;
    const PRIO_USER: i32 = 2;
    let me = process()?;
    let who = int(who);
    // In capability mode, only the caller itself.
    if crate::capsicum::in_capability_mode()
        && (int(which) != PRIO_PROCESS || (who != 0 && who != me.pid()))
    {
        return Err(Errno::ECAPMODE);
    }
    let mut targets: Vec<_> = match int(which) {
        PRIO_PROCESS => {
            let pid = if who == 0 { me.pid() } else { who };
            crate::proc::find(pid).into_iter().collect()
        }
        PRIO_PGRP => {
            let pgid = if who == 0 { me.pgid() } else { who };
            crate::proc::all()
                .into_iter()
                .filter(|p| p.pgid() == pgid)
                .collect()
        }
        PRIO_USER => {
            let uid = if who == 0 {
                me.cred().uid
            } else {
                who.cast_unsigned()
            };
            crate::proc::all()
                .into_iter()
                .filter(|p| p.cred().uid == uid)
                .collect()
        }
        _ => return Err(Errno::EINVAL),
    };
    // Only those the caller may see (`p_cansee`).
    targets.retain(|p| me.can_see(p).is_ok());
    Ok(targets)
}

/// `getpriority(which, who)`: the lowest nice value among them.
pub(super) fn getpriority(which: u64, who: u64) -> SysResult {
    let low = prio_targets(which, who)?
        .iter()
        .map(|p| p.nice.load(core::sync::atomic::Ordering::Relaxed))
        .min()
        .ok_or(Errno::ESRCH)?;
    Ok(i64::from(low).cast_unsigned())
}

/// `setpriority(which, who, prio)` (`donice`): the nice value, clamped
/// to -20 to 20, on the processes the caller may reschedule
/// (`p_cansched`); lowering it needs `PRIV_SCHED_SETPRIORITY`.
pub(super) fn setpriority(which: u64, who: u64, prio: u64) -> SysResult {
    let me = process()?;
    let n = int(prio).clamp(-20, 20);
    let targets = prio_targets(which, who)?;
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    let mut r = Ok(0);
    for p in targets {
        if let Err(e) = me.can_sched(&p) {
            r = Err(e);
            continue;
        }
        if n < p.nice.load(core::sync::atomic::Ordering::Relaxed)
            && me.priv_check(Priv::SchedSetpriority).is_err()
        {
            r = Err(Errno::EACCES);
            continue;
        }
        p.nice.store(n, core::sync::atomic::Ordering::Relaxed);
    }
    r
}

/// `getlogin(namebuf, namelen)`: the session's login name; `ERANGE` if it
/// and its NUL do not fit.
pub(super) fn getlogin(buf: u64, len: u64) -> SysResult {
    let len = usize::try_from(len & 0xffff_ffff)
        .unwrap_or(usize::MAX)
        .min(crate::proc::MAXLOGNAME);
    let mut name = process()?.login();
    if name.len() + 1 > len {
        return Err(Errno::ERANGE);
    }
    name.push(0);
    uaccess::copy_out(buf, &name).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// A name of at most `MAXLOGNAME` bytes with its NUL from user memory.
fn login_name(addr: u64) -> Result<Vec<u8>, uaccess::StrError> {
    let mut buf = [0u8; crate::proc::MAXLOGNAME];
    let n = uaccess::copy_in_str(addr, &mut buf)?;
    Ok(buf[..n].to_vec())
}

/// `setlogin(namebuf)`: names the session's login, with
/// `PRIV_PROC_SETLOGIN`; a name too long is `EINVAL`.
pub(super) fn setlogin(name: u64) -> SysResult {
    let p = process()?;
    p.priv_check(Priv::ProcSetlogin)?;
    let name = login_name(name).map_err(|e| match e {
        uaccess::StrError::Fault => Errno::EFAULT,
        uaccess::StrError::TooLong => Errno::EINVAL,
    })?;
    p.set_login(&name);
    Ok(0)
}

/// `getloginclass(namebuf, namelen)`: the credential's login class.
pub(super) fn getloginclass(buf: u64, len: u64) -> SysResult {
    let c = process()?.cred();
    let mut name = c.loginclass.name().to_vec();
    name.push(0);
    if usize::try_from(len).is_ok_and(|l| name.len() > l) {
        return Err(Errno::ERANGE);
    }
    uaccess::copy_out(buf, &name).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `setloginclass(namebuf)`: with `PRIV_PROC_SETLOGINCLASS`; an empty
/// name is `EINVAL`. Not a set-id change.
pub(super) fn setloginclass(name: u64) -> SysResult {
    let p = process()?;
    p.priv_check(Priv::ProcSetloginclass)?;
    let name = login_name(name).map_err(|e| match e {
        uaccess::StrError::Fault => Errno::EFAULT,
        uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    })?;
    if name.is_empty() {
        return Err(Errno::EINVAL);
    }
    p.replace_cred(|c| {
        let mut c = c.clone();
        c.loginclass = security::LoginClass::new(&name);
        Ok(c)
    })?;
    Ok(0)
}

// SAFETY: integers and addresses, no padding (`abi-check` holds the C
// layout).
unsafe impl Plain for abi::ucred::Setcred {}

/// `SETCREDF_*`.
mod setcredf {
    pub(super) const UID: u32 = 1 << 0;
    pub(super) const RUID: u32 = 1 << 1;
    pub(super) const SVUID: u32 = 1 << 2;
    pub(super) const GID: u32 = 1 << 3;
    pub(super) const RGID: u32 = 1 << 4;
    pub(super) const SVGID: u32 = 1 << 5;
    pub(super) const SUPP_GROUPS: u32 = 1 << 6;
    pub(super) const MAC_LABEL: u32 = 1 << 7;
    pub(super) const MASK: u32 = 0xff;
}

/// `setcred(flags, wcred, size)` (`user_setcred`, `kern_setcred`): sets
/// every id and the groups at once, with `PRIV_CRED_SETCRED`. A new real
/// uid already at its `RLIMIT_NPROC` is `EAGAIN` without
/// `PRIV_PROC_LIMIT`. There are no MAC labels to set.
pub(super) fn setcred(flags: u64, wcred: u64, size: u64) -> SysResult {
    use setcredf as f;
    let flags = u32::try_from(flags & 0xffff_ffff).unwrap_or(u32::MAX);
    if size != size_of::<abi::ucred::Setcred>() as u64 {
        return Err(Errno::EINVAL);
    }
    let w = UserPtr::<abi::ucred::Setcred>::new(wcred)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if flags & !f::MASK != 0 {
        return Err(Errno::EINVAL);
    }
    let groups = if flags & f::SUPP_GROUPS != 0 {
        let n = w.sc_supp_groups_nb as usize;
        if n > crate::cred::NGROUPS_MAX {
            return Err(Errno::EINVAL);
        }
        let mut raw = alloc::vec![0u8; n * 4];
        uaccess::copy_in(&mut raw, w.sc_supp_groups).map_err(|_| Errno::EFAULT)?;
        Some(crate::cred::groups_from(&raw))
    } else {
        None
    };
    if flags & f::MAC_LABEL != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let pick = |bit: u32, v: u32| (flags & bit != 0).then_some(v);
    let set = crate::cred::SetCred {
        uid: pick(f::UID, w.sc_uid),
        ruid: pick(f::RUID, w.sc_ruid),
        svuid: pick(f::SVUID, w.sc_svuid),
        gid: pick(f::GID, w.sc_gid),
        rgid: pick(f::RGID, w.sc_rgid),
        svgid: pick(f::SVGID, w.sc_svgid),
        groups,
    };
    let p = process()?;
    p.change_cred(|c| {
        let new = c.setcred(&set)?;
        if new.ruid != c.ruid {
            let max = p.limits.lock()[abi::resource::rlimit::NPROC as usize].rlim_cur;
            if max != abi::resource::RLIM_INFINITY
                && crate::proc::Process::count_of(new.ruid) as u64 >= max
                && c.priv_check(Priv::ProcLimit).is_err()
            {
                return Err(Errno::EAGAIN);
            }
        }
        Ok(new)
    })?;
    Ok(0)
}
