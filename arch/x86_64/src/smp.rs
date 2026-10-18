// SPDX-License-Identifier: BSD-2-Clause
//! Starting the other CPUs (INIT-SIPI-SIPI).
//!
//! A start-up IPI wakes a CPU in real mode at the start of a page below
//! 1 MiB. The kernel copies [`TRAMPOLINE`] there and fills its data area
//! (the `DATA_*` offsets) and the two far-jump targets. The trampoline
//! enters protected mode, then long mode under a temporary page table
//! (the trampoline page identity mapped plus the kernel's upper half),
//! and jumps to [`abyss_ap_entry64`] in the kernel image, which loads the
//! PAT and the kernel's own table and calls into Rust on the CPU's stack.
//!
//! Behavior reference: sys/amd64/amd64/mpboot.S, sys/x86/x86/mp_x86.c.

use core::arch::global_asm;

use crate::cpu::MSR_PAT;
use crate::gdt::CpuBlock;

/// Data area offsets within the trampoline page, for the kernel to fill.
pub const DATA_GDT_PTR: usize = 0xf20;
/// The temporary page table root (below 4 GiB), 32 bits.
pub const DATA_TEMP_CR3: usize = 0xf30;
/// The kernel's page table root, 64 bits.
pub const DATA_KERNEL_CR3: usize = 0xf38;
/// The CPU's initial stack pointer (kernel virtual), 64 bits.
pub const DATA_STACK: usize = 0xf40;
/// [`abyss_ap_entry64`]'s address, 64 bits.
pub const DATA_ENTRY: usize = 0xf48;
/// The [`ApStart`] pointer (kernel virtual), 64 bits.
pub const DATA_ARG: usize = 0xf50;
/// The trampoline GDT (flat 32-bit code and data, 64-bit code).
pub const DATA_GDT: usize = 0xf00;
/// The GDT contents.
pub const TRAMPOLINE_GDT: [u64; 4] = [
    0,
    0x00cf_9a00_0000_ffff, // 0x08: 32-bit code, flat
    0x00cf_9200_0000_ffff, // 0x10: data, flat
    0x00af_9a00_0000_ffff, // 0x18: 64-bit code
];

global_asm!(
    r#"
    .pushsection .rodata.abyss_ap_trampoline, "a"
    .global abyss_ap_trampoline
    .global abyss_ap_trampoline_end
    .global abyss_ap_jmp32
    .global abyss_ap_jmp64
    .global abyss_ap_pm32
    .global abyss_ap_lm64
abyss_ap_trampoline:
    .code16
    cli
    cld
    mov ax, cs
    mov ds, ax
    xor ebx, ebx
    mov bx, ax
    shl ebx, 4
    /* lgdtd [DATA_GDT_PTR] */
    .byte 0x66, 0x0f, 0x01, 0x16
    .word 0xf20
    mov eax, cr0
    or al, 1
    mov cr0, eax
    /* ljmpd 0x08:pm32 (patched) */
    .byte 0x66, 0xea
abyss_ap_jmp32:
    .long 0
    .word 0x08

    .code32
abyss_ap_pm32:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, cr4
    or eax, 0xa0                  /* PAE | PGE */
    mov cr4, eax
    mov eax, [ebx + 0xf30]
    mov cr3, eax
    mov ecx, 0xc0000080
    rdmsr
    or eax, 0x900                 /* LME | NXE */
    wrmsr
    mov eax, cr0
    or eax, 0x80010000            /* PG | WP */
    mov cr0, eax
    /* ljmp 0x18:lm64 (patched) */
    .byte 0xea
abyss_ap_jmp64:
    .long 0
    .word 0x18

    .code64
abyss_ap_lm64:
    mov ebx, ebx                  /* upper half is undefined after the switch */
    mov rsp, [rbx + 0xf40]
    mov rdi, [rbx + 0xf50]
    mov rcx, [rbx + 0xf38]
    mov rax, [rbx + 0xf48]
    jmp rax
abyss_ap_trampoline_end:
    .popsection

    .pushsection .text.abyss_ap_entry64, "ax"
    .global abyss_ap_entry64
    .type abyss_ap_entry64, @function
abyss_ap_entry64:
    mov r12, rdi
    mov r13, rcx
    mov ecx, {pat_msr}
    mov eax, {pat_lo}
    mov edx, {pat_hi}
    wrmsr
    mov cr3, r13
    xor ebp, ebp
    push rbp
    mov rdi, r12
    call {ap_start}
    ud2
    .size abyss_ap_entry64, . - abyss_ap_entry64
    .popsection
    "#,
    pat_msr = const MSR_PAT,
    pat_lo = const crate::cpu::split(pt::x86_64::PAT).0,
    pat_hi = const crate::cpu::split(pt::x86_64::PAT).1,
    ap_start = sym ap_start,
);

unsafe extern "C" {
    static abyss_ap_trampoline: u8;
    static abyss_ap_trampoline_end: u8;
    static abyss_ap_jmp32: u8;
    static abyss_ap_jmp64: u8;
    static abyss_ap_pm32: u8;
    static abyss_ap_lm64: u8;
    /// The 64-bit landing point in the kernel image.
    pub fn abyss_ap_entry64();
}

/// The trampoline code, to copy into the start-up page.
#[must_use]
pub fn trampoline() -> &'static [u8] {
    let start = &raw const abyss_ap_trampoline;
    let end = &raw const abyss_ap_trampoline_end;
    // SAFETY: both symbols bound one read-only blob in our image.
    unsafe { core::slice::from_raw_parts(start, end as usize - start as usize) }
}

/// Where to patch the far-jump targets, and what to: (offset of the jump
/// operand, offset of the target) for the 32-bit and 64-bit jumps. The
/// caller writes `page + target` at `page + operand`.
#[must_use]
pub fn jump_patches() -> [(usize, usize); 2] {
    let base = (&raw const abyss_ap_trampoline) as usize;
    let off = |p: *const u8| p as usize - base;
    [
        (
            off(&raw const abyss_ap_jmp32),
            off(&raw const abyss_ap_pm32),
        ),
        (
            off(&raw const abyss_ap_jmp64),
            off(&raw const abyss_ap_lm64),
        ),
    ]
}

/// What a starting CPU needs, prepared by the kernel.
#[repr(C)]
#[derive(Debug)]
pub struct ApStart {
    /// The logical CPU number to take.
    pub cpu: u32,
    /// The CPU's descriptor tables (leaked by the kernel).
    pub block: *mut CpuBlock,
    /// Passed to the kernel's AP hook.
    pub arg: usize,
}

/// The first Rust code on a started CPU.
extern "sysv64" fn ap_start(start: *const ApStart) -> ! {
    // SAFETY: the kernel keeps `start` and the block alive and dedicated
    // to this CPU; we run once, with interrupts masked.
    unsafe {
        let s = &*start;
        crate::gdt::load(&mut *s.block, s.cpu);
        crate::trap::load_idt();
        // CR4.PGE was set by the trampoline; CR0.WP too.
        crate::abyss_ap_main(s.arg)
    }
}
