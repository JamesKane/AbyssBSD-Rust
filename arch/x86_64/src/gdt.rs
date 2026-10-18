// SPDX-License-Identifier: BSD-2-Clause
//! Per-CPU descriptor tables: the GDT, the TSS with its interrupt stacks,
//! and the per-CPU block reached through `GS`.
//!
//! Selector layout (fixed by `SYSRET`'s expectations for later):
//! 0x08 kernel code, 0x10 kernel data, 0x18 user data, 0x20 user code,
//! 0x28 TSS (two slots).

use core::arch::asm;

use crate::Racy;
use crate::cpu::{self, MSR_GS_BASE};

/// Kernel code selector.
pub(crate) const KERNEL_CS: u16 = 0x08;
const KERNEL_DS: u16 = 0x10;
const TSS_SEL: u16 = 0x28;

/// Interrupt stack table slots (1-based, as the IDT encodes them).
pub(crate) const IST_DOUBLE_FAULT: u8 = 1;
pub(crate) const IST_NMI: u8 = 2;
pub(crate) const IST_MACHINE_CHECK: u8 = 3;

const IST_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct Stack([u8; IST_STACK_SIZE]);

/// The 64-bit TSS. Only the IST pointers matter until user mode exists.
#[repr(C, packed(4))]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

const TSS_INIT: Tss = Tss {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    // No I/O permission bitmap.
    iomap_base: crate::to_u16(size_of::<Tss>()),
};

/// Per-CPU data reached through `GS`. Field offsets are ABI for the
/// assembly that reads them.
#[repr(C)]
#[derive(Debug)]
struct CpuLocal {
    /// Points at itself, so `gs:[0]` yields the block's address.
    self_ptr: *const CpuLocal,
    /// Logical CPU number.
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

/// One CPU's descriptor tables, interrupt stacks, and `GS` block. The
/// boot CPU's is static; the kernel allocates one per other CPU. All-zero
/// memory is a valid unused block (it is large, so allocate it in place).
#[repr(C)]
pub struct CpuBlock {
    local: CpuLocal,
    gdt: [u64; 7],
    tss: Tss,
    ist: [Stack; 3],
}

impl core::fmt::Debug for CpuBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CpuBlock")
            .field("cpu", &self.local.cpu)
            .finish_non_exhaustive()
    }
}

impl CpuBlock {
    /// An unused block.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            local: CpuLocal {
                self_ptr: core::ptr::null(),
                cpu: 0,
                kernel: core::ptr::null_mut(),
                thread: core::ptr::null(),
                critical: 0,
                owed: 0,
                interrupts: 0,
            },
            gdt: [0; 7],
            tss: TSS_INIT,
            ist: [const { Stack([0; IST_STACK_SIZE]) }; 3],
        }
    }
}

impl Default for CpuBlock {
    fn default() -> Self {
        Self::new()
    }
}

// The boot CPU's block. Initialized once in `init_boot_cpu`, before
// interrupts or other CPUs exist; afterwards only that CPU touches it.
static CPU0: Racy<CpuBlock> = Racy::new(CpuBlock::new());

/// A code or data descriptor: present, DPL, S=1, and type bits.
const fn segment(dpl: u64, code: bool) -> u64 {
    let ty = if code { 0b1010 } else { 0b0010 }; // execute/read, read/write
    let long = if code { 1 << 53 } else { 0 };
    (1 << 47) | (dpl << 45) | (1 << 44) | (ty << 40) | long
}

/// Builds and loads the boot CPU's GDT, TSS, and GS base.
///
/// # Safety
///
/// Call once, on the boot CPU, before interrupts are enabled.
pub(crate) unsafe fn init_boot_cpu() {
    // SAFETY: single-threaded early boot; the block is static.
    unsafe { load(&mut *CPU0.get(), 0) };
}

/// Builds and loads this CPU's tables from `block`, as CPU `cpu`.
///
/// # Safety
///
/// Call once per CPU, with interrupts masked; `block` must live forever
/// and belong to this CPU alone.
pub(crate) unsafe fn load(block: &'static mut CpuBlock, cpu: u32) {
    // An all-zero block is valid input: everything is (re)built here.
    block.tss = TSS_INIT;
    for (slot, stack) in [IST_DOUBLE_FAULT, IST_NMI, IST_MACHINE_CHECK]
        .into_iter()
        .zip(block.ist.iter_mut())
    {
        // SAFETY: one past the end of the stack array.
        let top = unsafe { stack.0.as_mut_ptr().add(IST_STACK_SIZE) } as u64;
        block.tss.ist[usize::from(slot - 1)] = top;
    }
    let base = (&raw const block.tss) as u64;
    let limit = (size_of::<Tss>() - 1) as u64;
    let tss_low = (limit & 0xffff)
        | ((base & 0xff_ffff) << 16)
        | (0b1001 << 40) // available 64-bit TSS
        | (1 << 47)
        | (((limit >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);
    block.gdt = [
        0,
        segment(0, true),
        segment(0, false),
        segment(3, false),
        segment(3, true),
        tss_low,
        base >> 32,
    ];

    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }
    let ptr = Pointer {
        limit: crate::to_u16(size_of::<[u64; 7]>() - 1),
        base: (&raw const block.gdt) as u64,
    };
    // SAFETY: the GDT is well formed and lives forever; reloading the
    // segment registers and TR switches this CPU onto it.
    unsafe {
        asm!(
            "lgdt [{ptr}]",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov ss, {ds:x}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            // Reload CS with a far return.
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "ltr {tss:x}",
            ptr = in(reg) &raw const ptr,
            ds = in(reg) KERNEL_DS,
            cs = in(reg) u64::from(KERNEL_CS),
            tss = in(reg) TSS_SEL,
            tmp = out(reg) _,
        );
    }

    block.local.cpu = cpu;
    block.local.self_ptr = &raw const block.local;
    // SAFETY: GS now points at this CPU's block for good.
    unsafe { cpu::wrmsr(MSR_GS_BASE, (&raw const block.local) as u64) };
}

/// The current CPU's number.
pub fn cpu_id() -> u32 {
    let id: u32;
    // SAFETY: GS points at this CPU's `CpuLocal` from `load` on; `cpu` is
    // at offset 8.
    unsafe { asm!("mov {:e}, gs:[8]", out(reg) id, options(nostack, readonly)) };
    id
}

/// The kernel's per-CPU pointer.
pub(crate) fn cpu_local() -> *mut () {
    let p: *mut ();
    // SAFETY: as for `cpu_id`; `kernel` is at offset 16.
    unsafe { asm!("mov {}, gs:[16]", out(reg) p, options(nostack, readonly)) };
    p
}

/// Sets the kernel's per-CPU pointer.
pub(crate) fn set_cpu_local(p: *mut ()) {
    // SAFETY: as for `cpu_local`; only this CPU writes its block.
    unsafe { asm!("mov gs:[16], {}", in(reg) p, options(nostack)) };
}

/// The running thread. One `GS`-relative load, so a preemption cannot
/// split finding this CPU's block from reading it.
pub(crate) fn current_thread() -> *const () {
    let p: *const ();
    // SAFETY: as for `cpu_local`; `thread` is at offset 24.
    unsafe { asm!("mov {}, gs:[24]", out(reg) p, options(nostack, readonly)) };
    p
}

/// Records the running thread.
pub(crate) fn set_current_thread(p: *const ()) {
    // SAFETY: as for `set_cpu_local`.
    unsafe { asm!("mov gs:[24], {}", in(reg) p, options(nostack)) };
}

const _: () = assert!(core::mem::offset_of!(CpuLocal, kernel) == 16);
const _: () = assert!(core::mem::offset_of!(CpuLocal, thread) == 24);
const _: () = assert!(core::mem::offset_of!(CpuLocal, critical) == 32);
const _: () = assert!(core::mem::offset_of!(CpuLocal, owed) == 36);
const _: () = assert!(core::mem::offset_of!(CpuLocal, interrupts) == 40);

// Each per-CPU counter below changes in one instruction: an interrupt (and
// so a migration) cannot fall between finding this CPU's block and
// updating it.

/// Enters a critical section.
pub(crate) fn critical_enter() {
    // SAFETY: as for `cpu_local`; `critical` is at offset 32.
    unsafe { asm!("inc dword ptr gs:[32]", options(nostack)) };
}

/// Leaves one; true when the outermost section ended.
pub(crate) fn critical_exit() -> bool {
    let zero: u8;
    // SAFETY: as above.
    unsafe { asm!("dec dword ptr gs:[32]", "setz {}", out(reg_byte) zero, options(nostack)) };
    zero != 0
}

/// The critical-section depth.
pub(crate) fn critical_depth() -> u32 {
    let d: u32;
    // SAFETY: as above.
    unsafe { asm!("mov {:e}, gs:[32]", out(reg) d, options(nostack, readonly)) };
    d
}

/// Records or clears an owed preemption.
pub(crate) fn owe_preempt(owed: bool) {
    // SAFETY: as above; `owed` is at offset 36.
    unsafe { asm!("mov dword ptr gs:[36], {:e}", in(reg) u32::from(owed), options(nostack)) };
}

/// Whether a preemption is owed.
pub(crate) fn preempt_owed() -> bool {
    let o: u32;
    // SAFETY: as above.
    unsafe { asm!("mov {:e}, gs:[36]", out(reg) o, options(nostack, readonly)) };
    o != 0
}

/// Adds `delta` to the interrupt nesting; returns the new value.
pub(crate) fn interrupt_nest(delta: i32) -> u32 {
    let n: u32;
    // SAFETY: as above; `interrupts` is at offset 40. Changed only with
    // interrupts masked; read anywhere.
    unsafe {
        asm!(
            "add dword ptr gs:[40], {d:e}",
            "mov {n:e}, gs:[40]",
            d = in(reg) delta,
            n = out(reg) n,
            options(nostack),
        );
    }
    n
}
