// SPDX-License-Identifier: BSD-2-Clause
//! The initial page tables the kernel starts on (layout in
//! `bootproto::layout`):
//!
//! - every RAM range identity-mapped (W+X) in the lower half, so the
//!   kernel's entry code can switch tables while running at physical
//!   addresses;
//! - every RAM range in the DMAP;
//! - the kernel segments at their slid addresses, with ELF permissions;
//! - the console devices in the device window.
//!
//! Frames come from UEFI as boot data, so the kernel can find (and later
//! reclaim) them.

use alloc::vec::Vec;

use addr::{PhysAddr, VirtAddr};
use bootproto::layout::{DEVICE_WINDOW_BASE, DEVICE_WINDOW_SIZE, DMAP_SIZE, dmap};
use bootproto::{Framebuffer, Mmio, Serial};
use efi::{Allocate, MemoryType, SystemTable};
use pt::{Attrs, Frame, PageSize, PageTable};

use crate::devices::{self, SerialPort};
use crate::kernel::Kernel;
use crate::{Error, memtype};

#[cfg(target_arch = "x86_64")]
type Format = pt::x86_64::X86_64;
#[cfg(target_arch = "aarch64")]
type Format = pt::aarch64::Aarch64;

/// Page-table frames from UEFI.
struct Frames<'a> {
    st: &'a SystemTable,
}

impl pt::Memory for Frames<'_> {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        let pa = self
            .st
            .allocate_pages(Allocate::Anywhere, memtype::BOOT_DATA, 1)
            .ok()?;
        // SAFETY: a fresh page, identity mapped.
        unsafe { (pa as *mut Frame).write([0; pt::ENTRIES]) };
        Some(PhysAddr::new(pa))
    }

    fn frame(&mut self, pa: PhysAddr) -> &mut Frame {
        // SAFETY: `pa` is a frame `alloc_frame` returned; firmware identity
        // maps it and nothing else references it while the tables are
        // being built.
        unsafe { &mut *(pa.get() as *mut Frame) }
    }
}

/// The built hierarchy and where the console devices ended up.
#[derive(Debug)]
pub(crate) struct Built {
    pub(crate) kernel_root: PhysAddr,
    pub(crate) identity_root: PhysAddr,
    pub(crate) serial: Option<Serial>,
    pub(crate) framebuffer: Option<Framebuffer>,
}

/// Whether memory of this UEFI type is RAM the kernel maps.
pub(crate) fn is_ram(t: MemoryType) -> bool {
    matches!(
        t,
        MemoryType::LOADER_CODE
            | MemoryType::LOADER_DATA
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
            | MemoryType::RUNTIME_SERVICES_CODE
            | MemoryType::RUNTIME_SERVICES_DATA
            | MemoryType::CONVENTIONAL
            | MemoryType::ACPI_RECLAIM
            | MemoryType::ACPI_NVS
            | MemoryType::PERSISTENT
    ) || t.0 >= 0x8000_0000
}

/// The RAM ranges in the current memory map, sorted and coalesced.
pub(crate) fn ram_ranges(st: &SystemTable) -> Result<Vec<(u64, u64)>, Error> {
    let size = st
        .memory_map_size()
        .map_err(|s| Error::Efi("memory map size", s))?;
    let mut buf = alloc::vec![0u8; size];
    let map = st
        .memory_map(&mut buf)
        .map_err(|s| Error::Efi("memory map", s))?;
    let mut ranges: Vec<(u64, u64)> = map
        .iter()
        .filter(|d| is_ram(d.kind) && d.pages > 0)
        .map(|d| (d.start, d.start + d.pages * 4096))
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    Ok(merged)
}

/// Builds the tables.
pub(crate) fn build(
    st: &SystemTable,
    ram: &[(u64, u64)],
    kernel: &Kernel,
    serial: Option<SerialPort>,
    fb: Option<devices::Framebuffer>,
) -> Result<Built, Error> {
    let mut mem = Frames { st };
    let max = max_leaf();
    let mut upper = PageTable::<Format>::new(&mut mem, max).map_err(Error::Map)?;
    let mut lower = if cfg!(target_arch = "aarch64") {
        Some(PageTable::<Format>::new(&mut mem, max).map_err(Error::Map)?)
    } else {
        None
    };

    for &(start, end) in ram {
        if end > DMAP_SIZE {
            return Err(Error::Kernel("RAM beyond the DMAP"));
        }
        let len = end - start;
        let identity = lower.as_mut().unwrap_or(&mut upper);
        identity
            .map(
                &mut mem,
                VirtAddr::new(start),
                PhysAddr::new(start),
                len,
                Attrs::IDENTITY,
            )
            .map_err(Error::Map)?;
        let v = dmap(PhysAddr::new(start)).ok_or(Error::Kernel("RAM beyond the DMAP"))?;
        upper
            .map(&mut mem, v, PhysAddr::new(start), len, Attrs::KERNEL_RW)
            .map_err(Error::Map)?;
    }

    for s in &kernel.segments {
        let attrs = match (s.write, s.exec) {
            (false, true) => Attrs::KERNEL_RX,
            (false, false) => Attrs::KERNEL_RO,
            (true, false) => Attrs::KERNEL_RW,
            (true, true) => return Err(Error::Kernel("writable and executable segment")),
        };
        upper
            .map(&mut mem, s.virt, s.phys, s.len, attrs)
            .map_err(Error::Map)?;
    }

    let mut window = Window {
        next: DEVICE_WINDOW_BASE,
    };
    let serial = match serial {
        None => None,
        Some(SerialPort::Port(port)) => Some(Serial::Ns16550Port { port }),
        Some(SerialPort::Ns16550 { phys, shift, width }) => {
            let m = window.map(&mut upper, &mut mem, phys, 4096, Attrs::DEVICE)?;
            Some(Serial::Ns16550Mmio(Mmio {
                reg_shift: shift,
                access_width: width,
                ..m
            }))
        }
        Some(SerialPort::Pl011 { phys }) => {
            let m = window.map(&mut upper, &mut mem, phys, 4096, Attrs::DEVICE)?;
            Some(Serial::Pl011(Mmio {
                access_width: 4,
                ..m
            }))
        }
    };
    let framebuffer = match fb {
        None => None,
        Some(fb) => {
            let m = window.map(&mut upper, &mut mem, fb.phys, fb.size, Attrs::FRAMEBUFFER)?;
            Some(Framebuffer {
                phys: m.phys,
                virt: m.virt,
                size: m.size,
                width: fb.width,
                height: fb.height,
                stride: fb.stride,
                format: fb.format,
            })
        }
    };

    Ok(Built {
        kernel_root: upper.root(),
        identity_root: lower.as_ref().map_or(upper.root(), PageTable::root),
        serial,
        framebuffer,
    })
}

/// Hands out device-window addresses, 2 MiB aligned so large framebuffers
/// get large pages.
struct Window {
    next: u64,
}

impl Window {
    fn map(
        &mut self,
        table: &mut PageTable<Format>,
        mem: &mut Frames<'_>,
        phys: u64,
        size: u64,
        attrs: Attrs,
    ) -> Result<Mmio, Error> {
        let base = addr::align_down(phys, 4096);
        let offset = phys - base;
        let len = addr::align_up(size + offset, 4096).ok_or(Error::Kernel("device size"))?;
        // Keep the 2 MiB phase so large pages line up.
        let phase = base % (2 << 20);
        let virt =
            addr::align_up(self.next, 2 << 20).ok_or(Error::Kernel("device window"))? + phase;
        if virt + len > DEVICE_WINDOW_BASE + DEVICE_WINDOW_SIZE {
            return Err(Error::Kernel("device window full"));
        }
        table
            .map(mem, VirtAddr::new(virt), PhysAddr::new(base), len, attrs)
            .map_err(Error::Map)?;
        self.next = virt + len;
        Ok(Mmio {
            phys: PhysAddr::new(phys),
            virt: VirtAddr::new(virt + offset),
            size,
            reg_shift: 0,
            access_width: 1,
        })
    }
}

#[cfg(target_arch = "x86_64")]
fn max_leaf() -> PageSize {
    use core::arch::x86_64::__cpuid;
    if __cpuid(0x8000_0000).eax >= 0x8000_0001 && __cpuid(0x8000_0001).edx & (1 << 26) != 0 {
        PageSize::Size1G
    } else {
        PageSize::Size2M
    }
}

#[cfg(target_arch = "aarch64")]
fn max_leaf() -> PageSize {
    PageSize::Size1G
}
