// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/mem` and `/dev/kmem`: physical memory, and kernel virtual memory,
//! read with `read(2)` and (`/dev/mem` only) mapped with `mmap(2)`, as
//! `dmidecode`, fastfetch, and `acpidump` read firmware tables.
//!
//! As FreeBSD's: owned by root and group kmem, mode 0640; opening for
//! reading needs `PRIV_KMEM_READ` (which every user has), for writing
//! `PRIV_KMEM_WRITE` and a securelevel of 0 or less (`memopen`); a mapping
//! of `/dev/mem` at offset `pa` is the physical page at `pa` (`memmmap`).
//!
//! Unlike FreeBSD's (docs/compat-deviations.md):
//!
//! - Only memory is reachable: the RAM the memory map lists (which holds
//!   the ACPI tables and UEFI runtime regions), and the pages of firmware
//!   tables (SMBIOS's) in regions firmware reserved. Device registers and
//!   addresses no region covers are refused (`EFAULT` for a read, `EINVAL`
//!   for a mapping), where FreeBSD reads or maps them: on the Orange Pi 6
//!   Plus, reading the wrong address through FreeBSD's `/dev/mem` stops
//!   the machine.
//! - Writing is refused: an open for writing that passes FreeBSD's checks
//!   then fails with `EPERM`.
//! - `/dev/kmem` reaches only the DMAP (kernel addresses of RAM), not the
//!   rest of the kernel's address space; it cannot be mapped, as FreeBSD's
//!   cannot.
//!
//! Behavior reference: sys/dev/mem/memdev.c, sys/arm64/arm64/mem.c,
//! sys/amd64/amd64/mem.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use bootproto::MemoryKind;
use bootproto::layout::{dmap, dmap_to_phys};
use sched::Sleepable;
use security::Priv;
use sync::Once;
use vfs::{CharDevice, Uio};

const PAGE: u64 = 4096;
/// FreeBSD's `GID_KMEM`.
const GID_KMEM: u32 = 2;

/// Which device.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Minor {
    /// `CDEV_MINOR_MEM`: offsets are physical addresses.
    Mem,
    /// `CDEV_MINOR_KMEM`: offsets are kernel virtual addresses.
    Kmem,
}

struct MemDev(Minor);

/// How a physical page may be reached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reach {
    /// RAM, through the DMAP.
    Dmap,
    /// A firmware table's page outside the DMAP, mapped for the read.
    Firmware,
}

/// The firmware tables' byte ranges: SMBIOS's entry point and structure
/// table where the loader found them. Found once, at boot.
static RANGES: Once<Vec<(u64, u64)>> = Once::new();

fn firmware_ranges() -> &'static [(u64, u64)] {
    RANGES.get().map_or(&[], Vec::as_slice)
}

fn find_firmware_ranges() -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    // The loader's own setting, which `kenv` cannot change.
    let hint = crate::boot::env()
        .split(|&b| b == 0)
        .find_map(|e| e.strip_prefix(b"hint.smbios.0.mem=0x"))
        .and_then(|v| core::str::from_utf8(v).ok())
        .and_then(|v| u64::from_str_radix(v, 16).ok());
    if let Some(ep) = hint {
        let mut b = [0u8; smbios::ENTRY_POINT_MAX];
        if copy_phys(ep, &mut b).is_ok()
            && let Some(e) = smbios::EntryPoint::parse(&b)
        {
            let len = if e.kind == smbios::Kind::V3 {
                0x18
            } else {
                0x1f
            };
            out.push((ep, ep + len));
            out.push((e.table, e.table.saturating_add(u64::from(e.length))));
        }
    }
    out
}

/// The memory-map region holding `pa`.
fn region(pa: u64) -> Option<MemoryKind> {
    crate::boot::memory_map()
        .iter()
        .find(|r| r.range().start.get() <= pa && pa < r.range().end.get())
        .map(|r| r.kind)
}

/// Whether, and how, the page at `pa` may be read or mapped: RAM; or,
/// in a firmware-reserved region, a page holding part of a firmware
/// table. Never device memory, nor an address no region covers.
fn reach(pa: u64) -> Option<Reach> {
    let page = pa & !(PAGE - 1);
    match region(page)? {
        k if k.is_ram() => Some(Reach::Dmap),
        MemoryKind::Mmio => None,
        _ => firmware_ranges()
            .iter()
            .any(|&(s, e)| s < page + PAGE && page < e)
            .then_some(Reach::Firmware),
    }
}

/// Copies physical memory at `pa` into `out`, within one page: RAM
/// through the DMAP, other memory through a mapping made for the read;
/// never device memory. Callers decide what may be read ([`reach`]).
fn copy_phys(pa: u64, out: &mut [u8]) -> Result<(), Errno> {
    let len = u64::try_from(out.len()).map_err(|_| Errno::EFAULT)?;
    let page = pa & !(PAGE - 1);
    if pa + len > page + PAGE {
        return Err(Errno::EFAULT);
    }
    let kind = region(page).ok_or(Errno::EFAULT)?;
    if kind.is_ram() {
        let va = dmap(addr::PhysAddr::new(pa)).ok_or(Errno::EFAULT)?;
        // SAFETY: RAM the memory map lists, which the DMAP maps for the
        // life of the kernel; `out.len()` bytes stay within the page.
        let src = unsafe { core::slice::from_raw_parts(va.get() as *const u8, out.len()) };
        out.copy_from_slice(src);
        return Ok(());
    }
    if kind == MemoryKind::Mmio {
        return Err(Errno::EFAULT);
    }
    let va = crate::mm::map_device(addr::PhysAddr::new(pa), len).map_err(|_| Errno::ENOMEM)?;
    for (i, b) in out.iter_mut().enumerate() {
        // SAFETY: the mapping covers `len` bytes from `va`: firmware's
        // memory, read a byte at a time (it is mapped as device memory).
        *b = unsafe { core::ptr::read_volatile((va.get() as *const u8).add(i)) };
    }
    crate::mm::unmap_device(va, len);
    Ok(())
}

impl MemDev {
    /// The physical address of offset `off`.
    fn phys(&self, off: u64) -> Result<u64, Errno> {
        match self.0 {
            Minor::Mem => Ok(off),
            Minor::Kmem => dmap_to_phys(addr::VirtAddr::new(off))
                .map(addr::PhysAddr::get)
                .ok_or(Errno::EFAULT),
        }
    }
}

impl CharDevice for MemDev {
    fn instance_open(
        &self,
        _s: &Sleepable,
        _nonblock: bool,
        read: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        let p = crate::proc::Process::current().ok_or(Errno::EPERM)?;
        if read {
            p.priv_check(Priv::KmemRead)?;
        }
        if write {
            p.priv_check(Priv::KmemWrite)?;
            p.cred().securelevel_gt(0)?;
            // Not written through: see the module's notes.
            return Err(Errno::EPERM);
        }
        Ok(None)
    }

    /// `memrw`: a page at a time, each page checked.
    fn read(&self, _: &Sleepable, uio: &mut dyn Uio, offset: u64, _: bool) -> Result<usize, Errno> {
        let mut done = 0;
        let mut buf = [0u8; 512];
        while uio.resid() > 0 {
            let at = offset.checked_add(done as u64).ok_or(Errno::EFAULT)?;
            let pa = self.phys(at)?;
            let in_page = usize::try_from(PAGE - (pa & (PAGE - 1))).unwrap_or(0);
            let n = uio.resid().min(in_page).min(buf.len());
            let r = match reach(pa) {
                Some(_) => copy_phys(pa, &mut buf[..n]),
                None => Err(Errno::EFAULT),
            };
            if let Err(e) = r {
                return if done > 0 { Ok(done) } else { Err(e) };
            }
            let c = uio.copy_out(&buf[..n])?;
            done += c;
            if c < n {
                break;
            }
        }
        Ok(done)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }

    /// `memmmap`: `/dev/mem`'s pages are the physical pages at their
    /// offsets, each of which must be memory (above).
    fn mmap(&self, page: u64, pages: u64) -> Result<Arc<vm::object::Object>, Errno> {
        if self.0 != Minor::Mem || pages == 0 {
            return Err(Errno::EINVAL);
        }
        let end = page.checked_add(pages).ok_or(Errno::EINVAL)?;
        for p in page..end {
            let pa = p.checked_mul(PAGE).ok_or(Errno::EINVAL)?;
            if reach(pa).is_none() {
                return Err(Errno::EINVAL);
            }
        }
        // Pages from physical page 0, so that the mapping's offset is the
        // address; it reaches only to the last page checked.
        Ok(vm::object::Object::device(
            crate::mm::space::page_ops(),
            vm::Pfn(0),
            end,
            vm::object::MemAttr::WriteBack,
        ))
    }
}

/// Makes `/dev/mem` and `/dev/kmem`.
pub(crate) fn init() {
    let _ = RANGES.set(find_firmware_ranges());
    crate::fs::devfs::add(b"mem", 0o640, 0, GID_KMEM, Arc::new(MemDev(Minor::Mem)));
    crate::fs::devfs::add(b"kmem", 0o640, 0, GID_KMEM, Arc::new(MemDev(Minor::Kmem)));
}

#[cfg(feature = "ktest")]
mod ktests {
    use ktest::ktest;

    use super::*;

    ktest! {
    fn dev_mem_reaches_only_memory() {
        // RAM the kernel itself is in, through the DMAP.
        let k = crate::boot::info().kernel.phys.get();
        assert_eq!(reach(k), Some(Reach::Dmap));
        // Device registers and holes: refused.
        let mmio = crate::boot::memory_map()
            .iter()
            .find(|r| r.kind == MemoryKind::Mmio)
            .map(|r| r.range().start.get());
        if let Some(pa) = mmio {
            assert_eq!(reach(pa), None);
        }
        assert_eq!(reach(!(PAGE - 1)), None);
        let d = MemDev(Minor::Mem);
        assert!(d.mmap(u64::MAX / PAGE, 1).is_err());
        assert!(d.mmap(k / PAGE, 1).is_ok());
        assert!(MemDev(Minor::Kmem).mmap(k / PAGE, 1).is_err());
        // The SMBIOS entry point the loader found, if any.
        if let Some(&(ep, _)) = firmware_ranges().first() {
            assert!(reach(ep).is_some());
        }
    }
    }
}
