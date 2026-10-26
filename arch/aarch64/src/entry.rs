// SPDX-License-Identifier: BSD-2-Clause
//! Kernel entry from the loader (see `bootproto` for the contract).
//!
//! `_start` runs at its physical address under the firmware's identity
//! mapping with `x0` = BootInfo (physical), `x1` = upper-half root, `x2` =
//! identity root. If firmware ran at EL2 it installs a minimal EL2 stub
//! vector table (resident in the image, run with the EL2 MMU off),
//! configures EL1, and drops to EL1. At EL1 it turns the MMU off (execution
//! continues under the identity map), programs MAIR/TCR/TTBR0/TTBR1 for the
//! loader's tables, turns the MMU back on, and jumps to its linked address.
//!
//! Other CPUs enter at `abyss_ap_start` (from PSCI) and share the same
//! path, then switch to their own stack and call `smp::ap_start`.
//!
//! The MMU-off window makes no data accesses, so no cache maintenance is
//! needed: the table walker is coherent with the caches the loader wrote
//! the tables through.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};

/// `TCR_EL1` for the handoff: 48-bit VA in both halves, 4 KiB granules,
/// inner-shareable write-back walks. IPS is filled in from the CPU.
const TCR: u64 = 16          // T0SZ
    | (1 << 8)               // IRGN0 = WBWA
    | (1 << 10)              // ORGN0 = WBWA
    | (3 << 12)              // SH0 = inner
    | (16 << 16)             // T1SZ
    | (1 << 24)              // IRGN1
    | (1 << 26)              // ORGN1
    | (3 << 28)              // SH1
    | (2 << 30); // TG1 = 4 KiB

/// `SCTLR_EL1` RES1 bits (ARMv8.0: 29, 28, 23, 22, 20, 11), MMU off.
const SCTLR_RES1: u64 = 0x30d0_0800;
/// RES1 plus M, C, SA, SA0, I, and what FreeBSD lets user mode do
/// without trapping: `DC ZVA` (DZE), reading `CTR_EL0` (UCT), `WFI` and
/// `WFE` (nTWI, nTWE), and cache maintenance (UCI).
const SCTLR_MMU_ON: u64 = SCTLR_RES1
    | 1
    | (1 << 2)
    | (1 << 3)
    | (1 << 4)
    | (1 << 12)
    | (1 << 14)
    | (1 << 15)
    | (1 << 16)
    | (1 << 18)
    | (1 << 26);
/// `HCR_EL2`: EL1 is AArch64; don't trap pointer authentication.
const HCR_EL2: u64 = (1 << 31) | (1 << 41) | (1 << 40);

global_asm!(
    r#"
    .section .text.boot, "ax"
    .global _start
    .type _start, %function
_start:
    msr daifset, #0xf
    mov x21, #0                    /* the boot CPU */
    b 0f

    /*
     * Other CPUs (PSCI CPU_ON): x0 = physical address of an `ApBoot`,
     * cleaned to the point of coherency because caches are off here.
     */
    .global abyss_ap_start
    .type abyss_ap_start, %function
abyss_ap_start:
    msr daifset, #0xf
    mov x20, x0
    ldr x1, [x20, #0]              /* kernel root */
    ldr x2, [x20, #8]              /* identity root */
    ldr x22, [x20, #16]            /* stack top (virtual) */
    ldr x23, [x20, #24]            /* argument */
    mov x21, #1

0:
    mov x19, #0
    mrs x9, CurrentEL
    lsr x9, x9, #2
    cmp x9, #2
    b.ne 1f

    /* EL2: run the EL2 regime without the MMU from here on. */
    mov x19, #1
    mrs x9, sctlr_el2
    bic x9, x9, #1
    msr sctlr_el2, x9
    isb

    adr x9, abyss_el2_vectors
    msr vbar_el2, x9
    /*
     * HCR_EL2 first: firmware may have left E2H (VHE) set, which changes
     * the layout of CPTR_EL2 and CNTHCTL_EL2 programmed below.
     */
    ldr x9, ={hcr}
    msr hcr_el2, x9
    isb
    mov x9, #3                     /* EL1PCTEN | EL1PCEN */
    msr cnthctl_el2, x9
    msr cntvoff_el2, xzr
    mov x9, #0x32ff                /* CPTR_EL2: RES1 bits, no traps (TZ clear: SVE allowed) */
    msr cptr_el2, x9
    /* SVE: let EL1 use the largest vector length (ZCR_EL2.LEN = max). */
    mrs x9, id_aa64pfr0_el1
    ubfx x9, x9, #32, #4
    cbz x9, 6f
    mov x9, #0x1ff
    msr S3_4_C1_C2_0, x9
6:
    /*
     * FEAT_HCX: HCRX_EL2 all clear but MSCEn, so the FEAT_MOPS
     * instructions are not UNDEFINED at EL1 and EL0 (FreeBSD, running at
     * EL2 with VHE, gets that from E2H and TGE).
     */
    mrs x9, id_aa64mmfr1_el1
    ubfx x9, x9, #40, #4
    cbz x9, 7f
    mov x9, #(1 << 11)
    msr S3_4_C1_C2_2, x9
    isb
7:
    msr hstr_el2, xzr
    msr vttbr_el2, xzr
    mrs x9, midr_el1
    msr vpidr_el2, x9
    mrs x9, mpidr_el1
    msr vmpidr_el2, x9

    /* GICv3 system register access for EL1, if the CPU has it. */
    mrs x9, id_aa64pfr0_el1
    ubfx x9, x9, #24, #4
    cbz x9, 2f
    mrs x9, S3_4_C12_C9_5          /* ICC_SRE_EL2 */
    orr x9, x9, #0xf               /* SRE | DFB | DIB | Enable */
    msr S3_4_C12_C9_5, x9
    isb
2:
    ldr x9, ={sctlr_res1}
    msr sctlr_el1, x9
    mov x9, #0x3c5                 /* EL1h, DAIF masked */
    msr spsr_el2, x9
    adr x9, 1f
    msr elr_el2, x9
    eret

1:  /* EL1, still under the firmware's identity map. */
    /* FP/SIMD on at EL1: the kernel is soft-float, but UEFI runtime
       services, which we call, may use it. */
    mov x9, #(3 << 20)
    msr cpacr_el1, x9
    mrs x9, sctlr_el1
    bic x9, x9, #1
    msr sctlr_el1, x9
    isb
    tlbi vmalle1
    dsb nsh
    isb

    ldr x9, ={mair}
    msr mair_el1, x9
    ldr x9, ={tcr}
    mrs x10, id_aa64mmfr0_el1
    and x10, x10, #0xf             /* PARange */
    cmp x10, #5
    mov x11, #5
    csel x10, x10, x11, ls         /* 52-bit PA needs LPA2; cap at 48 */
    bfi x9, x10, #32, #3
    msr tcr_el1, x9
    msr ttbr0_el1, x2
    msr ttbr1_el1, x1
    isb
    ldr x9, ={sctlr_on}
    msr sctlr_el1, x9
    isb

    adrp x9, 4f
    ldr x9, [x9, :lo12:4f]
    br x9
3:
    cbnz x21, 5f
    adrp x9, __boot_stack_top
    add x9, x9, :lo12:__boot_stack_top
    mov sp, x9
    mov x29, #0
    mov x30, #0
    mov x1, x19
    bl {start}
    brk #0
5:
    mov sp, x22
    mov x29, #0
    mov x30, #0
    mov x0, x23
    bl {ap_start}
    brk #0
    .size _start, . - _start

    .pushsection .data.rel.ro.abyss_entry, "aw"
    .balign 8
4:  .quad 3b
    .popsection

    /*
     * The EL2 stub. Runs with the EL2 MMU off, at physical addresses.
     * HVC from EL1 returns -1 (no services yet); anything else hangs.
     */
    .balign 2048
abyss_el2_vectors:
    .set slot, 0
    .rept 16
    .balign 128
    .if slot == 8
    mov x0, #-1
    eret
    .else
    wfe
    b . - 4
    .endif
    .set slot, slot + 1
    .endr
    "#,
    hcr = const HCR_EL2,
    sctlr_res1 = const SCTLR_RES1,
    sctlr_on = const SCTLR_MMU_ON,
    mair = const pt::aarch64::MAIR,
    tcr = const TCR,
    start = sym start,
    ap_start = sym crate::smp::ap_start,
);

static BOOTED_AT_EL2: AtomicBool = AtomicBool::new(false);

/// Whether firmware handed over at EL2 (and the entry code dropped to EL1).
pub fn booted_at_el2() -> bool {
    BOOTED_AT_EL2.load(Ordering::Relaxed)
}

/// The first Rust code: vectors and per-CPU state, then the kernel.
extern "C" fn start(boot_info: u64, from_el2: u64) -> ! {
    BOOTED_AT_EL2.store(from_el2 != 0, Ordering::Relaxed);
    // SAFETY: we are the boot CPU, running once.
    unsafe {
        crate::cpu::init_boot_cpu();
        crate::trap::init_boot_cpu();
    }
    // SAFETY: defined by the kernel through `kernel_hooks!`.
    unsafe { crate::abyss_kernel_main(addr::PhysAddr::new(boot_info)) }
}
