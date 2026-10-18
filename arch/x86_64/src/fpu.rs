// SPDX-License-Identifier: BSD-2-Clause
//! FP/SIMD state: enabling it, and saving and restoring a thread's
//! registers with XSAVE (every component the CPU supports up to
//! AVX-512), or FXSAVE on CPUs without XSAVE.
//!
//! The kernel is built soft-float and never touches these registers, so
//! the scheduler saves and restores them only for threads that use them.
//!
//! Behavior reference: sys/amd64/amd64/fpu.c (Intel SDM Vol. 1 ch. 13).

use core::arch::asm;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::cpu;

/// x87 | SSE | AVX | AVX-512 opmask, upper ZMM0-15, ZMM16-31.
const XCR0_WANTED: u64 = 0b1110_0111;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const CR4_OSXSAVE: u64 = 1 << 18;

/// The XCR0 value every CPU uses (0 without XSAVE).
static XCR0: AtomicU64 = AtomicU64::new(0);
/// Bytes in a save area.
static AREA_SIZE: AtomicUsize = AtomicUsize::new(512);

fn has_xsave() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 26) != 0
}

/// Enables FP/SIMD on this CPU. The boot CPU also sizes save areas.
///
/// # Safety
///
/// Call once per CPU, before any thread with FP state runs on it.
pub unsafe fn init_cpu(boot: bool) {
    let mut cr4 = cpu::cr4() | CR4_OSFXSR | CR4_OSXMMEXCPT;
    if has_xsave() {
        cr4 |= CR4_OSXSAVE;
    }
    // SAFETY: enabling FXSR/XSAVE support changes no memory.
    unsafe { cpu::set_cr4(cr4) };
    if !has_xsave() {
        return;
    }
    if boot {
        let leaf = core::arch::x86_64::__cpuid_count(0xd, 0);
        let supported = (u64::from(leaf.edx) << 32) | u64::from(leaf.eax);
        XCR0.store(supported & XCR0_WANTED, Ordering::Relaxed);
    }
    let xcr0 = XCR0.load(Ordering::Relaxed);
    let (lo, hi) = cpu::split(xcr0);
    // SAFETY: XCR0 holds only components the CPU reports, x87 and SSE
    // included.
    unsafe { asm!("xsetbv", in("ecx") 0, in("eax") lo, in("edx") hi, options(nostack)) };
    if boot {
        // EBX: the area size for the components now enabled.
        let size = core::arch::x86_64::__cpuid_count(0xd, 0).ebx;
        AREA_SIZE.store(size as usize, Ordering::Relaxed);
    }
}

/// Bytes in a save area (64-byte aligned).
#[must_use]
pub fn area_size() -> usize {
    AREA_SIZE.load(Ordering::Relaxed)
}

/// What the save area holds, for diagnostics.
#[must_use]
pub fn describe() -> (&'static str, u64) {
    let xcr0 = XCR0.load(Ordering::Relaxed);
    let name = if xcr0 & 0xe0 == 0xe0 {
        "XSAVE (x87, SSE, AVX, AVX-512)"
    } else if xcr0 & 4 != 0 {
        "XSAVE (x87, SSE, AVX)"
    } else if xcr0 != 0 {
        "XSAVE (x87, SSE)"
    } else {
        "FXSAVE"
    };
    (name, xcr0)
}

/// Whether AVX registers exist.
#[must_use]
pub fn has_avx() -> bool {
    XCR0.load(Ordering::Relaxed) & 4 != 0
}

/// Fills `area` with the initial FP/SIMD state.
///
/// # Safety
///
/// `area` is [`area_size`] writable bytes, 64-byte aligned.
pub unsafe fn init_area(area: *mut u8) {
    // SAFETY: per the contract.
    unsafe {
        core::ptr::write_bytes(area, 0, area_size());
        // FCW: all exceptions masked, 64-bit precision; MXCSR likewise. A
        // zero XSTATE_BV makes XRSTOR load the init state for the rest.
        area.cast::<u16>().write(0x037f);
        area.add(24).cast::<u32>().write(0x1f80);
    }
}

/// Saves this CPU's FP/SIMD registers to `area`.
///
/// # Safety
///
/// As for [`init_area`]; FP/SIMD is enabled on this CPU.
pub unsafe fn save(area: *mut u8) {
    // SAFETY: per the contract.
    unsafe {
        if XCR0.load(Ordering::Relaxed) != 0 {
            asm!("xsave64 [{}]", in(reg) area, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack));
        } else {
            asm!("fxsave64 [{}]", in(reg) area, options(nostack));
        }
    }
}

/// Loads this CPU's FP/SIMD registers from `area`.
///
/// # Safety
///
/// As for [`save`]; `area` holds a state from [`save`] or [`init_area`].
pub unsafe fn restore(area: *const u8) {
    // SAFETY: per the contract.
    unsafe {
        if XCR0.load(Ordering::Relaxed) != 0 {
            asm!("xrstor64 [{}]", in(reg) area, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack));
        } else {
            asm!("fxrstor64 [{}]", in(reg) area, options(nostack));
        }
    }
}

/// Self-test support: loads XMM0-15 (YMM with AVX) from `pattern`, calls
/// `between`, and stores them to `out`. Each buffer holds 16 registers of
/// 32 bytes.
///
/// # Safety
///
/// FP/SIMD state must be enabled for the calling thread (the scheduler
/// saves it across `between`); both buffers are 512 bytes.
pub unsafe fn load_call_store(pattern: &[u8; 512], out: &mut [u8; 512], between: extern "C" fn()) {
    // SAFETY: per the contract; `between` is soft-float kernel code, which
    // leaves the vector registers alone, so only a context switch could
    // change them.
    unsafe {
        if has_avx() {
            asm!(
                "vmovdqu ymm0, [{p} + 0x000]", "vmovdqu ymm1, [{p} + 0x020]",
                "vmovdqu ymm2, [{p} + 0x040]", "vmovdqu ymm3, [{p} + 0x060]",
                "vmovdqu ymm4, [{p} + 0x080]", "vmovdqu ymm5, [{p} + 0x0a0]",
                "vmovdqu ymm6, [{p} + 0x0c0]", "vmovdqu ymm7, [{p} + 0x0e0]",
                "vmovdqu ymm8, [{p} + 0x100]", "vmovdqu ymm9, [{p} + 0x120]",
                "vmovdqu ymm10, [{p} + 0x140]", "vmovdqu ymm11, [{p} + 0x160]",
                "vmovdqu ymm12, [{p} + 0x180]", "vmovdqu ymm13, [{p} + 0x1a0]",
                "vmovdqu ymm14, [{p} + 0x1c0]", "vmovdqu ymm15, [{p} + 0x1e0]",
                p = in(reg) pattern.as_ptr(),
                options(nostack, readonly),
            );
        } else {
            asm!(
                "movdqu xmm0, [{p} + 0x000]", "movdqu xmm1, [{p} + 0x020]",
                "movdqu xmm2, [{p} + 0x040]", "movdqu xmm3, [{p} + 0x060]",
                "movdqu xmm4, [{p} + 0x080]", "movdqu xmm5, [{p} + 0x0a0]",
                "movdqu xmm6, [{p} + 0x0c0]", "movdqu xmm7, [{p} + 0x0e0]",
                "movdqu xmm8, [{p} + 0x100]", "movdqu xmm9, [{p} + 0x120]",
                "movdqu xmm10, [{p} + 0x140]", "movdqu xmm11, [{p} + 0x160]",
                "movdqu xmm12, [{p} + 0x180]", "movdqu xmm13, [{p} + 0x1a0]",
                "movdqu xmm14, [{p} + 0x1c0]", "movdqu xmm15, [{p} + 0x1e0]",
                p = in(reg) pattern.as_ptr(),
                options(nostack, readonly),
            );
        }
        between();
        if has_avx() {
            asm!(
                "vmovdqu [{o} + 0x000], ymm0", "vmovdqu [{o} + 0x020], ymm1",
                "vmovdqu [{o} + 0x040], ymm2", "vmovdqu [{o} + 0x060], ymm3",
                "vmovdqu [{o} + 0x080], ymm4", "vmovdqu [{o} + 0x0a0], ymm5",
                "vmovdqu [{o} + 0x0c0], ymm6", "vmovdqu [{o} + 0x0e0], ymm7",
                "vmovdqu [{o} + 0x100], ymm8", "vmovdqu [{o} + 0x120], ymm9",
                "vmovdqu [{o} + 0x140], ymm10", "vmovdqu [{o} + 0x160], ymm11",
                "vmovdqu [{o} + 0x180], ymm12", "vmovdqu [{o} + 0x1a0], ymm13",
                "vmovdqu [{o} + 0x1c0], ymm14", "vmovdqu [{o} + 0x1e0], ymm15",
                o = in(reg) out.as_mut_ptr(),
                options(nostack),
            );
        } else {
            asm!(
                "movdqu [{o} + 0x000], xmm0", "movdqu [{o} + 0x020], xmm1",
                "movdqu [{o} + 0x040], xmm2", "movdqu [{o} + 0x060], xmm3",
                "movdqu [{o} + 0x080], xmm4", "movdqu [{o} + 0x0a0], xmm5",
                "movdqu [{o} + 0x0c0], xmm6", "movdqu [{o} + 0x0e0], xmm7",
                "movdqu [{o} + 0x100], xmm8", "movdqu [{o} + 0x120], xmm9",
                "movdqu [{o} + 0x140], xmm10", "movdqu [{o} + 0x160], xmm11",
                "movdqu [{o} + 0x180], xmm12", "movdqu [{o} + 0x1a0], xmm13",
                "movdqu [{o} + 0x1c0], xmm14", "movdqu [{o} + 0x1e0], xmm15",
                o = in(reg) out.as_mut_ptr(),
                options(nostack),
            );
        }
    }
}

/// Bytes of each 32-byte register slot [`load_call_store`] round-trips.
#[must_use]
pub fn compared_bytes() -> usize {
    if has_avx() { 32 } else { 16 }
}
