// SPDX-License-Identifier: BSD-2-Clause
//! User address spaces: loading their tables, flushing their TLB entries,
//! keeping the kernel out of them (SMEP, SMAP), and copying across the
//! boundary.
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

const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;

/// Turns on SMEP (no executing user pages in kernel mode) and SMAP (no
/// touching them outside the copy routines) where the CPU has them. Call
/// on every CPU.
pub fn enable_protections() {
    let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
    let mut cr4 = crate::cpu::cr4();
    if leaf7.ebx & (1 << 7) != 0 {
        cr4 |= CR4_SMEP;
    }
    let smap = leaf7.ebx & (1 << 20) != 0;
    if smap {
        cr4 |= CR4_SMAP;
    }
    // SAFETY: both bits only restrict kernel access to user pages, which
    // the kernel makes only through the copy routines below.
    unsafe { crate::cpu::set_cr4(cr4) };
    // Relaxed: every CPU sets the same value before any user memory
    // exists.
    SMAP.store(smap, Ordering::Relaxed);
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
