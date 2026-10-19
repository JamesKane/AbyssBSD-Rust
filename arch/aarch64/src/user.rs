// SPDX-License-Identifier: BSD-2-Clause
//! User mode: entering it (traps and `svc` come back through `trap`), the
//! thread pointer, and user address spaces: loading their tables (TTBR0,
//! tagged with an ASID), flushing their TLB entries, keeping the kernel
//! out of them (PAN), and copying across the boundary.
//!
//! The copy routines are the only kernel code that touches user memory.
//! They use the unprivileged `ldtr`/`sttr` forms, which access memory with
//! EL0's permissions, so PAN needs no lifting around them. Each such
//! instruction has a fixup entry: the trap handler first offers the fault
//! to the VM (a page not yet faulted in), and only if that fails resumes
//! at the fixup, which reports [`Fault`].

use core::arch::{asm, global_asm};

use addr::PhysAddr;

/// The end of the user half: TTBR0's 48-bit range.
pub const USER_END: u64 = 0x0001_0000_0000_0000;

/// ASIDs available with the 8-bit ASIDs `TCR_EL1.AS = 0` selects; ASID 0
/// is left to the kernel.
pub const ASIDS: u16 = 256;

/// A user access that faulted and could not be resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault;

/// `TCR_EL1.EPD0`.
const TCR_EPD0: u64 = 1 << 7;
/// `SCTLR_EL1.SPAN`: when clear, taking an exception to EL1 sets PAN.
const SCTLR_SPAN: u64 = 1 << 23;

/// Prepares this CPU for user mode: turns on PAN (no kernel access to user
/// pages except through the unprivileged loads and stores) where the CPU
/// has it, and keeps it on across exceptions. Call on every CPU.
pub fn init_cpu() {
    let mmfr1: u64;
    // SAFETY: reading an ID register.
    unsafe { asm!("mrs {}, id_aa64mmfr1_el1", out(reg) mmfr1, options(nomem, nostack)) };
    if (mmfr1 >> 20) & 0xf == 0 {
        return;
    }
    // SAFETY: PAN only restricts privileged access to EL0-accessible
    // pages, which the kernel makes only through `ldtr`/`sttr`. The
    // instruction is `msr pan, #1`, encoded by hand for assemblers that
    // target ARMv8.0.
    unsafe {
        asm!(
            "mrs {s}, sctlr_el1",
            "bic {s}, {s}, {span}",
            "msr sctlr_el1, {s}",
            "isb",
            ".inst 0xd500419f",
            s = out(reg) _,
            span = in(reg) SCTLR_SPAN,
            options(nostack),
        );
    }
}

/// The user thread pointer (`TPIDR_EL0`).
pub fn tls() -> u64 {
    let v: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, tpidr_el0", out(reg) v, options(nomem, nostack)) };
    v
}

/// Sets the user thread pointer.
///
/// # Safety
///
/// The kernel must not rely on `TPIDR_EL0` (it does not).
pub unsafe fn set_tls(v: u64) {
    // SAFETY: per the caller; only user mode uses the register.
    unsafe { asm!("msr tpidr_el0, {}", in(reg) v, options(nomem, nostack)) };
}

/// A frame that enters user mode (EL0t, interrupts unmasked) at `entry`
/// with stack `sp` and `arg` in `x0`.
#[must_use]
pub fn user_frame(entry: u64, sp: u64, arg: u64) -> crate::trap::TrapFrame {
    let mut f = crate::trap::TrapFrame::default();
    f.x[0] = arg;
    f.elr = entry;
    // EL0t, nothing masked.
    f.spsr = 0;
    f.sp = sp;
    // A synchronous exception from EL0, for anything that looks.
    f.slot = 8;
    f
}

/// Leaves the kernel for user mode with `frame`'s registers.
///
/// # Safety
///
/// The thread must run in a user address space where `frame` makes
/// sense; its kernel stack above the caller is abandoned (`SP_EL1` ends
/// where the frame did). Interrupts must be masked.
pub unsafe fn enter_user(frame: &crate::trap::TrapFrame) -> ! {
    // SAFETY: per the caller's contract; the exception return path pops
    // `frame`, sets SP_EL0, and `eret`s to EL0.
    unsafe {
        asm!(
            "mov sp, {}",
            "b abyss_el1_exit",
            in(reg) core::ptr::from_ref(frame),
            options(noreturn),
        );
    }
}

/// Loads the user tables rooted at `root`, tagged `asid`, and enables
/// lower-half walks.
///
/// # Safety
///
/// `root` must be a valid table hierarchy for the lower half, and no
/// stale entries may be cached under `asid` for other tables.
pub unsafe fn activate(root: PhysAddr, asid: u16) {
    // SAFETY: per the caller's contract; only the lower half changes.
    unsafe {
        asm!(
            "msr ttbr0_el1, {ttbr}",
            "mrs {tcr}, tcr_el1",
            "bic {tcr}, {tcr}, {epd0}",
            "msr tcr_el1, {tcr}",
            "isb",
            ttbr = in(reg) root.get() | (u64::from(asid) << 48),
            tcr = out(reg) _,
            epd0 = in(reg) TCR_EPD0,
            options(nostack),
        );
    }
}

/// Drops every CPU's TLB entry for user page `va` under `asid` (the
/// broadcast form; no IPIs needed).
pub fn flush_page(va: u64, asid: u16) {
    let arg = ((va >> 12) & ((1 << 44) - 1)) | (u64::from(asid) << 48);
    // SAFETY: TLB invalidation has no effect on memory contents.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vae1is, {}",
            "dsb ish",
            "isb",
            in(reg) arg,
            options(nostack),
        );
    }
}

/// Drops every CPU's TLB entries tagged `asid`, before the ASID is reused.
pub fn flush_asid(asid: u16) {
    // SAFETY: TLB invalidation has no effect on memory contents.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi aside1is, {}",
            "dsb ish",
            "isb",
            in(reg) u64::from(asid) << 48,
            options(nostack),
        );
    }
}

global_asm!(
    r#"
    .section .text.abyss_user, "ax"

    /* x0 = dst (kernel), x1 = src (user), x2 = len; 0, or 1 on a fault. */
    .global abyss_copy_from_user
abyss_copy_from_user:
    cmp x2, #8
    b.lo 2f
1:  ldtr x3, [x1]
    str x3, [x0], #8
    add x1, x1, #8
    sub x2, x2, #8
    cmp x2, #8
    b.hs 1b
2:  cbz x2, 4f
3:  ldtrb w3, [x1]
    strb w3, [x0], #1
    add x1, x1, #1
    subs x2, x2, #1
    b.ne 3b
4:  mov x0, #0
    ret

    /* x0 = dst (user), x1 = src (kernel), x2 = len. */
    .global abyss_copy_to_user
abyss_copy_to_user:
    cmp x2, #8
    b.lo 6f
5:  ldr x3, [x1], #8
    sttr x3, [x0]
    add x0, x0, #8
    sub x2, x2, #8
    cmp x2, #8
    b.hs 5b
6:  cbz x2, 8f
    ldrb w3, [x1], #1
7:  sttrb w3, [x0]
    add x0, x0, #1
    subs x2, x2, #1
    b.ne 6b
8:  mov x0, #0
    ret

9:  mov x0, #1
    ret

    .pushsection abyss_extable, "aw"
    .balign 8
    .quad 1b, 9b
    .quad 3b, 9b
    .quad 5b + 4, 9b
    .quad 7b, 9b
    .popsection
    "#
);

unsafe extern "C" {
    fn abyss_copy_from_user(dst: *mut u8, src: u64, len: usize) -> u64;
    fn abyss_copy_to_user(dst: u64, src: *const u8, len: usize) -> u64;
}

/// Copies `len` bytes from user address `src` to `dst`.
///
/// # Safety
///
/// `dst` must be valid for writes of `len` bytes, and `[src, src + len)`
/// must lie below [`USER_END`].
pub unsafe fn copy_from_user(dst: *mut u8, src: u64, len: usize) -> Result<(), Fault> {
    // SAFETY: per the caller's contract; user accesses have fixups.
    let faulted = unsafe { abyss_copy_from_user(dst, src, len) };
    if faulted == 0 { Ok(()) } else { Err(Fault) }
}

/// Copies `len` bytes from `src` to user address `dst`.
///
/// # Safety
///
/// `src` must be valid for reads of `len` bytes, and `[dst, dst + len)`
/// must lie below [`USER_END`].
pub unsafe fn copy_to_user(dst: u64, src: *const u8, len: usize) -> Result<(), Fault> {
    // SAFETY: per the caller's contract; user accesses have fixups.
    let faulted = unsafe { abyss_copy_to_user(dst, src, len) };
    if faulted == 0 { Ok(()) } else { Err(Fault) }
}

// A user program for the kernel's tests (no libc): `write(1, "hello\n", 6)`,
// then an unknown system call, then `exit(w * 1000 + e + c)` where `w` is
// what `write` returned, `e` the unknown call's error number, and `c` 5000
// if the carry flag (error) was clear. A correct kernel exits with 6078.
global_asm!(
    r#"
    .section .rodata.abyss_user_test, "a"
    .global abyss_user_test_start
abyss_user_test_start:
    mov x8, #4
    mov x0, #1
    adr x1, 2f
    mov x2, #6
    svc #0
    mov x19, x0
    mov x8, #9999
    svc #0
    mov x20, x0
    mov x21, #0
    b.cs 1f
    mov x21, #5000
1:  mov x9, #1000
    mul x0, x19, x9
    add x0, x0, x20
    add x0, x0, x21
    mov x8, #1
    svc #0
    brk #0
2:  .ascii "hello\n"
    .global abyss_user_test_end
abyss_user_test_end:
    "#
);

unsafe extern "C" {
    static abyss_user_test_start: u8;
    static abyss_user_test_end: u8;
}

/// The test program's position-independent machine code.
#[must_use]
pub fn test_program() -> &'static [u8] {
    let start = &raw const abyss_user_test_start;
    let end = &raw const abyss_user_test_end;
    // SAFETY: the two labels bracket the program's bytes in read-only
    // data, which lives as long as the kernel.
    unsafe { core::slice::from_raw_parts(start, end as usize - start as usize) }
}
