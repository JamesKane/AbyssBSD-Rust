// SPDX-License-Identifier: BSD-2-Clause
//! Capsicum (docs/MULTIUSER.md §10.3): capability mode on the credential.
//! A process in it may make only the system calls `syscalls.master` marks
//! `CAPENABLED`; the rest fail with `ECAPMODE`, and with `PROC_TRAPCAP_CTL`
//! a capability error also raises `SIGTRAP` (`TRAP_CAP`).
//!
//! Behavior reference: sys/kern/sys_capability.c, sys/kern/subr_syscall.c.

use abi::errno::Errno;

use crate::proc::Process;

/// `ECAPMODE` if the caller is in capability mode (what FreeBSD's
/// `IN_CAPABILITY_MODE` checks guard).
pub(crate) fn check_capmode() -> Result<(), Errno> {
    if in_capability_mode() {
        return Err(Errno::ECAPMODE);
    }
    Ok(())
}

/// Whether the calling process is in capability mode.
pub(crate) fn in_capability_mode() -> bool {
    Process::with_current(Process::in_capability_mode).unwrap_or(false)
}

/// A system call `number` ended with `e`: a capability error in
/// capability mode raises `SIGTRAP` if the process asked for it
/// (`syscallret`'s `TRAP_CAP`; `kern.trap_enotcap` is 0).
pub(crate) fn syscall_failed(number: u64, e: Errno) {
    if e != Errno::ECAPMODE && e != Errno::ENOTCAPABLE {
        return;
    }
    let Some(t) = crate::uthread::UserThread::current() else {
        return;
    };
    let p = t.process();
    let trap = p.flag2.load(core::sync::atomic::Ordering::Relaxed) & crate::proc::p2::TRAPCAP != 0;
    if !trap || !p.in_capability_mode() {
        return;
    }
    let mut info = crate::signal::user_info(abi::signal::SIGTRAP, 0, abi::signal::code::TRAP_CAP);
    info.si_errno = e.get();
    info.si_pid = 0;
    // `si_syscall` shares `si_trapno`'s place.
    info.si_trapno = i32::try_from(number).unwrap_or(-1);
    let _ = crate::signal::post_thread(&t, info);
}

/// `cap_enter()`.
pub(crate) fn cap_enter() -> Result<u64, Errno> {
    Process::current()
        .ok_or(Errno::EINVAL)?
        .enter_capability_mode();
    Ok(0)
}

/// `cap_getmode(modep)`: 1 in capability mode, else 0.
pub(crate) fn cap_getmode(modep: u64) -> Result<u64, Errno> {
    let mode = u32::from(in_capability_mode());
    crate::uaccess::UserPtr::<u32>::new(modep)
        .write(&mode)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// The rights each operation needs (FreeBSD's `cap_*_rights`).
pub(crate) mod rights {
    use abi::capsicum::*;

    const fn one(r: u64) -> CapRights {
        CapRights::of(&[r])
    }

    pub(crate) const NO: CapRights = CapRights::NONE;
    pub(crate) const READ: CapRights = one(CAP_READ);
    pub(crate) const WRITE: CapRights = one(CAP_WRITE);
    pub(crate) const PREAD: CapRights = one(CAP_PREAD);
    pub(crate) const PWRITE: CapRights = one(CAP_PWRITE);
    pub(crate) const SEEK: CapRights = one(CAP_SEEK);
    pub(crate) const FSTAT: CapRights = one(CAP_FSTAT);
    pub(crate) const FSTATFS: CapRights = one(CAP_FSTATFS);
    pub(crate) const FPATHCONF: CapRights = one(CAP_FPATHCONF);
    pub(crate) const FCHMOD: CapRights = one(CAP_FCHMOD);
    pub(crate) const FCHOWN: CapRights = one(CAP_FCHOWN);
    pub(crate) const FUTIMES: CapRights = one(CAP_FUTIMES);
    pub(crate) const FCHFLAGS: CapRights = one(CAP_FCHFLAGS);
    pub(crate) const FSYNC: CapRights = one(CAP_FSYNC);
    pub(crate) const FTRUNCATE: CapRights = one(CAP_FTRUNCATE);
    pub(crate) const FCHDIR: CapRights = one(CAP_FCHDIR);
    pub(crate) const FCHROOT: CapRights = one(CAP_FCHROOT);
    pub(crate) const FEXECVE: CapRights = one(CAP_FEXECVE);
    pub(crate) const FLOCK: CapRights = one(CAP_FLOCK);
    pub(crate) const IOCTL: CapRights = one(CAP_IOCTL);
    pub(crate) const FCNTL: CapRights = one(CAP_FCNTL);
    pub(crate) const EVENT: CapRights = one(CAP_EVENT);
    pub(crate) const KQUEUE_EVENT: CapRights = one(CAP_KQUEUE_EVENT);
    pub(crate) const KQUEUE_CHANGE: CapRights = one(CAP_KQUEUE_CHANGE);
    pub(crate) const PDGETPID: CapRights = one(CAP_PDGETPID);
    pub(crate) const PDKILL: CapRights = one(CAP_PDKILL);
    pub(crate) const PDWAIT: CapRights = one(CAP_PDWAIT);
    pub(crate) const ACCEPT: CapRights = one(CAP_ACCEPT);
    pub(crate) const BIND: CapRights = one(CAP_BIND);
    pub(crate) const CONNECT: CapRights = one(CAP_CONNECT);
    pub(crate) const LISTEN: CapRights = one(CAP_LISTEN);
    pub(crate) const GETPEERNAME: CapRights = one(CAP_GETPEERNAME);
    pub(crate) const GETSOCKNAME: CapRights = one(CAP_GETSOCKNAME);
    pub(crate) const GETSOCKOPT: CapRights = one(CAP_GETSOCKOPT);
    pub(crate) const SETSOCKOPT: CapRights = one(CAP_SETSOCKOPT);
    pub(crate) const SHUTDOWN: CapRights = one(CAP_SHUTDOWN);
    pub(crate) const RECV: CapRights = one(CAP_RECV);
    pub(crate) const SEND: CapRights = one(CAP_SEND);
    pub(crate) const SEND_CONNECT: CapRights = CapRights::of(&[CAP_SEND, CAP_CONNECT]);
    pub(crate) const MMAP: CapRights = one(CAP_MMAP);
    pub(crate) const MKDIRAT: CapRights = one(CAP_MKDIRAT);
    pub(crate) const MKFIFOAT: CapRights = one(CAP_MKFIFOAT);
    pub(crate) const MKNODAT: CapRights = one(CAP_MKNODAT);
    pub(crate) const SYMLINKAT: CapRights = one(CAP_SYMLINKAT);
    pub(crate) const UNLINKAT: CapRights = one(CAP_UNLINKAT);
    pub(crate) const RENAMEAT_SOURCE: CapRights = one(CAP_RENAMEAT_SOURCE);
    pub(crate) const RENAMEAT_TARGET: CapRights = one(CAP_RENAMEAT_TARGET);
    pub(crate) const LINKAT_SOURCE: CapRights = one(CAP_LINKAT_SOURCE);
    pub(crate) const LINKAT_TARGET: CapRights = one(CAP_LINKAT_TARGET);
    pub(crate) const FCHMODAT: CapRights = one(CAP_FCHMODAT);
    pub(crate) const FCHOWNAT: CapRights = one(CAP_FCHOWNAT);
    pub(crate) const FUTIMESAT: CapRights = one(CAP_FUTIMESAT);
    pub(crate) const CHFLAGSAT: CapRights = one(CAP_CHFLAGSAT);
    pub(crate) const BINDAT: CapRights = one(CAP_BINDAT);
    pub(crate) const CONNECTAT: CapRights = one(CAP_CONNECTAT);
    pub(crate) const EXTATTR_GET: CapRights = one(CAP_EXTATTR_GET);
    pub(crate) const EXTATTR_SET: CapRights = one(CAP_EXTATTR_SET);
    pub(crate) const EXTATTR_DELETE: CapRights = one(CAP_EXTATTR_DELETE);
    pub(crate) const EXTATTR_LIST: CapRights = one(CAP_EXTATTR_LIST);
    pub(crate) const ACL_GET: CapRights = one(CAP_ACL_GET);
    pub(crate) const ACL_SET: CapRights = one(CAP_ACL_SET);
    pub(crate) const ACL_DELETE: CapRights = one(CAP_ACL_DELETE);
    pub(crate) const ACL_CHECK: CapRights = one(CAP_ACL_CHECK);

    /// What `openat` needs of its directory for `flags`
    /// (`flags_to_rights`, with `CAP_LOOKUP`).
    pub(crate) const fn of_open(flags: u32) -> CapRights {
        use abi::fcntl::*;
        let mut r = one(CAP_LOOKUP);
        if flags & O_EXEC != 0 {
            r = r.with(CAP_FEXECVE);
            if flags & O_PATH != 0 {
                return r;
            }
        } else {
            match flags & O_ACCMODE {
                O_RDONLY => r = r.with(CAP_READ),
                O_RDWR | O_WRONLY => {
                    if flags & O_ACCMODE == O_RDWR {
                        r = r.with(CAP_READ);
                    }
                    r = r.with(CAP_WRITE);
                    if flags & (O_APPEND | O_TRUNC) == 0 {
                        r = r.with(CAP_SEEK);
                    }
                }
                _ => {}
            }
        }
        if flags & O_CREAT != 0 {
            r = r.with(CAP_CREATE);
        }
        if flags & O_TRUNC != 0 {
            r = r.with(CAP_FTRUNCATE);
        }
        if flags & O_FSYNC != 0 {
            r = r.with(CAP_FSYNC);
        }
        if flags & (O_EXLOCK | O_SHLOCK) != 0 {
            r = r.with(CAP_FLOCK);
        }
        r
    }
}

use abi::capsicum::{
    CAP_FCNTL_ALL, CAP_IOCTLS_ALL, CAP_RIGHTS_VERSION, CapRights, IOCTLS_MAX_COUNT,
};
use sched::Sleepable;

use crate::uaccess::{self, Plain, UserPtr};

// SAFETY: two words, no padding (`abi-check`).
unsafe impl Plain for CapRights {}

/// The calling process.
fn me() -> Result<alloc::sync::Arc<Process>, Errno> {
    Process::current().ok_or(Errno::EINVAL)
}

/// `cap_rights_limit(fd, rightsp)`: only version 0, valid rights, and no
/// more than the descriptor holds (`ENOTCAPABLE`).
pub(crate) fn cap_rights_limit(s: &Sleepable, fd: u64, rightsp: u64) -> Result<u64, Errno> {
    let rights = UserPtr::<CapRights>::new(rightsp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if rights.version() != CAP_RIGHTS_VERSION || !rights.is_valid() {
        return Err(Errno::EINVAL);
    }
    let fd = i32::try_from(fd as i64).map_err(|_| Errno::EBADF)?;
    me()?.files.lock(s).limit_rights(fd, rights)?;
    Ok(0)
}

/// `__cap_rights_get(version, fd, rightsp)`.
pub(crate) fn cap_rights_get(
    s: &Sleepable,
    version: u64,
    fd: u64,
    rightsp: u64,
) -> Result<u64, Errno> {
    if version != u64::from(CAP_RIGHTS_VERSION) {
        return Err(Errno::EINVAL);
    }
    let fd = i32::try_from(fd as i64).map_err(|_| Errno::EBADF)?;
    let rights = me()?.files.lock(s).slot(fd)?.caps().rights;
    UserPtr::<CapRights>::new(rightsp)
        .write(&rights)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `cap_ioctls_limit(fd, cmds, ncmds)`: at most `IOCTLS_MAX_COUNT`, each
/// allowed now.
pub(crate) fn cap_ioctls_limit(
    s: &Sleepable,
    fd: u64,
    cmds: u64,
    ncmds: u64,
) -> Result<u64, Errno> {
    let n = usize::try_from(ncmds).map_err(|_| Errno::EINVAL)?;
    if n > IOCTLS_MAX_COUNT {
        return Err(Errno::EINVAL);
    }
    let mut raw = alloc::vec![0u8; n * 8];
    if n > 0 {
        uaccess::copy_in(&mut raw, cmds).map_err(|_| Errno::EFAULT)?;
    }
    let list = raw
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_ne_bytes(*c))
        .collect();
    let fd = i32::try_from(fd as i64).map_err(|_| Errno::EBADF)?;
    me()?.files.lock(s).limit_ioctls(fd, list)?;
    Ok(0)
}

/// `cap_ioctls_get(fd, cmds, maxcmds)`: how many are allowed (and up to
/// `maxcmds` of them), or `CAP_IOCTLS_ALL` with every one allowed.
pub(crate) fn cap_ioctls_get(
    s: &Sleepable,
    fd: u64,
    cmds: u64,
    maxcmds: u64,
) -> Result<u64, Errno> {
    let fd = i32::try_from(fd as i64).map_err(|_| Errno::EBADF)?;
    let list = me()?.files.lock(s).slot(fd)?.caps().ioctls;
    let Some(list) = list else {
        return Ok(CAP_IOCTLS_ALL.cast_unsigned());
    };
    if cmds != 0 {
        let n = list
            .len()
            .min(usize::try_from(maxcmds).unwrap_or(usize::MAX));
        let bytes: alloc::vec::Vec<u8> = list[..n].iter().flat_map(|c| c.to_ne_bytes()).collect();
        uaccess::copy_out(cmds, &bytes).map_err(|_| Errno::EFAULT)?;
    }
    Ok(list.len() as u64)
}

/// `cap_fcntls_limit(fd, fcntlrights)`.
pub(crate) fn cap_fcntls_limit(s: &Sleepable, fd: u64, mask: u64) -> Result<u64, Errno> {
    let mask = u32::try_from(mask & 0xffff_ffff).unwrap_or(u32::MAX);
    if mask & !CAP_FCNTL_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    let fd = i32::try_from(fd as i64).map_err(|_| Errno::EBADF)?;
    me()?.files.lock(s).limit_fcntls(fd, mask)?;
    Ok(0)
}

/// `cap_fcntls_get(fd, fcntlrightsp)`.
pub(crate) fn cap_fcntls_get(s: &Sleepable, fd: u64, maskp: u64) -> Result<u64, Errno> {
    let fd = i32::try_from(fd as i64).map_err(|_| Errno::EBADF)?;
    let mask = me()?.files.lock(s).slot(fd)?.caps().fcntls;
    UserPtr::<u32>::new(maskp)
        .write(&mask)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}
