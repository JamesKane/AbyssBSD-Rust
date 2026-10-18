// SPDX-License-Identifier: BSD-2-Clause
//! The AbyssBSD UEFI loader.
//!
//! Phase 0: identifies itself and the firmware, then powers off. Built with
//! the `ktest` feature it runs the in-firmware tests instead. Loading the
//! kernel arrives in Phase 1 (docs/ROADMAP.md).

#![no_std]
#![no_main]

#[cfg(feature = "ktest")]
mod tests;

use core::fmt::Write;
use core::sync::atomic::{AtomicPtr, Ordering};

use efi::{Handle, ResetKind, Status, SystemTable, raw};

/// The system table, kept for the panic handler.
static SYSTEM_TABLE: AtomicPtr<raw::SystemTable> = AtomicPtr::new(core::ptr::null_mut());

const ARCH: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unknown"
};

#[unsafe(no_mangle)]
extern "efiapi" fn efi_main(_image: Handle, st: *mut raw::SystemTable) -> Status {
    // SAFETY: firmware passes the live system table to the entry point.
    let Some(system) = (unsafe { SystemTable::from_raw(st) }) else {
        return Status::ABORTED;
    };
    SYSTEM_TABLE.store(st, Ordering::Release);

    let mut con = system.console();
    let fw = system.firmware();
    let rev = system.revision();
    let _ = writeln!(
        con,
        "\nAbyssBSD loader {} ({ARCH})",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(
        con,
        "UEFI {}.{} firmware: {fw} rev {:#x}",
        rev >> 16,
        (rev & 0xFFFF) / 10,
        fw.revision
    );
    let acpi = system
        .configuration_tables()
        .iter()
        .any(|t| t.vendor_guid.get() == efi::tables::ACPI_20);
    let _ = writeln!(
        con,
        "ACPI 2.0+ RSDP: {}",
        if acpi { "present" } else { "missing" }
    );

    #[cfg(feature = "ktest")]
    let _ = ktest::run_all(&mut con);

    #[cfg(not(feature = "ktest"))]
    let _ = writeln!(con, "Phase 0 stub: no kernel to load yet. Powering off.");

    system.reset(ResetKind::Shutdown, Status::SUCCESS)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let st = SYSTEM_TABLE.load(Ordering::Acquire);
    // SAFETY: the pointer is null or the system table stored by `efi_main`,
    // and the loader never exits boot services in this phase.
    if let Some(system) = unsafe { SystemTable::from_raw(st) } {
        ktest::report_panic(&mut system.console(), info);
        system.reset(ResetKind::Shutdown, Status::ABORTED);
    }
    loop {
        core::hint::spin_loop();
    }
}
