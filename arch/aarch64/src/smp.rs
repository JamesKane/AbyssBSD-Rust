// SPDX-License-Identifier: BSD-2-Clause
//! Starting the other CPUs with PSCI `CPU_ON`.
//!
//! A started CPU begins at `abyss_ap_start`'s physical address with its
//! MMU off, at EL2 or EL1 like the boot CPU, with `x0` = the physical
//! address of an [`ApBoot`]. It takes the same path as the boot CPU (EL2
//! stub, MMU on under an identity map of the image plus the kernel's upper
//! half), then calls [`ap_start`] on its own stack.
//!
//! Behavior reference: sys/arm64/arm64/mp_machdep.c, sys/dev/psci/psci.c
//! (Arm DEN0022 for PSCI).

use core::arch::asm;

use crate::cpu::CpuBlock;

/// Read by the entry assembly with caches off, at fixed offsets.
#[repr(C)]
#[derive(Debug)]
pub struct ApBoot {
    /// The kernel's upper-half root (TTBR1).
    pub kernel_root: u64,
    /// A table identity-mapping the kernel image (TTBR0), for the moment
    /// the MMU comes on.
    pub identity_root: u64,
    /// The CPU's initial stack pointer (kernel virtual).
    pub stack_top: u64,
    /// The [`ApStart`] address (kernel virtual).
    pub arg: u64,
}

/// What a starting CPU needs once running at kernel addresses.
#[repr(C)]
#[derive(Debug)]
pub struct ApStart {
    /// The logical CPU number to take.
    pub cpu: u32,
    /// The CPU's `TPIDR_EL1` block (leaked by the kernel).
    pub block: *mut CpuBlock,
    /// Passed to the kernel's AP hook.
    pub arg: usize,
}

unsafe extern "C" {
    fn abyss_ap_start();
}

/// The virtual address of the entry point; the kernel converts it to
/// physical for PSCI.
#[must_use]
pub fn entry_point() -> u64 {
    abyss_ap_start as *const () as u64
}

/// The first Rust code on a started CPU.
pub(crate) extern "C" fn ap_start(start: u64) -> ! {
    // SAFETY: the kernel keeps `start` and the block alive and dedicated
    // to this CPU; we run once, with interrupts masked, at kernel
    // addresses, so the identity map can go.
    unsafe {
        let s = &*(start as *const ApStart);
        crate::mmu::disable_lower_half();
        crate::cpu::init_cpu(&mut *s.block, s.cpu);
        crate::trap::init_boot_cpu();
        crate::abyss_ap_main(s.arg)
    }
}

/// How PSCI calls reach firmware (FADT `PSCI_USE_HVC`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conduit {
    /// Secure monitor call.
    Smc,
    /// Hypervisor call.
    Hvc,
}

/// PSCI `CPU_ON` (64-bit): starts the CPU with affinity `mpidr` at
/// physical `entry` with `x0` = `context`. Returns the PSCI status (0 is
/// success).
pub fn psci_cpu_on(conduit: Conduit, mpidr: u64, entry: u64, context: u64) -> i64 {
    const CPU_ON: u64 = 0xc400_0003;
    let ret: i64;
    // SAFETY: a firmware call with the PSCI calling convention; it only
    // starts another CPU.
    unsafe {
        match conduit {
            Conduit::Smc => asm!(
                "smc #0",
                inout("x0") CPU_ON => ret,
                inout("x1") mpidr => _,
                inout("x2") entry => _,
                inout("x3") context => _,
                options(nostack),
            ),
            Conduit::Hvc => asm!(
                "hvc #0",
                inout("x0") CPU_ON => ret,
                inout("x1") mpidr => _,
                inout("x2") entry => _,
                inout("x3") context => _,
                options(nostack),
            ),
        }
    }
    ret
}
