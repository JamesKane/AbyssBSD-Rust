// SPDX-License-Identifier: BSD-2-Clause
//! The GDT, the TSS with its interrupt stacks, and the per-CPU block.
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
pub(crate) struct CpuLocal {
    /// Points at itself, so `gs:[0]` yields the block's address.
    self_ptr: *const CpuLocal,
    /// Logical CPU number.
    cpu: u32,
}

// Boot CPU state. Written once in `init_boot_cpu`, on the boot CPU, before
// interrupts or other CPUs exist.
static GDT: Racy<[u64; 7]> = Racy::new([0; 7]);
static TSS: Racy<Tss> = Racy::new(TSS_INIT);
static IST_STACKS: Racy<[Stack; 3]> = Racy::new([const { Stack([0; IST_STACK_SIZE]) }; 3]);
static CPU0: Racy<CpuLocal> = Racy::new(CpuLocal {
    self_ptr: core::ptr::null(),
    cpu: 0,
});

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
    let gdt = GDT.get();
    let tss = TSS.get();
    // SAFETY: single-threaded early boot (see the statics above).
    unsafe {
        let stacks = &mut *IST_STACKS.get();
        for (slot, stack) in [IST_DOUBLE_FAULT, IST_NMI, IST_MACHINE_CHECK]
            .into_iter()
            .zip(stacks.iter_mut())
        {
            let top = stack.0.as_mut_ptr().add(IST_STACK_SIZE) as u64;
            (*tss).ist[usize::from(slot - 1)] = top;
        }
        let base = tss as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        let tss_low = (limit & 0xffff)
            | ((base & 0xff_ffff) << 16)
            | (0b1001 << 40) // available 64-bit TSS
            | (1 << 47)
            | (((limit >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        *gdt = [
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
            base: gdt as u64,
        };
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

        let cpu0 = CPU0.get();
        (*cpu0).self_ptr = cpu0;
        cpu::wrmsr(MSR_GS_BASE, cpu0 as u64);
    }
}

/// The current CPU's number.
pub fn cpu_id() -> u32 {
    let id: u32;
    // SAFETY: GS points at this CPU's `CpuLocal` from `init_boot_cpu` on;
    // `cpu` is at offset 8.
    unsafe { asm!("mov {:e}, gs:[8]", out(reg) id, options(nostack, readonly)) };
    id
}
