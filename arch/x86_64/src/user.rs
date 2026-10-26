// SPDX-License-Identifier: BSD-2-Clause
//! User mode: entering it and taking `syscall` back (the entry code is in
//! `trap`), the thread pointer, and user address spaces: loading their
//! tables, flushing their TLB entries, keeping the kernel out of them
//! (SMEP, SMAP), and copying across the boundary.
//!
//! The copy routines are the only kernel code that touches user memory.
//! Each faulting instruction has a fixup entry: the trap handler first
//! offers the fault to the VM (a page not yet faulted in), and only if
//! that fails resumes at the fixup, which reports [`Fault`].

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, Ordering};

use addr::PhysAddr;

/// The end of the user half: 47-bit addresses below the canonical hole.
pub const USER_END: u64 = 0x0000_8000_0000_0000;

/// A user access that faulted and could not be resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault;

/// Whether SMAP is on (so the copy routines must open user access).
static SMAP: AtomicBool = AtomicBool::new(false);
/// Whether `rdfsbase`/`wrfsbase` work (CR4.FSGSBASE).
static FSGSBASE: AtomicBool = AtomicBool::new(false);

const CR4_FSGSBASE: u64 = 1 << 16;
const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;

const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_SFMASK: u32 = 0xc000_0084;
const MSR_FS_BASE: u32 = 0xc000_0100;
const EFER_SCE: u64 = 1 << 0;

/// RFLAGS bits `syscall` clears on entry: TF, IF, DF, NT, AC.
const SYSCALL_MASK: u64 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 14) | (1 << 18);

unsafe extern "C" {
    fn abyss_syscall_entry();
}

/// Prepares this CPU for user mode: `syscall` entry, `rdfsbase` and
/// friends, SMEP (no executing user pages in kernel mode) and SMAP (no
/// touching them outside the copy routines) where the CPU has them. Call
/// on every CPU.
pub fn init_cpu() {
    let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
    let mut cr4 = crate::cpu::cr4();
    let fsgsbase = leaf7.ebx & 1 != 0;
    if fsgsbase {
        cr4 |= CR4_FSGSBASE;
    }
    if leaf7.ebx & (1 << 7) != 0 {
        cr4 |= CR4_SMEP;
    }
    let smap = leaf7.ebx & (1 << 20) != 0;
    if smap {
        cr4 |= CR4_SMAP;
    }
    // SAFETY: SMEP and SMAP only restrict kernel access to user pages,
    // which the kernel makes only through the copy routines below;
    // FSGSBASE lets user mode set its own FS and GS bases, as FreeBSD
    // allows. The MSRs point `syscall` at our entry code, with the
    // selectors the GDT has (`sysret` derives user CS/SS from STAR).
    unsafe {
        crate::cpu::set_cr4(cr4);
        // `sysret` loads CS from STAR[63:48] + 16 and SS from it + 8. On
        // AMD it takes their RPL from the field too, so the field carries
        // RPL 3 (with RPL 0, user code ran with SS 0x18 and the next
        // trap's `iretq` faulted).
        let star =
            (u64::from(crate::gdt::USER_DS) - 8) << 48 | u64::from(crate::gdt::KERNEL_CS) << 32;
        crate::cpu::wrmsr(MSR_STAR, star);
        crate::cpu::wrmsr(MSR_LSTAR, abyss_syscall_entry as *const () as u64);
        crate::cpu::wrmsr(MSR_SFMASK, SYSCALL_MASK);
        let efer = crate::cpu::rdmsr(crate::cpu::MSR_EFER);
        crate::cpu::wrmsr(crate::cpu::MSR_EFER, efer | EFER_SCE);
    }
    // Relaxed: every CPU sets the same values before any user code runs.
    SMAP.store(smap, Ordering::Relaxed);
    FSGSBASE.store(fsgsbase, Ordering::Relaxed);
}

/// The user thread pointer (FS base).
pub fn tls() -> u64 {
    if FSGSBASE.load(Ordering::Relaxed) {
        let v: u64;
        // SAFETY: CR4.FSGSBASE is set; reading the base changes nothing.
        unsafe { asm!("rdfsbase {}", out(reg) v, options(nomem, nostack)) };
        v
    } else {
        // SAFETY: reading the FS base MSR.
        unsafe { crate::cpu::rdmsr(MSR_FS_BASE) }
    }
}

/// Sets the user thread pointer (FS base).
///
/// # Safety
///
/// The kernel must not rely on FS (it does not).
pub unsafe fn set_tls(v: u64) {
    if FSGSBASE.load(Ordering::Relaxed) {
        // SAFETY: CR4.FSGSBASE is set; per the caller, only user mode
        // uses FS.
        unsafe { asm!("wrfsbase {}", in(reg) v, options(nomem, nostack)) };
    } else {
        // SAFETY: as above.
        unsafe { crate::cpu::wrmsr(MSR_FS_BASE, v) };
    }
}

/// The user code and data (stack) selectors.
pub const USER_CS: u16 = crate::gdt::USER_CS;
pub const USER_DS: u16 = crate::gdt::USER_DS;

/// `sigreturn`'s system-call number, which the trampoline makes.
pub const SYS_SIGRETURN: u64 = 417;
/// Where `struct sigframe` keeps the handler (`sf_ahu`) and the context
/// (`sf_uc`).
pub const SIGF_HANDLER: u64 = 0;
pub const SIGF_UC: u64 = 16;

// The signal trampoline, the code of the shared page's vdso (FreeBSD's
// `__vdso_sigcode`). The kernel enters it with `rsp` at the signal frame
// and the handler's arguments set; it calls the handler, then `sigreturn`
// with the frame's context. `sigreturn` does not return; if it fails,
// `hlt` faults.
global_asm!(
    ".section .rodata.sigcode, \"a\"",
    ".balign 16",
    ".global abyss_sigcode",
    "abyss_sigcode:",
    "call qword ptr [rsp + {handler}]",
    "lea rdi, [rsp + {uc}]",
    // A fake return address, where a call would have left one.
    "push 0",
    ".global abyss_sigcode_pushed",
    "abyss_sigcode_pushed:",
    "mov rax, {sigreturn}",
    "syscall",
    "2: hlt",
    "jmp 2b",
    ".global abyss_sigcode_end",
    "abyss_sigcode_end:",
    handler = const SIGF_HANDLER,
    uc = const SIGF_UC,
    sigreturn = const SYS_SIGRETURN,
);

unsafe extern "C" {
    static abyss_sigcode: u8;
    static abyss_sigcode_end: u8;
    static abyss_sigcode_pushed: u8;
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

/// How far into the trampoline its fake return address has been pushed,
/// from where the CFA is 8 bytes further up (the vdso's call frame
/// information).
#[must_use]
pub fn sigcode_pushed() -> usize {
    let start = &raw const abyss_sigcode;
    let pushed = &raw const abyss_sigcode_pushed;
    // SAFETY: both symbols are in the trampoline, the second after the
    // first.
    unsafe { pushed.offset_from_unsigned(start) }
}

/// A frame that enters user mode at `entry` with stack `sp`, interrupts
/// enabled, and `arg` in `rdi`.
#[must_use]
pub fn user_frame(entry: u64, sp: u64, arg: u64) -> crate::trap::TrapFrame {
    crate::trap::TrapFrame {
        rdi: arg,
        rip: entry,
        cs: u64::from(crate::gdt::USER_CS),
        rflags: 1 << 9,
        rsp: sp,
        ss: u64::from(crate::gdt::USER_DS),
        ..crate::trap::TrapFrame::default()
    }
}

/// Leaves the kernel for user mode with `frame`'s registers.
///
/// # Safety
///
/// The thread must run in a user address space where `frame` makes
/// sense, with its kernel stack set (`hal::user::set_kernel_stack`); its
/// kernel stack above the caller is abandoned. Interrupts must be masked.
pub unsafe fn enter_user(frame: &crate::trap::TrapFrame) -> ! {
    // SAFETY: per the caller's contract; the trap exit path pops `frame`
    // and returns to user mode with `iretq`, swapping GS.
    unsafe {
        asm!(
            "mov rsp, {}",
            "jmp abyss_trap_exit",
            in(reg) core::ptr::from_ref(frame),
            options(noreturn),
        );
    }
}

/// Loads the user tables rooted at `root`, whose upper half is the
/// kernel's. Non-global TLB entries go with the old tables.
///
/// # Safety
///
/// `root` must map the kernel exactly as the current tables do.
pub unsafe fn activate(root: PhysAddr) {
    // SAFETY: per the caller's contract, only the lower half changes.
    unsafe { asm!("mov cr3, {}", in(reg) root.get(), options(nostack)) };
}

/// Drops all of this CPU's user TLB entries. Reloading CR3 leaves the
/// kernel's global entries alone, and a hypervisor with nested paging
/// does not trap it (it traps a CR4.PGE toggle, which flushes those too).
pub fn flush_user() {
    let root = crate::cpu::cr3();
    // SAFETY: reloading the current root changes no mapping.
    unsafe { asm!("mov cr3, {}", in(reg) root, options(nostack)) };
}

/// Drops this CPU's TLB entry for user page `va`.
pub fn flush_page(va: u64) {
    // SAFETY: INVLPG only drops a cached translation.
    unsafe { asm!("invlpg [{}]", in(reg) va, options(nostack)) };
}

global_asm!(
    r#"
    .section .text.abyss_user, "ax"

    /* rdi = dst, rsi = src, rdx = len; returns 0, or 1 on a fault. */
    .global abyss_user_copy
abyss_user_copy:
    mov rcx, rdx
1:  rep movsb
    xor eax, eax
    ret
2:  mov eax, 1
    ret

    /* The same, with user access opened for SMAP. */
    .global abyss_user_copy_smap
abyss_user_copy_smap:
    mov rcx, rdx
    stac
3:  rep movsb
    clac
    xor eax, eax
    ret
4:  clac
    mov eax, 1
    ret

    .pushsection abyss_extable, "aw"
    .balign 8
    .quad 1b, 2b
    .quad 3b, 4b
    .popsection
    "#
);

unsafe extern "sysv64" {
    fn abyss_user_copy(dst: *mut u8, src: *const u8, len: usize) -> u64;
    fn abyss_user_copy_smap(dst: *mut u8, src: *const u8, len: usize) -> u64;
}

/// Copies `len` bytes between user and kernel memory; one side is user
/// memory, already checked to lie below [`USER_END`].
///
/// # Safety
///
/// The kernel side must be valid for `len` bytes (writable if `dst`).
unsafe fn copy(dst: *mut u8, src: *const u8, len: usize) -> Result<(), Fault> {
    // SAFETY: per the caller's contract for the kernel side; every access
    // to the user side is covered by a fixup.
    let faulted = unsafe {
        if SMAP.load(Ordering::Relaxed) {
            abyss_user_copy_smap(dst, src, len)
        } else {
            abyss_user_copy(dst, src, len)
        }
    };
    if faulted == 0 { Ok(()) } else { Err(Fault) }
}

/// Copies `len` bytes from user address `src` to `dst`.
///
/// # Safety
///
/// `dst` must be valid for writes of `len` bytes, and `[src, src + len)`
/// must lie below [`USER_END`].
pub unsafe fn copy_from_user(dst: *mut u8, src: u64, len: usize) -> Result<(), Fault> {
    // SAFETY: per the caller's contract.
    unsafe { copy(dst, src as *const u8, len) }
}

/// Copies `len` bytes from `src` to user address `dst`.
///
/// # Safety
///
/// `src` must be valid for reads of `len` bytes, and `[dst, dst + len)`
/// must lie below [`USER_END`].
pub unsafe fn copy_to_user(dst: u64, src: *const u8, len: usize) -> Result<(), Fault> {
    // SAFETY: per the caller's contract.
    unsafe { copy(dst as *mut u8, src, len) }
}

// A user program for the kernel's tests (no libc): `write(1, "hello\n", 6)`,
// then `kill(0, 1000)` (no such signal), then `exit(w * 1000 + e + c)`
// where `w` is what `write` returned, `e` the failed call's error number,
// and `c` 5000 if the carry flag (error) was clear. A correct kernel exits
// with 6022 (EINVAL).
global_asm!(
    r#"
    .section .rodata.abyss_user_test, "a"
    .global abyss_user_test_start
abyss_user_test_start:
    mov eax, 4
    mov edi, 1
    lea rsi, [rip + 2f]
    mov edx, 6
    syscall
    mov r12, rax
    mov eax, 37
    xor edi, edi
    mov esi, 1000
    syscall
    mov r13, rax
    mov r14d, 0
    jc 1f
    mov r14d, 5000
1:  imul rdi, r12, 1000
    add rdi, r13
    add rdi, r14
    mov eax, 1
    syscall
    ud2
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

// A second test program: sets its TLS base (FS) with
// `sysarch(AMD64_SET_TLSBASE)` to a stack slot holding 0x1234, reads it
// back through `fs:[0]`, checks `sysarch(AMD64_GET_TLSBASE)`, and exits
// with 42 if all held (1 otherwise).
global_asm!(
    r#"
    .section .rodata.abyss_user_test, "a"
    .global abyss_tls_test_start
abyss_tls_test_start:
    sub rsp, 64
    mov qword ptr [rsp], 0x1234
    mov [rsp + 8], rsp
    mov eax, 165
    mov edi, 136
    lea rsi, [rsp + 8]
    syscall
    jc 1f
    mov rax, fs:[0]
    cmp rax, 0x1234
    jne 1f
    mov eax, 165
    mov edi, 135
    lea rsi, [rsp + 16]
    syscall
    jc 1f
    cmp [rsp + 16], rsp
    jne 1f
    mov edi, 42
    mov eax, 1
    syscall
1:  mov edi, 1
    mov eax, 1
    syscall
    ud2
    .global abyss_tls_test_end
abyss_tls_test_end:
    "#
);

unsafe extern "C" {
    static abyss_tls_test_start: u8;
    static abyss_tls_test_end: u8;
}

/// The TLS test program's position-independent machine code.
#[must_use]
pub fn tls_test_program() -> &'static [u8] {
    let start = &raw const abyss_tls_test_start;
    let end = &raw const abyss_tls_test_end;
    // SAFETY: as for `test_program`.
    unsafe { core::slice::from_raw_parts(start, end as usize - start as usize) }
}
