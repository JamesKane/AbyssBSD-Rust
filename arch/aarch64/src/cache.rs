// SPDX-License-Identifier: BSD-2-Clause
//! Cache maintenance (Arm ARM D7.5).

use core::arch::asm;

/// Smallest data and instruction cache line sizes, from `CTR_EL0`.
fn line_sizes() -> (usize, usize) {
    let ctr: u64;
    // SAFETY: CTR_EL0 is readable at EL1 and EL2 and has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let d = 4usize << ((ctr >> 16) & 0xf);
    let i = 4usize << (ctr & 0xf);
    (d, i)
}

/// Makes freshly written code at `[addr, addr + len)` safe to execute,
/// including with the MMU and caches off: cleans the data cache to the
/// point of coherency and invalidates the instruction cache.
pub fn prepare_code(addr: usize, len: usize) {
    let (dline, _) = line_sizes();
    let end = addr.saturating_add(len);
    let mut p = addr & !(dline - 1);
    while p < end {
        // SAFETY: cleaning a line has no architectural effect on memory
        // contents; the range is memory the caller wrote.
        unsafe { asm!("dc cvac, {}", in(reg) p, options(nostack)) };
        p += dline;
    }
    // SAFETY: barriers and a full I-cache invalidate have no effect on
    // memory contents.
    unsafe { asm!("dsb sy", "ic iallu", "dsb sy", "isb", options(nostack)) };
}
