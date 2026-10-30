// SPDX-License-Identifier: BSD-2-Clause
//! Process identity and odds and ends: `getpid`, `issetugid`, `getrandom`,
//! `exterrctl`, `cpuset_getaffinity`, `cpuset_setaffinity`, `sysarch`.

use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::thr::{
    CPU_LEVEL_CPUSET, CPU_LEVEL_ROOT, CPU_LEVEL_WHICH, CPU_WHICH_CPUSET, CPU_WHICH_JAIL,
    CPU_WHICH_PID, CPU_WHICH_TID, CPU_WHICH_TIDPID, CPUSET_BYTES, EXTERRCTL_DISABLE,
    EXTERRCTL_ENABLE,
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

/// The numbered set `which` and `id` name: a set's id itself, a jail's
/// root set (a jail below the caller's), or the set of the thread's or
/// process's.
fn set_of(which: u64, id: u64) -> Result<i32, Errno> {
    if which == u64::from(CPU_WHICH_CPUSET) {
        let id = i32::try_from(id.cast_signed()).map_err(|_| Errno::ESRCH)?;
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
    let allowed = if level == CPU_LEVEL_WHICH && !names_set(which) {
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

/// Modules built into the kernel, by the names `kldload` takes.
const BUILT_IN: &[&[u8]] = &[b"geom_md"];

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
    if name == b"kernel" {
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
