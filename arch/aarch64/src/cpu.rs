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

/// Masks IRQs and FIQs, returning whether IRQs were enabled. A compiler
/// barrier: memory accesses do not move across it.
pub fn save_disable_interrupts() -> bool {
    let daif: u64;
    // SAFETY: reads DAIF and masks interrupts.
    unsafe { asm!("mrs {}, daif", "msr daifset, #3", out(reg) daif, options(nostack)) };
    daif & (1 << 7) == 0
}

/// Unmasks IRQs and FIQs. A compiler barrier.
pub fn enable_interrupts() {
    // SAFETY: the caller is ready for interrupts.
    unsafe { asm!("msr daifclr, #3", options(nostack)) };
}

/// Whether IRQs are enabled.
pub fn interrupts_enabled() -> bool {
    read_sysreg!("daif") & (1 << 7) == 0
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

/// `MPIDR_EL1`, the CPU's affinity (hardware ID).
pub fn mpidr() -> u64 {
    read_sysreg!("mpidr_el1") & 0xff_00ff_ffff
}

/// Per-CPU data reached through `TPIDR_EL1`.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct CpuLocal {
    self_ptr: *const CpuLocal,
    cpu: u32,
    /// The kernel's per-CPU data (`hal::cpu_local`).
    kernel: *mut (),
    /// The running thread (`hal::current_thread`).
    thread: *const (),
    /// Critical-section depth (`hal::critical`).
    critical: u32,
    /// A preemption is owed (`hal::critical::owe_preempt`).
    owed: u32,
    /// Interrupt nesting (`hal::interrupt`).
    interrupts: u32,
}

// Written once by `init_boot_cpu`, on the boot CPU, before anything reads it.
static CPU0: Racy<CpuLocal> = Racy::new(CpuLocal {
    self_ptr: core::ptr::null(),
    cpu: 0,
    kernel: core::ptr::null_mut(),
    thread: core::ptr::null(),
    critical: 0,
    owed: 0,
    interrupts: 0,
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

/// One CPU's `TPIDR_EL1` block. The boot CPU's is static; the kernel
/// allocates one per other CPU. All-zero memory is a valid unused block.
#[derive(Debug)]
pub struct CpuBlock(CpuLocal);

impl CpuBlock {
    /// An unused block.
    #[must_use]
    pub const fn new() -> Self {
        Self(CpuLocal {
            self_ptr: core::ptr::null(),
            cpu: 0,
            kernel: core::ptr::null_mut(),
            thread: core::ptr::null(),
            critical: 0,
            owed: 0,
            interrupts: 0,
        })
    }
}

impl Default for CpuBlock {
    fn default() -> Self {
        Self::new()
    }
}

/// Points `TPIDR_EL1` at `block`, as CPU `cpu`.
///
/// # Safety
///
/// Call once per CPU; `block` must live forever and belong to this CPU.
pub(crate) unsafe fn init_cpu(block: &'static mut CpuBlock, cpu: u32) {
    block.0.cpu = cpu;
    block.0.self_ptr = &raw const block.0;
    // SAFETY: per the contract.
    unsafe { asm!("msr tpidr_el1, {}", in(reg) &raw const block.0, options(nostack)) };
}

/// The current CPU's number.
pub fn cpu_id() -> u32 {
    let p = read_sysreg!("tpidr_el1") as *const CpuLocal;
    // SAFETY: TPIDR_EL1 points at this CPU's block from `init_boot_cpu` on.
    unsafe { (*p).cpu }
}

/// The kernel's per-CPU pointer.
pub fn cpu_local() -> *mut () {
    let p = read_sysreg!("tpidr_el1") as *const CpuLocal;
    // SAFETY: TPIDR_EL1 points at this CPU's block from `init_boot_cpu` on.
    unsafe { (*p).kernel }
}

/// Sets the kernel's per-CPU pointer.
pub fn set_cpu_local(v: *mut ()) {
    let p = read_sysreg!("tpidr_el1") as *mut CpuLocal;
    // SAFETY: as above; only this CPU writes its block.
    unsafe { (*p).kernel = v };
}

/// The running thread. Finding the block and reading it are two loads, so
/// interrupts are masked between them: a preemption there could move the
/// thread to another CPU and return that CPU's thread.
pub fn current_thread() -> *const () {
    let daif = read_sysreg!("daif");
    // SAFETY: masking IRQs on this CPU; restored below.
    unsafe { asm!("msr daifset, #2", options(nomem, nostack)) };
    let p = read_sysreg!("tpidr_el1") as *const CpuLocal;
    // SAFETY: TPIDR_EL1 points at this CPU's block from `init_boot_cpu` on.
    let t = unsafe { (*p).thread };
    // SAFETY: restores the saved mask.
    unsafe { asm!("msr daif, {}", in(reg) daif, options(nomem, nostack)) };
    t
}

/// Runs `f` on this CPU's block with IRQs masked: finding the block and
/// updating it take several instructions, and a preemption between them
/// could move the thread to another CPU.
fn with_local<R>(f: impl FnOnce(&mut CpuLocal) -> R) -> R {
    let daif = read_sysreg!("daif");
    // SAFETY: masking IRQs on this CPU; restored below.
    unsafe { asm!("msr daifset, #2", options(nomem, nostack)) };
    let p = read_sysreg!("tpidr_el1") as *mut CpuLocal;
    // SAFETY: TPIDR_EL1 points at this CPU's block, and with IRQs masked
    // nothing else on this CPU touches it.
    let r = f(unsafe { &mut *p });
    // SAFETY: restores the saved mask.
    unsafe { asm!("msr daif, {}", in(reg) daif, options(nomem, nostack)) };
    r
}

/// Enters a critical section.
pub fn critical_enter() {
    with_local(|l| l.critical += 1);
}

/// Leaves one; true when the outermost section ended.
pub fn critical_exit() -> bool {
    with_local(|l| {
        l.critical -= 1;
        l.critical == 0
    })
}

/// The critical-section depth.
pub fn critical_depth() -> u32 {
    with_local(|l| l.critical)
}

/// Records or clears an owed preemption.
pub fn owe_preempt(owed: bool) {
    with_local(|l| l.owed = u32::from(owed));
}

/// Whether a preemption is owed.
pub fn preempt_owed() -> bool {
    with_local(|l| l.owed != 0)
}

/// Adds `delta` to the interrupt nesting; returns the new value.
pub fn interrupt_nest(delta: i32) -> u32 {
    with_local(|l| {
        l.interrupts = l.interrupts.wrapping_add_signed(delta);
        l.interrupts
    })
}

/// Records the running thread. Interrupts must be masked.
pub fn set_current_thread(t: *const ()) {
    let p = read_sysreg!("tpidr_el1") as *mut CpuLocal;
    // SAFETY: as above; only this CPU writes its block.
    unsafe { (*p).thread = t };
}

/// Waits for an interrupt with interrupts masked; WFI wakes on a pending
/// interrupt even when it is masked.
pub fn wait_for_interrupt() {
    // SAFETY: WFI only idles.
    unsafe { asm!("dsb sy", "wfi", options(nostack)) };
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
