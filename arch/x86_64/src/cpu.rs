// SPDX-License-Identifier: BSD-2-Clause
//! Privileged instructions: control registers, MSRs, port I/O, halting.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

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

/// How idle CPUs wait: 0 not yet decided, 1 `MWAIT`, 2 `HLT`.
static IDLE_WAIT: AtomicU8 = AtomicU8::new(0);

/// The cache line `MONITOR` watches while idle. Nothing writes it: an
/// interrupt is what ends the wait, as it does a halt (the waker's IPI);
/// the line only has to be armed for `MWAIT` to wait.
#[repr(align(64))]
struct MonitorLine(#[allow(dead_code)] u64);
static MONITOR_LINE: MonitorLine = MonitorLine(0);

/// Whether idle CPUs may wait with `MWAIT` (hint 0, C1) instead of `HLT`:
/// the CPU has `MONITOR`/`MWAIT` with interrupts as break events, and
/// this is not a hypervisor's guest (where it is unavailable or emulated
/// as a spin). FreeBSD's default idle on such a CPU is the same wait
/// (`cpu_idle_mwait`, and the C1 of `acpi_cpu_idle`).
fn mwait_usable() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::__cpuid;
        let leaf1 = __cpuid(1);
        let monitor = leaf1.ecx & (1 << 3) != 0;
        let hypervisor = leaf1.ecx & (1 << 31) != 0;
        if !monitor || hypervisor || __cpuid(0).eax < 5 {
            return false;
        }
        // CPUID.5:ECX bit 0 (extensions enumerated) and bit 1 (interrupts
        // break a wait even when masked).
        return __cpuid(5).ecx & 0b11 == 0b11;
    }
    #[allow(unreachable_code)]
    false
}

/// Whether idle CPUs wait with `MWAIT`, once decided (`HLT` if this
/// CPU cannot, or `set_idle_halt` asked).
fn idle_uses_mwait() -> bool {
    match IDLE_WAIT.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let yes = mwait_usable();
            IDLE_WAIT.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
            yes
        }
    }
}

/// Makes idle CPUs wait with `HLT` even where `MWAIT` is usable (boot
/// option `abyss.idle.hlt`, for comparing the two). On a Ryzen 9 7900X a
/// core halted for a millisecond or more takes about ten microseconds to
/// wake (a hundred and more after five milliseconds), where `MWAIT` in C1
/// takes under one.
pub fn set_idle_halt() {
    IDLE_WAIT.store(2, Ordering::Relaxed);
}

/// How idle CPUs wait, for the boot log: "MWAIT (C1)" or "HLT".
pub fn idle_method() -> &'static str {
    if idle_uses_mwait() {
        "MWAIT (C1)"
    } else {
        "HLT"
    }
}

/// Waits for an interrupt with interrupts masked: `STI; HLT` (or
/// `STI; MWAIT`) opens the interrupt window only while waiting (STI's
/// one-instruction shadow covers it), services what arrives, and masks
/// again.
pub fn wait_for_interrupt() {
    if idle_uses_mwait() {
        // SAFETY: MONITOR/MWAIT are usable (`mwait_usable`); the line is
        // a valid, readable address; the caller is ready for interrupts
        // while waiting. A write to the line, or any interrupt, ends the
        // wait; the caller rechecks its condition either way.
        unsafe {
            asm!(
                "monitor",
                in("rax") core::ptr::addr_of!(MONITOR_LINE),
                in("ecx") 0u32,
                in("edx") 0u32,
                options(nostack, preserves_flags),
            );
            asm!("sti", "mwait", "cli", in("eax") 0u32, in("ecx") 0u32, options(nostack));
        }
    } else {
        // SAFETY: the caller is ready for interrupts while halted.
        unsafe { asm!("sti", "hlt", "cli", options(nostack)) };
    }
}

/// Asks the cache for the line at `p` in a state this CPU can write
/// (`PREFETCHW`), ahead of a read that a write follows: one coherence
/// transaction instead of two. A hint; changes nothing (a no-op where the
/// CPU lacks it).
#[inline(always)]
pub fn prefetch_write<T>(p: *const T) {
    // SAFETY: a prefetch never faults and has no architectural effect.
    unsafe { asm!("prefetchw [{}]", in(reg) p, options(nostack, preserves_flags)) };
}

/// Writes the cache lines of `[start, start + len)` back to memory and
/// evicts them from every cache in the system (`CLFLUSH`, which acts on all
/// cores' caches, where `WBINVD` acts on this core's alone), before a reset:
/// the persistent log ring (the previous boot's log, a panic's message) is
/// kept in RAM, and a platform reset drops what is still only in a cache,
/// the lines another core wrote last included. A core halted with `HLT` may
/// write its caches back by itself in a deep state; one waiting with
/// `MWAIT` in C1 does not.
///
/// # Safety
///
/// The range must be mapped and readable.
pub unsafe fn flush_cache_range(start: *const u8, len: usize) {
    // CPUID.1:EBX[15:8] is the CLFLUSH line size in quadwords.
    let line = match (core::arch::x86_64::__cpuid(1).ebx >> 8) & 0xff {
        0 => 64,
        q => q as usize * 8,
    };
    let mut at = start as usize & !(line - 1);
    let end = start as usize + len;
    while at < end {
        // SAFETY: the caller guarantees the range is mapped; CLFLUSH
        // changes no data.
        unsafe { asm!("clflush [{}]", in(reg) at, options(nostack, preserves_flags)) };
        at += line;
    }
    // SAFETY: a fence; no memory is touched.
    unsafe { asm!("mfence", options(nostack, preserves_flags)) };
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

/// The physical address width in bits (CPUID 8000_0008h EAX[7:0], what
/// FreeBSD's `cpu_maxphyaddr` holds), 36 where the leaf is missing.
#[must_use]
pub fn phys_addr_bits() -> u32 {
    let max = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    if max >= 0x8000_0008 {
        core::arch::x86_64::__cpuid(0x8000_0008).eax & 0xff
    } else {
        36
    }
}

/// Whether the time-stamp counter runs at a constant rate in every
/// power state (CPUID 8000_0007h EDX bit 8, `tsc_is_invariant`).
#[must_use]
pub fn tsc_invariant() -> bool {
    let max = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    max >= 0x8000_0007 && core::arch::x86_64::__cpuid(0x8000_0007).edx & (1 << 8) != 0
}

/// Whether the local APIC timer keeps running in deep C-states (ARAT,
/// CPUID 6 EAX bit 2).
#[must_use]
pub fn apic_timer_always_running() -> bool {
    let max = core::arch::x86_64::__cpuid(0).eax;
    max >= 6 && core::arch::x86_64::__cpuid(6).eax & (1 << 2) != 0
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
