// SPDX-License-Identifier: BSD-2-Clause
//! System registers and CPU control.

use core::arch::asm;

use crate::Racy;

/// Reads a system register by name.
macro_rules! read_sysreg {
    ($name:literal) => {{
        let v: u64;
        // SAFETY: reading this register has no side effects at EL1.
        unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)) };
        v
    }};
}
pub(crate) use read_sysreg;

/// Masks all interrupts (DAIF).
pub fn disable_interrupts() {
    // SAFETY: masking interrupts has no memory effects.
    unsafe { asm!("msr daifset, #0xf", options(nomem, nostack)) };
}

/// Stops this CPU for good.
pub fn halt_forever() -> ! {
    disable_interrupts();
    loop {
        // SAFETY: WFI with interrupts masked just idles.
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

/// The current frame pointer (x29).
#[inline(always)]
pub fn frame_pointer() -> u64 {
    let v;
    // SAFETY: reads a register.
    unsafe { asm!("mov {}, x29", out(reg) v, options(nomem, nostack)) };
    v
}

/// The virtual counter.
pub fn timestamp() -> u64 {
    read_sysreg!("cntvct_el0")
}

/// The counter frequency in Hz.
pub fn timestamp_frequency() -> u64 {
    read_sysreg!("cntfrq_el0")
}

/// `MIDR_EL1`, the CPU's implementer and part number.
pub fn midr() -> u64 {
    read_sysreg!("midr_el1")
}

/// Per-CPU data reached through `TPIDR_EL1`.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct CpuLocal {
    self_ptr: *const CpuLocal,
    cpu: u32,
}

// Written once by `init_boot_cpu`, on the boot CPU, before anything reads it.
static CPU0: Racy<CpuLocal> = Racy::new(CpuLocal {
    self_ptr: core::ptr::null(),
    cpu: 0,
});

/// Points `TPIDR_EL1` at the boot CPU's block.
///
/// # Safety
///
/// Call once, on the boot CPU.
pub(crate) unsafe fn init_boot_cpu() {
    let cpu0 = CPU0.get();
    // SAFETY: single-threaded early boot.
    unsafe {
        (*cpu0).self_ptr = cpu0;
        asm!("msr tpidr_el1, {}", in(reg) cpu0, options(nostack));
    }
}

/// The current CPU's number.
pub fn cpu_id() -> u32 {
    let p = read_sysreg!("tpidr_el1") as *const CpuLocal;
    // SAFETY: TPIDR_EL1 points at this CPU's block from `init_boot_cpu` on.
    unsafe { (*p).cpu }
}

/// Ends a QEMU session through semihosting (`SYS_EXIT_EXTENDED`).
///
/// Only call when QEMU runs with semihosting enabled: elsewhere the `HLT`
/// traps as an undefined instruction.
pub fn semihosting_exit(code: u64) {
    // ADP_Stopped_ApplicationExit and the exit code.
    let block: [u64; 2] = [0x2_0026, code];
    // SAFETY: the semihosting call reads the two-word block and ends the
    // emulator; if it returns, nothing was changed.
    unsafe {
        asm!(
            "hlt #0xf000",
            in("x0") 0x20u64,
            in("x1") block.as_ptr(),
            options(nostack, readonly),
        );
    }
}
