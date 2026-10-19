// SPDX-License-Identifier: BSD-2-Clause
//! Process identity and odds and ends: `getpid`, `issetugid`, `getrandom`,
//! `exterrctl`, `cpuset_getaffinity`, `cpuset_setaffinity`, `sysarch`.

use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::thr::{
    CPU_LEVEL_CPUSET, CPU_LEVEL_ROOT, CPU_LEVEL_WHICH, CPU_WHICH_PID, CPU_WHICH_TID,
    CPU_WHICH_TIDPID, CPUSET_BYTES, EXTERRCTL_DISABLE, EXTERRCTL_ENABLE,
};

use super::{SysResult, int, process};
use crate::uaccess;

/// `getpid()`.
pub(super) fn getpid() -> SysResult {
    Ok(u64::from(process()?.pid().unsigned_abs()))
}

/// `issetugid()`: no set-id programs yet.
pub(super) fn issetugid() -> SysResult {
    Ok(0)
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

/// `cpuset_getaffinity(level, which, id, setsize, mask)`: every process
/// may run on every CPU.
pub(super) fn cpuset_getaffinity(
    level: u64,
    which: u64,
    id: u64,
    setsize: u64,
    mask: u64,
) -> SysResult {
    let p = process()?;
    let size = usize::try_from(setsize).map_err(|_| Errno::ERANGE)?;
    if !(size_of::<u64>()..=CPUSET_BYTES).contains(&size) {
        return Err(Errno::ERANGE);
    }
    let level = u32::try_from(level).map_err(|_| Errno::EINVAL)?;
    if ![CPU_LEVEL_ROOT, CPU_LEVEL_CPUSET, CPU_LEVEL_WHICH].contains(&level) {
        return Err(Errno::EINVAL);
    }
    let me = int(id) == -1
        || match u32::try_from(which).map_err(|_| Errno::EINVAL)? {
            CPU_WHICH_PID => int(id) == p.pid(),
            CPU_WHICH_TID => id == sched::current().tid().0,
            CPU_WHICH_TIDPID => int(id) == p.pid() || id == sched::current().tid().0,
            _ => return Err(Errno::EINVAL),
        };
    if !me {
        return Err(Errno::ESRCH);
    }
    let mut set = [0u8; CPUSET_BYTES];
    // The root and the process's set are every CPU; its own, its mask.
    let allowed = if level == CPU_LEVEL_WHICH {
        p.cpus()
    } else {
        u64::MAX
    };
    for cpu in 0..sched::online_cpus().min(64) {
        if allowed & (1 << cpu) != 0 {
            set[cpu / 8] |= 1 << (cpu % 8);
        }
    }
    uaccess::copy_out(mask, &set[..size]).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `cpuset_setaffinity(level, which, id, setsize, mask)`: the CPUs a
/// process (this one, or another by pid) may run on.
pub(super) fn cpuset_setaffinity(
    level: u64,
    which: u64,
    id: u64,
    setsize: u64,
    mask: u64,
) -> SysResult {
    let me = process()?;
    let size = usize::try_from(setsize).map_err(|_| Errno::ERANGE)?;
    if size < size_of::<u64>() || size > 4096 {
        return Err(Errno::ERANGE);
    }
    if u32::try_from(level).map_err(|_| Errno::EINVAL)? != CPU_LEVEL_WHICH {
        return Err(Errno::EINVAL);
    }
    let target = match u32::try_from(which).map_err(|_| Errno::EINVAL)? {
        _ if int(id) == -1 => me,
        CPU_WHICH_PID | CPU_WHICH_TIDPID => crate::proc::find(int(id)).ok_or(Errno::ESRCH)?,
        CPU_WHICH_TID if id == sched::current().tid().0 => me,
        CPU_WHICH_TID => return Err(Errno::ESRCH),
        _ => return Err(Errno::EINVAL),
    };
    let mut set = alloc::vec![0u8; size];
    uaccess::copy_in(&mut set, mask).map_err(|_| Errno::EFAULT)?;
    let online = sched::online_cpus();
    let mut bits = 0u64;
    for (i, &b) in set.iter().enumerate() {
        for bit in 0..8 {
            if b & (1 << bit) == 0 {
                continue;
            }
            let cpu = i * 8 + bit;
            // A CPU that does not exist (FreeBSD: beyond the kernel's set
            // size, EINVAL; an absent one is simply never used).
            if cpu >= CPUSET_BYTES * 8 {
                return Err(Errno::EINVAL);
            }
            if cpu < online.min(64) {
                bits |= 1 << cpu;
            }
        }
    }
    // A thread with nowhere to run.
    if bits == 0 {
        return Err(Errno::EDEADLK);
    }
    target.set_cpus(bits);
    Ok(0)
}

/// `sysarch(op, parms)`: amd64's thread pointer (the FS base, which
/// libc's start-up sets as the TLS base).
#[cfg(target_arch = "x86_64")]
pub(super) fn sysarch(op: u64, parms: u64) -> SysResult {
    use abi::sysarch::amd64;
    let p = process()?;
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
                p.set_tls_base(Some(v));
            }
        }
        amd64::GET_TLSBASE => {
            let v = p.tls_base().ok_or(Errno::ESRCH)?;
            base.write(&v).map_err(|_| Errno::EFAULT)?;
        }
        amd64::DISABLE_TLSBASE => p.set_tls_base(None),
        // GS bases need switching per thread first.
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}

/// `sysarch(op, parms)`: arm64 sets its thread pointer from user mode;
/// its own operations are not in yet.
#[cfg(target_arch = "aarch64")]
pub(super) fn sysarch(_op: u64, _parms: u64) -> SysResult {
    Err(Errno::EINVAL)
}
