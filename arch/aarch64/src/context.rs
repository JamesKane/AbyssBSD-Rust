// SPDX-License-Identifier: BSD-2-Clause
//! Kernel thread context switching.
//!
//! A saved context holds the AAPCS64 callee-saved registers, the frame
//! pointer, the link register (the resume address), and the stack pointer.
//! The kernel is built soft-float, so no FP/SIMD registers are live across
//! the switch. `hal::Context` word layout: 0..=9 x19..x28, 10 x29, 11 x30,
//! 12 sp.

use core::arch::global_asm;

global_asm!(
    ".pushsection .text.abyss_context_switch, \"ax\"",
    ".global abyss_context_switch",
    ".type abyss_context_switch, %function",
    // x0 = from, x1 = to
    "abyss_context_switch:",
    "stp x19, x20, [x0, #0]",
    "stp x21, x22, [x0, #16]",
    "stp x23, x24, [x0, #32]",
    "stp x25, x26, [x0, #48]",
    "stp x27, x28, [x0, #64]",
    "stp x29, x30, [x0, #80]",
    "mov x9, sp",
    "str x9, [x0, #96]",
    "ldp x19, x20, [x1, #0]",
    "ldp x21, x22, [x1, #16]",
    "ldp x23, x24, [x1, #32]",
    "ldp x25, x26, [x1, #48]",
    "ldp x27, x28, [x1, #64]",
    "ldp x29, x30, [x1, #80]",
    "ldr x9, [x1, #96]",
    "mov sp, x9",
    "ret",
    ".size abyss_context_switch, . - abyss_context_switch",
    // A new thread: x19 = entry, x20 = argument. x29 = 0 ends backtraces.
    ".global abyss_thread_start",
    ".type abyss_thread_start, %function",
    "abyss_thread_start:",
    "mov x0, x20",
    "blr x19",
    "brk #1",
    ".size abyss_thread_start, . - abyss_thread_start",
    ".popsection",
);

unsafe extern "C" {
    fn abyss_thread_start();
}

/// Fills `c` so that switching to it runs `entry(arg)` on `stack_top`.
pub(crate) fn init(c: &mut [u64; 16], stack_top: u64, entry: hal::ThreadEntry, arg: usize) {
    *c = [0; 16];
    c[0] = entry as *const () as u64;
    c[1] = arg as u64;
    c[11] = abyss_thread_start as *const () as u64;
    c[12] = stack_top & !0xf;
}
