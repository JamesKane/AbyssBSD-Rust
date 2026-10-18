// SPDX-License-Identifier: BSD-2-Clause
//! Reporting a test verdict as QEMU's exit status, when the command line
//! asks for it (`abyss.qemu_exit=1`, set by `cargo xtask test`). x86 uses
//! the `isa-debug-exit` device; aarch64 uses semihosting. Both are absent
//! (and the semihosting call would trap) on other machines, hence the flag.

/// Exits QEMU with a status reflecting `passed`, if enabled; otherwise
/// returns.
pub(crate) fn exit(passed: bool) {
    if !crate::boot::flag("abyss.qemu_exit") {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    {
        // QEMU exits with (value << 1) | 1: 33 for pass, 35 for fail.
        let value = if passed { 0x10 } else { 0x11 };
        // SAFETY: xtask attaches isa-debug-exit at port 0xf4 whenever it
        // sets the flag.
        unsafe { crate::arch::cpu::outl(0xf4, value) };
    }
    #[cfg(target_arch = "aarch64")]
    crate::arch::cpu::semihosting_exit(if passed { 0 } else { 1 });
}
