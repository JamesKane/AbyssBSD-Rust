// SPDX-License-Identifier: BSD-2-Clause
//! Loading page tables and TLB maintenance.

use core::arch::asm;

use addr::PhysAddr;

/// The table format this architecture uses.
pub type Format = pt::aarch64::Aarch64;

/// `TCR_EL1.EPD0`: no TTBR0 walks.
const TCR_EPD0: u64 = 1 << 7;

/// Largest leaf the MMU supports with a 4 KiB granule.
pub fn max_leaf() -> pt::PageSize {
    pt::PageSize::Size1G
}

/// Switches the upper half to the kernel tables rooted at `root`, disables
/// the lower half (dropping the loader's identity map), and flushes the
/// TLB.
///
/// # Safety
///
/// `root` must map the running code, stack, and all data in use at the same
/// virtual addresses.
pub unsafe fn install_kernel_tables(root: PhysAddr) {
    // SAFETY: per the caller's contract; nothing in use lives in the lower
    // half once the kernel runs at its linked address.
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr1_el1, {root}",
            "mrs {tcr}, tcr_el1",
            "orr {tcr}, {tcr}, {epd0}",
            "msr tcr_el1, {tcr}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            root = in(reg) root.get(),
            epd0 = in(reg) TCR_EPD0,
            tcr = out(reg) _,
            options(nostack),
        );
    }
}

/// Runs `f` with `root` as the lower-half table, then disables the lower
/// half again. Used for UEFI runtime calls, whose table maps the runtime
/// regions 1:1.
///
/// # Safety
///
/// Nothing the caller relies on may live in the lower half. Interrupts must
/// be disabled.
pub unsafe fn with_tables<R>(root: PhysAddr, f: impl FnOnce() -> R) -> R {
    // SAFETY: only the lower half changes, and the caller keeps nothing
    // there.
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr0_el1, {root}",
            "mrs {tcr}, tcr_el1",
            "bic {tcr}, {tcr}, {epd0}",
            "msr tcr_el1, {tcr}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            root = in(reg) root.get(),
            epd0 = in(reg) TCR_EPD0,
            tcr = out(reg) _,
            options(nostack),
        );
    }
    let r = f();
    // SAFETY: as above.
    unsafe {
        asm!(
            "mrs {tcr}, tcr_el1",
            "orr {tcr}, {tcr}, {epd0}",
            "msr tcr_el1, {tcr}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            epd0 = in(reg) TCR_EPD0,
            tcr = out(reg) _,
            options(nostack),
        );
    }
    r
}

/// Flushes every TLB entry for this CPU's EL1&0 regime.
pub fn flush_all() {
    // SAFETY: TLB invalidation has no effect on memory contents.
    unsafe { asm!("tlbi vmalle1", "dsb ish", "isb", options(nostack)) };
}
