// SPDX-License-Identifier: BSD-2-Clause
//! Loading page tables and TLB maintenance.

use core::arch::asm;

use addr::PhysAddr;

/// The table format this architecture uses.
pub type Format = pt::x86_64::X86_64;

/// Largest leaf the MMU supports: 1 GiB if CPUID says so.
pub fn max_leaf() -> pt::PageSize {
    let ext = core::arch::x86_64::__cpuid(0x8000_0000);
    if ext.eax >= 0x8000_0001 {
        let f = core::arch::x86_64::__cpuid(0x8000_0001);
        if f.edx & (1 << 26) != 0 {
            return pt::PageSize::Size1G;
        }
    }
    pt::PageSize::Size2M
}

/// Switches to the kernel tables rooted at `root`, dropping whatever the
/// loader had (including its identity map), and flushes the TLB.
///
/// # Safety
///
/// `root` must map the running code, stack, and all data in use at the same
/// virtual addresses.
pub unsafe fn install_kernel_tables(root: PhysAddr) {
    // SAFETY: per the caller's contract.
    unsafe { asm!("mov cr3, {}", in(reg) root.get(), options(nostack)) };
    flush_all();
}

/// Runs `f` with `root` as the active table, then restores the previous
/// one. Used for UEFI runtime calls, whose table maps the runtime regions
/// 1:1 in the lower half and shares the kernel's upper half.
///
/// # Safety
///
/// `root` must map the running code and stack at the same addresses as the
/// current table. Interrupts must be disabled.
pub unsafe fn with_tables<R>(root: PhysAddr, f: impl FnOnce() -> R) -> R {
    let old = crate::cpu::cr3();
    // SAFETY: per the caller's contract.
    unsafe { asm!("mov cr3, {}", in(reg) root.get(), options(nostack)) };
    let r = f();
    // SAFETY: restoring the table that was live on entry.
    unsafe { asm!("mov cr3, {}", in(reg) old, options(nostack)) };
    r
}

/// Flushes every TLB entry, global ones included.
pub fn flush_all() {
    let cr4 = crate::cpu::cr4();
    // SAFETY: toggling CR4.PGE flushes the TLB and changes nothing else.
    unsafe {
        crate::cpu::set_cr4(cr4 & !(1 << 7));
        crate::cpu::set_cr4(cr4);
    }
}

/// Makes entries that changed from invalid to valid visible to this CPU's
/// table walks. Nothing to do on x86: walks are coherent with stores.
pub fn sync_new_mappings() {}

/// Invalidates this CPU's TLB entries for the page at `va`. Other CPUs
/// need a shootdown.
pub fn flush_page(va: addr::VirtAddr) {
    // SAFETY: INVLPG only drops cached translations.
    unsafe { asm!("invlpg [{}]", in(reg) va.get(), options(nostack)) };
}
