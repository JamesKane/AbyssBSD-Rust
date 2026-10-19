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
//! and the first use of each is logged.
//!
//! A call a signal interrupted may ask to be re-executed (`ERESTART`):
//! the program counter goes back to the `syscall`/`svc` instruction, whose
//! number and arguments are still in place.
//!
//! Behavior reference: sys/kern/subr_syscall.c, sys/amd64/amd64/trap.c,
//! sys/arm64/arm64/trap.c.

mod io;
mod mem;
mod misc;
mod path;
mod proc;
mod sig;
mod sysctl;
mod thr;
mod time;

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

/// A decoded call.
#[derive(Clone, Copy, Debug)]
struct Call {
    number: u64,
    args: [u64; 6],
}

#[cfg(target_arch = "x86_64")]
fn decode(f: &TrapFrame) -> Call {
    Call {
        number: f.rax,
        args: [f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9],
    }
}

#[cfg(target_arch = "aarch64")]
fn decode(f: &TrapFrame) -> Call {
    Call {
        number: f.x[8],
        args: [f.x[0], f.x[1], f.x[2], f.x[3], f.x[4], f.x[5]],
    }
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

/// The `kernel_hooks!` system-call hook. Entered with interrupts masked;
/// returns with them masked.
pub(crate) fn entry(frame: &mut TrapFrame) {
    hal::irq::enable();
    // SAFETY: a system call runs in its thread, which may sleep; it holds
    // nothing (it came from user mode).
    let s = unsafe { Sleepable::assume() };
    let mut call = decode(frame);
    // `syscall(2)`, `__syscall(2)`: the number is the first argument.
    if call.number == Sysno::Syscall as u64 || call.number == Sysno::SyscallQuad as u64 {
        call.number = call.args[0];
        call.args.copy_within(1.., 0);
        call.args[5] = 0;
    }
    // `None`: the handler set the frame itself (`execve`).
    match run(&s, frame, call) {
        Ok(Some(v)) => encode(frame, Ok(v)),
        Ok(None) => {}
        Err(e) => encode(frame, Err(e)),
    }
    let _ = hal::irq::disable();
}

fn run(s: &Sleepable, frame: &mut TrapFrame, call: Call) -> Result<Option<u64>, Errno> {
    let [a0, a1, a2, a3, a4, a5] = call.args;
    let Some(Slot::Live(n)) = lookup(call.number) else {
        return nosys(call.number).map(Some);
    };
    let r = match n {
        Sysno::Execve => return proc::execve(s, frame, a0, a1, a2),
        Sysno::Fork => proc::fork(s, frame, false),
        Sysno::Vfork => proc::fork(s, frame, true),
        Sysno::Pdfork => proc::pdfork(s, frame, a0, a1),
        Sysno::Pdgetpid => proc::pdgetpid(s, a0, a1),
        Sysno::Pdkill => proc::pdkill(s, a0, a1),
        Sysno::Wait4 => proc::wait4(s, a0, a1, a2, a3),
        Sysno::Getppid => proc::getppid(),
        Sysno::Getpgrp => proc::getpgrp(),
        Sysno::Getpgid => proc::getpgid(a0, false),
        Sysno::Getsid => proc::getpgid(a0, true),
        Sysno::Setsid => proc::setsid(),
        Sysno::Setpgid => proc::setpgid(a0, a1),
        Sysno::Getuid | Sysno::Geteuid | Sysno::Getgid | Sysno::Getegid => proc::root(),
        Sysno::Getgroups => proc::getgroups(a0, a1),
        Sysno::Getresuid | Sysno::Getresgid => proc::getres(a0, a1, a2),
        Sysno::Setuid | Sysno::Seteuid | Sysno::Setgid | Sysno::Setegid => proc::set_id(a0),
        Sysno::ClockGettime => time::clock_gettime(a0, a1),
        Sysno::ClockGetres => time::clock_getres(a0, a1),
        Sysno::Gettimeofday => time::gettimeofday(a0, a1),
        Sysno::Nanosleep => time::nanosleep(s, a0, a1),
        Sysno::Getrlimit => time::getrlimit(a0, a1),
        Sysno::Setrlimit => time::setrlimit(a0, a1),
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
        Sysno::Getcwd => path::getcwd(a0, a1),
        Sysno::Access => path::access(s, a0, a1, false),
        Sysno::Eaccess => path::access(s, a0, a1, true),
        Sysno::Faccessat => path::faccessat(s, a0, a1, a2, a3),
        Sysno::Readlink => path::readlink(s, a0, a1, a2),
        Sysno::Readlinkat => path::readlinkat(s, a0, a1, a2, a3),
        Sysno::Statfs => path::statfs(s, a0, a1),
        Sysno::Getfsstat => path::getfsstat(a0, a1, a2),
        Sysno::Umask => path::umask(a0),
        Sysno::Getpid => misc::getpid(),
        Sysno::Issetugid => misc::issetugid(),
        Sysno::Getrandom => misc::getrandom(a0, a1, a2),
        Sysno::Exterrctl => misc::exterrctl(a0, a1, a2),
        Sysno::CpusetGetaffinity => misc::cpuset_getaffinity(a0, a1, a2, a3, a4),
        Sysno::CpusetSetaffinity => misc::cpuset_setaffinity(s, a0, a1, a2, a3, a4),
        Sysno::Sysarch => misc::sysarch(a0, a1),
        Sysno::Mmap => mem::mmap(s, a0, a1, a2, a3, a4, a5),
        Sysno::Munmap => mem::munmap(s, a0, a1),
        Sysno::Mprotect => mem::mprotect(s, a0, a1, a2),
        Sysno::Minherit => mem::minherit(s, a0, a1, a2),
        Sysno::Madvise => mem::madvise(a0, a1, a2),
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
        Sysno::ThrSelf => thr::thr_self(a0),
        Sysno::ThrWake => thr::thr_wake(a0),
        Sysno::RtprioThread => thr::rtprio_thread(a0, a1, a2),
        Sysno::UmtxOp => thr::umtx_op(s, a0, a1, a2, a3, a4),
        _ => nosys(call.number),
    };
    r.map(Some)
}

/// The current thread's process (every thread that makes system calls
/// has one).
fn process() -> Result<Arc<Process>, Errno> {
    Process::current().ok_or(Errno::EINVAL)
}

/// An `int` argument: the low 32 bits.
const fn int(a: u64) -> i32 {
    let [b0, b1, b2, b3, ..] = a.to_le_bytes();
    i32::from_le_bytes([b0, b1, b2, b3])
}

/// Numbers already reported as unimplemented, one bit each; one more bit
/// for every number outside the table.
static REPORTED: [AtomicU64; SLOT_COUNT.div_ceil(64) + 1] =
    [const { AtomicU64::new(0) }; SLOT_COUNT.div_ceil(64) + 1];

fn nosys(number: u64) -> SysResult {
    let slot = usize::try_from(number)
        .unwrap_or(usize::MAX)
        .min(SLOT_COUNT);
    let bit = 1 << (slot % 64);
    // Relaxed: a duplicate report is harmless.
    if REPORTED[slot / 64].fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        match lookup(number) {
            Some(Slot::Live(n)) => {
                kprintln!("syscall: {} ({number}) not implemented", n.info().name)
            }
            _ => kprintln!("syscall: {number} is not a system call"),
        }
    }
    if !matches!(lookup(number), Some(Slot::Live(_)))
        && let Some(p) = Process::current()
    {
        let _ = crate::signal::post(
            &p,
            crate::signal::user_info(abi::signal::SIGSYS, 0, abi::signal::code::SI_KERNEL),
        );
    }
    Err(Errno::ENOSYS)
}

/// `_exit(rval)`: ends the process.
fn exit(s: &Sleepable, rval: u64) -> ! {
    if let Some(p) = Process::current() {
        p.exit(s, crate::proc::Exit::Code(int(rval)));
    }
    sched::exit()
}

/// The `kernel_hooks!` hook for a trap from user mode the VM did not
/// resolve: it becomes a signal. Interrupts masked on entry and exit.
pub(crate) fn user_trap(report: &arch::TrapReport, frame: &mut TrapFrame) {
    hal::irq::enable();
    // SAFETY: as for a system call: the thread came from user mode.
    let s = unsafe { Sleepable::assume() };
    crate::signal::trap(&s, report, frame);
    let _ = hal::irq::disable();
}
