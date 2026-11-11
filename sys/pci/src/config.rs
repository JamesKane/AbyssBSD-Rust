// SPDX-License-Identifier: BSD-2-Clause
//! Configuration-space access.

use devtree::strings::PciAddress;

/// The width of one configuration access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    Byte,
    Word,
    Dword,
}

impl Width {
    /// The width of `n` bytes: 1, 2, or 4.
    #[must_use]
    pub fn of(n: i32) -> Option<Self> {
        match n {
            1 => Some(Self::Byte),
            2 => Some(Self::Word),
            4 => Some(Self::Dword),
            _ => None,
        }
    }

    /// Its bytes.
    #[must_use]
    pub fn bytes(self) -> u16 {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::Dword => 4,
        }
    }
}

/// 32-bit access to functions' configuration space. Offsets are dword
/// aligned; narrower accesses are derived. An implementation returns
/// all-ones for a function that is not there, as hardware does.
pub trait ConfigSpace {
    /// Reads the dword at `offset`.
    fn read32(&self, a: PciAddress, offset: u16) -> u32;
    /// Writes the dword at `offset`.
    fn write32(&self, a: PciAddress, offset: u16, v: u32);

    /// Called with `true` before a window in which function `a`'s memory or
    /// I/O decoding is off (BAR sizing), and with `false` after it. Nothing
    /// may touch what the function decodes meanwhile (a boot framebuffer,
    /// say): the access is lost, or on bhyve stops the machine. The default
    /// does nothing.
    fn decoding_paused(&self, _a: PciAddress, _paused: bool) {}

    /// Reads the word at `offset` (2-byte aligned).
    fn read16(&self, a: PciAddress, offset: u16) -> u16 {
        (self.read32(a, offset & !3) >> ((offset & 2) * 8)) as u16
    }

    /// Reads the byte at `offset`.
    fn read8(&self, a: PciAddress, offset: u16) -> u8 {
        (self.read32(a, offset & !3) >> ((offset & 3) * 8)) as u8
    }

    /// Reads `width` bytes at `offset`, aligned to `width`, as one access
    /// where the mechanism has narrow accesses (FreeBSD's
    /// `pci_read_config`). Outside configuration space: all-ones.
    fn read_width(&self, a: PciAddress, offset: u16, width: Width) -> u32 {
        match width {
            Width::Byte => u32::from(self.read8(a, offset)),
            Width::Word => u32::from(self.read16(a, offset)),
            Width::Dword => self.read32(a, offset),
        }
    }

    /// Writes `width` bytes at `offset`, aligned to `width`, as one access
    /// where the mechanism has narrow accesses (FreeBSD's
    /// `pci_write_config`). The default rewrites the rest of the dword,
    /// as read.
    fn write_width(&self, a: PciAddress, offset: u16, width: Width, v: u32) {
        let shift = 8 * u32::from(offset & 3);
        let mask = match width {
            Width::Byte => 0xff,
            Width::Word => 0xffff,
            Width::Dword => u32::MAX,
        };
        if mask == u32::MAX {
            self.write32(a, offset, v);
            return;
        }
        let old = self.read32(a, offset & !3) & !(mask << shift);
        self.write32(a, offset & !3, old | (v & mask) << shift);
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

/// Writes function `a`'s command register and reads it back, so that the
/// change has reached the function before anything that depends on it.
///
/// A configuration write may complete early: ECAM is device memory with
/// early write acknowledgement, and an access to a BAR goes to another
/// peripheral, which Arm does not order after it (Arm ARM B2.8, Device
/// memory's Early Write Acknowledgement and ordering between peripherals).
/// So a driver's first register read right after turning on memory
/// decoding can reach the function while it still decodes nothing, a
/// synchronous external abort (QEMU `virt`, under TCG). A read of the same
/// register is ordered after the write, and returns only once the function
/// has taken it. PCIe makes configuration writes non-posted, so the read
/// costs one round trip and no more.
pub fn write_command(cfg: &dyn ConfigSpace, a: PciAddress, v: u16) {
    cfg.write16(a, crate::header::COMMAND, v);
    let _ = cfg.read16(a, crate::header::COMMAND);
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
    /// Run for [`ConfigSpace::decoding_paused`].
    on_pause: Option<fn(bool)>,
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
            on_pause: None,
        }
    }

    /// Runs `f` for [`ConfigSpace::decoding_paused`] (whatever the
    /// function).
    #[must_use]
    pub fn with_pause_hook(mut self, f: fn(bool)) -> Self {
        self.on_pause = Some(f);
        self
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
        self.addr_of(a, offset, Width::Dword)
    }

    /// The address of a `width` access at `offset`, which must be aligned
    /// to it.
    fn addr_of<T>(&self, a: PciAddress, offset: u16, width: Width) -> Option<*mut T> {
        if a.domain != self.segment
            || !self.buses().contains(&a.bus)
            || a.slot > 31
            || a.function > 7
            || offset >= 4096
            || !offset.is_multiple_of(width.bytes())
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
    // ECAM takes byte and word accesses (PCIe Base §7.2.2), as FreeBSD's
    // `generic_pcie_read_config` and `pciereg_cfgread` make them.
    fn read_width(&self, a: PciAddress, offset: u16, width: Width) -> u32 {
        match width {
            Width::Dword => self.read32(a, offset),
            Width::Word => match self.addr_of::<u16>(a, offset, width) {
                // SAFETY: inside the mapped window, aligned (checked by
                // `addr_of`), per `new`'s contract.
                Some(p) => u32::from(unsafe { p.read_volatile() }),
                None => u32::MAX,
            },
            Width::Byte => match self.addr_of::<u8>(a, offset, width) {
                // SAFETY: as above.
                Some(p) => u32::from(unsafe { p.read_volatile() }),
                None => u32::MAX,
            },
        }
    }

    fn write_width(&self, a: PciAddress, offset: u16, width: Width, v: u32) {
        match width {
            Width::Dword => self.write32(a, offset, v),
            Width::Word => {
                if let Some(p) = self.addr_of::<u16>(a, offset, width) {
                    // SAFETY: as in `read_width`.
                    unsafe { p.write_volatile(v as u16) }
                }
            }
            Width::Byte => {
                if let Some(p) = self.addr_of::<u8>(a, offset, width) {
                    // SAFETY: as in `read_width`.
                    unsafe { p.write_volatile(v as u8) }
                }
            }
        }
    }

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

    fn decoding_paused(&self, _a: PciAddress, paused: bool) {
        if let Some(f) = self.on_pause {
            f(paused);
        }
    }
}
