// SPDX-License-Identifier: BSD-2-Clause
//! Configuration-space access.

use devtree::strings::PciAddress;

/// 32-bit access to functions' configuration space. Offsets are dword
/// aligned; narrower accesses are derived. An implementation returns
/// all-ones for a function that is not there, as hardware does.
pub trait ConfigSpace {
    /// Reads the dword at `offset`.
    fn read32(&self, a: PciAddress, offset: u16) -> u32;
    /// Writes the dword at `offset`.
    fn write32(&self, a: PciAddress, offset: u16, v: u32);

    /// Reads the word at `offset` (2-byte aligned).
    fn read16(&self, a: PciAddress, offset: u16) -> u16 {
        (self.read32(a, offset & !3) >> ((offset & 2) * 8)) as u16
    }

    /// Reads the byte at `offset`.
    fn read8(&self, a: PciAddress, offset: u16) -> u8 {
        (self.read32(a, offset & !3) >> ((offset & 3) * 8)) as u8
    }

    /// Writes the word at `offset` (2-byte aligned), preserving its
    /// neighbor. Only for registers where rewriting the neighbor is
    /// harmless (the command register's neighbor is status, whose bits
    /// are write-one-to-clear: written as zeros here).
    fn write16(&self, a: PciAddress, offset: u16, v: u16) {
        let shift = (offset & 2) * 8;
        let old = self.read32(a, offset & !3);
        let keep = if offset & !3 == crate::header::COMMAND {
            // Status (the upper half) is write-one-to-clear: write zeros.
            if shift == 0 { 0 } else { old & 0xffff }
        } else {
            old & !(0xffff << shift)
        };
        self.write32(a, offset & !3, keep | (u32::from(v) << shift));
    }
}

/// An ECAM region (PCIe enhanced configuration access): 4 KiB of
/// configuration space per function, at `base + (bus << 20 | slot << 15 |
/// function << 12)`, for one segment's bus range.
#[derive(Debug)]
pub struct Ecam {
    va: usize,
    segment: u32,
    first_bus: u8,
    last_bus: u8,
}

impl Ecam {
    /// An accessor for the region mapped at `va`.
    ///
    /// # Safety
    ///
    /// `va` must map the segment's ECAM window as device memory, for
    /// buses `first_bus..=last_bus` (1 MiB each), for the accessor's life,
    /// and nothing else may use it to write BARs or bus numbers.
    #[must_use]
    pub unsafe fn new(va: usize, segment: u32, first_bus: u8, last_bus: u8) -> Self {
        Self {
            va,
            segment,
            first_bus,
            last_bus,
        }
    }

    /// The bus range.
    #[must_use]
    pub fn buses(&self) -> core::ops::RangeInclusive<u8> {
        self.first_bus..=self.last_bus
    }

    /// The segment.
    #[must_use]
    pub fn segment(&self) -> u32 {
        self.segment
    }

    fn addr(&self, a: PciAddress, offset: u16) -> Option<*mut u32> {
        if a.domain != self.segment
            || !self.buses().contains(&a.bus)
            || a.slot > 31
            || a.function > 7
            || offset >= 4096
            || offset & 3 != 0
        {
            return None;
        }
        let off = (usize::from(a.bus - self.first_bus) << 20)
            | (usize::from(a.slot) << 15)
            | (usize::from(a.function) << 12)
            | usize::from(offset);
        Some(core::ptr::with_exposed_provenance_mut(self.va + off))
    }
}

impl ConfigSpace for Ecam {
    fn read32(&self, a: PciAddress, offset: u16) -> u32 {
        match self.addr(a, offset) {
            // SAFETY: inside the mapped window (checked by `addr`), per
            // `new`'s contract.
            Some(p) => unsafe { p.read_volatile() },
            None => u32::MAX,
        }
    }

    fn write32(&self, a: PciAddress, offset: u16, v: u32) {
        if let Some(p) = self.addr(a, offset) {
            // SAFETY: as in `read32`.
            unsafe { p.write_volatile(v) }
        }
    }
}
