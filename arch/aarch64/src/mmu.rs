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

/// Stops lower-half walks (a starting CPU's identity map) and flushes this
/// CPU's TLB.
///
/// # Safety
///
/// Nothing in use may live in the lower half.
pub unsafe fn disable_lower_half() {
    // SAFETY: per the caller's contract.
    unsafe {
        asm!(
            "mrs {tcr}, tcr_el1",
            "orr {tcr}, {tcr}, {epd0}",
            "msr tcr_el1, {tcr}",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            epd0 = in(reg) TCR_EPD0,
            tcr = out(reg) _,
            options(nostack),
        );
    }
}

/// Runs `f` with `root` as the lower-half table, then puts back the lower
/// half as it was: a user thread's tables (TTBR0 with its ASID, walks
/// enabled), or none (walks disabled). Used for UEFI runtime calls, whose
/// table maps the runtime regions 1:1; the calls are made from system
/// calls (`/dev/efi`), so the caller's user tables must come back.
///
/// # Safety
///
/// Nothing the caller relies on may live in the lower half while `f` runs.
/// Interrupts must be disabled.
pub unsafe fn with_tables<R>(root: PhysAddr, f: impl FnOnce() -> R) -> R {
    let (ttbr0, tcr): (u64, u64);
    // SAFETY: only the lower half changes, and the caller keeps nothing
    // there while `f` runs. The firmware table is tagged ASID 0, and every
    // entry cached for it or for the old tables is dropped on this CPU.
    unsafe {
        asm!(
            "mrs {ttbr0}, ttbr0_el1",
            "mrs {tcr}, tcr_el1",
            "dsb ishst",
            "msr ttbr0_el1, {root}",
            "bic {tmp}, {tcr}, {epd0}",
            "msr tcr_el1, {tmp}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            root = in(reg) root.get(),
            epd0 = in(reg) TCR_EPD0,
            ttbr0 = out(reg) ttbr0,
            tcr = out(reg) tcr,
            tmp = out(reg) _,
            options(nostack),
        );
    }
    // The firmware may use SP_EL0, which holds the running thread
    // (`cpu::current_thread`): kept across the call.
    let sp_el0 = crate::cpu::read_sp_el0();
    let r = f();
    // SAFETY: puts back what it held, the running thread.
    unsafe { asm!("msr sp_el0, {}", in(reg) sp_el0, options(nomem, nostack)) };
    // SAFETY: restores the lower half that was live on entry; the
    // firmware table's entries are dropped from this CPU's TLB.
    unsafe {
        asm!(
            "msr ttbr0_el1, {ttbr0}",
            "msr tcr_el1, {tcr}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            ttbr0 = in(reg) ttbr0,
            tcr = in(reg) tcr,
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

/// Makes entries that changed from invalid to valid visible to table
/// walks: the stores must complete before a walk can observe them.
pub fn sync_new_mappings() {
    // SAFETY: barriers only.
    unsafe { asm!("dsb ishst", "isb", options(nostack)) };
}

/// Invalidates the TLB entries for the page at `va` on every CPU in the
/// inner-shareable domain (the broadcast form of TLBI).
pub fn flush_page(va: addr::VirtAddr) {
    // SAFETY: TLB invalidation has no effect on memory contents.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vaae1is, {}",
            "dsb ish",
            "isb",
            in(reg) (va.get() >> 12) & ((1 << 44) - 1),
            options(nostack),
        );
    }
}
