// SPDX-License-Identifier: BSD-2-Clause
//! Fault recovery: instructions allowed to fault, and where to resume.
//!
//! Assembly that may touch a bad address records `(fault_ip, fixup_ip)` in
//! the `abyss_extable` section. The trap handler resumes a kernel-mode page
//! or protection fault at the fixup instead of treating it as fatal. This is
//! the mechanism `copyin`/`copyout` will use; for now it backs [`probe_read`].

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

#[repr(C)]
struct Probe {
    value: u64,
    faulted: u64,
}

unsafe extern "sysv64" {
    fn abyss_probe_read_u64(addr: u64) -> Probe;
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
