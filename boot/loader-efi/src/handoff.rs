// SPDX-License-Identifier: BSD-2-Clause
//! Building the [`BootInfo`] handoff.
//!
//! Its memory is allocated before `ExitBootServices` ([`prepare`]); the
//! final memory map is converted into it afterwards ([`Handoff::finish`]),
//! when nothing may allocate.

use addr::PhysAddr;
use bootproto::{BootInfo, Header, MemoryKind, MemoryRegion, Table};
use efi::{Allocate, MemoryMap, MemoryType, SystemTable};

use crate::{Error, memtype};

/// Handoff memory, allocated and waiting for the final memory map.
#[derive(Debug)]
pub(crate) struct Handoff {
    info: PhysAddr,
    regions: PhysAddr,
    capacity: usize,
    cmdline: PhysAddr,
    cmdline_len: usize,
}

const INFO_SPACE: usize = size_of::<BootInfo>().next_multiple_of(64);

/// Allocates room for the handoff, a memory map of up to `capacity`
/// entries, and a copy of `cmdline`.
pub(crate) fn prepare(st: &SystemTable, capacity: usize, cmdline: &[u8]) -> Result<Handoff, Error> {
    let regions_bytes = capacity * size_of::<MemoryRegion>();
    let total = INFO_SPACE + regions_bytes + cmdline.len();
    let base = st
        .allocate_pages(Allocate::Anywhere, memtype::BOOT_DATA, total.div_ceil(4096))
        .map_err(|s| Error::Efi("allocate handoff", s))?;
    let base = PhysAddr::new(base);
    let regions = base + INFO_SPACE as u64;
    let cmd = regions + regions_bytes as u64;
    // SAFETY: freshly allocated, identity-mapped pages sized above.
    unsafe {
        core::ptr::copy_nonoverlapping(cmdline.as_ptr(), cmd.get() as *mut u8, cmdline.len());
    }
    Ok(Handoff {
        info: base,
        regions,
        capacity,
        cmdline: cmd,
        cmdline_len: cmdline.len(),
    })
}

/// What [`Handoff::finish`] needs besides the memory map.
#[derive(Debug)]
pub(crate) struct Fields {
    pub(crate) kernel: bootproto::KernelImage,
    pub(crate) serial: Option<bootproto::Serial>,
    pub(crate) framebuffer: Option<bootproto::Framebuffer>,
    pub(crate) firmware: bootproto::Firmware,
    pub(crate) symbols: Option<bootproto::Symbols>,
    pub(crate) entropy: bootproto::Entropy,
}

impl Handoff {
    /// Converts the final memory map and writes the handoff. Runs after
    /// `ExitBootServices`: no allocation, no firmware calls.
    pub(crate) fn finish(self, map: &MemoryMap<'_>, f: Fields) -> PhysAddr {
        // SAFETY: `prepare` allocated room for `capacity` regions here, and
        // nothing else uses the pages.
        let regions = unsafe {
            core::slice::from_raw_parts_mut(self.regions.get() as *mut MemoryRegion, self.capacity)
        };
        let mut n = 0;
        for d in map.iter().filter(|d| d.pages > 0) {
            if n == regions.len() {
                break; // `prepare` left generous slack; see `main`.
            }
            regions[n] = MemoryRegion {
                start: PhysAddr::new(d.start),
                pages: d.pages,
                kind: kind(d.kind),
                reserved: 0,
                firmware_attributes: d.attribute,
            };
            n += 1;
        }
        let kernel = f.kernel.phys.get()..f.kernel.phys.get() + f.kernel.size;
        let n = relabel(regions, n, kernel, MemoryKind::Kernel);
        let len = normalize(&mut regions[..n]);
        let info = BootInfo {
            header: Header::CURRENT,
            kernel: f.kernel,
            memory_map: Table::new(self.regions, len as u64),
            serial: f.serial.into(),
            framebuffer: f.framebuffer.into(),
            firmware: f.firmware,
            cmdline: Table::new(self.cmdline, self.cmdline_len as u64),
            symbols: f.symbols.into(),
            entropy: f.entropy,
        };
        // SAFETY: `prepare` reserved `INFO_SPACE` bytes at `info`, suitably
        // aligned (page start).
        unsafe { (self.info.get() as *mut BootInfo).write(info) };
        self.info
    }
}

/// Gives `range` (page aligned) `kind`, splitting the regions it overlaps.
/// Returns the new count; splits that do not fit are dropped (`prepare`
/// leaves slack for them).
fn relabel(
    r: &mut [MemoryRegion],
    mut n: usize,
    range: core::ops::Range<u64>,
    kind: MemoryKind,
) -> usize {
    let mut i = 0;
    while i < n {
        let cur = r[i];
        let (s, e) = (cur.start.get(), cur.range().end.get());
        if e <= range.start || s >= range.end || cur.kind == kind {
            i += 1;
            continue;
        }
        // Up to three pieces: before, inside, after.
        let pieces = [
            (s, s.max(range.start).min(e), cur.kind),
            (s.max(range.start), e.min(range.end), kind),
            (e.min(range.end).max(s), e, cur.kind),
        ];
        let mut out = [cur; 3];
        let mut m = 0;
        for (ps, pe, k) in pieces {
            if pe > ps {
                out[m] = MemoryRegion {
                    start: PhysAddr::new(ps),
                    pages: (pe - ps) / 4096,
                    kind: k,
                    ..cur
                };
                m += 1;
            }
        }
        r[i] = out[0];
        for piece in &out[1..m] {
            if n < r.len() {
                r[n] = *piece;
                n += 1;
            }
        }
        i += 1;
    }
    n
}

/// Sorts by address and merges neighbours of the same kind and attributes.
/// Returns the new length.
fn normalize(r: &mut [MemoryRegion]) -> usize {
    r.sort_unstable_by_key(|m| m.start);
    let mut out = 0;
    for i in 0..r.len() {
        let cur = r[i];
        if out > 0 {
            let last = &mut r[out - 1];
            if last.kind == cur.kind
                && last.firmware_attributes == cur.firmware_attributes
                && last.range().end == cur.start
            {
                last.pages += cur.pages;
                continue;
            }
        }
        r[out] = cur;
        out += 1;
    }
    out
}

/// The kernel's view of a UEFI memory type.
fn kind(t: MemoryType) -> MemoryKind {
    match t {
        MemoryType::LOADER_CODE
        | MemoryType::LOADER_DATA
        | MemoryType::BOOT_SERVICES_CODE
        | MemoryType::BOOT_SERVICES_DATA
        | MemoryType::CONVENTIONAL => MemoryKind::Free,
        MemoryType::RUNTIME_SERVICES_CODE => MemoryKind::RuntimeCode,
        MemoryType::RUNTIME_SERVICES_DATA => MemoryKind::RuntimeData,
        MemoryType::ACPI_RECLAIM => MemoryKind::AcpiReclaimable,
        MemoryType::ACPI_NVS => MemoryKind::AcpiNvs,
        MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE => MemoryKind::Mmio,
        MemoryType::UNUSABLE => MemoryKind::Unusable,
        MemoryType::PERSISTENT => MemoryKind::Persistent,
        memtype::KERNEL => MemoryKind::Kernel,
        memtype::BOOT_DATA => MemoryKind::BootData,
        memtype::LOG_RING => MemoryKind::LogRing,
        _ => MemoryKind::Reserved,
    }
}
