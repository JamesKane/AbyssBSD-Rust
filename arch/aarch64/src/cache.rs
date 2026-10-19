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

/// Makes code written at `[addr, addr + len)` visible to instruction
/// fetches on every CPU (the inner-shareable domain): cleans the data
/// cache to the point of unification and invalidates the instruction
/// cache by address, each skipped when `CTR_EL0` says the hardware keeps
/// them coherent (IDC, DIC).
pub fn sync_icache(addr: usize, len: usize) {
    let ctr: u64;
    // SAFETY: CTR_EL0 is readable at EL1 and has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let (dline, iline) = line_sizes();
    let end = addr.saturating_add(len);
    if ctr & (1 << 28) == 0 {
        let mut p = addr & !(dline - 1);
        while p < end {
            // SAFETY: cleaning a line leaves memory contents unchanged.
            unsafe { asm!("dc cvau, {}", in(reg) p, options(nostack)) };
            p += dline;
        }
    }
    // SAFETY: a barrier.
    unsafe { asm!("dsb ish", options(nostack)) };
    if ctr & (1 << 29) == 0 {
        let mut p = addr & !(iline - 1);
        while p < end {
            // SAFETY: invalidating instruction cache lines has no effect
            // on memory.
            unsafe { asm!("ic ivau, {}", in(reg) p, options(nostack)) };
            p += iline;
        }
        // SAFETY: a barrier.
        unsafe { asm!("dsb ish", options(nostack)) };
    }
    // SAFETY: a barrier.
    unsafe { asm!("isb", options(nostack)) };
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

/// Cleans `[addr, addr + len)` to the point of coherency, so an agent
/// reading memory with caches off (a CPU starting with its MMU off) sees
/// what was written.
pub fn clean_to_poc(addr: usize, len: usize) {
    let (dline, _) = line_sizes();
    let end = addr.saturating_add(len);
    let mut p = addr & !(dline - 1);
    while p < end {
        // SAFETY: as in `prepare_code`.
        unsafe { asm!("dc cvac, {}", in(reg) p, options(nostack)) };
        p += dline;
    }
    // SAFETY: a barrier.
    unsafe { asm!("dsb sy", options(nostack)) };
}

/// Cleans and invalidates `[addr, addr + len)` to the point of coherency,
/// so the CPU next reads what a non-coherent device wrote. Clean and
/// invalidate rather than invalidate alone: lines at the range's edges may
/// hold the CPU's own data beside the buffer.
pub fn invalidate_to_poc(addr: usize, len: usize) {
    let (dline, _) = line_sizes();
    let end = addr.saturating_add(len);
    let mut p = addr & !(dline - 1);
    while p < end {
        // SAFETY: cleaning and invalidating a line writes back dirty data
        // first, so no memory contents are lost.
        unsafe { asm!("dc civac, {}", in(reg) p, options(nostack)) };
        p += dline;
    }
    // SAFETY: a barrier.
    unsafe { asm!("dsb sy", options(nostack)) };
}
