// SPDX-License-Identifier: BSD-2-Clause
//! Kernel thread context switching.
//!
//! A saved context holds the System V callee-saved registers, the stack
//! pointer, and the resume address; caller-saved registers are dead across
//! the call to the switch routine. `hal::Context` word layout:
//! 0 rbx, 1 rbp, 2 r12, 3 r13, 4 r14, 5 r15, 6 rsp, 7 rip.

use core::arch::global_asm;

global_asm!(
    ".pushsection .text.abyss_context_switch, \"ax\"",
    ".global abyss_context_switch",
    ".type abyss_context_switch, @function",
    // rdi = from, rsi = to
    "abyss_context_switch:",
    "mov [rdi + 0x00], rbx",
    "mov [rdi + 0x08], rbp",
    "mov [rdi + 0x10], r12",
    "mov [rdi + 0x18], r13",
    "mov [rdi + 0x20], r14",
    "mov [rdi + 0x28], r15",
    "mov [rdi + 0x30], rsp",
    "lea rax, [rip + 2f]",
    "mov [rdi + 0x38], rax",
    "mov rbx, [rsi + 0x00]",
    "mov rbp, [rsi + 0x08]",
    "mov r12, [rsi + 0x10]",
    "mov r13, [rsi + 0x18]",
    "mov r14, [rsi + 0x20]",
    "mov r15, [rsi + 0x28]",
    "mov rsp, [rsi + 0x30]",
    "jmp [rsi + 0x38]",
    // Resumed: the stack holds our caller's return address.
    "2:",
    "ret",
    ".size abyss_context_switch, . - abyss_context_switch",
    // A new thread: r12 = entry, r13 = argument. rbp = 0 ends backtraces.
    ".global abyss_thread_start",
    ".type abyss_thread_start, @function",
    "abyss_thread_start:",
    "xor ebp, ebp",
    "mov rdi, r13",
    "call r12",
    "ud2",
    ".size abyss_thread_start, . - abyss_thread_start",
    ".popsection",
);

unsafe extern "C" {
    fn abyss_thread_start();
}

/// Fills `c` so that switching to it runs `entry(arg)` on `stack_top`.
pub(crate) fn init(c: &mut [u64; 16], stack_top: u64, entry: hal::ThreadEntry, arg: usize) {
    *c = [0; 16];
    c[2] = entry as *const () as u64;
    c[3] = arg as u64;
    // `call` in the trampoline leaves the callee's stack at 8 mod 16.
    c[6] = stack_top & !0xf;
    c[7] = abyss_thread_start as *const () as u64;
}
