// SPDX-License-Identifier: BSD-2-Clause
//! AES with the ARMv8 cryptographic extension (AESE, AESMC) and GHASH
//! with PMULL, for `sys/crypto`.
//!
//! Each routine is an assembly function with the C calling convention
//! and pointer arguments only, so the soft-float kernel calls it like any
//! other. It uses only v0-v7 and v16-v31, whose contents a caller may not
//! keep across a call (AAPCS64 §6.1.2). The caller holds a kernel FP/SIMD
//! section (`sched::simd`), as FreeBSD's armv8crypto driver holds
//! `fpu_kern_enter`.
//!
//! Round keys are FIPS 197 byte strings (`crypto::aes`). GHASH works on
//! byte-reversed blocks, where the bit-reflected field elements become
//! ordinary carry-less products shifted one bit, reduced as x86_64's is.

use core::arch::global_asm;

global_asm!(
    ".pushsection .text.abyss_crypto, \"ax\"",
    ".arch_extension fp",
    ".arch_extension simd",
    ".arch_extension aes",
    // Encrypts v0 with the `x1` rounds of keys at `x0`; uses x9, x10, v1.
    ".macro abyss_armv8_enc",
    "mov x9, x0",
    "sub x10, x1, #1",
    "1:",
    "ld1 {{v1.16b}}, [x9], #16",
    "aese v0.16b, v1.16b",
    "aesmc v0.16b, v0.16b",
    "subs x10, x10, #1",
    "b.ne 1b",
    "ld1 {{v1.16b}}, [x9], #16",
    "aese v0.16b, v1.16b",
    "ld1 {{v1.16b}}, [x9]",
    "eor v0.16b, v0.16b, v1.16b",
    ".endm",
    // Decrypts v0 likewise with the equivalent inverse cipher's keys.
    ".macro abyss_armv8_dec",
    "mov x9, x0",
    "sub x10, x1, #1",
    "1:",
    "ld1 {{v1.16b}}, [x9], #16",
    "aesd v0.16b, v1.16b",
    "aesimc v0.16b, v0.16b",
    "subs x10, x10, #1",
    "b.ne 1b",
    "ld1 {{v1.16b}}, [x9], #16",
    "aesd v0.16b, v1.16b",
    "ld1 {{v1.16b}}, [x9]",
    "eor v0.16b, v0.16b, v1.16b",
    ".endm",
    // Reverses the bytes of a whole register.
    ".macro abyss_bswap128 r",
    "rev64 \\r\\().16b, \\r\\().16b",
    "ext \\r\\().16b, \\r\\().16b, \\r\\().16b, #8",
    ".endm",
    //
    // abyss_aes_ecb_encrypt(rk, rounds, data, blocks)
    ".global abyss_aes_ecb_encrypt",
    ".type abyss_aes_ecb_encrypt, %function",
    "abyss_aes_ecb_encrypt:",
    "cbz x3, 9f",
    "2:",
    "ld1 {{v0.16b}}, [x2]",
    "abyss_armv8_enc",
    "st1 {{v0.16b}}, [x2], #16",
    "subs x3, x3, #1",
    "b.ne 2b",
    "9:",
    "ret",
    ".size abyss_aes_ecb_encrypt, . - abyss_aes_ecb_encrypt",
    //
    // abyss_aes_ctr32(rk, rounds, ctr, data, blocks)
    ".global abyss_aes_ctr32",
    ".type abyss_aes_ctr32, %function",
    "abyss_aes_ctr32:",
    "cbz x4, 9f",
    "ldr w5, [x2, #12]",
    "rev w5, w5",
    "2:",
    "ld1 {{v0.16b}}, [x2]",
    "abyss_armv8_enc",
    "ld1 {{v2.16b}}, [x3]",
    "eor v2.16b, v2.16b, v0.16b",
    "st1 {{v2.16b}}, [x3], #16",
    "add w5, w5, #1",
    "rev w6, w5",
    "str w6, [x2, #12]",
    "subs x4, x4, #1",
    "b.ne 2b",
    "9:",
    "ret",
    ".size abyss_aes_ctr32, . - abyss_aes_ctr32",
    //
    // abyss_aes_cbc_encrypt(rk, rounds, iv, data, blocks)
    ".global abyss_aes_cbc_encrypt",
    ".type abyss_aes_cbc_encrypt, %function",
    "abyss_aes_cbc_encrypt:",
    "cbz x4, 9f",
    "ld1 {{v0.16b}}, [x2]",
    "2:",
    "ld1 {{v2.16b}}, [x3]",
    "eor v0.16b, v0.16b, v2.16b",
    "abyss_armv8_enc",
    "st1 {{v0.16b}}, [x3], #16",
    "subs x4, x4, #1",
    "b.ne 2b",
    "st1 {{v0.16b}}, [x2]",
    "9:",
    "ret",
    ".size abyss_aes_cbc_encrypt, . - abyss_aes_cbc_encrypt",
    //
    // abyss_aes_cbc_decrypt(dk, rounds, iv, data, blocks)
    ".global abyss_aes_cbc_decrypt",
    ".type abyss_aes_cbc_decrypt, %function",
    "abyss_aes_cbc_decrypt:",
    "cbz x4, 9f",
    "ld1 {{v3.16b}}, [x2]",
    "2:",
    "ld1 {{v0.16b}}, [x3]",
    "mov v2.16b, v0.16b",
    "abyss_armv8_dec",
    "eor v0.16b, v0.16b, v3.16b",
    "st1 {{v0.16b}}, [x3], #16",
    "mov v3.16b, v2.16b",
    "subs x4, x4, #1",
    "b.ne 2b",
    "st1 {{v3.16b}}, [x2]",
    "9:",
    "ret",
    ".size abyss_aes_cbc_decrypt, . - abyss_aes_cbc_decrypt",
    //
    // abyss_ghash(h, y, data, blocks): y = (y ^ block) * h for each block.
    ".global abyss_ghash",
    ".type abyss_ghash, %function",
    "abyss_ghash:",
    "movi v7.2d, #0",
    "ld1 {{v1.16b}}, [x0]",
    "abyss_bswap128 v1",
    "ext v17.16b, v1.16b, v1.16b, #8",
    "ld1 {{v0.16b}}, [x1]",
    "abyss_bswap128 v0",
    "cbz x3, 9f",
    "2:",
    "ld1 {{v2.16b}}, [x2], #16",
    "abyss_bswap128 v2",
    "eor v0.16b, v0.16b, v2.16b",
    // The 256-bit product [v4:v3] = [X3:X2:X1:X0].
    "pmull v3.1q, v0.1d, v1.1d",
    "pmull2 v4.1q, v0.2d, v1.2d",
    "pmull v5.1q, v0.1d, v17.1d",
    "pmull2 v6.1q, v0.2d, v17.2d",
    "eor v5.16b, v5.16b, v6.16b",
    "ext v6.16b, v7.16b, v5.16b, #8",
    "eor v3.16b, v3.16b, v6.16b",
    "ext v6.16b, v5.16b, v7.16b, #8",
    "eor v4.16b, v4.16b, v6.16b",
    // Shift left one bit: the reflected product is one bit short.
    "ushr v5.2d, v3.2d, #63",
    "ushr v6.2d, v4.2d, #63",
    "shl v3.2d, v3.2d, #1",
    "shl v4.2d, v4.2d, #1",
    "ext v18.16b, v7.16b, v5.16b, #8",
    "orr v3.16b, v3.16b, v18.16b",
    "ext v18.16b, v5.16b, v7.16b, #8",
    "orr v4.16b, v4.16b, v18.16b",
    "ext v18.16b, v7.16b, v6.16b, #8",
    "orr v4.16b, v4.16b, v18.16b",
    // Reduce: D = X1 ^ X0<<63 ^ X0<<62 ^ X0<<57, so v3 = [D:X0] ...
    "shl v5.2d, v3.2d, #63",
    "shl v6.2d, v3.2d, #62",
    "shl v18.2d, v3.2d, #57",
    "eor v5.16b, v5.16b, v6.16b",
    "eor v5.16b, v5.16b, v18.16b",
    "ext v5.16b, v7.16b, v5.16b, #8",
    "eor v3.16b, v3.16b, v5.16b",
    // ... then [X3:X2] ^= [D:X0] ^ [D:X0]>>1 ^ >>2 ^ >>7 (128-bit shifts).
    "ushr v5.2d, v3.2d, #1",
    "ushr v6.2d, v3.2d, #2",
    "ushr v18.2d, v3.2d, #7",
    "eor v5.16b, v5.16b, v6.16b",
    "eor v5.16b, v5.16b, v18.16b",
    "shl v6.2d, v3.2d, #63",
    "shl v18.2d, v3.2d, #62",
    "shl v19.2d, v3.2d, #57",
    "eor v6.16b, v6.16b, v18.16b",
    "eor v6.16b, v6.16b, v19.16b",
    "ext v6.16b, v6.16b, v7.16b, #8",
    "eor v5.16b, v5.16b, v6.16b",
    "eor v3.16b, v3.16b, v5.16b",
    "eor v0.16b, v4.16b, v3.16b",
    "subs x3, x3, #1",
    "b.ne 2b",
    "9:",
    "abyss_bswap128 v0",
    "st1 {{v0.16b}}, [x1]",
    "ret",
    ".size abyss_ghash, . - abyss_ghash",
    ".purgem abyss_armv8_enc",
    ".purgem abyss_armv8_dec",
    ".purgem abyss_bswap128",
    ".popsection",
);

/// Whether the CPU has AES and PMULL (`ID_AA64ISAR0_EL1.AES` is 2).
#[must_use]
pub fn available() -> bool {
    (crate::cpu::read_sysreg!("id_aa64isar0_el1") >> 4) & 0xf >= 2
}
