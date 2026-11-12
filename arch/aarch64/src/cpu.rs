// SPDX-License-Identifier: BSD-2-Clause
//! System registers and CPU control.

use core::arch::asm;
use core::sync::atomic::Ordering::Relaxed;

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

/// Orders this CPU's earlier memory writes before its later device
/// writes, as seen by any observer (DSB ST): a device told by a register
/// write to read memory then sees what was written.
pub fn write_barrier() {
    // SAFETY: a barrier; no memory or register effects.
    unsafe { asm!("dsb st", options(nostack, preserves_flags)) };
}

/// `MIDR_EL1`, the CPU's implementer and part number.
pub fn midr() -> u64 {
    read_sysreg!("midr_el1")
}

/// `MPIDR_EL1`, the CPU's affinity (hardware ID).
pub fn mpidr() -> u64 {
    read_sysreg!("mpidr_el1") & 0xff_00ff_ffff
}

/// `MPIDR_EL1` whole, as user mode reads it.
pub fn mpidr_el1() -> u64 {
    read_sysreg!("mpidr_el1")
}

/// `REVIDR_EL1`, the implementation's revision bits.
pub fn revidr() -> u64 {
    read_sysreg!("revidr_el1")
}

/// Whether the activity monitors are implemented (`ID_AA64PFR0_EL1.AMU`).
pub fn has_amu() -> bool {
    (id_register(4, 0) >> 44) & 0xf != 0
}

/// The activity monitors' core cycle counter (`AMEVCNTR00_EL0`), which
/// counts at the core's clock while it runs; `None` without them.
pub fn amu_core_cycles() -> Option<u64> {
    // An access traps (undefined) where the monitors are absent.
    has_amu().then(|| read_sysreg!("s3_3_c13_c4_0"))
}

/// The activity monitors' constant cycle counter (`AMEVCNTR01_EL0`), at
/// the system counter's rate while the core runs; `None` without them.
pub fn amu_constant_cycles() -> Option<u64> {
    has_amu().then(|| read_sysreg!("s3_3_c13_c4_1"))
}

/// The AArch64 ID register at `op0` 3, `op1` 0, `CRn` 0, `CRm` 4–7 and
/// `op2` (by encoding, so that registers the assembler does not name
/// read too: unassigned ones read as zero).
pub fn id_register(crm: u8, op2: u8) -> u64 {
    macro_rules! regs {
        ($(($m:literal, $o:literal)),* $(,)?) => {
            match (crm, op2) {
                $(($m, $o) => {
                    let v: u64;
                    // SAFETY: reading an ID register has no side effects.
                    unsafe {
                        asm!(
                            concat!("mrs {}, s3_0_c0_c", $m, "_", $o),
                            out(reg) v,
                            options(nomem, nostack),
                        );
                    }
                    v
                })*
                _ => 0,
            }
        };
    }
    regs!(
        (4, 0),
        (4, 1),
        (4, 2),
        (4, 4),
        (5, 0),
        (5, 1),
        (5, 4),
        (5, 5),
        (6, 0),
        (6, 1),
        (6, 2),
        (7, 0),
        (7, 1),
        (7, 2),
        (7, 3),
        (7, 4),
    )
}

/// Where [`CpuLocal`]'s running thread lies, for the exception entry,
/// which puts it in `SP_EL0`.
pub(crate) const THREAD_OFFSET: usize = core::mem::offset_of!(CpuLocal, thread);

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
        // No thread runs yet (`current_thread`).
        asm!("msr sp_el0, xzr", options(nostack));
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
    // SAFETY: per the contract; no thread runs on it yet.
    unsafe {
        asm!("msr tpidr_el1, {}", in(reg) &raw const block.0, options(nostack));
        asm!("msr sp_el0, xzr", options(nostack));
    }
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

/// The running thread, from `SP_EL0`, one read: no preemption can split
/// it. In the kernel `SP_EL0` holds the running thread (the kernel's own
/// stack is `SP_EL1`): the exception entry from EL0 saves the user's stack
/// pointer and puts the thread there, the return to EL0 puts the user's
/// back, and a switch moves it along ([`set_current_thread`]), as Linux
/// keeps its current task. FreeBSD reads `curthread` through `x18`, which
/// it reserves for the per-CPU block; the per-CPU block's copy is what
/// the entry loads.
pub fn current_thread() -> *const () {
    read_sysreg!("sp_el0") as *const ()
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

/// The running thread's [`hal::ThreadLocal`], from `SP_EL0` in one read
/// (no preemption can split it), or none before a thread runs here: the
/// per-CPU block's counts serve until then.
fn thread_local() -> Option<&'static hal::ThreadLocal> {
    let t = current_thread().cast::<hal::ThreadLocal>();
    // SAFETY: a thread named by `set_current_thread` starts with its
    // `ThreadLocal` (the HAL's contract) and outlives its run here.
    unsafe { t.as_ref() }
}

/// Enters a critical section: the thread's depth, as FreeBSD's
/// `critical_enter` raises `td_critnest`.
pub fn critical_enter() {
    match thread_local() {
        Some(t) => t.critical.store(t.critical.load(Relaxed) + 1, Relaxed),
        None => with_local(|l| l.critical += 1),
    }
}

/// Leaves one; true when the outermost section ended.
pub fn critical_exit() -> bool {
    match thread_local() {
        Some(t) => {
            let d = t.critical.load(Relaxed) - 1;
            t.critical.store(d, Relaxed);
            d == 0
        }
        None => with_local(|l| {
            l.critical -= 1;
            l.critical == 0
        }),
    }
}

/// The critical-section depth.
pub fn critical_depth() -> u32 {
    match thread_local() {
        Some(t) => t.critical.load(Relaxed),
        None => with_local(|l| l.critical),
    }
}

/// Records or clears an owed preemption.
pub fn owe_preempt(owed: bool) {
    match thread_local() {
        Some(t) => t.owed.store(u32::from(owed), Relaxed),
        None => with_local(|l| l.owed = u32::from(owed)),
    }
}

/// Whether a preemption is owed.
pub fn preempt_owed() -> bool {
    match thread_local() {
        Some(t) => t.owed.load(Relaxed) != 0,
        None => with_local(|l| l.owed != 0),
    }
}

/// Adds `delta` to the interrupt nesting; returns the new value.
pub fn interrupt_nest(delta: i32) -> u32 {
    with_local(|l| {
        l.interrupts = l.interrupts.wrapping_add_signed(delta);
        l.interrupts
    })
}

/// `SP_EL0`'s value (the running thread, in the kernel).
pub(crate) fn read_sp_el0() -> u64 {
    read_sysreg!("sp_el0")
}

/// Records the running thread. Interrupts must be masked.
pub fn set_current_thread(t: *const ()) {
    let p = read_sysreg!("tpidr_el1") as *mut CpuLocal;
    // SAFETY: as above; only this CPU writes its block. `SP_EL0` is the
    // kernel's to use while it runs ([`current_thread`]). The first
    // thread on this CPU takes over the counts kept here until now (a
    // section begun before it, ended after).
    unsafe {
        if (*p).thread.is_null()
            && let Some(first) = t.cast::<hal::ThreadLocal>().as_ref()
        {
            first.critical.store((*p).critical, Relaxed);
            first.owed.store((*p).owed, Relaxed);
            (*p).critical = 0;
            (*p).owed = 0;
        }
        (*p).thread = t;
        asm!("msr sp_el0, {}", in(reg) t.addr(), options(nomem, nostack));
    }
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

/// 64 bits from the CPU's random number generator (`RNDR`, FEAT_RNG).
/// `None` if the CPU has none, or it reports failure.
#[must_use]
pub fn hardware_random() -> Option<u64> {
    // ID_AA64ISAR0_EL1.RNDR, bits 63:60.
    if read_sysreg!("id_aa64isar0_el1") >> 60 == 0 {
        return None;
    }
    for _ in 0..10 {
        let (v, nzcv): (u64, u64);
        // SAFETY: the CPU implements RNDR (checked above); reading it
        // writes the destination and NZCV, which we read back.
        unsafe {
            core::arch::asm!(
                "mrs {v}, s3_3_c2_c4_0",
                "mrs {f}, nzcv",
                v = out(reg) v,
                f = out(reg) nzcv,
                options(nomem, nostack),
            );
        }
        // Z set means no random number was available.
        if nzcv & (1 << 30) == 0 {
            return Some(v);
        }
    }
    None
}
