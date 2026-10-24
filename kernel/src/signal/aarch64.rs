// SPDX-License-Identifier: BSD-2-Clause
//! arm64 signal frames and user contexts.
//!
//! The handler's frame is a `struct sigframe` below the interrupted stack
//! or at the top of the alternate stack, under the list of extra register
//! contexts `mc_ptr` points at (here only its end marker: SVE contexts
//! are not written yet, so a handler sees and restores the V registers).
//! The handler gets the signal, the `siginfo`, and the context in `x0`
//! to `x2`, and the trampoline calls it through `x8`.
//!
//! A context coming back from user mode (`sigreturn`, `setcontext`) is
//! checked as FreeBSD checks it: EL0 in AArch64 state with no exception
//! masked, and a well-formed context list.
//!
//! Behavior reference: sys/arm64/arm64/exec_machdep.c.

use core::mem::offset_of;

use abi::errno::Errno;
use abi::signal::StackT;
use abi::ucontext::*;

use super::{Delivery, FpuImage};
use crate::arch::trap::TrapFrame;
use crate::uaccess::{Plain, UserPtr};

// SAFETY: integers only, padding explicit (`abi-check` holds the C layout).
unsafe impl Plain for McontextArm {}
// SAFETY: as above.
unsafe impl Plain for Ucontext<McontextArm> {}
// SAFETY: as above.
unsafe impl Plain for SigframeArm {}

/// `struct arm64_reg_context`'s identifiers.
const CTX_END: u32 = 0xa5a5_a5a5;
const CTX_SVE: u32 = 0x0065_7673;
/// SPSR: the mode bits (EL0t is 0), AArch32 state, and the DAIF masks.
const SPSR_M: u64 = 0xf;
const SPSR_AARCH32: u64 = 0x10;
const SPSR_DAIF: u64 = 0x3c0;
/// The FPSR and FPCR bits that exist (the rest are reserved).
const FPSR_BITS: u32 = 0xf800_009f;
const FPCR_BITS: u32 = 0x07ff_9f07;

/// arm64 has no I/O ports: `KDENABIO` and `KDDISABIO` change nothing.
pub(super) fn set_io_access(_frame: &mut TrapFrame, _on: bool) {}

/// The thread's registers as an `mcontext_t`.
fn mcontext(f: &TrapFrame, fpu: &FpuImage) -> McontextArm {
    let a = crate::arch::fpu::status_offsets();
    let b = fpu.bytes();
    let word = |o: usize| {
        let mut w = [0u8; 8];
        w.copy_from_slice(&b[o..o + 8]);
        u64::from_le_bytes(w)
    };
    let mut fp = Fpregs::default();
    for (n, q) in fp.fp_q.iter_mut().enumerate() {
        let o = crate::arch::fpu::vreg_offset(n);
        *q = [word(o), word(o + 8)];
    }
    fp.fp_sr = u32::try_from(word(a.0) & 0xffff_ffff).unwrap_or(0);
    fp.fp_cr = u32::try_from(word(a.1) & 0xffff_ffff).unwrap_or(0);
    let mut gp = Gpregs::default();
    gp.gp_x.copy_from_slice(&f.x[..30]);
    gp.gp_lr = f.x[30];
    gp.gp_sp = f.sp;
    gp.gp_elr = f.elr;
    gp.gp_spsr = f.spsr;
    McontextArm {
        mc_gpregs: gp,
        mc_fpregs: fp,
        mc_flags: MC_FP_VALID,
        ..McontextArm::default()
    }
}

/// Builds the handler's frame and points `frame` at the trampoline.
pub(super) fn sendsig(frame: &mut TrapFrame, d: &Delivery<'_>) -> Result<(), ()> {
    let fpu = FpuImage::saved().ok_or(())?;
    let (alt, uc_stack) = d.stack(frame.sp);
    let top = alt.unwrap_or(frame.sp);
    // The context list: just its end.
    let end = (top.wrapping_sub(8)) & !7;
    UserPtr::<[u32; 2]>::new(end)
        .write(&[CTX_END, 8])
        .map_err(|_| ())?;
    let fp = end.wrapping_sub(size_of::<SigframeArm>() as u64) & !15;
    let mut mc = mcontext(frame, &fpu);
    mc.mc_ptr = end;
    let mut si = *d.info;
    si.si_signo = d.sig.cast_signed();
    let sf = SigframeArm {
        sf_si: si,
        sf_uc: Ucontext {
            uc_sigmask: d.mask,
            uc_mcontext: mc,
            uc_stack,
            ..Ucontext::default()
        },
    };
    UserPtr::<SigframeArm>::new(fp).write(&sf).map_err(|_| ())?;
    frame.x[0] = u64::from(d.sig);
    frame.x[1] = fp + offset_of!(SigframeArm, sf_si) as u64;
    frame.x[2] = fp + offset_of!(SigframeArm, sf_uc) as u64;
    frame.x[8] = d.action.sa_handler;
    frame.sp = fp;
    frame.elr = d.trampoline;
    Ok(())
}

/// The context `getcontext` saves: the registers as the call returns 0.
pub(crate) fn get_context(
    frame: &TrapFrame,
    mask: abi::signal::Sigset,
    stack: StackT,
) -> Result<Ucontext<McontextArm>, Errno> {
    let fpu = FpuImage::saved().ok_or(Errno::ENOMEM)?;
    let mut mc = mcontext(frame, &fpu);
    // As the call returns: 0, carry clear.
    mc.mc_gpregs.gp_x[0] = 0;
    mc.mc_gpregs.gp_spsr &= !(1 << 29);
    Ok(Ucontext {
        uc_sigmask: mask,
        uc_mcontext: mc,
        uc_stack: stack,
        ..Ucontext::default()
    })
}

/// Checks `mc` and loads it into `frame` and the FPU (`sigreturn`,
/// `setcontext`). On an error nothing has changed.
pub(crate) fn set_context(frame: &mut TrapFrame, mc: &McontextArm) -> Result<(), Errno> {
    let gp = &mc.mc_gpregs;
    if gp.gp_spsr & (SPSR_M | SPSR_AARCH32) != 0 || gp.gp_spsr & SPSR_DAIF != frame.spsr & SPSR_DAIF
    {
        return Err(Errno::EINVAL);
    }
    check_contexts(mc.mc_ptr)?;
    let fpu = if mc.mc_flags & MC_FP_VALID != 0 {
        let mut img = FpuImage::saved().ok_or(Errno::ENOMEM)?;
        let (sr, cr) = crate::arch::fpu::status_offsets();
        let b = img.bytes_mut();
        // Writing the V registers clears what SVE adds to them.
        crate::arch::fpu::clear_sve_extra(b);
        for (n, q) in mc.mc_fpregs.fp_q.iter().enumerate() {
            let o = crate::arch::fpu::vreg_offset(n);
            b[o..o + 8].copy_from_slice(&q[0].to_le_bytes());
            b[o + 8..o + 16].copy_from_slice(&q[1].to_le_bytes());
        }
        b[sr..sr + 8].copy_from_slice(&u64::from(mc.mc_fpregs.fp_sr & FPSR_BITS).to_le_bytes());
        b[cr..cr + 8].copy_from_slice(&u64::from(mc.mc_fpregs.fp_cr & FPCR_BITS).to_le_bytes());
        Some(img)
    } else {
        None
    };
    if let Some(fpu) = fpu {
        // SAFETY: a saved state with register contents changed, and only
        // the defined status and control bits set.
        unsafe { fpu.load() };
    }
    frame.x[..30].copy_from_slice(&gp.gp_x);
    frame.x[30] = gp.gp_lr;
    frame.sp = gp.gp_sp;
    frame.elr = gp.gp_elr;
    frame.spsr = gp.gp_spsr;
    Ok(())
}

/// Walks the extra-context list at `ptr` (0 for none): only SVE contexts
/// (which are skipped) and the end marker are known.
fn check_contexts(mut ptr: u64) -> Result<(), Errno> {
    if ptr == 0 {
        return Ok(());
    }
    for _ in 0..16 {
        let [id, size] = UserPtr::<[u32; 2]>::new(ptr)
            .read()
            .map_err(|_| Errno::EINVAL)?;
        match id {
            CTX_END => return Ok(()),
            CTX_SVE if size >= 8 => ptr = ptr.wrapping_add(u64::from(size)),
            _ => return Err(Errno::EINVAL),
        }
    }
    Err(Errno::EINVAL)
}
