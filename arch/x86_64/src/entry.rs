// SPDX-License-Identifier: BSD-2-Clause
//! Kernel entry from the loader (see `bootproto` for the contract).
//!
//! `_start` runs at its physical address under the firmware's identity
//! mapping, with `rdi` = BootInfo (physical), `rsi` = the loader's PML4. It
//! turns on NX, programs the PAT the loader's tables assume, loads the new
//! tables (which identity-map RAM too, so execution continues), jumps to its
//! own virtual address, switches to the boot stack, and calls
//! [`start`].

use core::arch::global_asm;

use crate::cpu::{MSR_EFER, MSR_PAT};

global_asm!(
    r#"
    .section .text.boot, "ax"
    .global _start
    .type _start, @function
_start:
    cli
    cld
    mov r12, rdi

    /* EFER.NXE before any table with NX bits is live. */
    mov ecx, {efer}
    rdmsr
    or eax, 1 << 11
    wrmsr

    /* The PAT layout `pt::x86_64` encodes. */
    mov ecx, {pat_msr}
    mov eax, {pat_lo}
    mov edx, {pat_hi}
    wrmsr

    /* CR0.WP: honour read-only pages in supervisor mode. */
    mov rax, cr0
    or eax, 1 << 16
    mov cr0, rax

    mov cr3, rsi

    /* CR4.PGE: global pages. */
    mov rax, cr4
    or eax, 1 << 7
    mov cr4, rax

    /* Leave the identity mapping for our linked (and slid) address. */
    mov rax, [rip + 2f]
    jmp rax
1:
    lea rsp, [rip + __boot_stack_top]
    xor ebp, ebp
    push rbp
    mov rdi, r12
    call {start}
    ud2
    .size _start, . - _start

    .pushsection .data.rel.ro.abyss_entry, "aw"
    .balign 8
2:  .quad 1b
    .popsection
    "#,
    efer = const MSR_EFER,
    pat_msr = const MSR_PAT,
    pat_lo = const crate::cpu::split(pt::x86_64::PAT).0,
    pat_hi = const crate::cpu::split(pt::x86_64::PAT).1,
    start = sym start,
);

/// The first Rust code: CPU tables, then the kernel.
extern "sysv64" fn start(boot_info: u64) -> ! {
    // SAFETY: we are the boot CPU, running once, before anything else uses
    // the GDT, IDT, or GS.
    unsafe {
        crate::gdt::init_boot_cpu();
        crate::trap::init_boot_cpu();
    }
    // SAFETY: defined by the kernel through `kernel_hooks!`.
    unsafe { crate::abyss_kernel_main(addr::PhysAddr::new(boot_info)) }
}
