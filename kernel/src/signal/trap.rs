// SPDX-License-Identifier: BSD-2-Clause
//! Faults from user mode become signals (FreeBSD's `trap` and
//! `trapsignal`), with FreeBSD's signal, `si_code`, address, and trap
//! number for each kind of fault.
//!
//! A page fault the VM did not resolve is asked again for why: nothing
//! mapped (`SEGV_MAPERR`), the wrong access (`SEGV_ACCERR`), or past the
//! end of a file (`SIGBUS`).
//!
//! Behavior reference: sys/amd64/amd64/trap.c, sys/arm64/arm64/trap.c,
//! sys/kern/kern_sig.c (`trapsignal`).

use abi::signal::code::*;
use abi::signal::*;
use sched::Sleepable;
use vm::fault::{Access, FaultError};

#[cfg(target_arch = "x86_64")]
use super::FpuImage;
use super::{handle, post_thread};
use crate::arch::trap::TrapFrame;
use crate::proc::Process;

/// A fault, as a signal.
struct Fault {
    sig: u32,
    code: i32,
    addr: u64,
    trapno: i32,
}

impl Fault {
    const fn new(sig: u32, code: i32, addr: u64, trapno: i32) -> Self {
        Self {
            sig,
            code,
            addr,
            trapno,
        }
    }
}

/// Why the VM refused `addr` for `access`, as a signal (`None`: it
/// resolves now, a race with another thread).
fn page_fault(s: &Sleepable, p: &Process, addr: u64, access: Access, trapno: i32) -> Option<Fault> {
    let va = addr::VirtAddr::new(addr);
    let (sig, code) = match p.space().fault(s, va, access) {
        Ok(()) => return None,
        Err(FaultError::NotMapped) => (SIGSEGV, SEGV_MAPERR),
        Err(FaultError::Protection) => (SIGSEGV, SEGV_ACCERR),
        Err(FaultError::PastEnd) => (SIGBUS, BUS_OBJERR),
        Err(FaultError::OutOfMemory) => (SIGBUS, BUS_OOMERR),
    };
    Some(Fault::new(sig, code, addr, trapno))
}

/// A floating-point exception's code from the flags it raised and the
/// ones that were unmasked (FreeBSD's `fpetable` order of preference).
fn fpe_code(raised: u32) -> i32 {
    // Invalid, divide by zero, denormal, overflow, underflow, inexact.
    for (bit, code) in [
        (0, FPE_FLTINV),
        (2, FPE_FLTDIV),
        (1, FPE_FLTIDO),
        (3, FPE_FLTOVF),
        (4, FPE_FLTUND),
        (5, FPE_FLTRES),
    ] {
        if raised & (1 << bit) != 0 {
            return code;
        }
    }
    0
}

#[cfg(target_arch = "x86_64")]
fn classify(s: &Sleepable, p: &Process, report: &crate::arch::TrapReport) -> Option<Fault> {
    use crate::arch::trap::Trap;
    let f = &report.frame;
    let pc = f.rip;
    // FreeBSD's `T_` trap numbers.
    Some(match report.trap {
        Trap::DivideError => Fault::new(SIGFPE, FPE_INTDIV, pc, 18),
        Trap::Debug => Fault::new(SIGTRAP, TRAP_TRACE, pc, 10),
        Trap::Breakpoint => Fault::new(SIGTRAP, TRAP_BRKPT, pc, 3),
        Trap::Overflow => Fault::new(SIGFPE, FPE_INTOVF, pc, 20),
        Trap::BoundRange => Fault::new(SIGFPE, FPE_FLTSUB, pc, 21),
        Trap::InvalidOpcode => Fault::new(SIGILL, ILL_PRVOPC, pc, 1),
        Trap::DeviceNotAvailable => Fault::new(SIGILL, ILL_COPROC, pc, 22),
        Trap::InvalidTss { .. } => Fault::new(SIGBUS, BUS_OBJERR, pc, 25),
        Trap::SegmentNotPresent { .. } => Fault::new(SIGBUS, BUS_ADRERR, pc, 26),
        Trap::StackFault { .. } => Fault::new(SIGBUS, BUS_ADRERR, pc, 27),
        Trap::GeneralProtection { .. } => Fault::new(SIGBUS, BUS_OBJERR, pc, 9),
        Trap::AlignmentCheck => Fault::new(SIGBUS, BUS_ADRALN, pc, 14),
        Trap::PageFault { address, cause } => {
            let access = if cause.fetch {
                Access::Execute
            } else if cause.write {
                Access::Write
            } else {
                Access::Read
            };
            return page_fault(s, p, address, access, 12);
        }
        Trap::X87 => {
            let fpu = FpuImage::saved()?;
            let b = fpu.bytes();
            let (cw, sw) = (
                u16::from_le_bytes([b[0], b[1]]),
                u16::from_le_bytes([b[2], b[3]]),
            );
            match fpe_code(u32::from(sw & !cw & 0x3f)) {
                0 => return None,
                code => Fault::new(SIGFPE, code, pc, 6),
            }
        }
        Trap::Simd => {
            let fpu = FpuImage::saved()?;
            let b = fpu.bytes();
            let mxcsr = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
            match fpe_code(mxcsr & !(mxcsr >> 7) & 0x3f) {
                0 => return None,
                code => Fault::new(SIGFPE, code, pc, 29),
            }
        }
        _ => Fault::new(SIGILL, ILL_ILLTRP, pc, 0),
    })
}

/// Whether the trap was an `mrs` of the ID registers, now emulated
/// (FreeBSD's `undef_insn` and `undef_sys` handlers).
#[cfg(target_arch = "aarch64")]
fn emulated(report: &crate::arch::TrapReport, frame: &mut TrapFrame) -> bool {
    use crate::arch::trap::Trap;
    let read = match report.trap {
        Trap::Undefined => crate::uaccess::UserPtr::<u32>::new(frame.elr)
            .read()
            .ok()
            .and_then(idregs::MrsRead::from_insn),
        Trap::Other(0x18) => idregs::MrsRead::from_iss(frame.esr & 0x01ff_ffff),
        _ => None,
    };
    crate::idregs::emulate_mrs(frame, read)
}

#[cfg(target_arch = "aarch64")]
fn classify(s: &Sleepable, p: &Process, report: &crate::arch::TrapReport) -> Option<Fault> {
    use crate::arch::trap::{FaultStatus, Trap};
    let f = &report.frame;
    let pc = f.elr;
    // The exception class is the trap number.
    let ec = i32::try_from(f.esr >> 26 & 0x3f).unwrap_or(0);
    let abort = |address: u64, access: Access, status: FaultStatus| match status {
        FaultStatus::Translation(_) | FaultStatus::Permission(_) | FaultStatus::AccessFlag(_) => {
            page_fault(s, p, address, access, ec)
        }
        FaultStatus::Alignment => Some(Fault::new(SIGBUS, BUS_ADRALN, address, ec)),
        FaultStatus::Other(_) => Some(Fault::new(SIGBUS, BUS_OBJERR, address, ec)),
    };
    match report.trap {
        Trap::DataAbort {
            address,
            write,
            status,
            ..
        } => abort(
            address,
            if write { Access::Write } else { Access::Read },
            status,
        ),
        Trap::InstructionAbort {
            address, status, ..
        } => abort(address, Access::Execute, status),
        Trap::Breakpoint(_) => Some(Fault::new(SIGTRAP, TRAP_BRKPT, pc, ec)),
        Trap::PcAlignment => Some(Fault::new(SIGBUS, BUS_ADRALN, pc, ec)),
        Trap::SpAlignment => Some(Fault::new(SIGBUS, BUS_ADRALN, f.sp, ec)),
        // FreeBSD reports FAR_EL1 for an undefined instruction, and a
        // trapped system register access as privileged.
        Trap::Undefined => Some(Fault::new(SIGILL, ILL_ILLTRP, f.far, ec)),
        Trap::Other(0x18) => Some(Fault::new(SIGILL, ILL_PRVOPC, pc, ec)),
        Trap::IllegalState => Some(Fault::new(SIGILL, ILL_ILLOPC, pc, ec)),
        Trap::SError => Some(Fault::new(SIGBUS, BUS_OBJERR, pc, ec)),
        // A trapped floating-point exception: the ISS says which.
        Trap::Other(0x2c) => {
            let iss = u32::try_from(f.esr & 0x9f).unwrap_or(0);
            // IOF, DZF, OFF, UFF, IXF, IDF: reorder to x86's bit order.
            let raised = (iss & 1)
                | (iss >> 1 & 1) << 2
                | (iss >> 2 & 1) << 3
                | (iss >> 3 & 1) << 4
                | (iss >> 4 & 1) << 5
                | (iss >> 7 & 1) << 1;
            Some(Fault::new(SIGFPE, fpe_code(raised), pc, ec))
        }
        _ => Some(Fault::new(SIGILL, ILL_ILLTRP, pc, ec)),
    }
}

/// A fault from user mode the VM did not resolve (the `user_trap` hook,
/// FreeBSD's `trapsignal`): its signal is delivered at once if the
/// process catches it and does not block it (the `sigfastblock` word
/// notwithstanding). Otherwise, if it is blocked or ignored, it goes back
/// to its default action so the process cannot fault forever, and is
/// posted.
pub(crate) fn trap(s: &Sleepable, report: &crate::arch::TrapReport, frame: &mut TrapFrame) {
    let Some(t) = crate::uthread::UserThread::current() else {
        return;
    };
    let p = alloc::sync::Arc::clone(t.process());
    #[cfg(target_arch = "aarch64")]
    if emulated(report, frame) {
        return;
    }
    let Some(fault) = classify(s, &p, report) else {
        return;
    };
    let info = Siginfo {
        si_signo: fault.sig.cast_signed(),
        si_code: fault.code,
        si_addr: fault.addr,
        si_trapno: fault.trapno,
        ..Siginfo::default()
    };
    let action = p.signals.lock().action(fault.sig);
    let blocked = t.sig.lock().mask.has(fault.sig);
    if action.sa_handler > SIG_IGN && !blocked {
        if !handle(s, &t, frame, fault.sig, &info, &action) {
            drop(t);
            drop(p);
            super::kill_self(s, SIGILL);
        }
        return;
    }
    if blocked || action.sa_handler == SIG_IGN {
        t.sig.lock().mask.remove(fault.sig);
        super::set_action(&p, fault.sig, Sigaction::default());
    }
    let _ = post_thread(&t, info);
}
