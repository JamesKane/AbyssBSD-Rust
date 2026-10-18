// SPDX-License-Identifier: BSD-2-Clause
//! Privileged instructions: control registers, MSRs, port I/O, halting.

use core::arch::asm;

/// `IA32_EFER`.
pub const MSR_EFER: u32 = 0xc000_0080;
/// `IA32_GS_BASE`.
pub const MSR_GS_BASE: u32 = 0xc000_0101;
/// `IA32_KERNEL_GS_BASE`.
pub const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;
/// `IA32_PAT`.
pub const MSR_PAT: u32 = 0x277;

/// Reads an MSR.
///
/// # Safety
///
/// The MSR must exist on this CPU.
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: the caller guarantees the MSR exists; RDMSR has no other
    // effects.
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    u64::from(hi) << 32 | u64::from(lo)
}

/// Writes an MSR.
///
/// # Safety
///
/// The MSR must exist, and the value must not break memory safety (e.g. by
/// changing paging or segment state under running code).
pub unsafe fn wrmsr(msr: u32, v: u64) {
    let (lo, hi) = split(v);
    // SAFETY: per the caller's contract.
    unsafe { asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi, options(nostack)) };
}

/// `CR2`: the faulting address of the last page fault.
pub fn cr2() -> u64 {
    let v;
    // SAFETY: reading CR2 has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack)) };
    v
}

/// `CR3`: the current page-table root and PCID.
pub fn cr3() -> u64 {
    let v;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack)) };
    v
}

/// `CR4`.
pub fn cr4() -> u64 {
    let v;
    // SAFETY: reading CR4 has no side effects.
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack)) };
    v
}

/// Writes `CR4`.
///
/// # Safety
///
/// The new value must keep paging consistent with the running code.
pub unsafe fn set_cr4(v: u64) {
    // SAFETY: per the caller's contract.
    unsafe { asm!("mov cr4, {}", in(reg) v, options(nostack)) };
}

/// Writes a byte to an I/O port.
///
/// # Safety
///
/// The port write must be acceptable to whatever device decodes it.
pub unsafe fn outb(port: u16, v: u8) {
    // SAFETY: per the caller's contract.
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) };
}

/// Reads a byte from an I/O port.
///
/// # Safety
///
/// The port read must be acceptable to whatever device decodes it (reads
/// can have side effects).
pub unsafe fn inb(port: u16) -> u8 {
    let v;
    // SAFETY: per the caller's contract.
    unsafe { asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack)) };
    v
}

/// Writes a 16-bit value to an I/O port.
///
/// # Safety
///
/// As for [`outb`].
pub unsafe fn outw(port: u16, v: u16) {
    // SAFETY: per the caller's contract.
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack)) };
}

/// Reads a 16-bit I/O port.
///
/// # Safety
///
/// As for [`inb`].
pub unsafe fn inw(port: u16) -> u16 {
    let v;
    // SAFETY: per the caller's contract.
    unsafe { asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack)) };
    v
}

/// Writes a 32-bit value to an I/O port.
///
/// # Safety
///
/// As for [`outb`].
pub unsafe fn outl(port: u16, v: u32) {
    // SAFETY: per the caller's contract.
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack)) };
}

/// Reads a 32-bit I/O port.
///
/// # Safety
///
/// As for [`inb`].
pub unsafe fn inl(port: u16) -> u32 {
    let v;
    // SAFETY: per the caller's contract.
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack)) };
    v
}

/// Disables interrupts.
pub fn disable_interrupts() {
    // SAFETY: CLI only masks interrupts.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

/// Masks interrupts, returning whether they were enabled. A compiler
/// barrier: memory accesses do not move across it.
pub fn save_disable_interrupts() -> bool {
    let flags: u64;
    // SAFETY: reads RFLAGS and masks interrupts.
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags) };
    flags & (1 << 9) != 0
}

/// Unmasks interrupts. A compiler barrier.
pub fn enable_interrupts() {
    // SAFETY: the caller is ready for interrupts.
    unsafe { asm!("sti", options(nostack)) };
}

/// Whether interrupts are enabled.
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: reads RFLAGS.
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(nomem)) };
    flags & (1 << 9) != 0
}

/// Waits for an interrupt with interrupts masked: `STI; HLT` opens the
/// interrupt window only while halted (STI's one-instruction shadow
/// covers HLT), services what arrives, and masks again.
pub fn wait_for_interrupt() {
    // SAFETY: the caller is ready for interrupts while halted.
    unsafe { asm!("sti", "hlt", "cli", options(nostack)) };
}

/// Stops this CPU for good.
pub fn halt_forever() -> ! {
    loop {
        disable_interrupts();
        // SAFETY: HLT with interrupts masked waits for NMI/SMI only.
        unsafe { asm!("hlt", options(nomem, nostack)) };
    }
}

/// The current frame pointer.
#[inline(always)]
pub fn frame_pointer() -> u64 {
    let v;
    // SAFETY: reads a register.
    unsafe { asm!("mov {}, rbp", out(reg) v, options(nomem, nostack)) };
    v
}

/// Reads the time-stamp counter.
pub fn timestamp() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC is unprivileged and side-effect free.
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    u64::from(hi) << 32 | u64::from(lo)
}

/// An I/O port register block, for drivers written against
/// `regs::RegisterIo`-style access.
#[derive(Clone, Copy, Debug)]
pub struct PortBlock {
    base: u16,
}

impl PortBlock {
    /// Registers at ports `base..`.
    ///
    /// # Safety
    ///
    /// Accesses to those ports must be acceptable to the device there.
    pub const unsafe fn new(base: u16) -> Self {
        Self { base }
    }

    /// Reads register `n`.
    pub fn read(&self, n: u16) -> u8 {
        // SAFETY: the constructor's contract covers ports in the block.
        unsafe { inb(self.base + n) }
    }

    /// Writes register `n`.
    pub fn write(&self, n: u16, v: u8) {
        // SAFETY: as for `read`.
        unsafe { outb(self.base + n, v) }
    }
}

/// Splits a 64-bit value into its low and high halves.
pub const fn split(v: u64) -> (u32, u32) {
    let b = v.to_le_bytes();
    (
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
    )
}

/// 64 bits from the CPU's entropy source: RDSEED (conditioned entropy)
/// where the CPU has it, else RDRAND (its DRBG). `None` if it has neither
/// or they keep failing.
#[must_use]
pub fn hardware_random() -> Option<u64> {
    let seed = core::arch::x86_64::__cpuid_count(7, 0).ebx & (1 << 18) != 0;
    let rand = core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0;
    for _ in 0..10 {
        let (v, ok): (u64, u8);
        if seed {
            // SAFETY: the CPU has RDSEED (CPUID.7.EBX[18]); it only writes
            // the output register and the carry flag.
            unsafe {
                core::arch::asm!("rdseed {}", "setc {}", out(reg) v, out(reg_byte) ok, options(nomem, nostack))
            };
        } else if rand {
            // SAFETY: the CPU has RDRAND (CPUID.1.ECX[30]); as above.
            unsafe {
                core::arch::asm!("rdrand {}", "setc {}", out(reg) v, out(reg_byte) ok, options(nomem, nostack))
            };
        } else {
            return None;
        }
        if ok != 0 {
            return Some(v);
        }
    }
    None
}
