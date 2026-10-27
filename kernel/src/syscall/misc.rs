// SPDX-License-Identifier: BSD-2-Clause
//! Process identity and odds and ends: `getpid`, `issetugid`, `getrandom`,
//! `exterrctl`, `cpuset_getaffinity`, `cpuset_setaffinity`, `sysarch`.

use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::thr::{
    CPU_LEVEL_CPUSET, CPU_LEVEL_ROOT, CPU_LEVEL_WHICH, CPU_WHICH_PID, CPU_WHICH_TID,
    CPU_WHICH_TIDPID, CPUSET_BYTES, EXTERRCTL_DISABLE, EXTERRCTL_ENABLE,
};

use alloc::sync::Arc;

use super::{SysResult, process};
use crate::proc::Process;
use crate::uaccess;
use crate::uthread::UserThread;

/// `getpid()`.
pub(super) fn getpid() -> SysResult {
    Process::with_current(|p| u64::from(p.pid().unsigned_abs())).ok_or(Errno::EINVAL)
}

/// `utrace(addr, len)`: a record for ktrace (rtld's `LD_UTRACE`,
/// `malloc`'s); a process nobody traces drops it, as FreeBSD does (no
/// ktrace until P6.8).
pub(super) fn utrace() -> SysResult {
    Ok(0)
}

/// `issetugid()`: whether the process's credential changed since it last
/// ran `execve` (no set-id programs yet).
pub(super) fn issetugid() -> SysResult {
    Process::with_current(|p| u64::from(p.sugid())).ok_or(Errno::EINVAL)
}

/// `getrandom(buf, buflen, flags)`.
pub(super) fn getrandom(buf: u64, len: u64, flags: u64) -> SysResult {
    // GRND_NONBLOCK, GRND_RANDOM, GRND_INSECURE.
    if flags & !7 != 0 {
        return Err(Errno::EINVAL);
    }
    let len = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
    let mut chunk = [0u8; 256];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(chunk.len());
        crate::random::fill(&mut chunk[..n]).map_err(|_| Errno::EAGAIN)?;
        uaccess::copy_out(buf + done as u64, &chunk[..n]).map_err(|_| Errno::EFAULT)?;
        done += n;
    }
    Ok(len as u64)
}

/// `exterrctl(op, flags, ptr)`: the extended-error buffer libc registers.
/// It is recorded; the kernel writes no extended errors yet.
pub(super) fn exterrctl(op: u64, _flags: u64, ptr: u64) -> SysResult {
    let p = process()?;
    match u32::try_from(op).map_err(|_| Errno::EINVAL)? {
        EXTERRCTL_ENABLE => {
            if ptr == 0 {
                return Err(Errno::EINVAL);
            }
            p.exterr
                .compare_exchange(0, ptr, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| Errno::EBUSY)?;
        }
        EXTERRCTL_DISABLE => p.exterr.store(0, Ordering::Release),
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}

/// What a `cpuset` call names (FreeBSD's `cpuset_which`): a thread, or a
/// process (all its threads).
enum Target {
    Thread(Arc<UserThread>),
    Process(Arc<Process>),
}

impl Target {
    /// `which` and `id`: -1 is the caller's thread or process; a
    /// `CPU_WHICH_TIDPID` id above `PID_MAX` is a tid. A tid may be any
    /// process's.
    fn find(which: u64, id: u64) -> Result<Self, Errno> {
        let id = id.cast_signed();
        let tid = |id: i64| {
            i32::try_from(id)
                .ok()
                .and_then(crate::uthread::find)
                .ok_or(Errno::ESRCH)
        };
        let pid = |id: i64| {
            i32::try_from(id)
                .ok()
                .and_then(crate::proc::find)
                .ok_or(Errno::ESRCH)
        };
        Ok(match u32::try_from(which).map_err(|_| Errno::EINVAL)? {
            CPU_WHICH_PID if id == -1 => Self::Process(process()?),
            CPU_WHICH_PID => Self::Process(pid(id)?),
            CPU_WHICH_TID | CPU_WHICH_TIDPID if id == -1 => Self::Thread(super::thread()?),
            CPU_WHICH_TID => Self::Thread(tid(id)?),
            CPU_WHICH_TIDPID if id > i64::from(crate::proc::PID_MAX) => Self::Thread(tid(id)?),
            CPU_WHICH_TIDPID => Self::Process(pid(id)?),
            _ => return Err(Errno::EINVAL),
        })
    }

    fn cpus(&self) -> u64 {
        match self {
            Self::Thread(t) => t.cpus(),
            Self::Process(p) => p.cpus(),
        }
    }
}

/// `cpuset_getaffinity(level, which, id, setsize, mask)`: the CPUs a
/// thread may run on, or a process's threads (the union); at the root and
/// cpuset levels, every CPU (there is one set).
pub(super) fn cpuset_getaffinity(
    level: u64,
    which: u64,
    id: u64,
    setsize: u64,
    mask: u64,
) -> SysResult {
    let size = usize::try_from(setsize).map_err(|_| Errno::ERANGE)?;
    let level = u32::try_from(level).map_err(|_| Errno::EINVAL)?;
    if ![CPU_LEVEL_ROOT, CPU_LEVEL_CPUSET, CPU_LEVEL_WHICH].contains(&level) {
        return Err(Errno::EINVAL);
    }
    let target = Target::find(which, id)?;
    let allowed = if level == CPU_LEVEL_WHICH {
        target.cpus()
    } else {
        u64::MAX
    };
    let mut set = [0u8; CPUSET_BYTES];
    let mut highest = 0;
    for cpu in 0..sched::online_cpus().min(64) {
        if allowed & (1 << cpu) != 0 {
            set[cpu / 8] |= 1 << (cpu % 8);
            highest = cpu + 1;
        }
    }
    // Any size the mask fits in (FreeBSD's `user_cpuset_getaffinity`):
    // the kernel's set, then zeros.
    if size < highest.div_ceil(8) {
        return Err(Errno::ERANGE);
    }
    let n = size.min(CPUSET_BYTES);
    uaccess::copy_out(mask, &set[..n]).map_err(|_| Errno::EFAULT)?;
    let zeros = [0u8; CPUSET_BYTES];
    let mut at = n;
    while at < size {
        let k = (size - at).min(zeros.len());
        uaccess::copy_out(mask.wrapping_add(at as u64), &zeros[..k]).map_err(|_| Errno::EFAULT)?;
        at += k;
    }
    Ok(0)
}

/// `cpuset_setaffinity(level, which, id, setsize, mask)`: the CPUs a
/// thread may run on, or every thread of a process.
pub(super) fn cpuset_setaffinity(
    level: u64,
    which: u64,
    id: u64,
    setsize: u64,
    mask: u64,
) -> SysResult {
    let size = usize::try_from(setsize).map_err(|_| Errno::ERANGE)?;
    // The kernel's set from the front; anything past it must be clear
    // (FreeBSD's `user_cpuset_setaffinity`).
    let mut set = [0u8; CPUSET_BYTES];
    let n = size.min(CPUSET_BYTES);
    uaccess::copy_in(&mut set[..n], mask).map_err(|_| Errno::EFAULT)?;
    let mut at = n;
    while at < size {
        let mut chunk = [0u8; CPUSET_BYTES];
        let k = (size - at).min(chunk.len());
        uaccess::copy_in(&mut chunk[..k], mask.wrapping_add(at as u64))
            .map_err(|_| Errno::EFAULT)?;
        if chunk[..k].iter().any(|&b| b != 0) {
            return Err(Errno::EINVAL);
        }
        at += k;
    }
    if u32::try_from(level).map_err(|_| Errno::EINVAL)? != CPU_LEVEL_WHICH {
        return Err(Errno::EINVAL);
    }
    let target = Target::find(which, id)?;
    let online = sched::online_cpus();
    let mut bits = 0u64;
    for (i, &b) in set.iter().enumerate() {
        for bit in 0..8 {
            if b & (1 << bit) == 0 {
                continue;
            }
            let cpu = i * 8 + bit;
            // An absent CPU is simply never used.
            if cpu < online.min(64) {
                bits |= 1 << cpu;
            }
        }
    }
    // A thread with nowhere to run.
    if bits == 0 {
        return Err(Errno::EDEADLK);
    }
    match target {
        Target::Thread(t) => t.set_cpus(bits),
        Target::Process(p) => p.set_cpus(bits),
    }
    Ok(0)
}

// SAFETY: integers only, padding explicit (`abi-check` holds the C layout).
#[cfg(target_arch = "x86_64")]
unsafe impl uaccess::Plain for abi::sysarch::amd64::GetXfpustate {}

/// `sysarch(op, parms)`: amd64's thread pointer (the FS base, which
/// libc's start-up sets as the TLS base).
#[cfg(target_arch = "x86_64")]
pub(super) fn sysarch(op: u64, parms: u64) -> SysResult {
    use abi::sysarch::amd64;
    let t = super::thread()?;
    let base = uaccess::UserPtr::<u64>::new(parms);
    match op {
        amd64::GET_FSBASE => base.write(&hal::user::tls()).map_err(|_| Errno::EFAULT)?,
        amd64::SET_FSBASE | amd64::SET_TLSBASE => {
            let v = base.read().map_err(|_| Errno::EFAULT)?;
            if v >= crate::mm::space::USER_END {
                return Err(Errno::EINVAL);
            }
            // SAFETY: the current thread is a user thread; the kernel does
            // not use FS.
            unsafe { hal::user::set_tls(v) };
            if op == amd64::SET_TLSBASE {
                t.set_tls_base(Some(v));
            }
        }
        amd64::GET_GSBASE => base.write(&hal::user::tls2()).map_err(|_| Errno::EFAULT)?,
        amd64::SET_GSBASE => {
            let v = base.read().map_err(|_| Errno::EFAULT)?;
            if v >= crate::mm::space::USER_END {
                return Err(Errno::EINVAL);
            }
            // SAFETY: a canonical user address; the kernel does not use
            // the user's GS.
            unsafe { hal::user::set_tls2(v) };
        }
        amd64::GET_TLSBASE => {
            let v = t.tls_base().ok_or(Errno::ESRCH)?;
            base.write(&v).map_err(|_| Errno::EFAULT)?;
        }
        amd64::DISABLE_TLSBASE => t.set_tls_base(None),
        amd64::GET_XFPUSTATE => {
            let req = uaccess::UserPtr::<amd64::GetXfpustate>::new(parms)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            // What follows the 512-byte legacy region: the XSAVE header and
            // the extended state.
            let fpu = crate::signal::FpuImage::saved().ok_or(Errno::ENOMEM)?;
            let ext = fpu.bytes().get(512..).unwrap_or(&[]);
            let len = usize::try_from(req.len).map_err(|_| Errno::EINVAL)?;
            if len > ext.len() {
                return Err(Errno::EINVAL);
            }
            uaccess::copy_out(req.addr, &ext[..len]).map_err(|_| Errno::EFAULT)?;
        }
        _ => {
            crate::firstuse::report(crate::firstuse::Kind::Sysarch, format_args!("{op}"));
            return Err(Errno::EINVAL);
        }
    }
    Ok(0)
}

/// `sysarch(op, parms)`: arm64 sets its thread pointer from user mode;
/// its own operations are not in yet.
#[cfg(target_arch = "aarch64")]
pub(super) fn sysarch(op: u64, _parms: u64) -> SysResult {
    crate::firstuse::report(crate::firstuse::Kind::Sysarch, format_args!("{op}"));
    Err(Errno::EINVAL)
}

/// Modules built into the kernel, by the names `kldload` takes.
const BUILT_IN: &[&[u8]] = &[b"geom_md"];

/// `kldload(file)`: there are no loadable modules; one built in is
/// `EEXIST`, as FreeBSD answers for a module compiled into its kernel.
pub(super) fn kldload(file: u64) -> SysResult {
    let file = super::path::path(file)?;
    let p = Process::current().ok_or(Errno::EINVAL)?;
    if !p.cred().is_root() {
        return Err(Errno::EPERM);
    }
    let name = file.rsplit(|&b| b == b'/').next().unwrap_or(&file);
    let name = name.strip_suffix(b".ko").unwrap_or(name);
    if BUILT_IN.contains(&name) {
        Err(Errno::EEXIST)
    } else {
        Err(Errno::ENOENT)
    }
}

/// `kldnext(fileid)`: there are no loaded files (the kernel's own modules
/// are built in), so the list is empty.
pub(super) fn kldnext(_fileid: u64) -> SysResult {
    Ok(0)
}
