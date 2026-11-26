// SPDX-License-Identifier: BSD-2-Clause
//! GPU address spaces' memory: page tables in device memory, the kernel's own buffers mapped into
//! an address space (spec 09 §9.7), and the update sequence a live address space needs
//! (04 §4.4.1: lock the range, change the tables, flush the caches, unlock).

use alloc::vec::Vec;

use panthor::control;
use panthor::mmu::{self, Map};
use vm::dma::{DmaCoherent, DmaTag};

use crate::mcu::TableMem;
use crate::{Spin, Window};

/// An address space's page tables.
pub(crate) struct PageTables {
    pub(crate) pages: Vec<DmaCoherent<u64>>,
    pub(crate) root: u64,
}

impl PageTables {
    pub(crate) fn new(tag: &DmaTag) -> Option<Self> {
        let root = tag.coherent::<u64>(512).ok()?;
        let pa = root.bus_addr();
        Some(Self {
            pages: alloc::vec![root],
            root: pa,
        })
    }

    /// Maps the pages `pas` (4 KiB each) from `va`.
    pub(crate) fn map_pages(
        &mut self,
        tag: &DmaTag,
        va_bits: u8,
        va: u64,
        pas: &[u64],
        m: Map,
    ) -> Result<(), &'static str> {
        let mut t = TableMem {
            tag,
            pages: core::mem::take(&mut self.pages),
        };
        let mut r = Ok(());
        for (k, &pa) in pas.iter().enumerate() {
            if let Err(e) = mmu::map(
                &mut t,
                self.root,
                va_bits,
                va + 0x1000 * k as u64,
                pa,
                0x1000,
                m,
            ) {
                r = Err(match e {
                    panthor::Error::Bad(why) => why,
                    _ => "mapping failed",
                });
                break;
            }
        }
        self.pages = t.pages;
        r
    }

    /// Maps the contiguous `[pa, pa + len)` at `va`.
    pub(crate) fn map(
        &mut self,
        tag: &DmaTag,
        va_bits: u8,
        va: u64,
        pa: u64,
        len: u64,
        m: Map,
    ) -> Result<(), &'static str> {
        let mut t = TableMem {
            tag,
            pages: core::mem::take(&mut self.pages),
        };
        let r = mmu::map(&mut t, self.root, va_bits, va, pa, len, m).map_err(|_| "mapping failed");
        self.pages = t.pages;
        r
    }

    pub(crate) fn unmap(&mut self, tag: &DmaTag, va_bits: u8, va: u64, len: u64) {
        let mut t = TableMem {
            tag,
            pages: core::mem::take(&mut self.pages),
        };
        let _ = mmu::unmap(&mut t, self.root, va_bits, va, len);
        self.pages = t.pages;
    }
}

/// Runs `f` (which changes the tables of the address space on `slot`, if it is bound) inside the
/// update sequence (04 §4.4.1). With no slot, the tables are changed alone: the next bind makes
/// the GPU see them.
pub(crate) fn update<T>(
    gpu: &Window,
    slot: Option<usize>,
    start: u64,
    end: u64,
    f: impl FnOnce() -> T,
) -> Result<T, &'static str> {
    let Some(slot) = slot else {
        return Ok(f());
    };
    mmu::lock(gpu, &mut Spin, slot, start, end).map_err(|_| "address space lock")?;
    let out = f();
    let flushed = control::flush(gpu, &mut Spin, control::Flush::PAGE_TABLES);
    // Unlocked whatever happened: a region left locked stalls every access to it.
    mmu::command(gpu, &mut Spin, slot, mmu::CMD_UNLOCK).map_err(|_| "address space unlock")?;
    flushed.map_err(|_| "cache flush")?;
    Ok(out)
}

/// A kernel buffer mapped at `va` in one address space.
pub(crate) struct Kbo {
    pub(crate) mem: DmaCoherent<u8>,
    pub(crate) va: u64,
}

impl Kbo {
    pub(crate) fn len(&self) -> u64 {
        self.mem.len() as u64
    }
}

/// An allocator of an address space's kernel range: ranges given back are reused first.
pub(crate) struct VaRange {
    pub(crate) next: u64,
    pub(crate) end: u64,
    pub(crate) free: Vec<(u64, u64)>,
}

impl VaRange {
    /// `size` bytes, 2 MiB aligned for 2 MiB or more, else 4 KiB (09 §9.7).
    pub(crate) fn take(&mut self, size: u64) -> Option<u64> {
        let size = size.next_multiple_of(0x1000);
        let align = if size >= 0x20_0000 { 0x20_0000 } else { 0x1000 };
        if let Some(k) = self
            .free
            .iter()
            .position(|&(va, len)| len == size && va % align == 0)
        {
            return Some(self.free.swap_remove(k).0);
        }
        let va = self.next.checked_next_multiple_of(align)?;
        let end = va.checked_add(size.next_multiple_of(0x1000))?;
        if end > self.end {
            return None;
        }
        self.next = end;
        Some(va)
    }
}

/// Allocates `size` bytes of kernel buffer and maps it in the address space `pt` (bound to
/// `slot`, if it is), from `range`.
#[allow(
    clippy::too_many_arguments,
    reason = "an address space's parts, borrowed apart"
)]
pub(crate) fn kbo(
    gpu: &Window,
    tag: &DmaTag,
    pt: &mut PageTables,
    slot: Option<usize>,
    va_bits: u8,
    range: &mut VaRange,
    size: u64,
    m: Map,
) -> Result<Kbo, &'static str> {
    let size = size.next_multiple_of(0x1000);
    let mem = tag
        .coherent::<u8>(usize::try_from(size).map_err(|_| "kernel buffer too large")?)
        .map_err(|_| "no memory for a kernel buffer")?;
    let va = range.take(size).ok_or("address space full")?;
    let pa = mem.bus_addr();
    update(gpu, slot, va, va + size, || {
        pt.map(tag, va_bits, va, pa, size, m)
    })??;
    Ok(Kbo { mem, va })
}
