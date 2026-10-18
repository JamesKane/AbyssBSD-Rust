// SPDX-License-Identifier: BSD-2-Clause
//! Powering off and resetting the machine, as FreeBSD's shutdown_final
//! handlers do: ACPI S5 and the FADT reset register on platforms with ACPI
//! hardware, PSCI on arm64, and UEFI's ResetSystem when those are absent
//! or return.
//!
//! The S5 sleep type comes from `\_S5` at boot, so the fatal-trap path,
//! which cannot run AML, can still power off; an orderly power-off runs
//! `\_PTS(5)` first.

#![allow(
    clippy::cast_possible_truncation,
    reason = "registers take the low bits of their values; a 64-bit target"
)]

use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch;

/// `\_S5`'s SLP_TYPa and SLP_TYPb, packed; `u32::MAX` if unknown.
static S5: AtomicU32 = AtomicU32::new(u32::MAX);

/// PM1 control: SLP_TYP (bits 12:10) and SLP_EN (bit 13).
#[cfg(target_arch = "x86_64")]
const SLP_TYP_SHIFT: u32 = 10;
#[cfg(target_arch = "x86_64")]
const SLP_EN: u32 = 1 << 13;
/// Sleep control register (hardware-reduced): SLP_TYP (4:2), SLP_EN (5).
const SCR_SLP_EN: u8 = 1 << 5;

/// Records `\_S5`'s sleep types. Call once ACPI is up.
pub(crate) fn init(s: &sched::Sleepable) {
    let Some(svc) = acpi::service() else { return };
    let types = svc.run(s, |interp, host| {
        match interp.evaluate_path("\\_S5", alloc::vec::Vec::new(), host) {
            Ok(aml::Value::Package(p)) => {
                let int = |i: usize| match p.get(i) {
                    Some(aml::Value::Integer(v)) => Some(*v as u32 & 7),
                    _ => None,
                };
                Some((int(0)?, int(1).unwrap_or(0)))
            }
            _ => None,
        }
    });
    if let Some((a, b)) = types {
        S5.store(a | b << 8, Ordering::Release);
    }
}

/// Tells firmware the system is about to enter S5 (`\_PTS(5)`), for an
/// orderly power-off.
pub(crate) fn prepare(s: &sched::Sleepable) {
    if let Some(svc) = acpi::service() {
        svc.run(s, |interp, host| {
            let _ = interp.evaluate_path("\\_PTS", alloc::vec![aml::Value::Integer(5)], host);
        });
    }
}

fn fadt() -> Option<acpi_tables::fadt::Fadt<'static>> {
    crate::acpi::find(*b"FACP").and_then(|t| acpi_tables::fadt::parse(&t).ok())
}

/// Writes `value` to `reg` (`bytes` wide): I/O ports or memory.
fn write_gas(reg: acpi_tables::Gas, bytes: u8, value: u32) {
    match reg.space {
        #[cfg(target_arch = "x86_64")]
        acpi_tables::AddressSpace::Io => {
            let Ok(port) = u16::try_from(reg.address) else {
                return;
            };
            // SAFETY: a fixed-hardware register the FADT names, written as
            // ACPI specifies, on the way to powering off.
            unsafe {
                match bytes {
                    1 => arch::cpu::outb(port, value as u8),
                    2 => arch::cpu::outw(port, value as u16),
                    _ => arch::cpu::outl(port, value),
                }
            }
        }
        acpi_tables::AddressSpace::Memory => {
            if let Ok(va) = crate::mm::map_device(addr::PhysAddr::new(reg.address), 4) {
                let p = va.get() as usize;
                // SAFETY: as above, through a fresh uncached mapping.
                unsafe {
                    match bytes {
                        1 => core::ptr::with_exposed_provenance_mut::<u8>(p)
                            .write_volatile(value as u8),
                        2 => core::ptr::with_exposed_provenance_mut::<u16>(p)
                            .write_volatile(value as u16),
                        _ => core::ptr::with_exposed_provenance_mut::<u32>(p).write_volatile(value),
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(target_arch = "x86_64")]
fn read_gas16(reg: acpi_tables::Gas) -> u32 {
    match (reg.space, u16::try_from(reg.address)) {
        // SAFETY: as in `write_gas`.
        (acpi_tables::AddressSpace::Io, Ok(port)) => u32::from(unsafe { arch::cpu::inw(port) }),
        _ => 0,
    }
}

/// Enters S5 through the FADT's registers, if `\_S5` was found.
fn acpi_s5() {
    let packed = S5.load(Ordering::Acquire);
    let Some(f) = fadt() else { return };
    if packed == u32::MAX {
        return;
    }
    let a = packed & 7;
    #[cfg(target_arch = "x86_64")]
    let b = packed >> 8 & 7;
    if f.flags().is_some_and(|fl| fl.hw_reduced_acpi()) {
        if let Some(scr) = f.sleep_control_register().filter(|g| g.address != 0) {
            kprintln!("power: off (ACPI S5, sleep control register)");
            write_gas(scr, 1, u32::from((a as u8) << 2 | SCR_SLP_EN));
        }
    } else {
        #[cfg(target_arch = "x86_64")]
        pm1_sleep(&f, a, b);
    }
}

/// Writes SLP_TYP, then SLP_TYP with SLP_EN, to the PM1 control blocks.
#[cfg(target_arch = "x86_64")]
fn pm1_sleep(f: &acpi_tables::fadt::Fadt<'_>, a: u32, b: u32) {
    let Some(cnt_a) = f.pm1a_control_block().filter(|g| g.address != 0) else {
        return;
    };
    let cnt_b = f.pm1b_control_block().filter(|g| g.address != 0);
    kprintln!("power: off (ACPI S5)");
    for (reg, typ) in [(Some(cnt_a), a), (cnt_b, b)] {
        if let Some(r) = reg {
            let v = read_gas16(r) & !(7 << SLP_TYP_SHIFT | SLP_EN) | typ << SLP_TYP_SHIFT;
            write_gas(r, 2, v);
        }
    }
    for (reg, typ) in [(Some(cnt_a), a), (cnt_b, b)] {
        if let Some(r) = reg {
            let v = read_gas16(r) & !(7 << SLP_TYP_SHIFT) | typ << SLP_TYP_SHIFT | SLP_EN;
            write_gas(r, 2, v);
        }
    }
}

/// Spins long enough for a power-off or reset to take effect.
fn settle() {
    for _ in 0..100_000_000u64 {
        core::hint::spin_loop();
    }
}

/// Powers the machine off. Interrupts go off first; nothing returns.
pub(crate) fn off() -> ! {
    arch::cpu::disable_interrupts();
    #[cfg(target_arch = "aarch64")]
    {
        kprintln!("power: off (PSCI SYSTEM_OFF)");
        arch::smp::psci_system_off(crate::smp::psci_conduit());
    }
    acpi_s5();
    settle();
    kprintln!("power: off (UEFI ResetSystem)");
    crate::firmware::reset(efi::ResetKind::Shutdown)
}

/// Resets the machine: the FADT reset register, PSCI `SYSTEM_RESET`, then
/// UEFI.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn reset() -> ! {
    #[cfg(target_arch = "aarch64")]
    kprintln!("power: reset (PSCI SYSTEM_RESET)");
    arch::cpu::disable_interrupts();
    if let Some(r) = fadt().and_then(|f| f.reset_register()) {
        kprintln!("power: reset (ACPI reset register)");
        write_gas(r.register, 1, u32::from(r.value));
        settle();
    }
    #[cfg(target_arch = "aarch64")]
    arch::smp::psci_system_reset(crate::smp::psci_conduit());
    kprintln!("power: reset (UEFI ResetSystem)");
    crate::firmware::reset(efi::ResetKind::Cold)
}
