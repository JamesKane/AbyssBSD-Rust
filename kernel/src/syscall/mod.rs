// SPDX-License-Identifier: BSD-2-Clause
//! System calls: decoding the trap frame, dispatch by number, and the
//! result convention (ARCHITECTURE.md §7).
//!
//! amd64 passes the number in `rax` and arguments in `rdi, rsi, rdx, r10,
//! r8, r9`; arm64 the number in `x8` and arguments in `x0..x7`. A result
//! comes back in `rax`/`x0` with the carry flag clear; an error number
//! with it set. `syscall(2)` and `__syscall(2)` take the real number as
//! their first argument.
//!
//! Numbers come from `syscalls.master` through the generated
//! [`abi::syscall`] table. Calls not implemented yet return `ENOSYS`; the
//! first use of each is logged. (FreeBSD also sends `SIGSYS`; that comes
//! with signals.)
//!
//! Behavior reference: sys/kern/subr_syscall.c, sys/amd64/amd64/trap.c,
//! sys/arm64/arm64/trap.c.

mod io;
mod mem;
mod misc;
mod sig;
mod thr;

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
        Ok(v) => {
            f.x[0] = v;
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
    let r = run(&s, call);
    encode(frame, r);
    let _ = hal::irq::disable();
}

fn run(s: &Sleepable, call: Call) -> SysResult {
    let [a0, a1, a2, a3, a4, a5] = call.args;
    let Some(Slot::Live(n)) = lookup(call.number) else {
        return nosys(call.number);
    };
    match n {
        Sysno::Exit => exit(s, a0),
        Sysno::Write => io::write(a0, a1, a2),
        Sysno::Writev => io::writev(a0, a1, a2),
        Sysno::Getpid => misc::getpid(),
        Sysno::Issetugid => misc::issetugid(),
        Sysno::Readlink => misc::readlink(a0, a1, a2),
        Sysno::Getrandom => misc::getrandom(a0, a1, a2),
        Sysno::Exterrctl => misc::exterrctl(a0, a1, a2),
        Sysno::CpusetGetaffinity => misc::cpuset_getaffinity(a0, a1, a2, a3, a4),
        Sysno::Sysarch => misc::sysarch(a0, a1),
        Sysno::Mmap => mem::mmap(s, a0, a1, a2, a3, a4, a5),
        Sysno::Munmap => mem::munmap(s, a0, a1),
        Sysno::Mprotect => mem::mprotect(s, a0, a1, a2),
        Sysno::Minherit => mem::minherit(s, a0, a1, a2),
        Sysno::Madvise => mem::madvise(a0, a1, a2),
        Sysno::Sigaction => sig::sigaction(a0, a1, a2),
        Sysno::Sigprocmask => sig::sigprocmask(a0, a1, a2),
        Sysno::Sigfastblock => sig::sigfastblock(a0, a1),
        Sysno::ThrSelf => thr::thr_self(a0),
        Sysno::ThrWake => thr::thr_wake(a0),
        Sysno::RtprioThread => thr::rtprio_thread(a0, a1, a2),
        Sysno::UmtxOp => thr::umtx_op(s, a0, a1, a2, a3, a4),
        _ => nosys(call.number),
    }
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
    Err(Errno::ENOSYS)
}

/// `_exit(rval)`: ends the process.
fn exit(s: &Sleepable, rval: u64) -> ! {
    let status = int(rval);
    if let Some(p) = Process::current() {
        p.exit(s, status);
    }
    sched::exit()
}

/// The `kernel_hooks!` hook for a trap from user mode the VM did not
/// resolve. Until there are signals (P4.4), the process is killed.
pub(crate) fn user_trap(report: &arch::TrapReport, _frame: &mut TrapFrame) {
    kprintln!("user trap: {}", report.trap);
    hal::irq::enable();
    // SAFETY: as for a system call: the thread came from user mode.
    let s = unsafe { Sleepable::assume() };
    if let Some(p) = Process::current() {
        // FreeBSD's status for a death by signal; SIGSEGV for now.
        p.exit(&s, 11);
    }
    sched::exit()
}
