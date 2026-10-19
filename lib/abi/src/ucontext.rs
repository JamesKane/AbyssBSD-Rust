// SPDX-License-Identifier: BSD-2-Clause
//! Saved user contexts: `ucontext_t` with each architecture's
//! `mcontext_t`, and the signal frame the kernel builds on the user's
//! stack (`struct sigframe`).
//!
//! Both architectures' layouts are defined everywhere, so `abi-check` can
//! check each; [`Mcontext`], [`Ucontext`]'s target instance, and
//! [`Sigframe`] name the target's. Padding the C compiler would insert is
//! an explicit field, so a copy to user space carries no stray bytes.
//!
//! Behavior reference: sys/sys/_ucontext.h, sys/x86/include/ucontext.h,
//! sys/arm64/include/ucontext.h, sys/x86/include/sigframe.h,
//! sys/arm64/include/frame.h.

use crate::layout::{Arches, ConstValue};
use crate::signal::{Siginfo, Sigset, StackT};

/// amd64's `mc_fpstate`: the FXSAVE image (legacy x87 and SSE state).
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FpState(pub [u8; 512]);

impl Default for FpState {
    fn default() -> Self {
        Self([0; 512])
    }
}

/// amd64's `mcontext_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McontextX86 {
    pub mc_onstack: u64,
    pub mc_rdi: u64,
    pub mc_rsi: u64,
    pub mc_rdx: u64,
    pub mc_rcx: u64,
    pub mc_r8: u64,
    pub mc_r9: u64,
    pub mc_rax: u64,
    pub mc_rbx: u64,
    pub mc_rbp: u64,
    pub mc_r10: u64,
    pub mc_r11: u64,
    pub mc_r12: u64,
    pub mc_r13: u64,
    pub mc_r14: u64,
    pub mc_r15: u64,
    pub mc_trapno: u32,
    pub mc_fs: u16,
    pub mc_gs: u16,
    pub mc_addr: u64,
    pub mc_flags: u32,
    pub mc_es: u16,
    pub mc_ds: u16,
    pub mc_err: u64,
    pub mc_rip: u64,
    pub mc_cs: u64,
    pub mc_rflags: u64,
    pub mc_rsp: u64,
    pub mc_ss: u64,
    /// `sizeof(mcontext_t)`, as a check.
    pub mc_len: i64,
    pub mc_fpformat: i64,
    pub mc_ownedfp: i64,
    pub mc_fpstate: FpState,
    pub mc_fsbase: u64,
    pub mc_gsbase: u64,
    /// Where the XSAVE state past the legacy area is, and its length.
    pub mc_xfpustate: u64,
    pub mc_xfpustate_len: u64,
    pub mc_tlsbase: u64,
    pub mc_spare: [i64; 3],
}

/// arm64's `struct gpregs`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gpregs {
    pub gp_x: [u64; 30],
    pub gp_lr: u64,
    pub gp_sp: u64,
    pub gp_elr: u64,
    pub gp_spsr: u64,
}

/// arm64's `struct fpregs`: the 128-bit V registers, FPSR, FPCR.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fpregs {
    /// Each register as its low and high halves.
    pub fp_q: [[u64; 2]; 32],
    pub fp_sr: u32,
    pub fp_cr: u32,
    pub fp_flags: i32,
    pub fp_pad: i32,
}

/// arm64's `mcontext_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McontextArm {
    pub mc_gpregs: Gpregs,
    pub mc_fpregs: Fpregs,
    pub mc_flags: i32,
    pub mc_pad: i32,
    /// Extra register contexts (SVE), or 0.
    pub mc_ptr: u64,
    /// (16-CURRENT puts `mc_esr` in the first spare.)
    pub mc_spare: [u64; 7],
    /// The C struct's tail padding.
    pub _pad: [u64; 1],
}

/// `ucontext_t` around an architecture's `mcontext_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ucontext<M> {
    pub uc_sigmask: Sigset,
    pub uc_mcontext: M,
    pub uc_link: u64,
    pub uc_stack: StackT,
    pub uc_flags: i32,
    pub uc_spare: [i32; 4],
    /// The C struct's tail padding.
    pub _pad: [u32; 3],
}

/// amd64's `struct sigframe`: the handler, then the context, then the
/// signal's information.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigframeX86 {
    /// The handler the trampoline calls.
    pub sf_ahu: u64,
    /// Padding to `sf_uc`'s 16-byte alignment.
    pub _pad: u64,
    pub sf_uc: Ucontext<McontextX86>,
    pub sf_si: Siginfo,
}

/// arm64's `struct sigframe`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigframeArm {
    pub sf_si: Siginfo,
    pub sf_uc: Ucontext<McontextArm>,
}

/// The target's `mcontext_t`.
#[cfg(target_arch = "x86_64")]
pub type Mcontext = McontextX86;
/// The target's `mcontext_t`.
#[cfg(target_arch = "aarch64")]
pub type Mcontext = McontextArm;
/// The target's `struct sigframe`.
#[cfg(target_arch = "x86_64")]
pub type Sigframe = SigframeX86;
/// The target's `struct sigframe`.
#[cfg(target_arch = "aarch64")]
pub type Sigframe = SigframeArm;

/// amd64 `mc_flags`: segment registers, FS/GS bases, extended FPU state,
/// and the TLS base are valid.
pub const MC_HASSEGS: u32 = 0x1;
pub const MC_HASBASES: u32 = 0x2;
pub const MC_HASFPXSTATE: u32 = 0x4;
pub const MC_HASTLSBASE: u32 = 0x8;
pub const MC_FLAG_MASK: u32 = 0xf;
/// amd64 `mc_fpformat`: the XMM (FXSAVE) format.
pub const MC_FPFMT_XMM: i64 = 0x10002;
/// amd64 `mc_ownedfp`.
pub const MC_FPOWNED_NONE: i64 = 0x20000;
pub const MC_FPOWNED_FPU: i64 = 0x20001;
pub const MC_FPOWNED_PCB: i64 = 0x20002;
/// arm64 `mc_flags`: `mc_fpregs` is valid.
pub const MC_FP_VALID: i32 = 0x1;
/// `uc_flags`: set by `swapcontext(3)`.
pub const UCF_SWAPPED: i32 = 0x1;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("_MC_HASSEGS", MC_HASSEGS as i64, Arches::Amd64),
    c("_MC_HASBASES", MC_HASBASES as i64, Arches::Amd64),
    c("_MC_HASFPXSTATE", MC_HASFPXSTATE as i64, Arches::Amd64),
    c("_MC_HASTLSBASE", MC_HASTLSBASE as i64, Arches::Amd64),
    c("_MC_FLAG_MASK", MC_FLAG_MASK as i64, Arches::Amd64),
    c("_MC_FPFMT_XMM", MC_FPFMT_XMM, Arches::Amd64),
    c("_MC_FPOWNED_NONE", MC_FPOWNED_NONE, Arches::Amd64),
    c("_MC_FPOWNED_FPU", MC_FPOWNED_FPU, Arches::Amd64),
    c("_MC_FPOWNED_PCB", MC_FPOWNED_PCB, Arches::Amd64),
    c("_MC_FP_VALID", MC_FP_VALID as i64, Arches::Arm64),
    c("UCF_SWAPPED", UCF_SWAPPED as i64, Arches::All),
];

const fn c(c_name: &'static str, value: i64, arches: Arches) -> ConstValue {
    ConstValue {
        c_name,
        header: "ucontext.h",
        value,
        arches,
    }
}
