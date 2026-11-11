// SPDX-License-Identifier: BSD-2-Clause
//! Process identity and odds and ends: `getpid`, `issetugid`, `getrandom`,
//! `exterrctl`, `cpuset_getaffinity`, `cpuset_setaffinity`, `sysarch`.

use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::thr::{
    CPU_LEVEL_CPUSET, CPU_LEVEL_ROOT, CPU_LEVEL_WHICH, CPU_WHICH_CPUSET, CPU_WHICH_DOMAIN,
    CPU_WHICH_INTRHANDLER, CPU_WHICH_IRQ, CPU_WHICH_ITHREAD, CPU_WHICH_JAIL, CPU_WHICH_PID,
    CPU_WHICH_TID, CPU_WHICH_TIDPID, CPUSET_BYTES, DOMAINSET_KERNEL_BYTES, DOMAINSET_MAXSIZE,
    DOMAINSET_POLICY_INVALID, DOMAINSET_POLICY_MAX, EXTERRCTL_DISABLE, EXTERRCTL_ENABLE,
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

/// `utrace(addr, len)`: a `KTR_USER` record (rtld's `LD_UTRACE`,
/// `malloc`'s); a process nobody traces drops it.
pub(super) fn utrace(s: &sched::Sleepable, addr: u64, len: u64) -> SysResult {
    let len = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
    crate::ktrace::user(s, addr, len).map(|()| 0)
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

    /// The numbered set its process is in (`cpuset_refbase`).
    fn set(&self) -> i32 {
        match self {
            Self::Thread(t) => t.process().cpuset(),
            Self::Process(p) => p.cpuset(),
        }
    }
}

/// The numbered set `which` and `id` name: a set's id itself (-1 for the
/// caller's), a jail's root set (a jail below the caller's), or the set
/// of the thread's or process's.
fn set_of(which: u64, id: u64) -> Result<i32, Errno> {
    if which == u64::from(CPU_WHICH_CPUSET) {
        let id = i32::try_from(id.cast_signed()).map_err(|_| Errno::ESRCH)?;
        // -1: the caller's own set (`cpuset_refbase` of its thread's).
        if id == -1 {
            return Ok(process()?.cpuset());
        }
        crate::cpuset::mask(id)?;
        return Ok(id);
    }
    if which == u64::from(CPU_WHICH_JAIL) {
        let id = i32::try_from(id.cast_signed()).map_err(|_| Errno::ESRCH)?;
        let pr = crate::prison::find_child(&crate::prison::current(), id).ok_or(Errno::ESRCH)?;
        return Ok(pr.cpuset());
    }
    Ok(Target::find(which, id)?.set())
}

/// Whether `which` names a numbered set rather than threads: a set or a
/// jail's.
fn names_set(which: u64) -> bool {
    which == u64::from(CPU_WHICH_CPUSET) || which == u64::from(CPU_WHICH_JAIL)
}

/// The set at `level` above set `set`: itself, or its root.
fn at_level(level: u32, set: i32) -> i32 {
    if level == CPU_LEVEL_ROOT {
        crate::cpuset::root_of(set)
    } else {
        set
    }
}

/// `cpuset(setid)`: a new numbered set the caller's process joins.
pub(super) fn cpuset(setid: u64) -> SysResult {
    let p = process()?;
    let id = crate::cpuset::create(&p)?;
    uaccess::UserPtr::<i32>::new(setid)
        .write(&id)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `cpuset_setid(which, id, setid)`: process `id` joins set `setid`
/// (only processes, as FreeBSD).
pub(super) fn cpuset_setid(which: u64, id: u64, setid: u64) -> SysResult {
    if which != u64::from(CPU_WHICH_PID) {
        return Err(Errno::EINVAL);
    }
    let Target::Process(p) = Target::find(which, id)? else {
        return Err(Errno::EINVAL);
    };
    let setid = i32::try_from(setid.cast_signed()).map_err(|_| Errno::ESRCH)?;
    crate::cpuset::join(&p, setid)?;
    Ok(0)
}

/// Whether `which` names an interrupt (`CPU_WHICH_IRQ`,
/// `CPU_WHICH_INTRHANDLER`, `CPU_WHICH_ITHREAD`).
fn is_irq(which: u64) -> bool {
    [CPU_WHICH_IRQ, CPU_WHICH_INTRHANDLER, CPU_WHICH_ITHREAD]
        .iter()
        .any(|&w| u64::from(w) == which)
}

/// Every online CPU (up to 64), as a mask.
fn all_cpus() -> u64 {
    let n = sched::online_cpus().min(64);
    if n == 64 { u64::MAX } else { (1u64 << n) - 1 }
}

/// `cpuset_getid(level, which, id, setid)`.
pub(super) fn cpuset_getid(level: u64, which: u64, id: u64, setid: u64) -> SysResult {
    let level = u32::try_from(level).map_err(|_| Errno::EINVAL)?;
    if ![CPU_LEVEL_ROOT, CPU_LEVEL_CPUSET, CPU_LEVEL_WHICH].contains(&level)
        || (level == CPU_LEVEL_WHICH && which != u64::from(CPU_WHICH_CPUSET))
    {
        return Err(Errno::EINVAL);
    }
    let set = at_level(level, set_of(which, id)?);
    uaccess::UserPtr::<i32>::new(setid)
        .write(&set)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// Capability mode allows only the caller's own thread or process at the
/// `CPU_LEVEL_WHICH` level (`cpuset_check_capabilities`,
/// `cpuset_capmode_allowed`); anything else is `ECAPMODE` there, and
/// recorded if traced.
fn check_capabilities(level: u32, which: u64, id: u64) -> Result<(), Errno> {
    if !crate::capsicum::in_capability_mode() && !crate::ktrace::cap_tracing() {
        return Ok(());
    }
    let id = id.cast_signed();
    let (tid, pid) =
        UserThread::with_current(|t| (i64::from(t.tid()), i64::from(t.process().pid())))
            .unwrap_or((-1, -1));
    let which = u32::try_from(which).unwrap_or(u32::MAX);
    let allowed = level == CPU_LEVEL_WHICH
        && [CPU_WHICH_TID, CPU_WHICH_PID, CPU_WHICH_TIDPID].contains(&which)
        && (id == -1
            || (which == CPU_WHICH_TID && id == tid)
            || (which == CPU_WHICH_PID && id == pid)
            || (which == CPU_WHICH_TIDPID && (id == tid || id == pid)));
    if allowed {
        return Ok(());
    }
    crate::capsicum::refuse(crate::ktrace::CapFail::Cpuset)
}

/// `cpuset_getaffinity(level, which, id, setsize, mask)`: the CPUs a
/// thread may run on, or a process's threads (the union); at the root and
/// cpuset levels, those of the numbered set named or its root.
pub(super) fn cpuset_getaffinity(
    s: &sched::Sleepable,
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
    check_capabilities(level, which, id)?;
    let allowed = if level == CPU_LEVEL_WHICH && is_irq(which) {
        // `intr_getaffinity`: where the interrupt's threads are bound.
        let n = u32::try_from(id).map_err(|_| Errno::ESRCH)?;
        match ::irq::ithread_cpu(n).ok_or(Errno::ESRCH)? {
            Some(cpu) => 1 << cpu.index(),
            None => all_cpus(),
        }
    } else if level == CPU_LEVEL_WHICH && !names_set(which) {
        Target::find(which, id)?.cpus()
    } else {
        crate::cpuset::mask(at_level(level, set_of(which, id)?))?
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
    if crate::ktrace::point(abi::ktrace::KTR_STRUCT) {
        crate::ktrace::structure(s, "cpuset_t", &set[..n]);
    }
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
    s: &sched::Sleepable,
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
    let level = u32::try_from(level).map_err(|_| Errno::EINVAL)?;
    check_capabilities(level, which, id)?;
    if ![CPU_LEVEL_ROOT, CPU_LEVEL_CPUSET, CPU_LEVEL_WHICH].contains(&level) {
        return Err(Errno::EINVAL);
    }
    if crate::ktrace::point(abi::ktrace::KTR_STRUCT) {
        crate::ktrace::structure(s, "cpuset_t", &set);
    }
    let online = sched::online_cpus();
    let mut bits = 0u64;
    let mut absent = false;
    for (i, &b) in set.iter().enumerate() {
        for bit in 0..8 {
            if b & (1 << bit) == 0 {
                continue;
            }
            let cpu = i * 8 + bit;
            if cpu < online.min(64) {
                bits |= 1 << cpu;
            } else {
                absent = true;
            }
        }
    }
    // A thread with nowhere to run (`kern_cpuset_setaffinity`), then a CPU
    // outside the root set, which has every CPU there is (`cpuset_modify`,
    // `cpuset_testshadow`).
    if bits == 0 && !absent {
        return Err(Errno::EDEADLK);
    }
    if absent {
        return Err(Errno::EINVAL);
    }
    // An interrupt (`intr_setaffinity`): one CPU, or all of them to
    // unbind it. Its threads are bound; the interrupt stays where it was
    // connected (docs/compat-deviations.md).
    if level == CPU_LEVEL_WHICH && is_irq(which) {
        process()?.priv_check(crate::cred::Priv::SchedCpusetIntr)?;
        let n = u32::try_from(id).map_err(|_| Errno::ESRCH)?;
        let cpu = if bits == all_cpus() {
            None
        } else if bits.count_ones() == 1 {
            Some(hal::CpuId(bits.trailing_zeros()))
        } else {
            return Err(Errno::EINVAL);
        };
        if which != u64::from(CPU_WHICH_INTRHANDLER) && !::irq::bind_ithread(n, cpu) {
            return Err(Errno::ESRCH);
        }
        if which == u64::from(CPU_WHICH_INTRHANDLER) && ::irq::ithread_cpu(n).is_none() {
            return Err(Errno::ESRCH);
        }
        return Ok(0);
    }
    // A numbered set (`cpuset_modify`), or a thread's or process's own
    // CPUs within its set.
    if level != CPU_LEVEL_WHICH || names_set(which) {
        let set = at_level(level, set_of(which, id)?);
        crate::cpuset::modify(&*process()?, set, bits)?;
        return Ok(0);
    }
    let target = Target::find(which, id)?;
    // Within the set it belongs to (`cpuset_testshadow`).
    let within = crate::cpuset::mask(target.set())?;
    if bits & !within != 0 {
        return Err(Errno::EINVAL);
    }
    match target {
        Target::Thread(t) => t.set_own_cpus(bits),
        Target::Process(p) => p.set_own_cpus(bits),
    }
    Ok(0)
}

/// A `domainset_t` size the domain calls take: from the kernel's set to
/// user space's largest (`ERANGE` otherwise).
fn domainset_size(size: u64) -> Result<usize, Errno> {
    usize::try_from(size)
        .ok()
        .filter(|&n| (DOMAINSET_KERNEL_BYTES..=DOMAINSET_MAXSIZE as usize / 8).contains(&n))
        .ok_or(Errno::ERANGE)
}

/// `EINVAL` for what a domain call's `which` names besides threads and
/// sets: an interrupt or a domain, which take no domain policy.
fn domain_which_check(which: u64) -> Result<(), Errno> {
    if which == u64::from(CPU_WHICH_DOMAIN) || is_irq(which) {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// `cpuset_getdomain(level, which, id, domainsetsize, mask, policy)`
/// (`kern_cpuset_getdomain`): the memory domains and policy of a thread
/// (at `CPU_LEVEL_WHICH`; a process's last thread's), or of a numbered
/// set or its root. One domain: the mask is always domain 0, a preferred
/// domain included.
pub(super) fn cpuset_getdomain(
    level: u64,
    which: u64,
    id: u64,
    size: u64,
    mask: u64,
    policy: u64,
) -> SysResult {
    let size = domainset_size(size)?;
    let level = u32::try_from(level).map_err(|_| Errno::EINVAL)?;
    check_capabilities(level, which, id)?;
    domain_which_check(which)?;
    let p = match level {
        CPU_LEVEL_ROOT | CPU_LEVEL_CPUSET => {
            crate::cpuset::policy(at_level(level, set_of(which, id)?))?
        }
        CPU_LEVEL_WHICH if names_set(which) => crate::cpuset::policy(set_of(which, id)?)?,
        CPU_LEVEL_WHICH => match Target::find(which, id)? {
            Target::Thread(t) => t.domain_policy(),
            Target::Process(p) => p
                .threads()
                .last()
                .map_or(crate::cpuset::FIRSTTOUCH, |t| t.domain_policy()),
        },
        _ => return Err(Errno::EINVAL),
    };
    let mut set = [0u8; DOMAINSET_MAXSIZE as usize / 8];
    set[0] = 1;
    uaccess::copy_out(mask, &set[..size]).map_err(|_| Errno::EFAULT)?;
    uaccess::UserPtr::<i32>::new(policy)
        .write(&i32::from(p))
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `cpuset_setdomain(level, which, id, domainsetsize, mask, policy)`
/// (`kern_cpuset_setdomain`, `domainset_populate`): any policy, over
/// domain 0, the one there is (a preferred domain must be it alone); for
/// a thread or every thread of a process, as its own, or for a numbered
/// set or its root (`cpuset_modify_domain`).
pub(super) fn cpuset_setdomain(
    level: u64,
    which: u64,
    id: u64,
    size: u64,
    mask: u64,
    policy: u64,
) -> SysResult {
    let level = u32::try_from(level).map_err(|_| Errno::EINVAL)?;
    check_capabilities(level, which, id)?;
    let size = domainset_size(size)?;
    let mut set = [0u8; DOMAINSET_MAXSIZE as usize / 8];
    uaccess::copy_in(&mut set[..size], mask).map_err(|_| Errno::EFAULT)?;
    let policy = u32::try_from(super::int(policy)).map_err(|_| Errno::EINVAL)?;
    if policy == DOMAINSET_POLICY_INVALID || policy > DOMAINSET_POLICY_MAX {
        return Err(Errno::EINVAL);
    }
    // Past the kernel's set, nothing may be set.
    if set[DOMAINSET_KERNEL_BYTES..].iter().any(|&b| b != 0) {
        return Err(Errno::EINVAL);
    }
    let kernel = &set[..DOMAINSET_KERNEL_BYTES];
    if kernel.iter().all(|&b| b == 0) {
        return Err(Errno::EDEADLK);
    }
    // Only domain 0 exists (`all_domains`); a preferred domain is one,
    // so it.
    if kernel[0] & !1 != 0 || kernel[1..].iter().any(|&b| b != 0) {
        return Err(Errno::EINVAL);
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "at most DOMAINSET_POLICY_MAX"
    )]
    let policy = policy as u8;
    match level {
        CPU_LEVEL_ROOT | CPU_LEVEL_CPUSET => {
            domain_which_check(which)?;
            let set = at_level(level, set_of(which, id)?);
            crate::cpuset::modify_policy(&*process()?, set, policy)?;
        }
        CPU_LEVEL_WHICH if names_set(which) => {
            crate::cpuset::modify_policy(&*process()?, set_of(which, id)?, policy)?;
        }
        CPU_LEVEL_WHICH => {
            domain_which_check(which)?;
            match Target::find(which, id)? {
                Target::Thread(t) => t.set_domain_policy(policy),
                Target::Process(p) => {
                    for t in p.threads() {
                        t.set_domain_policy(policy);
                    }
                }
            }
        }
        _ => return Err(Errno::EINVAL),
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
            // Only the thread-state requests in capability mode.
            crate::capsicum::refuse(crate::ktrace::CapFail::Syscall(Some(super::int(op))))?;
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

/// Modules built into the kernel, by the names `kldload` takes: the
/// tunnel devices' module and its two aliases (`if_tuntap`, `if_tun`,
/// `if_tap`), the bridge, `epair`, and `md`.
const BUILT_IN: &[&[u8]] = &[
    b"geom_md",
    b"if_epair",
    b"if_bridge",
    b"if_tuntap",
    b"if_tun",
    b"if_tap",
];

/// `kldload(file)`: there are no loadable modules; one built in is
/// `EEXIST`, as FreeBSD answers for a module compiled into its kernel.
pub(super) fn kldload(file: u64) -> SysResult {
    let file = super::path::path(file)?;
    let p = Process::current().ok_or(Errno::EINVAL)?;
    // Nothing loads above securelevel 0 (FreeBSD's `kern_kldload`).
    let cred = p.cred();
    cred.securelevel_gt(0)?;
    cred.priv_check(crate::cred::Priv::KldLoad)?;
    let name = file.rsplit(|&b| b == b'/').next().unwrap_or(&file);
    let name = name.strip_suffix(b".ko").unwrap_or(name);
    if BUILT_IN.contains(&name) {
        Err(Errno::EEXIST)
    } else {
        Err(Errno::ENOENT)
    }
}

/// The kernel's file id: the only file, everything being built in.
const KERNEL_FILE: i32 = 1;

/// The modules the kernel file holds, by FreeBSD's module names
/// (`DECLARE_MODULE`'s, `bus/driver` for drivers), for what AbyssBSD
/// implements; `modfind` of any other is `ENOENT`, as on a kernel built
/// without it. Each one's id is its place here plus one.
const MODULES: &[&[u8]] = &[
    b"shell",
    b"elf64",
    b"sysvmsg",
    b"sysvsem",
    b"sysvshm",
    b"ufs",
    b"tmpfs",
    b"msdosfs",
    b"devfs",
    b"acl_posix1e",
    b"g_class",
    b"g_part",
    b"g_part_gpt",
    b"g_label",
    b"g_vfs",
    b"g_disk",
    b"g_dev",
    b"g_md",
    b"g_flashmap",
    b"g_raid",
    b"cam",
    b"xpt",
    b"da",
    b"nda",
    b"nvme",
    b"virtio",
    b"random_device",
    b"rootbus",
    b"efirt",
    b"mem",
    b"null",
    b"if_lo",
    b"if_epair",
    b"if_tuntap",
    b"if_tun",
    b"if_tap",
    b"if_bridge",
    b"netlink",
    b"kbdmux",
    b"watchdog",
    b"pci/nvme",
    b"pci/xhci",
    b"pci/ahci",
    b"pci/virtio_pci_modern",
    b"uhub/uhub",
    b"uhub/umass",
    b"uhub/usbhid",
    b"usbhid/hidbus",
    b"hidbus/hkbd",
    b"hidbus/hms",
    b"acpi/uart",
    b"nexus/efirtc",
];

/// The module with id `id`.
fn module(id: i32) -> Result<(i32, &'static [u8]), Errno> {
    let i = usize::try_from(id).ok().and_then(|i| i.checked_sub(1));
    let name = i.and_then(|i| MODULES.get(i)).ok_or(Errno::ENOENT)?;
    Ok((id, name))
}

/// `kldnext(fileid)`: the file after `fileid`, the kernel after 0.
pub(super) fn kldnext(fileid: u64) -> SysResult {
    match super::int(fileid) {
        0 => Ok(KERNEL_FILE as u64),
        KERNEL_FILE => Ok(0),
        _ => Err(Errno::ENOENT),
    }
}

/// `kldfind(file)`: the kernel's id for `kernel`, by base name.
pub(super) fn kldfind(file: u64) -> SysResult {
    let file = super::path::path(file)?;
    let name = file.rsplit(|&b| b == b'/').next().unwrap_or(&file);
    // A built-in module's file is the kernel that holds it: `kldstat -n
    // if_epair.ko` succeeds as it does where the module was loaded, which
    // is what a jail, unable to load it, checks for (`vnet_init`).
    let module = name.strip_suffix(b".ko").filter(|m| BUILT_IN.contains(m));
    if name == b"kernel" || module.is_some() {
        Ok(KERNEL_FILE as u64)
    } else {
        Err(Errno::ENOENT)
    }
}

/// `kldstat(fileid, stat)`: the kernel's record, in either version's
/// size. The address is the link address, not where KASLR put it.
pub(super) fn kldstat(fileid: u64, stat: u64) -> SysResult {
    use abi::linker::{KLD_FILE_STAT_1_SIZE, KldFileStat};
    if super::int(fileid) != KERNEL_FILE {
        return Err(Errno::ENOENT);
    }
    let version = uaccess::load_u32(stat).map_err(|_| Errno::EFAULT)?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    if version != KLD_FILE_STAT_1_SIZE && version != size_of::<KldFileStat>() {
        return Err(Errno::EINVAL);
    }
    let k = crate::boot::info().kernel;
    let mut st = KldFileStat {
        version: i32::try_from(version).unwrap_or(0),
        name: [0; abi::linker::MAXPATHLEN],
        refs: i32::try_from(MODULES.len() + 1).unwrap_or(i32::MAX),
        id: KERNEL_FILE,
        _pad0: [0; 4],
        address: k.virt.get().wrapping_sub(k.slide),
        size: k.size,
        pathname: [0; abi::linker::MAXPATHLEN],
    };
    st.name[..6].copy_from_slice(b"kernel");
    st.pathname[..19].copy_from_slice(b"/boot/kernel/kernel");
    uaccess::copy_out(stat, &uaccess::bytes_of(&st)[..version]).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `kldfirstmod(fileid)`: the kernel's first module.
pub(super) fn kldfirstmod(fileid: u64) -> SysResult {
    if super::int(fileid) != KERNEL_FILE {
        return Err(Errno::ENOENT);
    }
    Ok(1)
}

/// `modnext(modid)`: the module after `modid`, the first after 0; 0
/// after the last.
pub(super) fn modnext(modid: u64) -> SysResult {
    match super::int(modid) {
        0 => Ok(1),
        _ => modfnext(modid),
    }
}

/// `modfnext(modid)`: the next module in `modid`'s file.
pub(super) fn modfnext(modid: u64) -> SysResult {
    let (id, _) = module(super::int(modid))?;
    Ok(if usize::try_from(id).is_ok_and(|i| i < MODULES.len()) {
        u64::from(id.cast_unsigned()) + 1
    } else {
        0
    })
}

/// `modstat(modid, stat)`: its name, references, id, and (past the first
/// version) data, in whichever of the three versions the caller's
/// `version` names, as `sys_modstat` does. The name is cut to the
/// version's length, its NUL included when it fits.
pub(super) fn modstat(modid: u64, stat: u64) -> SysResult {
    use abi::linker::{MODULE_STAT_V1_SIZE, MODULE_STAT_V2_SIZE, ModuleStat, ModuleStatV2};
    // `buf` is zero: the NUL is there unless the name fills it.
    fn name_into(buf: &mut [u8], name: &[u8]) {
        let n = name.len().min(buf.len());
        buf[..n].copy_from_slice(&name[..n]);
    }
    let (id, name) = module(super::int(modid))?;
    let version = uaccess::load_u32(stat).map_err(|_| Errno::EFAULT)?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    let record = if version == size_of::<ModuleStat>() {
        let mut st = ModuleStat {
            version: 0,
            name: [0; abi::linker::MAXMODNAME],
            refs: 1,
            id,
            _pad0: [0; 4],
            data: 0,
        };
        name_into(&mut st.name, name);
        uaccess::bytes_of(&st).to_vec()
    } else if version == MODULE_STAT_V1_SIZE || version == MODULE_STAT_V2_SIZE {
        let mut st = ModuleStatV2 {
            version: 0,
            name: [0; abi::linker::MAXMODNAMEV1V2],
            refs: 1,
            id,
            _pad0: [0; 4],
            data: 0,
        };
        name_into(&mut st.name, name);
        uaccess::bytes_of(&st).to_vec()
    } else {
        return Err(Errno::EINVAL);
    };
    // `version` is not written back.
    uaccess::copy_out(stat + 4, &record[4..version]).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `modfind(name)`: the id of the module `name`.
pub(super) fn modfind(name: u64) -> SysResult {
    let name = uaccess::copy_in_str_vec(name, abi::linker::MAXMODNAME).map_err(|e| match e {
        uaccess::StrError::Fault => Errno::EFAULT,
        uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    })?;
    MODULES
        .iter()
        .position(|m| *m == name.as_slice())
        .map(|i| i as u64 + 1)
        .ok_or(Errno::ENOENT)
}

/// `uuidgen(store, count)`: `count` version-1 UUIDs, 1 to 2048 of them,
/// consecutive in time (`sys_uuidgen`, `kern_uuidgen`).
pub(super) fn uuidgen(store: u64, count: u64) -> SysResult {
    let count = usize::try_from(super::int(count))
        .ok()
        .filter(|n| (1..=2048).contains(n))
        .ok_or(Errno::EINVAL)?;
    let mut out = alloc::vec::Vec::new();
    out.try_reserve_exact(count).map_err(|_| Errno::ENOMEM)?;
    out.resize(count, [0u8; 16]);
    crate::uuid::generate(&mut out);
    uaccess::copy_out(store, out.as_flattened()).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// The `membarrier` commands there are (`MEMBARRIER_SUPPORTED_CMDS`).
const MEMBARRIER_SUPPORTED: u32 = abi::thr::MEMBARRIER_CMD_GLOBAL
    | abi::thr::MEMBARRIER_CMD_GLOBAL_EXPEDITED
    | abi::thr::MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
    | abi::thr::MEMBARRIER_CMD_PRIVATE_EXPEDITED
    | abi::thr::MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
    | abi::thr::MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE
    | abi::thr::MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE
    | abi::thr::MEMBARRIER_CMD_GET_REGISTRATIONS;

/// `membarrier(cmd, flags, cpu_id)` (`kern_membarrier`): a full memory
/// barrier on the CPUs that run the process's threads (`PRIVATE`, once
/// registered) or any user thread (`GLOBAL`), by an interrupt to each;
/// the registrations, which `execve` ends; and `QUERY`, the commands
/// there are. `MEMBARRIER_CMD_GLOBAL` interrupts every CPU where FreeBSD
/// waits for each to switch threads, and `GLOBAL_EXPEDITED` every CPU,
/// not only those running a registered process (docs/compat-deviations.md).
pub(super) fn membarrier(cmd: u64, flags: u64, _cpu_id: u64) -> SysResult {
    use crate::proc::p2;
    use abi::thr::{
        MEMBARRIER_CMD_GET_REGISTRATIONS, MEMBARRIER_CMD_GLOBAL, MEMBARRIER_CMD_GLOBAL_EXPEDITED,
        MEMBARRIER_CMD_PRIVATE_EXPEDITED, MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE,
        MEMBARRIER_CMD_QUERY, MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED,
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE,
    };
    let cmd = super::int(cmd).cast_unsigned();
    if super::int(flags) != 0 || cmd & !MEMBARRIER_SUPPORTED != 0 {
        return Err(Errno::EINVAL);
    }
    if cmd == MEMBARRIER_CMD_QUERY {
        return Ok(u64::from(MEMBARRIER_SUPPORTED));
    }
    let p = process()?;
    let f = p.flag2.load(Ordering::Relaxed);
    let register = |bit: u32| {
        p.flag2.fetch_or(bit, Ordering::Relaxed);
        Ok(0)
    };
    match cmd {
        MEMBARRIER_CMD_GLOBAL_EXPEDITED if f & p2::MEMBAR_GLOBE == 0 => Err(Errno::EPERM),
        MEMBARRIER_CMD_GLOBAL | MEMBARRIER_CMD_GLOBAL_EXPEDITED => {
            crate::smp::barrier_cpus(all_cpus());
            Ok(0)
        }
        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED => register(p2::MEMBAR_GLOBE),
        MEMBARRIER_CMD_PRIVATE_EXPEDITED if f & p2::MEMBAR_PRIVE == 0 => Err(Errno::EPERM),
        MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE if f & p2::MEMBAR_PRIVE_SYNCORE == 0 => {
            Err(Errno::EPERM)
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE => {
            crate::smp::barrier_cpus(p.space().loaded_cpus());
            Ok(0)
        }
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => register(p2::MEMBAR_PRIVE),
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => register(p2::MEMBAR_PRIVE_SYNCORE),
        MEMBARRIER_CMD_GET_REGISTRATIONS => {
            let mut res = 0;
            for (bit, reg) in [
                (p2::MEMBAR_GLOBE, MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED),
                (p2::MEMBAR_PRIVE, MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED),
                (
                    p2::MEMBAR_PRIVE_SYNCORE,
                    MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE,
                ),
            ] {
                if f & bit != 0 {
                    res |= reg;
                }
            }
            Ok(u64::from(res))
        }
        // More than one command at once.
        _ => Err(Errno::EINVAL),
    }
}

/// A process `kcmp` compares (`kcmp_pget`): the caller, or one it may
/// debug that is not exiting.
fn kcmp_process(me: &Arc<Process>, pid: i32) -> Result<Arc<Process>, Errno> {
    if pid == me.pid() {
        return Ok(Arc::clone(me));
    }
    let p = crate::proc::find(pid).ok_or(Errno::ESRCH)?;
    if p.has_exited() {
        return Err(Errno::ESRCH);
    }
    me.can_debug(&p)?;
    Ok(p)
}

/// `kcmp`'s order of two kernel objects (`kcmp_cmp`): 0 for the same,
/// 1 if the first comes first, 2 if after.
fn kcmp_order(a: usize, b: usize) -> u64 {
    match a.cmp(&b) {
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Less => 1,
        core::cmp::Ordering::Greater => 2,
    }
}

/// What a file is open on, for `KCMP_FILEOBJ` (`fo_cmp`): its kind and
/// the object's address. Vnodes and named pipes are their vnode; kinds
/// whose objects have one open file each are the file itself.
fn kcmp_object(f: &Arc<crate::fs::file::File>) -> (u8, usize) {
    use crate::fs::file::FileKind as K;
    let file = Arc::as_ptr(f).addr();
    match f.kind() {
        K::Vnode(v) => (0, Arc::as_ptr(v.vnode.vnode()).addr()),
        K::Fifo(v) => (1, Arc::as_ptr(v.vnode.vnode()).addr()),
        K::Shm(m) => (2, Arc::as_ptr(m).addr()),
        K::Timerfd(t) => (3, Arc::as_ptr(t).addr()),
        K::Eventfd(e) => (4, Arc::as_ptr(e).addr()),
        K::Pts(m) => (5, Arc::as_ptr(m).addr()),
        K::Pipe(_) => (6, file),
        K::Process(_) => (7, file),
        K::Jail(_) => (8, file),
        K::Socket(_) | K::Inet(_) | K::Netlink(_) => (9, file),
        K::Kqueue(_) => (10, file),
        K::Inotify(_) => (11, file),
    }
}

/// `kcmp(pid1, pid2, type, idx1, idx2)` (`kern_kcmp`): whether two
/// processes share a kernel object, and if not, an order of the two: an
/// open file (`KCMP_FILE`), what two files are open on (`KCMP_FILEOBJ`;
/// 3 for files of different kinds), the descriptor table, the signal
/// actions, or the address space.
pub(super) fn kcmp(
    s: &sched::Sleepable,
    pid1: u64,
    pid2: u64,
    ty: u64,
    idx1: u64,
    idx2: u64,
) -> SysResult {
    let me = process()?;
    let p1 = kcmp_process(&me, super::int(pid1))?;
    let p2 = kcmp_process(&me, super::int(pid2))?;
    let ty = super::int(ty).cast_unsigned();
    match ty {
        abi::thr::KCMP_FILE | abi::thr::KCMP_FILEOBJ => {
            let f1 = p1.files.lock(s).get(super::int(idx1))?;
            let f2 = p2.files.lock(s).get(super::int(idx2))?;
            if ty == abi::thr::KCMP_FILE {
                return Ok(kcmp_order(Arc::as_ptr(&f1).addr(), Arc::as_ptr(&f2).addr()));
            }
            let (k1, o1) = kcmp_object(&f1);
            let (k2, o2) = kcmp_object(&f2);
            if k1 != k2 {
                return Ok(3);
            }
            Ok(kcmp_order(o1, o2))
        }
        abi::thr::KCMP_FILES => Ok(kcmp_order(
            Arc::as_ptr(&p1.files).addr(),
            Arc::as_ptr(&p2.files).addr(),
        )),
        // Each process has signal actions of its own (no `RFSIGSHARE`).
        abi::thr::KCMP_SIGHAND => Ok(kcmp_order(Arc::as_ptr(&p1).addr(), Arc::as_ptr(&p2).addr())),
        abi::thr::KCMP_VM => Ok(kcmp_order(
            Arc::as_ptr(&p1.space()).addr(),
            Arc::as_ptr(&p2.space()).addr(),
        )),
        _ => Err(Errno::EINVAL),
    }
}

/// What `abort2` logs, and the signal it ends the process with
/// (`kern_abort2`): `SIGABRT`, or `SIGKILL` if what it was given cannot
/// be read.
fn abort2_reason(why: u64, nargs: u64, args: u64) -> (alloc::string::String, u32) {
    use core::fmt::Write;
    let mut msg = alloc::string::String::new();
    if let Some(p) = Process::current() {
        let comm = p.with_comm(|c| alloc::string::String::from_utf8_lossy(c).into_owned());
        let _ = write!(
            msg,
            "{comm}(pid {} uid {}) aborted: ",
            p.pid(),
            p.cred().uid
        );
    }
    let nargs = super::int(nargs);
    let mut ok = (0..=16).contains(&nargs);
    let n = usize::try_from(nargs).unwrap_or(0).min(16);
    let mut raw = [0u8; 16 * 8];
    if ok && n > 0 {
        ok = args != 0 && uaccess::copy_in(&mut raw[..n * 8], args).is_ok();
    }
    if ok {
        if why == 0 {
            msg.push_str("(null)");
        } else {
            // At most 128 bytes of it.
            let mut text = [0u8; 128];
            match uaccess::copy_in_str(why, &mut text) {
                Ok(len) => msg.push_str(&alloc::string::String::from_utf8_lossy(&text[..len])),
                Err(uaccess::StrError::TooLong) => {
                    msg.push_str(&alloc::string::String::from_utf8_lossy(&text));
                }
                Err(_) => ok = false,
            }
        }
    }
    if ok && n > 0 {
        msg.push('(');
        for (i, w) in raw[..n * 8].as_chunks::<8>().0.iter().enumerate() {
            let a = u64::from_ne_bytes(*w);
            let sep = if i == 0 { "" } else { ", " };
            // The kernel's `%p`: no `0x` before a null pointer.
            let _ = if a == 0 {
                write!(msg, "{sep}0")
            } else {
                write!(msg, "{sep}{a:#x}")
            };
        }
        msg.push(')');
    }
    if !ok {
        while msg.ends_with(' ') {
            msg.pop();
        }
        msg.push_str(" (Reason text inaccessible)");
    }
    msg.push('\n');
    let sig = if ok {
        abi::signal::SIGABRT
    } else {
        abi::signal::SIGKILL
    };
    (msg, sig)
}

/// `abort2(why, nargs, args)` (`kern_abort2`): logs why the process
/// aborts, with up to 16 pointer arguments, and ends it with `SIGABRT`,
/// or `SIGKILL` given what it cannot read.
pub(super) fn abort2(s: &sched::Sleepable, why: u64, nargs: u64, args: u64) -> ! {
    let (msg, sig) = abort2_reason(why, nargs, args);
    crate::klog::log(abi::syslog::LOG_INFO, format_args!("{msg}"));
    drop(msg);
    crate::signal::kill_self(s, sig)
}
