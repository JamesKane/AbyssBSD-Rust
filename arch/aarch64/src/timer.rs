// SPDX-License-Identifier: BSD-2-Clause
//! The generic timer: the virtual counter as the clock, and this CPU's
//! virtual timer (EL1) as its event timer.

use core::arch::asm;

use crate::cpu::read_sysreg;

/// The virtual count, ordered after earlier instructions.
pub fn counter() -> u64 {
    let v: u64;
    // SAFETY: ISB keeps the read from being hoisted; the read is harmless.
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nostack)) };
    v
}

/// The counter frequency in Hz.
pub fn frequency() -> u64 {
    read_sysreg!("cntfrq_el0")
}

/// Arms this CPU's virtual timer for count `deadline` (fires at once if it
/// has passed).
pub fn set_deadline(deadline: u64) {
    // SAFETY: programming this CPU's own timer.
    unsafe {
        asm!(
            "msr cntv_cval_el0, {}",
            "msr cntv_ctl_el0, {}",
            "isb",
            in(reg) deadline,
            in(reg) 1u64,
            options(nostack),
        );
    }
}

/// Disarms this CPU's virtual timer (masks its output so the level
/// interrupt drops).
pub fn cancel_deadline() {
    // SAFETY: as above.
    unsafe { asm!("msr cntv_ctl_el0, {}", "isb", in(reg) 2u64, options(nostack)) };
}
