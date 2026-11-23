// SPDX-License-Identifier: BSD-2-Clause
//! Fault recovery: instructions allowed to fault, and where to resume.
//!
//! Assembly that may touch a bad address records `(fault_ip, fixup_ip)` in
//! the `abyss_extable` section. The trap handler resumes a kernel-mode page
//! or protection fault at the fixup instead of treating it as fatal. This is
//! the mechanism `copyin`/`copyout` will use; for now it backs [`probe_read`]
//! and the MSR accesses that may fault ([`rdmsr_safe`], [`wrmsr_safe`]:
//! FreeBSD's `rdmsr_safe` and `wrmsr_safe`).

use core::arch::global_asm;

global_asm!(
    r#"
    .section .text.abyss_probe, "ax"
    .global abyss_probe_read_u64
abyss_probe_read_u64:
    xor edx, edx
1:  mov rax, [rdi]
    ret
2:  xor eax, eax
    mov edx, 1
    ret

    .pushsection abyss_extable, "aw"
    .balign 8
    .quad 1b, 2b
    .popsection
    "#
);

// An MSR access that a missing or locked register turns into #GP: the
// fixup returns 1 instead. `rdi` the MSR, `rsi` where `rdmsr` stores the
// value or the value `wrmsr` writes.
global_asm!(
    r#"
    .section .text.abyss_probe, "ax"
    .global abyss_rdmsr_safe
abyss_rdmsr_safe:
    mov ecx, edi
1:  rdmsr
    shl rdx, 32
    or rax, rdx
    mov [rsi], rax
    xor eax, eax
    ret
2:  mov eax, 1
    ret

    .global abyss_wrmsr_safe
abyss_wrmsr_safe:
    mov ecx, edi
    mov rax, rsi
    mov rdx, rsi
    shr rdx, 32
3:  wrmsr
    xor eax, eax
    ret
4:  mov eax, 1
    ret

    .pushsection abyss_extable, "aw"
    .balign 8
    .quad 1b, 2b
    .quad 3b, 4b
    .popsection
    "#
);

#[repr(C)]
struct Probe {
    value: u64,
    faulted: u64,
}

unsafe extern "sysv64" {
    fn abyss_probe_read_u64(addr: u64) -> Probe;
    fn abyss_rdmsr_safe(msr: u32, out: *mut u64) -> u64;
    fn abyss_wrmsr_safe(msr: u32, value: u64) -> u64;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    fault: u64,
    fixup: u64,
}

unsafe extern "C" {
    static __start_abyss_extable: Entry;
    static __stop_abyss_extable: Entry;
}

/// The fixup for a fault at `ip`, if `ip` is allowed to fault.
pub(crate) fn lookup(ip: u64) -> Option<u64> {
    let start = &raw const __start_abyss_extable;
    let stop = &raw const __stop_abyss_extable;
    let len = (stop as usize - start as usize) / size_of::<Entry>();
    // SAFETY: the linker places every `Entry` the assembly emits between
    // the two symbols, and the loader relocated their addresses.
    let entries = unsafe { core::slice::from_raw_parts(start, len) };
    entries.iter().find(|e| e.fault == ip).map(|e| e.fixup)
}

/// A fault taken while probing memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault;

/// Reads the 8-byte value at `addr`, returning [`Fault`] instead of
/// crashing if the address is not mapped readable.
pub fn probe_read(addr: u64) -> Result<u64, Fault> {
    if !addr.is_multiple_of(8) {
        return Err(Fault);
    }
    // SAFETY: the probe's only memory access is covered by a fixup entry,
    // so a bad address returns `faulted` instead of trapping fatally.
    let p = unsafe { abyss_probe_read_u64(addr) };
    if p.faulted == 0 {
        Ok(p.value)
    } else {
        Err(Fault)
    }
}

/// Reads an MSR, returning [`Fault`] instead of crashing if it does not
/// exist or may not be read (`#GP`).
///
/// # Safety
///
/// Reading the MSR must have no effect that breaks memory safety: reads
/// of the registers this is used on are side-effect free.
pub unsafe fn rdmsr_safe(msr: u32) -> Result<u64, Fault> {
    let mut v = 0;
    // SAFETY: `v` is a valid location; a #GP from the one RDMSR is covered
    // by a fixup entry and returns 1.
    let faulted = unsafe { abyss_rdmsr_safe(msr, &raw mut v) };
    if faulted == 0 { Ok(v) } else { Err(Fault) }
}

/// Writes an MSR, returning [`Fault`] instead of crashing if it does not
/// exist or may not be written (`#GP`).
///
/// # Safety
///
/// The write must not break memory safety (it must not change paging,
/// segment, or interrupt state under running code).
pub unsafe fn wrmsr_safe(msr: u32, value: u64) -> Result<(), Fault> {
    // SAFETY: per the caller's contract; a #GP from the one WRMSR is
    // covered by a fixup entry and returns 1.
    let faulted = unsafe { abyss_wrmsr_safe(msr, value) };
    if faulted == 0 { Ok(()) } else { Err(Fault) }
}
