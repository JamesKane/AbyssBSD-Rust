// SPDX-License-Identifier: BSD-2-Clause
//! FP/SIMD state: the 32 vector registers with FPSR/FPCR, or with SVE the
//! scalable Z registers, P registers, and FFR at the largest vector length
//! the CPU offers.
//!
//! The kernel is built soft-float and never touches these registers, so
//! the scheduler saves and restores them only for threads that use them.
//!
//! Behavior reference: sys/arm64/arm64/vfp.c (Arm ARM for SVE).

use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::cpu::read_sysreg;

/// SVE vector length in bytes (0 without SVE).
static VL: AtomicUsize = AtomicUsize::new(0);

/// Whether the CPU implements SVE (`ID_AA64PFR0_EL1.SVE`).
#[must_use]
pub fn has_sve() -> bool {
    (read_sysreg!("id_aa64pfr0_el1") >> 32) & 0xf != 0
}

/// Enables FP/SIMD (and SVE at its largest vector length) on this CPU.
///
/// # Safety
///
/// Call once per CPU, before any thread with FP state runs on it.
pub unsafe fn init_cpu(boot: bool) {
    let mut cpacr = read_sysreg!("cpacr_el1") | (3 << 20);
    if has_sve() {
        cpacr |= 3 << 16;
    }
    // SAFETY: enabling FP/SIMD and SVE access at EL1 changes no memory.
    unsafe {
        asm!("msr cpacr_el1, {}", "isb", in(reg) cpacr, options(nostack));
        if has_sve() {
            // ZCR_EL1.LEN = max; the CPU clamps it to what it supports.
            asm!("msr S3_0_C1_C2_0, {}", "isb", in(reg) 0x1ffu64, options(nostack));
            if boot {
                let vl: usize;
                asm!(".arch_extension sve", "rdvl {}, #1", out(reg) vl, options(nomem, nostack));
                VL.store(vl, Ordering::Relaxed);
            }
        }
    }
}

/// SVE vector length in bytes, 0 without SVE.
#[must_use]
pub fn vector_length() -> usize {
    VL.load(Ordering::Relaxed)
}

/// Bytes in a save area: Z (32 × VL), P (16 × VL/8), FFR (VL/8), then
/// FPSR and FPCR; without SVE, 32 × 16 bytes of V registers instead.
#[must_use]
pub fn area_size() -> usize {
    let vl = vector_length();
    let regs = if vl == 0 {
        32 * 16
    } else {
        32 * vl + 17 * (vl / 8)
    };
    regs.next_multiple_of(16) + 16
}

/// What the save area holds, for diagnostics.
#[must_use]
pub fn describe() -> (&'static str, usize) {
    if vector_length() == 0 {
        ("FP/SIMD", 128)
    } else {
        ("SVE", vector_length() * 8)
    }
}

/// Fills `area` with the initial FP/SIMD state (all zero).
///
/// # Safety
///
/// `area` is [`area_size`] writable bytes, 16-byte aligned.
pub unsafe fn init_area(area: *mut u8) {
    // SAFETY: per the contract.
    unsafe { core::ptr::write_bytes(area, 0, area_size()) };
}

fn control_offset() -> usize {
    area_size() - 16
}

/// Where V register `n` (Z register `n`'s low 128 bits, with SVE) starts
/// in a save area.
#[must_use]
pub fn vreg_offset(n: usize) -> usize {
    n * vector_length().max(16)
}

/// Where FPSR and FPCR are in a save area (8 bytes each).
#[must_use]
pub fn status_offsets() -> (usize, usize) {
    (control_offset(), control_offset() + 8)
}

/// Clears what SVE adds to the V registers in a save area (the Z
/// registers' upper bits, the P registers, FFR), as writing the V
/// registers does architecturally.
pub fn clear_sve_extra(area: &mut [u8]) {
    let vl = vector_length();
    if vl == 0 {
        return;
    }
    for n in 0..32 {
        area[n * vl + 16..(n + 1) * vl].fill(0);
    }
    area[32 * vl..32 * vl + 17 * (vl / 8)].fill(0);
}

/// Saves this CPU's FP/SIMD registers to `area`.
///
/// # Safety
///
/// As for [`init_area`]; FP/SIMD is enabled on this CPU.
pub unsafe fn save(area: *mut u8) {
    let vl = vector_length();
    // SAFETY: per the contract; offsets stay within the area.
    unsafe {
        if vl == 0 {
            asm!(
                ".arch_extension fp", ".arch_extension simd",
                "stp q0, q1, [{a}, #0]", "stp q2, q3, [{a}, #32]",
                "stp q4, q5, [{a}, #64]", "stp q6, q7, [{a}, #96]",
                "stp q8, q9, [{a}, #128]", "stp q10, q11, [{a}, #160]",
                "stp q12, q13, [{a}, #192]", "stp q14, q15, [{a}, #224]",
                "stp q16, q17, [{a}, #256]", "stp q18, q19, [{a}, #288]",
                "stp q20, q21, [{a}, #320]", "stp q22, q23, [{a}, #352]",
                "stp q24, q25, [{a}, #384]", "stp q26, q27, [{a}, #416]",
                "stp q28, q29, [{a}, #448]", "stp q30, q31, [{a}, #480]",
                a = in(reg) area,
                options(nostack),
            );
        } else {
            let p = area.add(32 * vl);
            asm!(
                ".arch_extension sve",
                "str z0, [{z}, #0, mul vl]", "str z1, [{z}, #1, mul vl]",
                "str z2, [{z}, #2, mul vl]", "str z3, [{z}, #3, mul vl]",
                "str z4, [{z}, #4, mul vl]", "str z5, [{z}, #5, mul vl]",
                "str z6, [{z}, #6, mul vl]", "str z7, [{z}, #7, mul vl]",
                "str z8, [{z}, #8, mul vl]", "str z9, [{z}, #9, mul vl]",
                "str z10, [{z}, #10, mul vl]", "str z11, [{z}, #11, mul vl]",
                "str z12, [{z}, #12, mul vl]", "str z13, [{z}, #13, mul vl]",
                "str z14, [{z}, #14, mul vl]", "str z15, [{z}, #15, mul vl]",
                "str z16, [{z}, #16, mul vl]", "str z17, [{z}, #17, mul vl]",
                "str z18, [{z}, #18, mul vl]", "str z19, [{z}, #19, mul vl]",
                "str z20, [{z}, #20, mul vl]", "str z21, [{z}, #21, mul vl]",
                "str z22, [{z}, #22, mul vl]", "str z23, [{z}, #23, mul vl]",
                "str z24, [{z}, #24, mul vl]", "str z25, [{z}, #25, mul vl]",
                "str z26, [{z}, #26, mul vl]", "str z27, [{z}, #27, mul vl]",
                "str z28, [{z}, #28, mul vl]", "str z29, [{z}, #29, mul vl]",
                "str z30, [{z}, #30, mul vl]", "str z31, [{z}, #31, mul vl]",
                "str p0, [{p}, #0, mul vl]", "str p1, [{p}, #1, mul vl]",
                "str p2, [{p}, #2, mul vl]", "str p3, [{p}, #3, mul vl]",
                "str p4, [{p}, #4, mul vl]", "str p5, [{p}, #5, mul vl]",
                "str p6, [{p}, #6, mul vl]", "str p7, [{p}, #7, mul vl]",
                "str p8, [{p}, #8, mul vl]", "str p9, [{p}, #9, mul vl]",
                "str p10, [{p}, #10, mul vl]", "str p11, [{p}, #11, mul vl]",
                "str p12, [{p}, #12, mul vl]", "str p13, [{p}, #13, mul vl]",
                "str p14, [{p}, #14, mul vl]", "str p15, [{p}, #15, mul vl]",
                // FFR goes through p0, which is already saved.
                "rdffr p0.b",
                "str p0, [{p}, #16, mul vl]",
                z = in(reg) area,
                p = in(reg) p,
                options(nostack),
            );
        }
        let ctl = area.add(control_offset()).cast::<u64>();
        let (fpsr, fpcr): (u64, u64);
        // FPSR and FPCR by encoding (the names need the FP extension).
        asm!("mrs {}, S3_3_C4_C4_1", "mrs {}, S3_3_C4_C4_0", out(reg) fpsr, out(reg) fpcr, options(nomem, nostack));
        ctl.write(fpsr);
        ctl.add(1).write(fpcr);
    }
}

/// Loads this CPU's FP/SIMD registers from `area`.
///
/// # Safety
///
/// As for [`save`]; `area` holds a state from [`save`] or [`init_area`].
pub unsafe fn restore(area: *const u8) {
    let vl = vector_length();
    // SAFETY: per the contract.
    unsafe {
        if vl == 0 {
            asm!(
                ".arch_extension fp", ".arch_extension simd",
                "ldp q0, q1, [{a}, #0]", "ldp q2, q3, [{a}, #32]",
                "ldp q4, q5, [{a}, #64]", "ldp q6, q7, [{a}, #96]",
                "ldp q8, q9, [{a}, #128]", "ldp q10, q11, [{a}, #160]",
                "ldp q12, q13, [{a}, #192]", "ldp q14, q15, [{a}, #224]",
                "ldp q16, q17, [{a}, #256]", "ldp q18, q19, [{a}, #288]",
                "ldp q20, q21, [{a}, #320]", "ldp q22, q23, [{a}, #352]",
                "ldp q24, q25, [{a}, #384]", "ldp q26, q27, [{a}, #416]",
                "ldp q28, q29, [{a}, #448]", "ldp q30, q31, [{a}, #480]",
                a = in(reg) area,
                options(nostack, readonly),
            );
        } else {
            let p = area.add(32 * vl);
            asm!(
                ".arch_extension sve",
                "ldr z0, [{z}, #0, mul vl]", "ldr z1, [{z}, #1, mul vl]",
                "ldr z2, [{z}, #2, mul vl]", "ldr z3, [{z}, #3, mul vl]",
                "ldr z4, [{z}, #4, mul vl]", "ldr z5, [{z}, #5, mul vl]",
                "ldr z6, [{z}, #6, mul vl]", "ldr z7, [{z}, #7, mul vl]",
                "ldr z8, [{z}, #8, mul vl]", "ldr z9, [{z}, #9, mul vl]",
                "ldr z10, [{z}, #10, mul vl]", "ldr z11, [{z}, #11, mul vl]",
                "ldr z12, [{z}, #12, mul vl]", "ldr z13, [{z}, #13, mul vl]",
                "ldr z14, [{z}, #14, mul vl]", "ldr z15, [{z}, #15, mul vl]",
                "ldr z16, [{z}, #16, mul vl]", "ldr z17, [{z}, #17, mul vl]",
                "ldr z18, [{z}, #18, mul vl]", "ldr z19, [{z}, #19, mul vl]",
                "ldr z20, [{z}, #20, mul vl]", "ldr z21, [{z}, #21, mul vl]",
                "ldr z22, [{z}, #22, mul vl]", "ldr z23, [{z}, #23, mul vl]",
                "ldr z24, [{z}, #24, mul vl]", "ldr z25, [{z}, #25, mul vl]",
                "ldr z26, [{z}, #26, mul vl]", "ldr z27, [{z}, #27, mul vl]",
                "ldr z28, [{z}, #28, mul vl]", "ldr z29, [{z}, #29, mul vl]",
                "ldr z30, [{z}, #30, mul vl]", "ldr z31, [{z}, #31, mul vl]",
                // FFR first, through p0, which is loaded last.
                "ldr p0, [{p}, #16, mul vl]",
                "wrffr p0.b",
                "ldr p0, [{p}, #0, mul vl]", "ldr p1, [{p}, #1, mul vl]",
                "ldr p2, [{p}, #2, mul vl]", "ldr p3, [{p}, #3, mul vl]",
                "ldr p4, [{p}, #4, mul vl]", "ldr p5, [{p}, #5, mul vl]",
                "ldr p6, [{p}, #6, mul vl]", "ldr p7, [{p}, #7, mul vl]",
                "ldr p8, [{p}, #8, mul vl]", "ldr p9, [{p}, #9, mul vl]",
                "ldr p10, [{p}, #10, mul vl]", "ldr p11, [{p}, #11, mul vl]",
                "ldr p12, [{p}, #12, mul vl]", "ldr p13, [{p}, #13, mul vl]",
                "ldr p14, [{p}, #14, mul vl]", "ldr p15, [{p}, #15, mul vl]",
                z = in(reg) area,
                p = in(reg) p,
                options(nostack, readonly),
            );
        }
        let ctl = area.add(control_offset()).cast::<u64>();
        asm!("msr S3_3_C4_C4_1, {}", "msr S3_3_C4_C4_0, {}", in(reg) ctl.read(), in(reg) ctl.add(1).read(), options(nostack));
    }
}

/// Self-test support: loads V0-V31 (the low 128 bits of each Z register)
/// from `pattern`, calls `between`, and stores them to `out`. With SVE,
/// also loads Z0-Z31 at full length first and compares those in `sve_*`.
///
/// # Safety
///
/// FP/SIMD state must be enabled for the calling thread (the scheduler
/// saves it across `between`). `sve_pattern` and `sve_out` hold 32 ×
/// [`vector_length`] bytes when SVE exists.
pub unsafe fn load_call_store(
    pattern: &[u8; 512],
    out: &mut [u8; 512],
    sve_pattern: &[u8],
    sve_out: &mut [u8],
    between: extern "C" fn(),
) {
    let vl = vector_length();
    // SAFETY: per the contract; `between` is soft-float kernel code.
    unsafe {
        if vl != 0 {
            asm!(
                ".arch_extension sve",
                "ldr z0, [{z}, #0, mul vl]", "ldr z1, [{z}, #1, mul vl]",
                "ldr z2, [{z}, #2, mul vl]", "ldr z3, [{z}, #3, mul vl]",
                "ldr z4, [{z}, #4, mul vl]", "ldr z5, [{z}, #5, mul vl]",
                "ldr z6, [{z}, #6, mul vl]", "ldr z7, [{z}, #7, mul vl]",
                "ldr z8, [{z}, #8, mul vl]", "ldr z9, [{z}, #9, mul vl]",
                "ldr z10, [{z}, #10, mul vl]", "ldr z11, [{z}, #11, mul vl]",
                "ldr z12, [{z}, #12, mul vl]", "ldr z13, [{z}, #13, mul vl]",
                "ldr z14, [{z}, #14, mul vl]", "ldr z15, [{z}, #15, mul vl]",
                "ldr z16, [{z}, #16, mul vl]", "ldr z17, [{z}, #17, mul vl]",
                "ldr z18, [{z}, #18, mul vl]", "ldr z19, [{z}, #19, mul vl]",
                "ldr z20, [{z}, #20, mul vl]", "ldr z21, [{z}, #21, mul vl]",
                "ldr z22, [{z}, #22, mul vl]", "ldr z23, [{z}, #23, mul vl]",
                "ldr z24, [{z}, #24, mul vl]", "ldr z25, [{z}, #25, mul vl]",
                "ldr z26, [{z}, #26, mul vl]", "ldr z27, [{z}, #27, mul vl]",
                "ldr z28, [{z}, #28, mul vl]", "ldr z29, [{z}, #29, mul vl]",
                "ldr z30, [{z}, #30, mul vl]", "ldr z31, [{z}, #31, mul vl]",
                z = in(reg) sve_pattern.as_ptr(),
                options(nostack, readonly),
            );
            between();
            asm!(
                ".arch_extension sve",
                "str z0, [{z}, #0, mul vl]", "str z1, [{z}, #1, mul vl]",
                "str z2, [{z}, #2, mul vl]", "str z3, [{z}, #3, mul vl]",
                "str z4, [{z}, #4, mul vl]", "str z5, [{z}, #5, mul vl]",
                "str z6, [{z}, #6, mul vl]", "str z7, [{z}, #7, mul vl]",
                "str z8, [{z}, #8, mul vl]", "str z9, [{z}, #9, mul vl]",
                "str z10, [{z}, #10, mul vl]", "str z11, [{z}, #11, mul vl]",
                "str z12, [{z}, #12, mul vl]", "str z13, [{z}, #13, mul vl]",
                "str z14, [{z}, #14, mul vl]", "str z15, [{z}, #15, mul vl]",
                "str z16, [{z}, #16, mul vl]", "str z17, [{z}, #17, mul vl]",
                "str z18, [{z}, #18, mul vl]", "str z19, [{z}, #19, mul vl]",
                "str z20, [{z}, #20, mul vl]", "str z21, [{z}, #21, mul vl]",
                "str z22, [{z}, #22, mul vl]", "str z23, [{z}, #23, mul vl]",
                "str z24, [{z}, #24, mul vl]", "str z25, [{z}, #25, mul vl]",
                "str z26, [{z}, #26, mul vl]", "str z27, [{z}, #27, mul vl]",
                "str z28, [{z}, #28, mul vl]", "str z29, [{z}, #29, mul vl]",
                "str z30, [{z}, #30, mul vl]", "str z31, [{z}, #31, mul vl]",
                z = in(reg) sve_out.as_mut_ptr(),
                options(nostack),
            );
        }
        asm!(
            ".arch_extension fp", ".arch_extension simd",
            "ldp q0, q1, [{a}, #0]", "ldp q2, q3, [{a}, #32]",
            "ldp q4, q5, [{a}, #64]", "ldp q6, q7, [{a}, #96]",
            "ldp q8, q9, [{a}, #128]", "ldp q10, q11, [{a}, #160]",
            "ldp q12, q13, [{a}, #192]", "ldp q14, q15, [{a}, #224]",
            "ldp q16, q17, [{a}, #256]", "ldp q18, q19, [{a}, #288]",
            "ldp q20, q21, [{a}, #320]", "ldp q22, q23, [{a}, #352]",
            "ldp q24, q25, [{a}, #384]", "ldp q26, q27, [{a}, #416]",
            "ldp q28, q29, [{a}, #448]", "ldp q30, q31, [{a}, #480]",
            a = in(reg) pattern.as_ptr(),
            options(nostack, readonly),
        );
        between();
        asm!(
            ".arch_extension fp", ".arch_extension simd",
            "stp q0, q1, [{a}, #0]", "stp q2, q3, [{a}, #32]",
            "stp q4, q5, [{a}, #64]", "stp q6, q7, [{a}, #96]",
            "stp q8, q9, [{a}, #128]", "stp q10, q11, [{a}, #160]",
            "stp q12, q13, [{a}, #192]", "stp q14, q15, [{a}, #224]",
            "stp q16, q17, [{a}, #256]", "stp q18, q19, [{a}, #288]",
            "stp q20, q21, [{a}, #320]", "stp q22, q23, [{a}, #352]",
            "stp q24, q25, [{a}, #384]", "stp q26, q27, [{a}, #416]",
            "stp q28, q29, [{a}, #448]", "stp q30, q31, [{a}, #480]",
            a = in(reg) out.as_mut_ptr(),
            options(nostack),
        );
    }
}
