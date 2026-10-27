// SPDX-License-Identifier: BSD-2-Clause
//! User mode: entering it (traps and `svc` come back through `trap`), the
//! thread pointer, and user address spaces: loading their tables (TTBR0,
//! tagged with an ASID), flushing their TLB entries, keeping the kernel
//! out of them (PAN), and copying across the boundary.
//!
//! The copy routines and the single-word accesses are the only kernel
//! code that touches user memory. They use the unprivileged `ldtr`/`sttr`
//! forms, which access memory with EL0's permissions, so PAN needs no
//! lifting around them; compare-and-swap, which has no unprivileged form,
//! lifts it for its loop. Each such
//! instruction has a fixup entry: the trap handler first offers the fault
//! to the VM (a page not yet faulted in), and only if that fails resumes
//! at the fixup, which reports [`Fault`].

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, Ordering};

use addr::PhysAddr;

/// `sigreturn`'s and `exit`'s system-call numbers, which the trampoline
/// makes.
pub const SYS_SIGRETURN: u64 = 417;
pub const SYS_EXIT: u64 = 1;
/// Where `struct sigframe` keeps the context (`sf_uc`).
pub const SIGF_UC: u64 = 80;

// The signal trampoline, copied into the shared page (FreeBSD's
// `sigcode`). The kernel enters it with `sp` at the signal frame, the
// handler in `x8`, and its arguments in `x0`-`x2`; it calls the handler,
// then `sigreturn` with the frame's context, and exits if that fails.
global_asm!(
    ".section .rodata.sigcode, \"a\"",
    ".balign 16",
    ".global abyss_sigcode",
    "abyss_sigcode:",
    "blr x8",
    "mov x0, sp",
    "add x0, x0, #{uc}",
    "1: mov x8, #{sigreturn}",
    "svc 0",
    "mov x8, #{exit}",
    "svc 0",
    "b 1b",
    ".global abyss_sigcode_end",
    "abyss_sigcode_end:",
    uc = const SIGF_UC,
    sigreturn = const SYS_SIGRETURN,
    exit = const SYS_EXIT,
);

unsafe extern "C" {
    static abyss_sigcode: u8;
    static abyss_sigcode_end: u8;
}

/// The signal trampoline's code.
#[must_use]
pub fn sigcode() -> &'static [u8] {
    let start = &raw const abyss_sigcode;
    let end = &raw const abyss_sigcode_end;
    // SAFETY: the two symbols bracket the trampoline in read-only data,
    // which lives as long as the kernel.
    unsafe { core::slice::from_raw_parts(start, end.offset_from_unsigned(start)) }
}

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
/// `SCTLR_EL1.MSCEn`: the FEAT_MOPS instructions are enabled at EL0.
const SCTLR_MSCEN: u64 = 1 << 33;

/// `CNTKCTL_EL1`: EL0 may read the virtual count (`EL0VCTEN`), and nothing
/// else of the timers (`EL0PCTEN`, `EL0VTEN`, `EL0PTEN` clear).
const CNTKCTL_EL0VCTEN: u64 = 1 << 1;
const CNTKCTL_EL0_BITS: u64 = 0b11 << 8 | 0b11;

/// Prepares this CPU for user mode: lets user code read the virtual count
/// (libc's `clock_gettime` reads it through the shared page's timehands)
/// and use FEAT_MOPS where the CPU has it, and turns on PAN (no kernel
/// access to user pages except through the unprivileged loads and stores)
/// where the CPU has it, keeping it on across exceptions. Call on every
/// CPU.
pub fn init_cpu() {
    // SAFETY: this CPU's timer access controls for EL0; the kernel's own
    // timer use is unaffected.
    unsafe {
        asm!(
            "mrs {v}, cntkctl_el1",
            "bic {v}, {v}, {clear}",
            "orr {v}, {v}, {set}",
            "msr cntkctl_el1, {v}",
            "isb",
            v = out(reg) _,
            clear = in(reg) CNTKCTL_EL0_BITS,
            set = in(reg) CNTKCTL_EL0VCTEN,
            options(nostack),
        );
    }
    // FEAT_MOPS: user mode may use the memory copy and set instructions,
    // which libc's string functions pick when `AT_HWCAP2` has them
    // (FreeBSD's `mops_enable`).
    if (crate::cpu::id_register(6, 2) >> 16) & 0xf != 0 {
        // SAFETY: MSCEn only stops the instructions from being UNDEFINED
        // at EL0.
        unsafe {
            asm!(
                "mrs {s}, sctlr_el1",
                "orr {s}, {s}, {mscen}",
                "msr sctlr_el1, {s}",
                "isb",
                s = out(reg) _,
                mscen = in(reg) SCTLR_MSCEN,
                options(nostack),
            );
        }
    }
    let mmfr1: u64;
    // SAFETY: reading an ID register.
    unsafe { asm!("mrs {}, id_aa64mmfr1_el1", out(reg) mmfr1, options(nomem, nostack)) };
    if (mmfr1 >> 20) & 0xf == 0 {
        return;
    }
    PAN.store(true, Ordering::Relaxed);
    // SAFETY: PAN only restricts privileged access to EL0-accessible
    // pages, which the kernel makes only through `ldtr`/`sttr` (and
    // compare-and-swap, which lifts it). The
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

/// arm64 has one user thread register: the second is always 0.
#[must_use]
pub fn tls2() -> u64 {
    0
}

/// Nothing to set (see [`tls2`]).
///
/// # Safety
///
/// None needed; `unsafe` for the HAL's signature.
pub unsafe fn set_tls2(_v: u64) {}

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

// Single-word user accesses for `_umtx_op`: one load or store each, so
// atomic, unlike the copy routines' byte loops. Each returns the value
// (or the word's previous value) in x0 and 1 in x1 on a fault. There is
// no unprivileged exclusive access, so compare-and-swap lifts PAN for its
// loop (FreeBSD's `casueword32` does the same); the `_pan` forms do that,
// for CPUs that have it. An exception taken in the loop sets PAN on entry
// and puts it back as it was on return.
macro_rules! user_words {
    ($suffix:literal, $open:literal, $close:literal) => {
        global_asm!(concat!(
            r#"
    .section .text.abyss_user, "ax"

    /* x0 = addr */
    .global abyss_user_load32"#,
            $suffix,
            r#"
abyss_user_load32"#,
            $suffix,
            r#":
1:  ldtr w0, [x0]
    mov x1, #0
    ret

    .global abyss_user_load64"#,
            $suffix,
            r#"
abyss_user_load64"#,
            $suffix,
            r#":
2:  ldtr x0, [x0]
    mov x1, #0
    ret

    /* x0 = addr, x1 = value */
    .global abyss_user_store32"#,
            $suffix,
            r#"
abyss_user_store32"#,
            $suffix,
            r#":
3:  sttr w1, [x0]
    mov x0, #0
    mov x1, #0
    ret

    /* x0 = addr, x1 = expected, x2 = new */
    .global abyss_user_cas32"#,
            $suffix,
            r#"
abyss_user_cas32"#,
            $suffix,
            r#":
    "#,
            $open,
            r#"
4:  ldaxr w3, [x0]
    cmp w3, w1
    b.ne 6f
5:  stlxr w4, w2, [x0]
    cbnz w4, 4b
    b 7f
6:  clrex
7:  "#,
            $close,
            r#"
    mov x0, x3
    mov x1, #0
    ret

    .global abyss_user_cas64"#,
            $suffix,
            r#"
abyss_user_cas64"#,
            $suffix,
            r#":
    "#,
            $open,
            r#"
8:  ldaxr x3, [x0]
    cmp x3, x1
    b.ne 10f
9:  stlxr w4, x2, [x0]
    cbnz w4, 8b
    b 11f
10: clrex
11: "#,
            $close,
            r#"
    mov x0, x3
    mov x1, #0
    ret

12: "#,
            $close,
            r#"
    mov x0, #0
    mov x1, #1
    ret

    .pushsection abyss_extable, "aw"
    .balign 8
    .quad 1b, 12b
    .quad 2b, 12b
    .quad 3b, 12b
    .quad 4b, 12b
    .quad 5b, 12b
    .quad 8b, 12b
    .quad 9b, 12b
    .popsection
    "#
        ));
    };
}
// `msr pan, #0` and `msr pan, #1`, encoded by hand for assemblers that
// target ARMv8.0.
user_words!("", "", "");
user_words!("_pan", ".inst 0xd500409f", ".inst 0xd500419f");

/// Whether PAN is on (so compare-and-swap must lift it).
static PAN: AtomicBool = AtomicBool::new(false);

/// What a single-word access returns: the value, and whether it faulted.
#[repr(C)]
struct Word {
    value: u64,
    faulted: u64,
}

unsafe extern "C" {
    fn abyss_user_load32(addr: u64) -> Word;
    fn abyss_user_load64(addr: u64) -> Word;
    fn abyss_user_store32(addr: u64, v: u64) -> Word;
    fn abyss_user_cas32(addr: u64, old: u64, new: u64) -> Word;
    fn abyss_user_cas64(addr: u64, old: u64, new: u64) -> Word;
    fn abyss_user_cas32_pan(addr: u64, old: u64, new: u64) -> Word;
    fn abyss_user_cas64_pan(addr: u64, old: u64, new: u64) -> Word;
}

fn word(w: Word) -> Result<u64, Fault> {
    if w.faulted == 0 {
        Ok(w.value)
    } else {
        Err(Fault)
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "a 32-bit access leaves the upper half clear"
)]
fn word32(w: Word) -> Result<u32, Fault> {
    word(w).map(|v| v as u32)
}

/// Reads the aligned 32-bit word at user address `addr` atomically.
///
/// # Safety
///
/// `[addr, addr + 4)` must lie below [`USER_END`].
pub unsafe fn load_u32(addr: u64) -> Result<u32, Fault> {
    // SAFETY: the one user access has a fixup.
    word32(unsafe { abyss_user_load32(addr) })
}

/// Reads the aligned 64-bit word at user address `addr` atomically.
///
/// # Safety
///
/// `[addr, addr + 8)` must lie below [`USER_END`].
pub unsafe fn load_u64(addr: u64) -> Result<u64, Fault> {
    // SAFETY: the one user access has a fixup.
    word(unsafe { abyss_user_load64(addr) })
}

/// Writes the aligned 32-bit word at user address `addr` atomically.
///
/// # Safety
///
/// `[addr, addr + 4)` must lie below [`USER_END`].
pub unsafe fn store_u32(addr: u64, v: u32) -> Result<(), Fault> {
    // SAFETY: the one user access has a fixup.
    word(unsafe { abyss_user_store32(addr, u64::from(v)) }).map(|_| ())
}

/// Replaces the aligned 32-bit word at user address `addr` with `new` if
/// it holds `old`, atomically (acquire and release). Returns what it
/// held: `old` exactly when the swap happened.
///
/// # Safety
///
/// `[addr, addr + 4)` must lie below [`USER_END`].
pub unsafe fn cas_u32(addr: u64, old: u32, new: u32) -> Result<u32, Fault> {
    // SAFETY: the loop's user accesses have fixups, and PAN is restored
    // on every way out.
    let w = unsafe {
        if PAN.load(Ordering::Relaxed) {
            abyss_user_cas32_pan(addr, u64::from(old), u64::from(new))
        } else {
            abyss_user_cas32(addr, u64::from(old), u64::from(new))
        }
    };
    word32(w)
}

/// [`cas_u32`] for a 64-bit word.
///
/// # Safety
///
/// `[addr, addr + 8)` must lie below [`USER_END`].
pub unsafe fn cas_u64(addr: u64, old: u64, new: u64) -> Result<u64, Fault> {
    // SAFETY: as for `cas_u32`.
    let w = unsafe {
        if PAN.load(Ordering::Relaxed) {
            abyss_user_cas64_pan(addr, old, new)
        } else {
            abyss_user_cas64(addr, old, new)
        }
    };
    word(w)
}

// A user program for the kernel's tests (no libc): `write(1, "hello\n", 6)`,
// then `kill(0, 1000)` (no such signal), then `exit(w * 1000 + e + c + x)`
// where `w` is what `write` returned, `e` the failed call's error number,
// `c` 5000 if the carry flag (error) was clear, and `x` 7 if `write` left
// its second result register (x1, which held the buffer) nonzero. A
// correct kernel exits with 6022 (EINVAL).
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
    mov x22, #0
    cbz x1, 3f
    mov x22, #7
3:  mov x8, #37
    mov x0, #0
    mov x1, #1000
    svc #0
    mov x20, x0
    mov x21, #0
    b.cs 1f
    mov x21, #5000
1:  mov x9, #1000
    mul x0, x19, x9
    add x0, x0, x20
    add x0, x0, x21
    add x0, x0, x22
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
