// SPDX-License-Identifier: BSD-2-Clause
//! UEFI runtime services, called in physical mode: the loader does not
//! call `SetVirtualAddressMap`, so each call runs under a table that maps
//! the runtime regions 1:1 (built in `mm`), as FreeBSD's `efirt` does.

use addr::PhysAddr;
use bootproto::BootInfo;
use efi::{ResetKind, Status};
use sync::SpinLock;

use crate::arch;

#[derive(Clone, Copy, Debug)]
struct Runtime {
    table: PhysAddr,
    services: PhysAddr,
}

static RUNTIME: SpinLock<Option<Runtime>> = SpinLock::new(None);

/// Records where the runtime services are and the table to call them
/// under.
pub(crate) fn init(info: &BootInfo, table: PhysAddr) {
    *RUNTIME.lock() = Some(Runtime {
        table,
        services: info.firmware.uefi_runtime_services,
    });
}

/// Resets or powers off the machine through firmware.
pub(crate) fn reset(kind: ResetKind) -> ! {
    arch::cpu::disable_interrupts();
    let rt = RUNTIME.try_lock().and_then(|r| *r);
    if let Some(rt) = rt {
        // SAFETY: interrupts are off; the firmware table maps the runtime
        // regions 1:1 and shares the kernel's upper half (code and stack).
        // The services table lives in runtime data, so its physical address
        // is valid under that table, as is the function pointer in it.
        unsafe {
            arch::mmu::with_tables(rt.table, || {
                let services = &*(rt.services.get() as *const efi::raw::RuntimeServices);
                (services.reset_system)(kind as u32, Status::SUCCESS, 0, core::ptr::null())
            })
        }
    }
    arch::cpu::halt_forever()
}
