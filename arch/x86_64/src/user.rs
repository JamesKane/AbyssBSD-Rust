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
const MSR_TSC_AUX: u32 = 0xc000_0103;
const EFER_SCE: u64 = 1 << 0;

/// RFLAGS bits `syscall` clears on entry: TF, IF, DF, NT, AC.
const SYSCALL_MASK: u64 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 14) | (1 << 18);

unsafe extern "C" {
    fn abyss_syscall_entry();
}

/// Prepares this CPU for user mode: `syscall` entry, `rdfsbase` and
/// friends, SMEP (no executing user pages in kernel mode) and SMAP (no
/// touching them outside the copy routines) where the CPU has them, and
/// the CPU's number in `IA32_TSC_AUX` for `rdtscp` and `rdpid` (libc's
/// `sched_getcpu`). Call on every CPU, after [`crate::gdt::load`].
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
    // FreeBSD's `initializecpu`: `cpu_auxmsr`, the CPU's number.
    let ext = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    let rdtscp =
        ext >= 0x8000_0001 && core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 27) != 0;
    let rdpid = leaf7.ecx & (1 << 22) != 0;
    if rdtscp || rdpid {
        // SAFETY: the MSR exists where either instruction does; it only
        // holds the value they return.
        unsafe { crate::cpu::wrmsr(MSR_TSC_AUX, u64::from(crate::gdt::cpu_id())) };
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

/// The user GS base, which the kernel's `swapgs` keeps in
/// `IA32_KERNEL_GS_BASE` while it runs.
pub fn tls2() -> u64 {
    if FSGSBASE.load(Ordering::Relaxed) {
        let irq = crate::cpu::save_disable_interrupts();
        let v: u64;
        // SAFETY: interrupts are masked, so nothing runs on the user GS in
        // between (an NMI finds out which GS is live by its value).
        unsafe {
            asm!("swapgs", "rdgsbase {}", "swapgs", out(reg) v, options(nomem, nostack));
        }
        if irq {
            crate::cpu::enable_interrupts();
        }
        v
    } else {
        // SAFETY: reading the swapped GS base MSR.
        unsafe { crate::cpu::rdmsr(crate::cpu::MSR_KERNEL_GS_BASE) }
    }
}

/// Sets the user GS base (see [`tls2`]).
///
/// # Safety
///
/// `v` must be canonical; the kernel does not use the user's GS.
pub unsafe fn set_tls2(v: u64) {
    if FSGSBASE.load(Ordering::Relaxed) {
        let irq = crate::cpu::save_disable_interrupts();
        // SAFETY: as in `tls2`.
        unsafe {
            asm!("swapgs", "wrgsbase {}", "swapgs", in(reg) v, options(nomem, nostack));
        }
        if irq {
            crate::cpu::enable_interrupts();
        }
    } else {
        // SAFETY: per the caller; the swapped GS base is the user's.
        unsafe { crate::cpu::wrmsr(crate::cpu::MSR_KERNEL_GS_BASE, v) };
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

// The copy between user and kernel memory: rdi = dst, rsi = src, rdx =
// len; returns 0, or 1 on a fault. As FreeBSD's `MEMMOVE` (amd64
// support.S): up to 32 bytes in overlapping words, up to 256 in a loop of
// 32, and only more with `rep movsb`, whose startup a small copy (a
// `msghdr`, an iovec, a control message) would mostly be. Every access to
// memory may fault. The `_smap` form opens user access around it.
macro_rules! user_copy {
    ($suffix:literal, $open:literal, $close:literal) => {
        global_asm!(concat!(
            r#"
    .section .text.abyss_user, "ax"

    .global abyss_user_copy"#,
            $suffix,
            r#"
    .type abyss_user_copy"#,
            $suffix,
            r#", @function
abyss_user_copy"#,
            $suffix,
            r#":
    "#,
            $open,
            r#"
    cmp rdx, 32
    ja 40f
20: cmp edx, 16
    jb 24f
1:  mov r8, [rsi]
2:  mov r9, [rsi + 8]
3:  mov r10, [rsi + rdx - 16]
4:  mov r11, [rsi + rdx - 8]
5:  mov [rdi], r8
6:  mov [rdi + 8], r9
7:  mov [rdi + rdx - 16], r10
8:  mov [rdi + rdx - 8], r11
    jmp 50f
24: cmp edx, 8
    jb 25f
9:  mov r8, [rsi]
10: mov r9, [rsi + rdx - 8]
11: mov [rdi], r8
12: mov [rdi + rdx - 8], r9
    jmp 50f
25: cmp edx, 4
    jb 26f
13: mov r8d, [rsi]
14: mov r9d, [rsi + rdx - 4]
15: mov [rdi], r8d
16: mov [rdi + rdx - 4], r9d
    jmp 50f
26: cmp edx, 2
    jb 27f
17: movzx r8d, word ptr [rsi]
18: movzx r9d, word ptr [rsi + rdx - 2]
19: mov [rdi], r8w
21: mov [rdi + rdx - 2], r9w
    jmp 50f
27: test edx, edx
    jz 50f
22: mov r8b, [rsi]
23: mov [rdi], r8b
    jmp 50f
40: cmp rdx, 256
    ja 45f
28: mov r8, [rsi]
29: mov r9, [rsi + 8]
30: mov r10, [rsi + 16]
31: mov r11, [rsi + 24]
32: mov [rdi], r8
33: mov [rdi + 8], r9
34: mov [rdi + 16], r10
35: mov [rdi + 24], r11
    add rsi, 32
    add rdi, 32
    sub rdx, 32
    cmp rdx, 32
    jae 28b
    jmp 20b
45: mov rcx, rdx
36: rep movsb
50: "#,
            $close,
            r#"
    xor eax, eax
    ret
60: "#,
            $close,
            r#"
    mov eax, 1
    ret
    .size abyss_user_copy"#,
            $suffix,
            r#", . - abyss_user_copy"#,
            $suffix,
            r#"

    .pushsection abyss_extable, "aw"
    .balign 8
    .quad 1b, 60b
    .quad 2b, 60b
    .quad 3b, 60b
    .quad 4b, 60b
    .quad 5b, 60b
    .quad 6b, 60b
    .quad 7b, 60b
    .quad 8b, 60b
    .quad 9b, 60b
    .quad 10b, 60b
    .quad 11b, 60b
    .quad 12b, 60b
    .quad 13b, 60b
    .quad 14b, 60b
    .quad 15b, 60b
    .quad 16b, 60b
    .quad 17b, 60b
    .quad 18b, 60b
    .quad 19b, 60b
    .quad 21b, 60b
    .quad 22b, 60b
    .quad 23b, 60b
    .quad 28b, 60b
    .quad 29b, 60b
    .quad 30b, 60b
    .quad 31b, 60b
    .quad 32b, 60b
    .quad 33b, 60b
    .quad 34b, 60b
    .quad 35b, 60b
    .quad 36b, 60b
    .popsection
    "#
        ));
    };
}
user_copy!("", "", "");
user_copy!("_smap", "stac", "clac");

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

// Single-word user accesses for `_umtx_op`: each one instruction, so
// atomic, unlike the copy routines' `rep movsb`. Each returns the value
// (or the word's previous value) in rax and 1 in rdx on a fault. The
// `_smap` forms open user access for their one instruction.
macro_rules! user_words {
    ($suffix:literal, $open:literal, $close:literal) => {
        global_asm!(concat!(
            r#"
    .section .text.abyss_user, "ax"

    /* rdi = addr */
    .global abyss_user_load32"#,
            $suffix,
            r#"
abyss_user_load32"#,
            $suffix,
            r#":
    "#,
            $open,
            r#"
1:  mov eax, [rdi]
    "#,
            $close,
            r#"
    xor edx, edx
    ret

    .global abyss_user_load64"#,
            $suffix,
            r#"
abyss_user_load64"#,
            $suffix,
            r#":
    "#,
            $open,
            r#"
2:  mov rax, [rdi]
    "#,
            $close,
            r#"
    xor edx, edx
    ret

    /* rdi = addr, rsi = value */
    .global abyss_user_store32"#,
            $suffix,
            r#"
abyss_user_store32"#,
            $suffix,
            r#":
    "#,
            $open,
            r#"
3:  mov [rdi], esi
    "#,
            $close,
            r#"
    xor eax, eax
    xor edx, edx
    ret

    /* rdi = addr, rsi = expected, rdx = new */
    .global abyss_user_cas32"#,
            $suffix,
            r#"
abyss_user_cas32"#,
            $suffix,
            r#":
    mov eax, esi
    "#,
            $open,
            r#"
4:  lock cmpxchg [rdi], edx
    "#,
            $close,
            r#"
    xor edx, edx
    ret

    .global abyss_user_cas64"#,
            $suffix,
            r#"
abyss_user_cas64"#,
            $suffix,
            r#":
    mov rax, rsi
    "#,
            $open,
            r#"
5:  lock cmpxchg [rdi], rdx
    "#,
            $close,
            r#"
    xor edx, edx
    ret

6:  "#,
            $close,
            r#"
    xor eax, eax
    mov edx, 1
    ret

    .pushsection abyss_extable, "aw"
    .balign 8
    .quad 1b, 6b
    .quad 2b, 6b
    .quad 3b, 6b
    .quad 4b, 6b
    .quad 5b, 6b
    .popsection
    "#
        ));
    };
}
user_words!("", "", "");
user_words!("_smap", "stac", "clac");

/// What a single-word access returns: the value, and whether it faulted.
#[repr(C)]
struct Word {
    value: u64,
    faulted: u64,
}

unsafe extern "sysv64" {
    fn abyss_user_load32(addr: u64) -> Word;
    fn abyss_user_load64(addr: u64) -> Word;
    fn abyss_user_store32(addr: u64, v: u64) -> Word;
    fn abyss_user_cas32(addr: u64, old: u64, new: u64) -> Word;
    fn abyss_user_cas64(addr: u64, old: u64, new: u64) -> Word;
    fn abyss_user_load32_smap(addr: u64) -> Word;
    fn abyss_user_load64_smap(addr: u64) -> Word;
    fn abyss_user_store32_smap(addr: u64, v: u64) -> Word;
    fn abyss_user_cas32_smap(addr: u64, old: u64, new: u64) -> Word;
    fn abyss_user_cas64_smap(addr: u64, old: u64, new: u64) -> Word;
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
    let w = unsafe {
        if SMAP.load(Ordering::Relaxed) {
            abyss_user_load32_smap(addr)
        } else {
            abyss_user_load32(addr)
        }
    };
    word32(w)
}

/// Reads the aligned 64-bit word at user address `addr` atomically.
///
/// # Safety
///
/// `[addr, addr + 8)` must lie below [`USER_END`].
pub unsafe fn load_u64(addr: u64) -> Result<u64, Fault> {
    // SAFETY: the one user access has a fixup.
    let w = unsafe {
        if SMAP.load(Ordering::Relaxed) {
            abyss_user_load64_smap(addr)
        } else {
            abyss_user_load64(addr)
        }
    };
    word(w)
}

/// Writes the aligned 32-bit word at user address `addr` atomically.
///
/// # Safety
///
/// `[addr, addr + 4)` must lie below [`USER_END`].
pub unsafe fn store_u32(addr: u64, v: u32) -> Result<(), Fault> {
    // SAFETY: the one user access has a fixup.
    let w = unsafe {
        if SMAP.load(Ordering::Relaxed) {
            abyss_user_store32_smap(addr, u64::from(v))
        } else {
            abyss_user_store32(addr, u64::from(v))
        }
    };
    word(w).map(|_| ())
}

/// Replaces the aligned 32-bit word at user address `addr` with `new` if
/// it holds `old`, atomically (a full barrier). Returns what it held:
/// `old` exactly when the swap happened.
///
/// # Safety
///
/// `[addr, addr + 4)` must lie below [`USER_END`].
pub unsafe fn cas_u32(addr: u64, old: u32, new: u32) -> Result<u32, Fault> {
    // SAFETY: the one user access has a fixup.
    let w = unsafe {
        if SMAP.load(Ordering::Relaxed) {
            abyss_user_cas32_smap(addr, u64::from(old), u64::from(new))
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
    // SAFETY: the one user access has a fixup.
    let w = unsafe {
        if SMAP.load(Ordering::Relaxed) {
            abyss_user_cas64_smap(addr, old, new)
        } else {
            abyss_user_cas64(addr, old, new)
        }
    };
    word(w)
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
