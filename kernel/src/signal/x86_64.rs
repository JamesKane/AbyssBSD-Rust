// SPDX-License-Identifier: BSD-2-Clause
//! amd64 signal frames and user contexts.
//!
//! The handler's frame is a `struct sigframe` below the interrupted stack
//! (past its 128-byte red zone) or at the top of the alternate stack. The
//! FPU state's legacy FXSAVE part sits in `mc_fpstate`; the rest of the
//! XSAVE area (its header and the AVX and later components) is copied out
//! beside the frame, 64-byte aligned, where `mc_xfpustate` points. The
//! handler starts with the FPU in its initial state. Its arguments are the
//! signal, the `siginfo` (or the code, without `SA_SIGINFO`), the context,
//! and the fault address, in `rdi`, `rsi`, `rdx`, and `rcx`.
//!
//! A context coming back from user mode (`sigreturn`, `setcontext`) is
//! checked as FreeBSD checks it: only the flags user code may change,
//! only the user code and stack selectors, a known FPU format, and an
//! XSAVE header the CPU accepts (a bad one would fault in the kernel).
//!
//! Behavior reference: sys/amd64/amd64/exec_machdep.c,
//! sys/amd64/amd64/fpu.c.

use core::mem::offset_of;

use abi::errno::Errno;
use abi::signal::{SA_SIGINFO, StackT};
use abi::ucontext::*;

use super::{Delivery, FpuImage, Prepared};
use crate::arch::trap::TrapFrame;
use crate::uaccess::{Plain, UserPtr, copy_in, copy_out};

// SAFETY: integers only, padding explicit (`abi-check` holds the C layout).
unsafe impl Plain for McontextX86 {}
// SAFETY: as above.
unsafe impl Plain for Ucontext<McontextX86> {}
// SAFETY: as above.
unsafe impl Plain for SigframeX86 {}

/// The System V red zone below the stack pointer, which a frame skips.
const REDZONE: u64 = 128;
/// The legacy (FXSAVE) part of a save area.
const LEGACY: usize = 512;
/// The XSAVE header that follows it.
const HEADER: usize = 64;
/// RFLAGS bits user code may change (FreeBSD's `PSL_USERCHANGE`): the
/// arithmetic flags, TF, DF, OF, NT, RF, AC, and ID. RF matters: the CPU
/// sets it in the flags it saves for a fault, so a context from a signal
/// delivered on the way out of a page fault carries it.
const RFLAGS_USER: u64 = 0x1
    | 0x4
    | 0x10
    | 0x40
    | 0x80
    | 0x100
    | 0x400
    | 0x800
    | 0x4000
    | 0x1_0000
    | 0x4_0000
    | 0x20_0000;
const RFLAGS_CF: u64 = 0x1;
const RFLAGS_TF: u64 = 0x100;
const RFLAGS_DF: u64 = 0x400;
/// FreeBSD's `T_` numbers for `mc_trapno`: a system call.
const T_SYSCALL: u32 = 34;

fn user_cs() -> u64 {
    u64::from(crate::arch::user::USER_CS)
}

fn user_ds() -> u64 {
    u64::from(crate::arch::user::USER_DS)
}

/// Whether the FPU saves with XSAVE (a header after the legacy area).
fn xsave() -> bool {
    crate::arch::fpu::describe().1 != 0
}

/// The thread's registers as an `mcontext_t`, with the FPU's legacy part.
fn mcontext(f: &TrapFrame, fpu: &FpuImage) -> McontextX86 {
    let mut fpstate = FpState::default();
    fpstate.0.copy_from_slice(&fpu.bytes()[..LEGACY]);
    let ds = crate::arch::user::USER_DS;
    McontextX86 {
        mc_rdi: f.rdi,
        mc_rsi: f.rsi,
        mc_rdx: f.rdx,
        mc_rcx: f.rcx,
        mc_r8: f.r8,
        mc_r9: f.r9,
        mc_rax: f.rax,
        mc_rbx: f.rbx,
        mc_rbp: f.rbp,
        mc_r10: f.r10,
        mc_r11: f.r11,
        mc_r12: f.r12,
        mc_r13: f.r13,
        mc_r14: f.r14,
        mc_r15: f.r15,
        mc_trapno: if f.vector == crate::arch::trap::SYSCALL_VECTOR {
            T_SYSCALL
        } else {
            0
        },
        mc_fs: ds,
        mc_gs: ds,
        mc_es: ds,
        mc_ds: ds,
        mc_flags: MC_HASSEGS,
        mc_err: f.error,
        mc_rip: f.rip,
        mc_cs: f.cs,
        mc_rflags: f.rflags,
        mc_rsp: f.rsp,
        mc_ss: f.ss,
        mc_len: size_of::<McontextX86>() as i64,
        mc_fpformat: MC_FPFMT_XMM,
        mc_ownedfp: MC_FPOWNED_FPU,
        mc_fpstate: fpstate,
        mc_fsbase: hal::user::tls(),
        mc_gsbase: hal::user::tls2(),
        ..McontextX86::default()
    }
}

/// Builds the handler's frame and points `frame` at the trampoline.
pub(super) fn sendsig(frame: &mut TrapFrame, d: &Delivery<'_>) -> Result<(), ()> {
    let fpu = FpuImage::saved().ok_or(())?;
    let (alt, uc_stack) = d.stack(frame.rsp);
    let mut mc = mcontext(frame, &fpu);
    mc.mc_onstack = u64::from(uc_stack.ss_flags == abi::signal::SS_ONSTACK);
    let mut sp = alt.unwrap_or(frame.rsp.wrapping_sub(REDZONE));
    let xstate = if xsave() {
        &fpu.bytes()[LEGACY..]
    } else {
        &[][..]
    };
    if !xstate.is_empty() {
        sp = sp.wrapping_sub(xstate.len() as u64) & !63;
        mc.mc_xfpustate = sp;
        mc.mc_xfpustate_len = xstate.len() as u64;
        mc.mc_flags |= MC_HASFPXSTATE;
    }
    let sfp = sp.wrapping_sub(size_of::<SigframeX86>() as u64) & !15;
    let mut sf = SigframeX86 {
        sf_ahu: d.action.sa_handler,
        sf_uc: Ucontext {
            uc_sigmask: d.mask,
            uc_mcontext: mc,
            uc_stack,
            ..Ucontext::default()
        },
        ..SigframeX86::default()
    };
    frame.rdi = u64::from(d.sig);
    frame.rdx = sfp + offset_of!(SigframeX86, sf_uc) as u64;
    frame.rcx = d.info.si_addr;
    if d.action.sa_flags & SA_SIGINFO != 0 {
        sf.sf_si = *d.info;
        sf.sf_si.si_signo = d.sig.cast_signed();
        frame.rsi = sfp + offset_of!(SigframeX86, sf_si) as u64;
    } else {
        // FreeBSD's old-style second argument: the code, sign-extended.
        frame.rsi = i64::from(d.info.si_code).cast_unsigned();
    }
    UserPtr::<SigframeX86>::new(sfp)
        .write(&sf)
        .map_err(|_| ())?;
    if !xstate.is_empty() {
        copy_out(mc.mc_xfpustate, xstate).map_err(|_| ())?;
    }
    // The handler starts with a clean FPU (FreeBSD's `fpstate_drop`).
    let init = FpuImage::initial().ok_or(())?;
    // SAFETY: the initial state.
    unsafe { init.load() };
    frame.rsp = sfp;
    frame.rip = d.trampoline;
    frame.rflags &= !(RFLAGS_TF | RFLAGS_DF);
    frame.cs = user_cs();
    frame.ss = user_ds();
    Ok(())
}

/// The context `getcontext` saves: the registers as the call returns 0.
pub(crate) fn get_context(
    frame: &TrapFrame,
    mask: abi::signal::Sigset,
    stack: StackT,
) -> Result<Ucontext<McontextX86>, Errno> {
    let fpu = FpuImage::saved().ok_or(Errno::ENOMEM)?;
    let mut mc = mcontext(frame, &fpu);
    mc.mc_rax = 0;
    mc.mc_rdx = 0;
    mc.mc_rflags &= !RFLAGS_CF;
    mc.mc_flags |= MC_HASBASES;
    Ok(Ucontext {
        uc_sigmask: mask,
        uc_mcontext: mc,
        uc_stack: stack,
        ..Ucontext::default()
    })
}

/// `PSL_IOPL`: I/O privilege level 3, the ports open to user mode.
const RFLAGS_IOPL: u64 = 0x3000;

/// Sets or clears IOPL 3 in `frame` (`KDENABIO`, `KDDISABIO`).
pub(super) fn set_io_access(frame: &mut TrapFrame, on: bool) {
    if on {
        frame.rflags |= RFLAGS_IOPL;
    } else {
        frame.rflags &= !RFLAGS_IOPL;
    }
}

/// Checks `mc` and puts its registers in `frame`; the FP state and FS
/// base it carries are for the thread to load (`sigreturn`, `setcontext`,
/// `thr_create`). On an error nothing has changed.
pub(crate) fn prepare_context(frame: &mut TrapFrame, mc: &McontextX86) -> Result<Prepared, Errno> {
    if mc.mc_len != size_of::<McontextX86>() as i64 || mc.mc_flags & !MC_FLAG_MASK != 0 {
        return Err(Errno::EINVAL);
    }
    // FreeBSD tells the console why it refuses a context; so do we.
    let pid = || crate::proc::Process::current().map_or(0, |p| p.pid());
    if (mc.mc_rflags ^ frame.rflags) & !RFLAGS_USER != 0 {
        kprintln!("pid {}: sigreturn rflags = {:#x}", pid(), mc.mc_rflags);
        return Err(Errno::EINVAL);
    }
    if mc.mc_cs != user_cs() || mc.mc_ss != user_ds() {
        kprintln!("pid {}: sigreturn cs = {:#x}", pid(), mc.mc_cs);
        return Err(Errno::EINVAL);
    }
    if mc.mc_flags & MC_HASBASES != 0 && !(canonical(mc.mc_fsbase) && canonical(mc.mc_gsbase)) {
        return Err(Errno::EINVAL);
    }
    let fpu = fpu_image(mc).inspect_err(|e| {
        kprintln!("pid {}: sigreturn set_fpcontext err {}", pid(), e.get());
    })?;
    // Checked: from here on nothing fails.
    frame.rdi = mc.mc_rdi;
    frame.rsi = mc.mc_rsi;
    frame.rdx = mc.mc_rdx;
    frame.rcx = mc.mc_rcx;
    frame.r8 = mc.mc_r8;
    frame.r9 = mc.mc_r9;
    frame.rax = mc.mc_rax;
    frame.rbx = mc.mc_rbx;
    frame.rbp = mc.mc_rbp;
    frame.r10 = mc.mc_r10;
    frame.r11 = mc.mc_r11;
    frame.r12 = mc.mc_r12;
    frame.r13 = mc.mc_r13;
    frame.r14 = mc.mc_r14;
    frame.r15 = mc.mc_r15;
    frame.rip = mc.mc_rip;
    frame.rsp = mc.mc_rsp;
    frame.rflags = mc.mc_rflags;
    // `fpu_image` checked everything XRSTOR checks; the base is canonical.
    let bases = mc.mc_flags & MC_HASBASES != 0;
    Ok(Prepared {
        fpu,
        tls: bases.then_some(mc.mc_fsbase),
        tls2: bases.then_some(mc.mc_gsbase),
    })
}

fn canonical(a: u64) -> bool {
    a < crate::arch::user::USER_END
}

/// The FPU state `mc` describes, checked so the CPU loads it without
/// faulting; `None` to leave the FPU as it is (no FPU state).
fn fpu_image(mc: &McontextX86) -> Result<Option<FpuImage>, Errno> {
    match mc.mc_fpformat {
        MC_FPFMT_XMM => {}
        // No FPU state in the context.
        0x10000 => return Ok(None),
        _ => return Err(Errno::EINVAL),
    }
    let mut img = FpuImage::saved().ok_or(Errno::ENOMEM)?;
    let mask = mxcsr_mask(&img);
    img.reset();
    if mc.mc_ownedfp == MC_FPOWNED_NONE {
        return Ok(Some(img));
    }
    let bytes = img.bytes_mut();
    bytes[..LEGACY].copy_from_slice(&mc.mc_fpstate.0);
    // MXCSR's reserved bits fault on load.
    let mxcsr = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) & mask;
    bytes[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    if xsave() {
        let xcr0 = crate::arch::fpu::describe().1;
        let ext = bytes.len() - LEGACY;
        let mut bv = xcr0 & 0x3;
        if mc.mc_flags & MC_HASFPXSTATE != 0 {
            let len = usize::try_from(mc.mc_xfpustate_len).map_err(|_| Errno::EINVAL)?;
            if len < HEADER || len > ext {
                return Err(Errno::EINVAL);
            }
            copy_in(&mut bytes[LEGACY..LEGACY + len], mc.mc_xfpustate)
                .map_err(|_| Errno::EINVAL)?;
            let word = |i: usize| {
                let mut w = [0u8; 8];
                w.copy_from_slice(&bytes[LEGACY + i..LEGACY + i + 8]);
                u64::from_le_bytes(w)
            };
            bv = word(0);
            // Components the CPU is not set up for, or the compacted
            // format, fault on load.
            if bv & !xcr0 != 0 || word(8) != 0 {
                return Err(Errno::EINVAL);
            }
        }
        // XSTATE_BV, then a zero XCOMP_BV and reserved bytes.
        bytes[LEGACY..LEGACY + 8].copy_from_slice(&bv.to_le_bytes());
        bytes[LEGACY + 8..LEGACY + HEADER].fill(0);
    }
    Ok(Some(img))
}

/// The MXCSR bits the CPU accepts (FXSAVE's `MXCSR_MASK`; 0 means the
/// architectural default).
fn mxcsr_mask(img: &FpuImage) -> u32 {
    let b = img.bytes();
    match u32::from_le_bytes([b[28], b[29], b[30], b[31]]) {
        0 => 0xffbf,
        m => m,
    }
}
