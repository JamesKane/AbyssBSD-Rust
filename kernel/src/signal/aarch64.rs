// SPDX-License-Identifier: BSD-2-Clause
//! arm64 signal frames and user contexts.
//!
//! The handler's frame is a `struct sigframe` below the interrupted stack
//! or at the top of the alternate stack, under the list of extra register
//! contexts `mc_ptr` points at: with SVE, a `struct sve_context` holding
//! the Z and P registers, FFR, FPSR, and FPCR at the thread's vector
//! length, then the end marker. A context coming back may carry one SVE
//! context, of that size; the V registers in `mc_fpregs` then still set
//! the Z registers' low 128 bits (FreeBSD's `vfp_to_sve_sync`).
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

use super::{Delivery, FpuImage, Prepared};
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
/// `struct arm64_reg_context`, and `struct sve_context` past it.
const CTX_HEADER: u32 = 8;
const SVE_HEADER: usize = 16;

/// Bytes of SVE state at vector length `vl` (FreeBSD's `sve_buf_size`): Z0
/// to Z31, P0 to P15 and FFR, then 16 bytes for FPSR and FPCR (32-bit
/// words, the rest zero).
const fn sve_buf_size(vl: usize) -> usize {
    32 * vl + 17 * vl / 8 + 16
}

/// An SVE context's size with its header (`CTX_SIZE_SVE`), kept to the
/// header's 4-byte alignment.
const fn sve_ctx_size(vl: usize) -> usize {
    (SVE_HEADER + sve_buf_size(vl)).next_multiple_of(4)
}

/// The SVE state in a save area as an SVE context's data: the register
/// file as it lies there, with FPSR and FPCR narrowed to 32 bits.
fn sve_state(fpu: &FpuImage, vl: usize) -> alloc::vec::Vec<u8> {
    let b = fpu.bytes();
    let regs = 32 * vl + 17 * vl / 8;
    let (sr, cr) = crate::arch::fpu::status_offsets();
    let mut out = alloc::vec::Vec::with_capacity(sve_buf_size(vl));
    out.extend_from_slice(&b[..regs]);
    out.extend_from_slice(&b[sr..sr + 4]);
    out.extend_from_slice(&b[cr..cr + 4]);
    out.resize(sve_buf_size(vl), 0);
    out
}

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
    // The context list, built downwards from its end (FreeBSD's
    // `ctx_funcs` order).
    let mut first = (top.wrapping_sub(u64::from(CTX_HEADER))) & !7;
    UserPtr::<[u32; 2]>::new(first)
        .write(&[CTX_END, CTX_HEADER])
        .map_err(|_| ())?;
    let vl = crate::arch::fpu::vector_length();
    if vl != 0 {
        let size = sve_ctx_size(vl);
        first = first.wrapping_sub(size as u64);
        let mut ctx = alloc::vec::Vec::with_capacity(size);
        ctx.extend_from_slice(&CTX_SVE.to_le_bytes());
        ctx.extend_from_slice(&u32::try_from(size).map_err(|_| ())?.to_le_bytes());
        ctx.extend_from_slice(&u16::try_from(vl).map_err(|_| ())?.to_le_bytes());
        // Flags and reserved.
        ctx.extend_from_slice(&[0; 6]);
        ctx.extend_from_slice(&sve_state(&fpu, vl));
        ctx.resize(size, 0);
        crate::uaccess::copy_out(first, &ctx).map_err(|_| ())?;
    }
    let fp = first.wrapping_sub(size_of::<SigframeArm>() as u64) & !15;
    let mut mc = mcontext(frame, &fpu);
    mc.mc_ptr = first;
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
pub(crate) fn prepare_context(frame: &mut TrapFrame, mc: &McontextArm) -> Result<Prepared, Errno> {
    let gp = &mc.mc_gpregs;
    if gp.gp_spsr & (SPSR_M | SPSR_AARCH32) != 0 || gp.gp_spsr & SPSR_DAIF != frame.spsr & SPSR_DAIF
    {
        return Err(Errno::EINVAL);
    }
    let sve = sve_context(mc.mc_ptr)?;
    let fp_valid = mc.mc_flags & MC_FP_VALID != 0;
    let fpu = if fp_valid || sve.is_some() {
        let mut img = FpuImage::saved().ok_or(Errno::ENOMEM)?;
        let (sr, cr) = crate::arch::fpu::status_offsets();
        let b = img.bytes_mut();
        let vl = crate::arch::fpu::vector_length();
        let mut v = [[0u8; 16]; 32];
        for (n, q) in v.iter_mut().enumerate() {
            let o = crate::arch::fpu::vreg_offset(n);
            q.copy_from_slice(&b[o..o + 16]);
        }
        match &sve {
            // The whole register file; FPSR and FPCR come from the V
            // state below, as FreeBSD's sync does.
            Some(state) => {
                let regs = 32 * vl + 17 * vl / 8;
                b[..regs].copy_from_slice(&state[..regs]);
            }
            // Writing the V registers clears what SVE adds to them.
            None => crate::arch::fpu::clear_sve_extra(b),
        }
        if fp_valid {
            for (n, q) in mc.mc_fpregs.fp_q.iter().enumerate() {
                v[n][..8].copy_from_slice(&q[0].to_le_bytes());
                v[n][8..].copy_from_slice(&q[1].to_le_bytes());
            }
            b[sr..sr + 8].copy_from_slice(&u64::from(mc.mc_fpregs.fp_sr & FPSR_BITS).to_le_bytes());
            b[cr..cr + 8].copy_from_slice(&u64::from(mc.mc_fpregs.fp_cr & FPCR_BITS).to_le_bytes());
        }
        for (n, q) in v.iter().enumerate() {
            let o = crate::arch::fpu::vreg_offset(n);
            b[o..o + 16].copy_from_slice(q);
        }
        Some(img)
    } else {
        None
    };
    frame.x[..30].copy_from_slice(&gp.gp_x);
    frame.x[30] = gp.gp_lr;
    frame.sp = gp.gp_sp;
    frame.elr = gp.gp_elr;
    frame.spsr = gp.gp_spsr;
    // A saved state with register contents changed, and only the defined
    // status and control bits set: it loads without faulting.
    Ok(Prepared {
        fpu,
        tls: None,
        tls2: None,
    })
}

/// Walks the extra-context list at `ptr` (0 for none), as FreeBSD's
/// `set_mcontext` does: each 4-byte aligned, at most one SVE context (of
/// this CPU's size, and only where there is SVE), then the end marker.
/// Returns the SVE context's data, if one was given.
fn sve_context(mut ptr: u64) -> Result<Option<alloc::vec::Vec<u8>>, Errno> {
    if ptr == 0 {
        return Ok(None);
    }
    let mut sve = None;
    loop {
        if !ptr.is_multiple_of(4) {
            return Err(Errno::EINVAL);
        }
        let [id, size] = UserPtr::<[u32; 2]>::new(ptr)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        match id {
            CTX_END => return Ok(sve),
            CTX_SVE => {
                let vl = crate::arch::fpu::vector_length();
                if sve.is_some() || vl == 0 || size as usize != sve_ctx_size(vl) {
                    return Err(Errno::EINVAL);
                }
                let mut state = alloc::vec![0u8; sve_buf_size(vl)];
                crate::uaccess::copy_in(&mut state, ptr.wrapping_add(SVE_HEADER as u64))
                    .map_err(|_| Errno::EINVAL)?;
                sve = Some(state);
            }
            _ => return Err(Errno::EINVAL),
        }
        ptr = ptr.wrapping_add(u64::from(size));
    }
}
