// SPDX-License-Identifier: BSD-2-Clause
//! System calls: decoding the trap frame, dispatch by number, and the
//! result convention (ARCHITECTURE.md §7).
//!
//! amd64 passes the number in `rax` and arguments in `rdi, rsi, rdx, r10,
//! r8, r9`; arm64 the number in `x8` and arguments in `x0..x7`. A result
//! comes back in `rax`/`x0` with the carry flag clear; an error number
//! with it set. A second result goes in `rdx`/`x1`: on arm64 it is always
//! written (0 unless the call has one), on amd64 `rdx` keeps its value. `syscall(2)` and `__syscall(2)` take the real number as
//! their first argument.
//!
//! Numbers come from `syscalls.master` through the generated
//! [`abi::syscall`] table. A number that is no system call gets `SIGSYS`
//! and `ENOSYS`, as on FreeBSD. Calls FreeBSD has that are not
//! implemented yet return `ENOSYS` without the signal (libc probes some),
//! and the first use of each by each program is logged
//! ([`crate::firstuse`]); a number that is no call is logged once.
//!
//! A call a signal interrupted may ask to be re-executed (`ERESTART`):
//! the program counter goes back to the `syscall`/`svc` instruction, whose
//! number and arguments are still in place.
//!
//! Behavior reference: sys/kern/subr_syscall.c, sys/amd64/amd64/trap.c,
//! sys/arm64/arm64/trap.c.

mod acl;
mod event;
mod extattr;
mod fsops;
mod inet;
mod inotify;
mod io;
mod ipc;
mod jail;
mod mem;
mod misc;
mod netlink;
mod path;
mod proc;
mod procctl;
mod shm;
mod sig;
mod socket;
mod sysctl;
mod thr;
mod time;
mod timers;
mod umtx;

pub(crate) use path::{open_core_file, open_trace_file};

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::syscall::{SLOT_COUNT, Slot, Sysno, lookup};
use sched::Sleepable;

use crate::arch;
use crate::arch::trap::TrapFrame;
use crate::proc::Process;

/// What a system call returns.
pub(crate) type SysResult = Result<u64, Errno>;

/// A decoded call: its number and up to `sy_narg` arguments (the most
/// any call takes is 8).
#[derive(Clone, Copy, Debug)]
struct Call {
    number: u64,
    args: [u64; 8],
    /// How many of `args` the call takes (`sy_narg`).
    narg: usize,
}

/// [`narg`] for a debugger (`sy_narg`).
pub(crate) fn narg_of(code: u32) -> usize {
    narg(u64::from(code))
}

/// The arguments of the call `f` holds, as `syscallenter` read them.
pub(crate) fn args_of(f: &TrapFrame) -> [u64; 8] {
    decode(f).0.args
}

/// How many arguments the call numbered `number` takes; none for one
/// that is no call (`nosys_sysent`).
fn narg(number: u64) -> usize {
    match lookup(number) {
        Some(Slot::Live(n)) => n.info().args.len(),
        _ => 0,
    }
}

/// The call in `f` (`cpu_fetch_syscall_args`): `syscall(2)` and
/// `__syscall(2)` take the real number as their first argument. Past the
/// six argument registers the rest are on the user stack, after the
/// return address; `EFAULT` if they cannot be read.
#[cfg(target_arch = "x86_64")]
fn decode(f: &TrapFrame) -> (Call, Result<(), Errno>) {
    let regs = [f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9];
    let indirect = f.rax == Sysno::Syscall as u64 || f.rax == Sysno::SyscallQuad as u64;
    let (number, regs) = if indirect {
        (f.rdi, &regs[1..])
    } else {
        (f.rax, &regs[..])
    };
    let mut call = Call {
        number,
        args: [0; 8],
        narg: narg(number),
    };
    call.args[..regs.len()].copy_from_slice(regs);
    let mut fetched = Ok(());
    for (i, arg) in call.args[regs.len()..call.narg.max(regs.len())]
        .iter_mut()
        .enumerate()
    {
        match crate::uaccess::UserPtr::<u64>::new(f.rsp + 8 + 8 * i as u64).read() {
            Ok(v) => *arg = v,
            Err(_) => fetched = Err(Errno::EFAULT),
        }
    }
    (call, fetched)
}

#[cfg(target_arch = "aarch64")]
fn decode(f: &TrapFrame) -> (Call, Result<(), Errno>) {
    let indirect = f.x[8] == Sysno::Syscall as u64 || f.x[8] == Sysno::SyscallQuad as u64;
    let mut args = [0; 8];
    let number = if indirect {
        args.copy_from_slice(&f.x[1..9]);
        f.x[0]
    } else {
        args.copy_from_slice(&f.x[..8]);
        f.x[8]
    };
    let call = Call {
        number,
        args,
        narg: narg(number),
    };
    (call, Ok(()))
}

#[cfg(target_arch = "x86_64")]
fn encode(f: &mut TrapFrame, r: SysResult) {
    const CARRY: u64 = 1;
    match r {
        Err(Errno::EJUSTRETURN) => {}
        // `syscall` is 2 bytes; rax still holds the number.
        Err(Errno::ERESTART) => f.rip -= 2,
        Ok(v) => {
            f.rax = v;
            f.rflags &= !CARRY;
        }
        Err(e) => {
            f.rax = u64::from(e.get().unsigned_abs());
            f.rflags |= CARRY;
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn encode(f: &mut TrapFrame, r: SysResult) {
    const CARRY: u64 = 1 << 29;
    match r {
        Err(Errno::EJUSTRETURN) => {}
        // `svc` is 4 bytes; x8 and the arguments are as they were.
        Err(Errno::ERESTART) => f.elr -= 4,
        Ok(v) => {
            // The second result (`td_retval[1]`), 0 for every call here.
            // libc's `vfork` computes `x0 & (x1 - 1)`, so a stale x1
            // garbles the parent's pid.
            f.x[0] = v;
            f.x[1] = 0;
            f.spsr &= !CARRY;
        }
        Err(e) => {
            f.x[0] = u64::from(e.get().unsigned_abs());
            f.spsr |= CARRY;
        }
    }
}

/// The `kernel_hooks!` system-call hook (`syscallenter`, `syscallret`).
/// Entered with interrupts masked; returns with them masked.
pub(crate) fn entry(frame: &mut TrapFrame) {
    hal::irq::enable();
    // SAFETY: a system call runs in its thread, which may sleep; it holds
    // nothing (it came from user mode).
    let s = unsafe { Sleepable::assume() };
    let (mut call, mut fetched) = decode(frame);
    let code = u32::try_from(call.number).unwrap_or(u32::MAX);
    // Where a debugger finds the registers while the thread is in here;
    // and whether ktrace or a debugger traces the process, looked up once
    // for the untraced common case.
    let (ktraced, debugged) = crate::uthread::UserThread::with_current(|t| {
        t.dbg.entered(frame);
        t.set_syscall_code(code);
        let p = t.process();
        (p.ktrace.flags() != 0, p.is_traced())
    })
    .unwrap_or((false, false));
    if ktraced && crate::ktrace::point(abi::ktrace::KTR_SYSCALL) {
        crate::ktrace::syscall(&s, code, &call.args[..call.narg]);
    }
    // A debugger sees the call first, and may change it (`syscallenter`).
    if debugged
        && fetched.is_ok()
        && crate::ptrace::syscall_enter(&s, code, &call.args[..call.narg])
    {
        (call, fetched) = decode(frame);
        if ktraced && crate::ktrace::point(abi::ktrace::KTR_SYSCALL) {
            crate::ktrace::syscall(&s, code, &call.args[..call.narg]);
        }
    }
    // `None`: the handler set the frame itself (`execve`), which returns
    // as `EJUSTRETURN`.
    let r = fetched.and_then(|()| run(&s, frame, call));
    let ret = match r {
        Ok(Some(v)) => {
            encode(frame, Ok(v));
            Ok(v)
        }
        Ok(None) => Err(Errno::EJUSTRETURN),
        Err(e) => {
            crate::capsicum::syscall_failed(call.number, e);
            encode(frame, Err(e));
            Err(e)
        }
    };
    let (error, retval) = match ret {
        Ok(v) => (0, v),
        Err(e) => (e.get(), 0),
    };
    if ktraced && crate::ktrace::point(abi::ktrace::KTR_SYSRET) {
        // The thread's code now (`td_sa.code`), which arm64's
        // `sigreturn` changes.
        let now =
            crate::uthread::UserThread::with_current(crate::uthread::UserThread::syscall_code)
                .unwrap_or(code);
        crate::ktrace::sysret(&s, now, error, retval);
    }
    // The debugger sees the call return (`syscallret`): a process traced
    // now, or one whose call made the tracing (`PT_TRACE_ME`).
    if debugged || (code == Sysno::Ptrace as u32 && crate::ptrace::current_traced()) {
        crate::ptrace::syscall_exit(&s, error, [retval, second_result(frame)]);
        // A traced `vfork` parent waits for its child here, after its
        // debugger has seen the fork (`ast_vfork`).
        crate::proc::vfork_wait(&s);
        crate::ptrace::vfork_done(&s);
    }
    let _ = hal::irq::disable();
}

/// The second result register as the call left it (`td_retval[1]`).
fn second_result(f: &TrapFrame) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        f.rdx
    }
    #[cfg(target_arch = "aarch64")]
    {
        f.x[1]
    }
}

fn run(s: &Sleepable, frame: &mut TrapFrame, call: Call) -> Result<Option<u64>, Errno> {
    let [a0, a1, a2, a3, a4, a5, ..] = call.args;
    let Some(Slot::Live(n)) = lookup(call.number) else {
        return nosys(call.number).map(Some);
    };
    // Capability mode allows only the calls marked for it; a traced
    // process's others are recorded as if it were in it.
    if !n.info().cap_enabled {
        if crate::ktrace::cap_tracing() {
            crate::ktrace::capfail(crate::ktrace::CapFail::Syscall(None));
        }
        if crate::capsicum::in_capability_mode() {
            return Err(Errno::ECAPMODE);
        }
    }
    let r = match n {
        Sysno::Execve => return proc::execve(s, frame, a0, a1, a2),
        Sysno::Fexecve => return proc::fexecve(s, frame, a0, a1, a2),
        Sysno::Fork => proc::fork(s, frame, false),
        Sysno::Vfork => proc::fork(s, frame, true),
        Sysno::Rfork => proc::rfork(s, frame, a0),
        Sysno::Getpriority => proc::getpriority(a0, a1),
        Sysno::Setpriority => proc::setpriority(a0, a1, a2),
        Sysno::Pdfork => proc::pdfork(s, frame, a0, a1),
        Sysno::Pdrfork => proc::pdrfork(s, frame, a0, a1, a2),
        Sysno::Pdgetpid => proc::pdgetpid(s, a0, a1),
        Sysno::Pdkill => proc::pdkill(s, a0, a1),
        Sysno::Getrusage => proc::getrusage(s, a0, a1),
        Sysno::Wait4 => proc::wait4(s, a0, a1, a2, a3),
        Sysno::Wait6 => proc::wait6(s, a0, a1, a2, a3, a4, a5),
        Sysno::Pdwait => proc::pdwait(s, a0, a1, a2, a3, a4),
        Sysno::Getppid => proc::getppid(),
        Sysno::Getpgrp => proc::getpgrp(),
        Sysno::Getpgid => proc::getpgid(a0, false),
        Sysno::Getsid => proc::getpgid(a0, true),
        Sysno::Setsid => proc::setsid(),
        Sysno::Setpgid => proc::setpgid(a0, a1),
        Sysno::Getuid => proc::get_id(proc::Id::Uid),
        Sysno::Geteuid => proc::get_id(proc::Id::Euid),
        Sysno::Getgid => proc::get_id(proc::Id::Gid),
        Sysno::Getegid => proc::get_id(proc::Id::Egid),
        Sysno::Getgroups => proc::getgroups(a0, a1),
        Sysno::Setgroups => proc::setgroups(a0, a1),
        Sysno::Getresuid => proc::getres(a0, a1, a2, false),
        Sysno::Getresgid => proc::getres(a0, a1, a2, true),
        Sysno::Setuid => proc::set_id(proc::SetId::Uid, a0),
        Sysno::Seteuid => proc::set_id(proc::SetId::Euid, a0),
        Sysno::Setgid => proc::set_id(proc::SetId::Gid, a0),
        Sysno::Setegid => proc::set_id(proc::SetId::Egid, a0),
        Sysno::Setreuid => proc::setre(a0, a1, false),
        Sysno::Setregid => proc::setre(a0, a1, true),
        Sysno::Setresuid => proc::setres(a0, a1, a2, false),
        Sysno::Setresgid => proc::setres(a0, a1, a2, true),
        Sysno::Setcred => proc::setcred(a0, a1, a2),
        Sysno::Procctl => procctl::procctl(a0, a1, a2, a3),
        Sysno::Jail => jail::jail(s, a0),
        Sysno::JailSet => jail::jail_set(s, a0, a1, a2),
        Sysno::JailGet => jail::jail_get(s, a0, a1, a2),
        Sysno::JailAttach => jail::jail_attach(s, a0),
        Sysno::JailRemove => jail::jail_remove(a0),
        Sysno::JailAttachJd => jail::jail_attach_jd(s, a0),
        Sysno::JailRemoveJd => jail::jail_remove_jd(s, a0),
        Sysno::CapEnter => crate::capsicum::cap_enter(),
        Sysno::CapGetmode => crate::capsicum::cap_getmode(a0),
        Sysno::CapRightsLimit => crate::capsicum::cap_rights_limit(s, a0, a1),
        Sysno::CapRightsGet => crate::capsicum::cap_rights_get(s, a0, a1, a2),
        Sysno::CapIoctlsLimit => crate::capsicum::cap_ioctls_limit(s, a0, a1, a2),
        Sysno::CapIoctlsGet => crate::capsicum::cap_ioctls_get(s, a0, a1, a2),
        Sysno::CapFcntlsLimit => crate::capsicum::cap_fcntls_limit(s, a0, a1),
        Sysno::CapFcntlsGet => crate::capsicum::cap_fcntls_get(s, a0, a1),
        Sysno::Getlogin => proc::getlogin(a0, a1),
        Sysno::Setlogin => proc::setlogin(a0),
        Sysno::Getloginclass => proc::getloginclass(a0, a1),
        Sysno::Setloginclass => proc::setloginclass(a0),
        Sysno::ClockGettime => time::clock_gettime(a0, a1),
        Sysno::ClockGetres => time::clock_getres(a0, a1),
        Sysno::Gettimeofday => time::gettimeofday(a0, a1),
        Sysno::Nanosleep => timers::nanosleep(s, a0, a1),
        Sysno::ClockNanosleep => timers::clock_nanosleep(s, a0, a1, a2, a3),
        Sysno::ClockSettime => timers::clock_settime(a0, a1),
        Sysno::Settimeofday => timers::settimeofday(a0, a1),
        Sysno::Adjtime => timers::adjtime(a0, a1),
        Sysno::ClockGetcpuclockid2 => timers::clock_getcpuclockid2(a0, a1, a2),
        Sysno::Setitimer => timers::setitimer(s, a0, a1, a2),
        Sysno::Getitimer => timers::getitimer(s, a0, a1),
        Sysno::KtimerCreate => timers::ktimer_create(a0, a1, a2),
        Sysno::KtimerDelete => timers::ktimer_delete(a0),
        Sysno::KtimerSettime => timers::ktimer_settime(a0, a1, a2, a3),
        Sysno::KtimerGettime => timers::ktimer_gettime(a0, a1),
        Sysno::KtimerGetoverrun => timers::ktimer_getoverrun(a0),
        Sysno::TimerfdCreate => timers::timerfd_create(s, a0, a1),
        Sysno::TimerfdGettime => timers::timerfd_gettime(s, a0, a1),
        Sysno::TimerfdSettime => timers::timerfd_settime(s, a0, a1, a2, a3),
        Sysno::Specialfd => timers::specialfd(s, a0, a1, a2),
        Sysno::InotifyAddWatchAt => inotify::inotify_add_watch_at(s, a0, a1, a2, a3),
        Sysno::InotifyRmWatch => inotify::inotify_rm_watch(s, a0, a1),
        Sysno::Poll => event::poll(s, a0, a1, a2),
        Sysno::Ppoll => event::ppoll(s, a0, a1, a2, a3),
        Sysno::Select => event::select(s, a0, [a1, a2, a3], a4),
        Sysno::Pselect => event::pselect(s, a0, [a1, a2, a3], a4, a5),
        Sysno::Kqueue => event::kqueue(s),
        Sysno::Kqueuex => event::kqueuex(s, a0),
        Sysno::Kevent => event::kevent(s, a0, a1, a2, a3, a4, a5),
        Sysno::Getrlimit => time::getrlimit(a0, a1),
        Sysno::Setrlimit => time::setrlimit(s, a0, a1),
        Sysno::Sysctl => sysctl::sysctl(s, a0, a1, a2, a3, a4, a5),
        Sysno::Sysctlbyname => sysctl::sysctlbyname(s, a0, a1, a2, a3, a4, a5),
        Sysno::Exit => exit(s, a0),
        Sysno::Read => io::read(s, a0, a1, a2),
        Sysno::Readv => io::readv(s, a0, a1, a2),
        Sysno::Pread => io::pread(s, a0, a1, a2, a3),
        Sysno::Preadv => io::preadv(s, a0, a1, a2, a3),
        Sysno::Write => io::write(s, a0, a1, a2),
        Sysno::Writev => io::writev(s, a0, a1, a2),
        Sysno::Pwrite => io::pwrite(s, a0, a1, a2, a3),
        Sysno::Pwritev => io::pwritev(s, a0, a1, a2, a3),
        Sysno::Lseek => io::lseek(s, a0, a1, a2),
        Sysno::Close => io::close(s, a0),
        Sysno::CloseRange => io::close_range(s, a0, a1, a2),
        Sysno::Dup => io::dup(s, a0),
        Sysno::Dup2 => io::dup2(s, a0, a1),
        Sysno::Fcntl => io::fcntl(s, a0, a1, a2),
        Sysno::Flock => io::flock(s, a0, a1),
        Sysno::Ioctl => io::ioctl(s, a0, a1, a2),
        Sysno::Pipe2 => io::pipe2(s, a0, a1),
        Sysno::Fstat => io::fstat(s, a0, a1),
        Sysno::Fstatfs => io::fstatfs(s, a0, a1),
        Sysno::Getdirentries => io::getdirentries(s, a0, a1, a2, a3),
        Sysno::CopyFileRange => io::copy_file_range(s, a0, a1, a2, a3, a4, a5),
        Sysno::Open => path::open(s, a0, a1, a2),
        Sysno::Openat => path::openat(s, a0, a1, a2, a3),
        Sysno::Fstatat => path::fstatat(s, a0, a1, a2, a3),
        Sysno::Chdir => path::chdir(s, a0),
        Sysno::Fchdir => path::fchdir(s, a0),
        Sysno::Chroot => path::chroot(s, a0),
        Sysno::Fchroot => path::fchroot(s, a0),
        Sysno::Getcwd => path::getcwd(s, a0, a1),
        Sysno::Realpathat => path::realpathat(s, a0, a1, a2, a3, a4),
        Sysno::Access => path::access(s, a0, a1, false),
        Sysno::Eaccess => path::access(s, a0, a1, true),
        Sysno::Faccessat => path::faccessat(s, a0, a1, a2, a3),
        Sysno::Readlink => path::readlink(s, a0, a1, a2),
        Sysno::Readlinkat => path::readlinkat(s, a0, a1, a2, a3),
        Sysno::Statfs => path::statfs(s, a0, a1),
        Sysno::Getfsstat => path::getfsstat(s, a0, a1, a2),
        Sysno::Umask => path::umask(a0),
        Sysno::Pathconf => path::pathconf(s, a0, a1, false),
        Sysno::AclGetFile => acl::get(s, extattr::By::File, a0, a1, a2),
        Sysno::AclSetFile => acl::set(s, extattr::By::File, a0, a1, a2),
        Sysno::AclDeleteFile => acl::delete(s, extattr::By::File, a0, a1),
        Sysno::AclAclcheckFile => acl::check(s, extattr::By::File, a0, a1, a2),
        Sysno::AclGetLink => acl::get(s, extattr::By::Link, a0, a1, a2),
        Sysno::AclSetLink => acl::set(s, extattr::By::Link, a0, a1, a2),
        Sysno::AclDeleteLink => acl::delete(s, extattr::By::Link, a0, a1),
        Sysno::AclAclcheckLink => acl::check(s, extattr::By::Link, a0, a1, a2),
        Sysno::AclGetFd => acl::get(s, extattr::By::Fd, a0, a1, a2),
        Sysno::AclSetFd => acl::set(s, extattr::By::Fd, a0, a1, a2),
        Sysno::AclDeleteFd => acl::delete(s, extattr::By::Fd, a0, a1),
        Sysno::AclAclcheckFd => acl::check(s, extattr::By::Fd, a0, a1, a2),
        Sysno::ExtattrSetFile => extattr::set(s, extattr::By::File, a0, a1, a2, a3, a4),
        Sysno::ExtattrSetLink => extattr::set(s, extattr::By::Link, a0, a1, a2, a3, a4),
        Sysno::ExtattrSetFd => extattr::set(s, extattr::By::Fd, a0, a1, a2, a3, a4),
        Sysno::ExtattrGetFile => extattr::get(s, extattr::By::File, a0, a1, a2, a3, a4),
        Sysno::ExtattrGetLink => extattr::get(s, extattr::By::Link, a0, a1, a2, a3, a4),
        Sysno::ExtattrGetFd => extattr::get(s, extattr::By::Fd, a0, a1, a2, a3, a4),
        Sysno::ExtattrDeleteFile => extattr::delete(s, extattr::By::File, a0, a1, a2),
        Sysno::ExtattrDeleteLink => extattr::delete(s, extattr::By::Link, a0, a1, a2),
        Sysno::ExtattrDeleteFd => extattr::delete(s, extattr::By::Fd, a0, a1, a2),
        Sysno::ExtattrListFile => extattr::list(s, extattr::By::File, a0, a1, a2, a3),
        Sysno::ExtattrListLink => extattr::list(s, extattr::By::Link, a0, a1, a2, a3),
        Sysno::ExtattrListFd => extattr::list(s, extattr::By::Fd, a0, a1, a2, a3),
        Sysno::Lpathconf => path::pathconf(s, a0, a1, true),
        Sysno::Fpathconf => path::fpathconf(s, a0, a1),
        Sysno::Mkdir => fsops::mkdir(s, a0, a1),
        Sysno::Mkdirat => fsops::mkdirat(s, a0, a1, a2),
        Sysno::Mkfifo => fsops::mkfifo(s, a0, a1),
        Sysno::Mkfifoat => fsops::mkfifoat(s, a0, a1, a2),
        Sysno::Mknodat => fsops::mknodat(s, a0, a1, a2, a3),
        Sysno::Symlink => fsops::symlink(s, a0, a1),
        Sysno::Symlinkat => fsops::symlinkat(s, a0, a1, a2),
        Sysno::Unlink => fsops::unlink(s, a0),
        Sysno::Unlinkat => fsops::unlinkat(s, a0, a1, a2),
        Sysno::Funlinkat => fsops::funlinkat(s, a0, a1, a2, a3),
        Sysno::Rmdir => fsops::rmdir(s, a0),
        Sysno::Rename => fsops::rename(s, a0, a1),
        Sysno::Renameat => fsops::renameat(s, a0, a1, a2, a3),
        Sysno::Link => fsops::link(s, a0, a1),
        Sysno::Linkat => fsops::linkat(s, a0, a1, a2, a3, a4),
        Sysno::Chmod => fsops::chmod(s, a0, a1, false),
        Sysno::Lchmod => fsops::chmod(s, a0, a1, true),
        Sysno::Fchmod => fsops::fchmod(s, a0, a1),
        Sysno::Fchmodat => fsops::fchmodat(s, a0, a1, a2, a3),
        Sysno::Chown => fsops::chown(s, a0, a1, a2, false),
        Sysno::Lchown => fsops::chown(s, a0, a1, a2, true),
        Sysno::Fchown => fsops::fchown(s, a0, a1, a2),
        Sysno::Fchownat => fsops::fchownat(s, a0, a1, a2, a3, a4),
        Sysno::Utimensat => fsops::utimensat(s, a0, a1, a2, a3),
        Sysno::Futimens => fsops::futimens(s, a0, a1),
        Sysno::Utimes => fsops::utimes(s, a0, a1, false),
        Sysno::Lutimes => fsops::utimes(s, a0, a1, true),
        Sysno::Futimes => fsops::futimes(s, a0, a1),
        Sysno::Futimesat => fsops::futimesat(s, a0, a1, a2),
        Sysno::Chflags => fsops::chflags(s, a0, a1, false),
        Sysno::Lchflags => fsops::chflags(s, a0, a1, true),
        Sysno::Fchflags => fsops::fchflags(s, a0, a1),
        Sysno::Revoke => fsops::revoke(s, a0),
        Sysno::PosixOpenpt => io::posix_openpt(s, a0),
        Sysno::Chflagsat => fsops::chflagsat(s, a0, a1, a2, a3),
        Sysno::Truncate => fsops::truncate(s, a0, a1),
        Sysno::Ftruncate => fsops::ftruncate(s, a0, a1),
        Sysno::Fsync | Sysno::Fdatasync => fsops::fsync(s, a0),
        Sysno::Sync => fsops::sync(s),
        Sysno::Nmount => fsops::nmount(s, a0, a1, a2),
        Sysno::Unmount => fsops::unmount(s, a0, a1),
        Sysno::Getpid => misc::getpid(),
        Sysno::Issetugid => misc::issetugid(),
        Sysno::Utrace => misc::utrace(s, a0, a1),
        Sysno::Ktrace => crate::ktrace::ktrace(s, a0, a1, a2, a3),
        Sysno::Ptrace => crate::ptrace::ptrace(s, a0, a1, a2, a3),
        Sysno::Kldload => misc::kldload(a0),
        Sysno::Kldnext => misc::kldnext(a0),
        Sysno::Kldfind => misc::kldfind(a0),
        Sysno::Kldstat => misc::kldstat(a0, a1),
        Sysno::Kldfirstmod => misc::kldfirstmod(a0),
        Sysno::Modnext => misc::modnext(a0),
        Sysno::Modfnext => misc::modfnext(a0),
        Sysno::Modstat => misc::modstat(a0, a1),
        Sysno::Modfind => misc::modfind(a0),
        Sysno::Kenv => crate::kenv::kenv(a0, a1, a2, a3),
        Sysno::Reboot => crate::shutdown::reboot(s, a0),
        Sysno::Socket => socket::socket(s, a0, a1, a2),
        Sysno::Socketpair => socket::socketpair(s, a0, a1, a2, a3),
        Sysno::Bind => socket::bind(s, a0, a1, a2),
        Sysno::Bindat => socket::bindat(s, a0, a1, a2, a3),
        Sysno::Connect => socket::connect(s, a0, a1, a2),
        Sysno::Connectat => socket::connectat(s, a0, a1, a2, a3),
        Sysno::Listen => socket::listen(s, a0, a1),
        Sysno::Accept => socket::accept4(s, a0, a1, a2, None),
        Sysno::Accept4 => socket::accept4(s, a0, a1, a2, Some(a3)),
        Sysno::Shutdown => socket::shutdown(s, a0, a1),
        Sysno::Getsockname => socket::getname(s, a0, a1, a2, false),
        Sysno::Getpeername => socket::getname(s, a0, a1, a2, true),
        Sysno::Setsockopt => socket::setsockopt(s, a0, a1, a2, a3, a4),
        Sysno::Getsockopt => socket::getsockopt(s, a0, a1, a2, a3, a4),
        Sysno::Sendto => socket::sendto(s, a0, a1, a2, a3, a4, a5),
        Sysno::Sendmsg => socket::sendmsg(s, a0, a1, a2),
        Sysno::Recvfrom => socket::recvfrom(s, a0, a1, a2, a3, a4, a5),
        Sysno::Recvmsg => socket::recvmsg(s, a0, a1, a2),
        Sysno::Getrandom => misc::getrandom(a0, a1, a2),
        Sysno::Exterrctl => misc::exterrctl(a0, a1, a2),
        Sysno::CpusetGetaffinity => misc::cpuset_getaffinity(s, a0, a1, a2, a3, a4),
        Sysno::Cpuset => misc::cpuset(a0),
        Sysno::CpusetSetid => misc::cpuset_setid(a0, a1, a2),
        Sysno::CpusetGetid => misc::cpuset_getid(a0, a1, a2, a3),
        Sysno::CpusetSetaffinity => misc::cpuset_setaffinity(s, a0, a1, a2, a3, a4),
        Sysno::Sysarch => misc::sysarch(a0, a1),
        Sysno::Mmap => mem::mmap(s, a0, a1, a2, a3, a4, a5),
        Sysno::Munmap => mem::munmap(s, a0, a1),
        Sysno::Mlock => mem::mlock(s, a0, a1),
        Sysno::Munlock => mem::munlock(a0, a1),
        Sysno::Mlockall => mem::mlockall(s, a0),
        Sysno::Munlockall => mem::munlockall(),
        Sysno::Mprotect => mem::mprotect(s, a0, a1, a2),
        Sysno::Minherit => mem::minherit(s, a0, a1, a2),
        Sysno::Madvise => mem::madvise(a0, a1, a2),
        Sysno::Msync => mem::msync(s, a0, a1, a2),
        Sysno::Sigaction => sig::sigaction(a0, a1, a2),
        Sysno::Sigprocmask => sig::sigprocmask(a0, a1, a2),
        Sysno::Sigpending => sig::sigpending(a0),
        Sysno::Sigsuspend => sig::sigsuspend(s, a0),
        Sysno::Sigreturn => sig::sigreturn(frame, a0),
        Sysno::Getcontext => sig::getcontext(frame, a0),
        Sysno::Setcontext => sig::setcontext(frame, a0),
        Sysno::Swapcontext => sig::swapcontext(frame, a0, a1),
        Sysno::Sigaltstack => sig::sigaltstack(frame, a0, a1),
        Sysno::Kill => sig::kill(a0, a1),
        Sysno::ThrKill => sig::thr_kill(a0, a1),
        Sysno::ThrKill2 => sig::thr_kill2(a0, a1, a2),
        Sysno::Sigqueue => sig::sigqueue(a0, a1, a2),
        Sysno::Sigtimedwait => sig::sigtimedwait(s, a0, a1, a2),
        Sysno::Sigwaitinfo => sig::sigwaitinfo(s, a0, a1),
        Sysno::Sigwait => sig::sigwait(s, a0, a1),
        Sysno::Sigfastblock => sig::sigfastblock(a0, a1),
        Sysno::SchedYield => thr::sched_yield(),
        Sysno::SchedGetcpu => Ok(thr::sched_getcpu()),
        Sysno::SchedGetPriorityMax => thr::sched_priority_bound(a0, true),
        Sysno::SchedGetscheduler => thr::sched_getscheduler(a0),
        Sysno::SchedGetparam => thr::sched_getparam(a0, a1),
        Sysno::SchedSetscheduler => thr::sched_setscheduler(a0, a1, a2),
        Sysno::SchedSetparam => thr::sched_setparam(a0, a1),
        Sysno::SchedGetPriorityMin => thr::sched_priority_bound(a0, false),
        Sysno::ThrSelf => thr::thr_self(a0),
        Sysno::ThrWake => thr::thr_wake(a0),
        Sysno::ThrNew => thr::thr_new(s, frame, a0, a1),
        Sysno::ThrCreate => thr::thr_create(frame, a0, a1, a2),
        Sysno::ThrExit => thr::thr_exit(s, a0),
        Sysno::ThrSuspend => thr::thr_suspend(s, a0),
        Sysno::ThrSetName => thr::thr_set_name(a0, a1),
        Sysno::RtprioThread => thr::rtprio_thread(a0, a1, a2),
        Sysno::UmtxOp => umtx::umtx_op(s, a0, a1, a2, a3, a4),
        Sysno::ShmOpen2 => shm::shm_open2(s, a0, a1, a2, a3, a4),
        Sysno::Shmget => ipc::shmget(a0, a1, a2),
        Sysno::Shmat => ipc::shmat(s, a0, a1, a2),
        Sysno::Shmdt => ipc::shmdt(s, a0),
        Sysno::Shmctl => ipc::shmctl(a0, a1, a2),
        Sysno::Semget => ipc::semget(a0, a1, a2),
        Sysno::Semop => ipc::semop(s, a0, a1, a2),
        Sysno::Semctl => ipc::semctl(a0, a1, a2, a3),
        Sysno::Msgget => ipc::msgget(a0, a1),
        Sysno::Msgsnd => ipc::msgsnd(s, a0, a1, a2, a3),
        Sysno::Msgrcv => ipc::msgrcv(s, a0, a1, a2, a3, a4),
        Sysno::Msgctl => ipc::msgctl(a0, a1, a2),
        Sysno::ShmUnlink => shm::shm_unlink(s, a0),
        Sysno::ShmRename => shm::shm_rename(s, a0, a1, a2),
        Sysno::PosixFallocate => shm::posix_fallocate(s, a0, a1, a2),
        Sysno::Fspacectl => shm::fspacectl(s, a0, a1, a2, a3, a4),
        _ => nosys(call.number),
    };
    r.map(Some)
}

/// The file open on `fd` in process `p`, with the rights in `need`
/// (`fget`), for the kernel's own users.
pub(crate) fn file_of(
    s: &Sleepable,
    p: &Process,
    fd: i32,
    need: &abi::capsicum::CapRights,
) -> Result<Arc<crate::fs::file::File>, Errno> {
    p.files.lock(s).get_checked(fd, need)
}

/// `PT_SC_REMOTE`: system call `code` with `args`, made by the current
/// thread (stopped for its debugger) on the debugger's behalf
/// (`ptrace_syscallreq`); its registers are left as they were.
pub(crate) fn remote(s: &Sleepable, code: u32, args: &[u64]) -> abi::ptrace::PtraceScRet {
    let mut ret = abi::ptrace::PtraceScRet::default();
    let (mut number, mut args) = (u64::from(code), args.to_vec());
    if number == Sysno::Syscall as u64 || number == Sysno::SyscallQuad as u64 {
        if args.is_empty() {
            ret.sr_error = Errno::EINVAL.get();
            return ret;
        }
        number = args.remove(0);
    }
    let Some(Slot::Live(_)) = lookup(number) else {
        ret.sr_error = Errno::ENOSYS.get();
        return ret;
    };
    let mut call = Call {
        number,
        args: [0; 8],
        narg: narg(number),
    };
    let n = args.len().min(8);
    call.args[..n].copy_from_slice(&args[..n]);
    let saved = crate::uthread::UserThread::with_current(|t| {
        t.dbg.frame.load(core::sync::atomic::Ordering::Acquire)
    });
    let mut frame = match saved {
        // SAFETY: the current thread's own entry frame, on its stack.
        Some(f) if !f.is_null() => unsafe { *f },
        _ => TrapFrame::default(),
    };
    match run(s, &mut frame, call) {
        Ok(Some(v)) => {
            ret.sr_retval[0] = v;
            ret.sr_retval[1] = second_result(&frame);
        }
        Ok(None) => {}
        Err(e) => ret.sr_error = e.get(),
    }
    ret
}

/// The current thread's process (every thread that makes system calls
/// has one).
fn process() -> Result<Arc<Process>, Errno> {
    Process::current().ok_or(Errno::EINVAL)
}

/// The time on clock `clock`, in nanoseconds (`clock_gettime`'s).
pub(crate) fn clock_ns(clock: u32) -> Result<u64, Errno> {
    time::read(u64::from(clock))
}

/// The current user thread.
fn thread() -> Result<Arc<crate::uthread::UserThread>, Errno> {
    crate::uthread::UserThread::current().ok_or(Errno::EINVAL)
}

/// Runs `f` on the current thread's process, borrowed: no reference
/// count changes, for calls as hot as `stat`, `open` and `close`.
fn with_process<R>(f: impl FnOnce(&Process) -> Result<R, Errno>) -> Result<R, Errno> {
    Process::with_current(f).unwrap_or(Err(Errno::EINVAL))
}

/// An `int` argument: the low 32 bits.
pub(crate) const fn int(a: u64) -> i32 {
    let [b0, b1, b2, b3, ..] = a.to_le_bytes();
    i32::from_le_bytes([b0, b1, b2, b3])
}

/// Numbers that are no system call already reported, one bit each; one
/// more bit for every number outside the table.
static REPORTED: [AtomicU64; SLOT_COUNT.div_ceil(64) + 1] =
    [const { AtomicU64::new(0) }; SLOT_COUNT.div_ceil(64) + 1];

fn nosys(number: u64) -> SysResult {
    if let Some(Slot::Live(n)) = lookup(number) {
        crate::firstuse::report(
            crate::firstuse::Kind::Syscall,
            format_args!("{} ({number})", n.info().name),
        );
    } else {
        let slot = usize::try_from(number)
            .unwrap_or(usize::MAX)
            .min(SLOT_COUNT);
        let bit = 1 << (slot % 64);
        // Relaxed: a duplicate report is harmless.
        if REPORTED[slot / 64].fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            kprintln!("syscall: {number} is not a system call");
        }
    }
    if !matches!(lookup(number), Some(Slot::Live(_)))
        && let Some(t) = crate::uthread::UserThread::current()
    {
        let _ = crate::signal::post_thread(
            &t,
            crate::signal::user_info(abi::signal::SIGSYS, 0, abi::signal::code::SI_KERNEL),
        );
    }
    Err(Errno::ENOSYS)
}

/// `_exit(rval)`: ends the process.
fn exit(s: &Sleepable, rval: u64) -> ! {
    Process::exit_current(s, crate::proc::Exit::Code(int(rval)))
}

/// The `kernel_hooks!` hook for a trap from user mode the VM did not
/// resolve: it becomes a signal. Interrupts masked on entry and exit.
pub(crate) fn user_trap(report: &arch::TrapReport, frame: &mut TrapFrame) {
    hal::irq::enable();
    // SAFETY: as for a system call: the thread came from user mode.
    let s = unsafe { Sleepable::assume() };
    crate::uthread::UserThread::with_current(|t| t.dbg.entered(frame));
    crate::signal::trap(&s, report, frame);
    let _ = hal::irq::disable();
}
