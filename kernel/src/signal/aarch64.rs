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

/// Writes an SVE context of the state in `fpu` at `at`: its header, the
/// register file as it lies in the save area, then FPSR and FPCR narrowed
/// to 32 bits and zeros to its size.
fn write_sve(at: u64, fpu: &FpuImage, vl: usize) -> Result<(), ()> {
    let size = sve_ctx_size(vl);
    let mut head = [0u8; SVE_HEADER];
    head[..4].copy_from_slice(&CTX_SVE.to_le_bytes());
    head[4..8].copy_from_slice(&u32::try_from(size).map_err(|_| ())?.to_le_bytes());
    head[8..10].copy_from_slice(&u16::try_from(vl).map_err(|_| ())?.to_le_bytes());
    let b = fpu.bytes();
    let regs = 32 * vl + 17 * vl / 8;
    let (sr, cr) = crate::arch::fpu::status_offsets();
    let mut tail = [0u8; 16];
    tail[..4].copy_from_slice(&b[sr..sr + 4]);
    tail[4..8].copy_from_slice(&b[cr..cr + 4]);
    let data = at.wrapping_add(SVE_HEADER as u64);
    crate::uaccess::copy_out(at, &head).map_err(|_| ())?;
    crate::uaccess::copy_out(data, &b[..regs]).map_err(|_| ())?;
    // The status words, then zeros to the context's end.
    let rest = size - SVE_HEADER - regs;
    let mut off = 0;
    while off < rest {
        let n = (rest - off).min(tail.len());
        let chunk = if off == 0 {
            &tail[..n]
        } else {
            &[0u8; 16][..n]
        };
        crate::uaccess::copy_out(data.wrapping_add((regs + off) as u64), chunk).map_err(|_| ())?;
        off += n;
    }
    Ok(())
}

/// SPSR: the mode bits (EL0t is 0), AArch32 state, and the DAIF masks.
const SPSR_M: u64 = 0xf;
const SPSR_AARCH32: u64 = 0x10;
const SPSR_DAIF: u64 = 0x3c0;
/// The software step bit (`PSR_SS`).
const SPSR_SS: u64 = 1 << 21;
/// The FPSR and FPCR bits that exist (the rest are reserved).
const FPSR_BITS: u32 = 0xf800_009f;
const FPCR_BITS: u32 = 0x07ff_9f07;

/// arm64 has no I/O ports: `KDENABIO` and `KDDISABIO` change nothing.
pub(super) fn set_io_access(_frame: &mut TrapFrame, _on: bool) {}

/// The thread's registers as an `mcontext_t`.
fn mcontext(f: &TrapFrame, fpu: &FpuImage) -> McontextArm {
    // A word at a time (the target is little-endian, and the byte view
    // would be read a byte at a time: `FpuImage::words`). Each V register
    // is a Z register's low two words, `stride` words apart.
    let (sr, cr) = crate::arch::fpu::status_offsets();
    let w = fpu.words();
    let stride = crate::arch::fpu::vreg_offset(1) / 8;
    let mut fp = Fpregs::default();
    for (q, at) in fp.fp_q.iter_mut().zip(w.chunks(stride)) {
        *q = [at[0], at[1]];
    }
    fp.fp_sr = u32::try_from(w[sr / 8] & 0xffff_ffff).unwrap_or(0);
    fp.fp_cr = u32::try_from(w[cr / 8] & 0xffff_ffff).unwrap_or(0);
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
        first = first.wrapping_sub(sve_ctx_size(vl) as u64);
        write_sve(first, &fpu, vl)?;
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
    // A debugger's step waits out the handler (`PCB_SINGLE_STEP` cleared);
    // `sigreturn` brings it back from the saved state.
    frame.spsr &= !SPSR_SS;
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
        // An SVE context alone leaves the V registers and the status as
        // they are (its low 128 bits do not set them, as FreeBSD's sync
        // goes), so the registers are saved first; any other context sets
        // everything, on an image of zeros (nothing to save).
        let restore = sve.is_some() && !fp_valid;
        let mut img = if restore {
            FpuImage::saved()
        } else {
            FpuImage::zeroed()
        }
        .ok_or(Errno::ENOMEM)?;
        let (sr, cr) = crate::arch::fpu::status_offsets();
        let vl = crate::arch::fpu::vector_length();
        let stride = crate::arch::fpu::vreg_offset(1) / 8;
        let mut keep = [[0u64; 2]; 32];
        if restore {
            for (q, at) in keep.iter_mut().zip(img.words().chunks(stride)) {
                *q = [at[0], at[1]];
            }
        }
        // The whole register file, if the context has one; FPSR and FPCR
        // come from the V state below, as FreeBSD's sync does. Without
        // one, what SVE adds to the V registers is clear: the image is
        // zeros.
        if let Some(at) = &sve {
            let regs = 32 * vl + 17 * vl / 8;
            crate::uaccess::copy_in(&mut img.bytes_mut()[..regs], *at)
                .map_err(|_| Errno::EINVAL)?;
        }
        // A word at a time (`FpuImage::words`).
        let w = img.words_mut();
        let v = if fp_valid { &mc.mc_fpregs.fp_q } else { &keep };
        if fp_valid || restore {
            for (q, at) in v.iter().zip(w.chunks_mut(stride)) {
                at[..2].copy_from_slice(q);
            }
        }
        if fp_valid {
            w[sr / 8] = u64::from(mc.mc_fpregs.fp_sr & FPSR_BITS);
            w[cr / 8] = u64::from(mc.mc_fpregs.fp_cr & FPCR_BITS);
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
/// Returns where the SVE context's data is, if one was given.
fn sve_context(mut ptr: u64) -> Result<Option<u64>, Errno> {
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
                sve = Some(ptr.wrapping_add(SVE_HEADER as u64));
            }
            _ => return Err(Errno::EINVAL),
        }
        ptr = ptr.wrapping_add(u64::from(size));
    }
}
