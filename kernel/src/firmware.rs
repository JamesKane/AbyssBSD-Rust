// SPDX-License-Identifier: BSD-2-Clause
//! UEFI runtime services, called in physical mode: the loader does not
//! call `SetVirtualAddressMap`, so each call runs under a table that maps
//! the runtime regions 1:1 (built in `mm`), as FreeBSD's `efirt` does.
//!
//! Every call goes through [`call`]: one at a time (runtime services are
//! not reentrant), with interrupts off, and with the CPU's FP/SIMD
//! registers saved around it, since firmware may use them and they hold
//! the interrupted user thread's state (`fpu_kern_enter` in FreeBSD's
//! `efi_enter`). Whatever a service reads or writes must be mapped under
//! the firmware table: kernel memory (the upper half) is, user memory is
//! not, so callers pass kernel buffers.
//!
//! Behavior reference: sys/dev/efidev/efirt.c.

use alloc::vec::Vec;

use abi::errno::Errno;
use addr::PhysAddr;
use bootproto::{BootInfo, MemoryKind};
use efi::raw::{RawGuid, RuntimeServices, Time, TimeCapabilities};
use efi::{ResetKind, Status};
use sync::SpinLock;

use crate::arch;

#[derive(Clone, Copy, Debug)]
struct Runtime {
    table: PhysAddr,
    services: PhysAddr,
    system_table: PhysAddr,
}

static RUNTIME: SpinLock<Option<Runtime>> = SpinLock::new(None);

/// Records where the runtime services are and the table to call them
/// under. Without a services table (as `efi_init` finds none) there are
/// no runtime services.
pub(crate) fn init(info: &BootInfo, table: PhysAddr) {
    let services = info.firmware.uefi_runtime_services;
    if services.get() == 0 {
        return;
    }
    *RUNTIME.lock() = Some(Runtime {
        table,
        services,
        system_table: info.firmware.uefi_system_table,
    });
}

/// Whether runtime services are available (`efi_rt_ok`).
pub(crate) fn available() -> bool {
    RUNTIME.lock().is_some()
}

/// A save area for the FP/SIMD registers across a call.
struct FpuSave {
    buf: Vec<u8>,
    offset: usize,
}

impl FpuSave {
    fn new() -> Result<Self, Errno> {
        let size = hal::fpu::area_size();
        let mut buf = Vec::new();
        buf.try_reserve_exact(size + hal::fpu::ALIGN)
            .map_err(|_| Errno::ENOMEM)?;
        buf.resize(size + hal::fpu::ALIGN, 0);
        let offset = buf.as_ptr().align_offset(hal::fpu::ALIGN);
        Ok(Self { buf, offset })
    }

    fn area(&mut self) -> Option<*mut u8> {
        (hal::fpu::area_size() != 0).then(|| self.buf[self.offset..].as_mut_ptr())
    }
}

/// Runs `f` on the services table: under the firmware table, with
/// interrupts off and the FP/SIMD registers saved, holding the runtime
/// lock.
///
/// # Errors
///
/// `ENXIO` without runtime services, `ENOMEM` without memory for the
/// FP/SIMD save area.
///
/// # Safety
///
/// Every pointer `f` passes to firmware must be valid for the service and
/// mapped under the firmware table (kernel memory, not user memory).
unsafe fn call<R>(f: impl FnOnce(&RuntimeServices) -> R) -> Result<R, Errno> {
    let mut fpu = FpuSave::new()?;
    // Held across the call: runtime services are not reentrant.
    let guard = RUNTIME.lock();
    let rt = (*guard).ok_or(Errno::ENXIO)?;
    let saved = arch::cpu::save_disable_interrupts();
    let area = fpu.area();
    if let Some(area) = area {
        // SAFETY: `area` is `area_size` bytes aligned to `ALIGN`, in
        // `fpu`'s buffer; saving only reads the registers.
        unsafe { hal::fpu::save(area) };
    }
    // SAFETY: interrupts are off, so nothing else runs on this CPU while
    // the lower half is the firmware's; the firmware table maps the
    // runtime regions 1:1 and shares the kernel's upper half (code, stack,
    // heap). The services table lives in runtime data, so its physical
    // address is valid under that table, as are the function pointers in
    // it. The caller vouches for the arguments.
    let r = unsafe {
        arch::mmu::with_tables(rt.table, || {
            let services = &*(rt.services.get() as *const RuntimeServices);
            f(services)
        })
    };
    if let Some(area) = area {
        // SAFETY: the area holds the state saved above.
        unsafe { hal::fpu::restore(area) };
    }
    drop(guard);
    if saved {
        arch::cpu::enable_interrupts();
    }
    Ok(r)
}

/// What a service's status returns (`efi_status_to_errno`).
fn status(s: Result<Status, Errno>) -> Result<(), Errno> {
    abi::efi::status_to_errno(u64::try_from(s?.0).unwrap_or(u64::MAX))
}

/// `GetTime` (`efi_get_time`), as an error number on failure.
pub(crate) fn get_time() -> Result<Time, Errno> {
    let mut t = Time::default();
    // Some firmware fails a null capabilities pointer, optional as the
    // spec makes it; FreeBSD passes one it ignores.
    let mut cap = TimeCapabilities::default();
    // SAFETY: both are on the kernel stack.
    status(unsafe { call(|rs| (rs.get_time)(&raw mut t, &raw mut cap)) })?;
    Ok(t)
}

/// `GetTime`'s capabilities (`efi_get_time_capabilities`): the clock's
/// resolution in counts per second, its accuracy, and whether `SetTime`
/// zeroes the part of the second below its resolution.
pub(crate) fn get_time_capabilities() -> Result<TimeCapabilities, Errno> {
    let mut t = Time::default();
    let mut cap = TimeCapabilities::default();
    // SAFETY: both are on the kernel stack.
    status(unsafe { call(|rs| (rs.get_time)(&raw mut t, &raw mut cap)) })?;
    Ok(cap)
}

/// The time from UEFI's `GetTime`, if runtime services are available.
pub(crate) fn time() -> Option<Time> {
    get_time().ok()
}

/// `SetTime` (`efi_set_time`).
pub(crate) fn set_time(t: &Time) -> Result<(), Errno> {
    // SAFETY: `t` is kernel memory.
    status(unsafe { call(|rs| (rs.set_time)(t)) })
}

/// `GetWakeupTime` (`efi_get_waketime`): whether the alarm is enabled,
/// whether it is pending, and its time.
pub(crate) fn get_wakeup_time() -> Result<(u8, u8, Time), Errno> {
    let (mut enabled, mut pending, mut t) = (0u8, 0u8, Time::default());
    // SAFETY: all three are on the kernel stack.
    status(unsafe {
        call(|rs| (rs.get_wakeup_time)(&raw mut enabled, &raw mut pending, &raw mut t))
    })?;
    Ok((enabled, pending, t))
}

/// `SetWakeupTime` (`efi_set_waketime`).
pub(crate) fn set_wakeup_time(enable: u8, t: &Time) -> Result<(), Errno> {
    // SAFETY: `t` is kernel memory.
    status(unsafe { call(|rs| (rs.set_wakeup_time)(enable, t)) })
}

/// `GetVariable` (`efi_var_get`): `name` is NUL-terminated UCS-2 (the
/// caller checks), `attrib` and `size` are updated as firmware leaves
/// them; `size` starts as the room in `data`, at most `data.len()`.
pub(crate) fn get_variable(
    name: &[u16],
    vendor: &RawGuid,
    attrib: &mut u32,
    size: &mut usize,
    data: &mut [u8],
) -> Result<(), Errno> {
    *size = (*size).min(data.len());
    let data = data.as_mut_ptr();
    // SAFETY: every pointer is to kernel memory: `name` ends in a NUL
    // within it, and `data` holds `*size` bytes.
    status(unsafe { call(|rs| (rs.get_variable)(name.as_ptr(), vendor, attrib, size, data)) })
}

/// `GetNextVariableName` (`efi_var_nextname`): `name` holds the previous
/// name (NUL-terminated within it, the caller checks) and receives the
/// next; `size` is the room in bytes (at most `name`'s), updated to the
/// size needed or written.
pub(crate) fn next_variable_name(
    size: &mut usize,
    name: &mut [u16],
    vendor: &mut RawGuid,
) -> Result<(), Errno> {
    *size = (*size).min(name.len() * 2);
    let name = name.as_mut_ptr();
    // SAFETY: kernel memory; `name` has `*size` bytes of room.
    status(unsafe { call(|rs| (rs.get_next_variable_name)(size, name, vendor)) })
}

/// `SetVariable` (`efi_var_set`): empty `data` deletes the variable (a
/// null data pointer, as FreeBSD passes).
pub(crate) fn set_variable(
    name: &[u16],
    vendor: &RawGuid,
    attrib: u32,
    data: &[u8],
) -> Result<(), Errno> {
    let ptr = if data.is_empty() {
        core::ptr::null()
    } else {
        data.as_ptr()
    };
    // SAFETY: kernel memory: `name` ends in a NUL within it, `ptr` holds
    // `data.len()` bytes or is null for none.
    status(unsafe { call(|rs| (rs.set_variable)(name.as_ptr(), vendor, attrib, data.len(), ptr)) })
}

/// Memory that holds firmware tables for good: kept by the kernel (never
/// handed to the allocator) and in the DMAP.
fn kept(pa: u64, len: u64) -> bool {
    let Some(end) = pa.checked_add(len) else {
        return false;
    };
    crate::boot::memory_map().iter().any(|r| {
        matches!(
            r.kind,
            MemoryKind::RuntimeCode
                | MemoryKind::RuntimeData
                | MemoryKind::AcpiReclaimable
                | MemoryKind::AcpiNvs
        ) && {
            let range = r.range();
            range.start.get() <= pa && end <= range.end.get()
        }
    })
}

/// `len` bytes of physical memory at `pa`, if they are [`kept`].
fn read_kept(pa: u64, out: &mut [u8]) -> Result<(), Errno> {
    let len = u64::try_from(out.len()).map_err(|_| Errno::EINVAL)?;
    if !kept(pa, len) {
        return Err(Errno::ENOENT);
    }
    let range = addr::PhysRange::from_len(PhysAddr::new(pa), len).ok_or(Errno::ENOENT)?;
    let bytes = crate::boot::phys_bytes(range);
    if bytes.len() != out.len() {
        return Err(Errno::ENOENT);
    }
    out.copy_from_slice(bytes);
    Ok(())
}

/// The physical address of the configuration table `guid` names
/// (`efi_get_table`), read from the system table.
///
/// # Errors
///
/// `ENXIO` without a system table or configuration tables, `ENOENT` if no
/// entry has `guid`.
pub(crate) fn config_table(guid: &[u8; 16]) -> Result<u64, Errno> {
    let st = RUNTIME
        .lock()
        .map(|r| r.system_table.get())
        .ok_or(Errno::ENXIO)?;
    // `EFI_SYSTEM_TABLE`: the header (24 bytes), then pointers; the entry
    // count and the array are its last two fields.
    let mut hdr = [0u8; 8];
    read_kept(st, &mut hdr).map_err(|_| Errno::ENXIO)?;
    if u64::from_ne_bytes(hdr) != efi::raw::SystemTable::SIGNATURE {
        return Err(Errno::ENXIO);
    }
    let field = |off: usize| -> Result<u64, Errno> {
        let mut b = [0u8; 8];
        let at = u64::try_from(off)
            .ok()
            .and_then(|o| st.checked_add(o))
            .ok_or(Errno::ENXIO)?;
        read_kept(at, &mut b).map_err(|_| Errno::ENXIO)?;
        Ok(u64::from_ne_bytes(b))
    };
    let count = field(core::mem::offset_of!(
        efi::raw::SystemTable,
        number_of_table_entries
    ))?;
    let array = field(core::mem::offset_of!(
        efi::raw::SystemTable,
        configuration_table
    ))?;
    if array == 0 {
        return Err(Errno::ENXIO);
    }
    // `EFI_CONFIGURATION_TABLE`: a GUID and a pointer.
    // Bounded: a corrupt count stops at memory the kernel did not keep.
    const ENTRY: usize = 24;
    let mut at = array;
    for _ in 0..count.min(4096) {
        let mut e = [0u8; ENTRY];
        read_kept(at, &mut e).map_err(|_| Errno::ENXIO)?;
        at = at.checked_add(24).ok_or(Errno::ENXIO)?;
        if e[..16] == guid[..] {
            let mut p = [0u8; 8];
            p.copy_from_slice(&e[16..]);
            return Ok(u64::from_ne_bytes(p));
        }
    }
    Err(Errno::ENOENT)
}

/// Copies `out.len()` bytes of the configuration table at `pa`
/// (`physcopyout`), if the table is in memory the kernel kept.
///
/// # Errors
///
/// `ENOENT` for a table in memory the kernel has reused (boot-services
/// memory, where the spec puts the ESRT) or does not map.
pub(crate) fn read_table(pa: u64, out: &mut [u8]) -> Result<(), Errno> {
    read_kept(pa, out)
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
                let services = &*(rt.services.get() as *const RuntimeServices);
                (services.reset_system)(kind as u32, Status::SUCCESS, 0, core::ptr::null())
            })
        }
    }
    arch::cpu::halt_forever()
}
