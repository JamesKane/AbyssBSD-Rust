// SPDX-License-Identifier: BSD-2-Clause
//! The `pci` bus driver: `pci0` behind a bridge, with a child for every
//! function on its bus (FreeBSD's `pci`/`acpi_pci`).
//!
//! Children carry their BARs as resources (I/O BARs on x86 only), their
//! INTx line routed through the bridge's table, the ACPI handle of the
//! namespace device whose `_ADR` names them, and a [`PciChild`] view for
//! their drivers.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::strings::PciAddress;

use crate::bar::BarKind;
use crate::config::ConfigSpace;
use crate::enumerate::{Function, scan};
use crate::header::{CMD_BUS_MASTER, CMD_IO, CMD_MEMORY, COMMAND};

/// What a bridge hands its `pci` child.
pub struct PciBusData {
    /// Configuration space for the bus.
    pub cfg: &'static (dyn ConfigSpace + Sync),
    /// ACPI devices under the bridge by `_ADR` (`slot << 16 | function`,
    /// function 0xFFFF for all), with their paths.
    pub adr_paths: Vec<(u32, String)>,
    /// INTx routing: (slot, pin 0 = INTA .. 3 = INTD) to an interrupt.
    pub intx: Vec<(u8, u8, Resource)>,
    /// Bridges above the bus (0 behind a host bridge).
    pub depth: u8,
}

impl core::fmt::Debug for PciBusData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PciBusData")
            .field("adr_paths", &self.adr_paths.len())
            .field("intx", &self.intx.len())
            .finish_non_exhaustive()
    }
}

/// What a PCI function's driver gets as bus data: what enumeration found,
/// and its configuration space through a view that keeps bus-owned fields
/// to the bus.
///
/// The bus owns the header (BARs, command, bus numbers, the interrupt
/// line), the capability list's links, and the MSI and MSI-X capabilities
/// (DRIVER-MODEL.md §5): drivers read anything, write only
/// device-specific registers, and turn on decoding and bus mastering, and
/// message-signalled interrupts, through the bus.
pub struct PciChild {
    pub(crate) cfg: &'static (dyn ConfigSpace + Sync),
    /// What enumeration found.
    pub function: Function,
    /// The bus it is on (a bridge routes its interrupts through it).
    pub(crate) bus: Arc<PciBusData>,
}

impl core::fmt::Debug for PciChild {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PciChild")
            .field("addr", &self.function.addr)
            .finish_non_exhaustive()
    }
}

/// A write to a field the bus owns, or outside configuration space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Protected;

/// Bytes of an MSI capability (the largest form: 64-bit, per-vector
/// masking) and of an MSI-X capability.
const MSI_BYTES: u16 = 24;
const MSIX_BYTES: u16 = 12;

impl PciChild {
    /// Reads the byte at `offset`.
    #[must_use]
    pub fn read8(&self, offset: u16) -> u8 {
        self.cfg.read8(self.function.addr, offset)
    }

    /// Reads the 16-bit register at `offset`.
    #[must_use]
    pub fn read16(&self, offset: u16) -> u16 {
        self.cfg.read16(self.function.addr, offset)
    }

    /// Reads the 32-bit register at `offset`.
    #[must_use]
    pub fn read32(&self, offset: u16) -> u32 {
        self.cfg.read32(self.function.addr, offset)
    }

    /// Whether a driver may write `width` bytes at `offset`.
    fn writable(&self, offset: u16, width: u16) -> Result<(), Protected> {
        let end = offset.checked_add(width).ok_or(Protected)?;
        if !offset.is_multiple_of(width) || end > 4096 || offset < 0x40 {
            return Err(Protected);
        }
        let overlaps = |start: u16, len: u16| offset < start + len && start < end;
        let caps = self.function.caps.iter().any(|c| {
            let at = u16::from(c.offset);
            match c.id {
                crate::caps::id::MSI => overlaps(at, MSI_BYTES),
                crate::caps::id::MSIX => overlaps(at, MSIX_BYTES),
                _ => overlaps(at, 2),
            }
        });
        let ext = self.function.ext_caps.iter().any(|c| overlaps(c.offset, 4));
        if caps || ext { Err(Protected) } else { Ok(()) }
    }

    /// Writes the byte at `offset`, a device-specific register.
    pub fn write8(&self, offset: u16, v: u8) -> Result<(), Protected> {
        self.writable(offset, 1)?;
        let shift = 8 * u32::from(offset & 3);
        let old = self.read32(offset & !3) & !(0xff << shift);
        self.cfg
            .write32(self.function.addr, offset & !3, old | u32::from(v) << shift);
        Ok(())
    }

    /// Writes the 16-bit register at `offset`, a device-specific register.
    pub fn write16(&self, offset: u16, v: u16) -> Result<(), Protected> {
        self.writable(offset, 2)?;
        self.cfg.write16(self.function.addr, offset, v);
        Ok(())
    }

    /// Writes the 32-bit register at `offset`, a device-specific register.
    pub fn write32(&self, offset: u16, v: u32) -> Result<(), Protected> {
        self.writable(offset, 4)?;
        self.cfg.write32(self.function.addr, offset, v);
        Ok(())
    }

    fn set_command(&self, set: u16, clear: u16) {
        let a = self.function.addr;
        let cmd = self.cfg.read16(a, COMMAND);
        self.cfg.write16(a, COMMAND, (cmd | set) & !clear);
    }

    /// Turns on decoding of the function's BARs: memory, and I/O ports if
    /// it has I/O BARs.
    pub fn enable_decoding(&self) {
        let io = self.function.bars.iter().any(|b| b.kind == BarKind::Io);
        self.set_command(CMD_MEMORY | if io { CMD_IO } else { 0 }, 0);
    }

    /// Lets the function master the bus (DMA, and message-signalled
    /// interrupts), or stops it.
    pub fn set_bus_master(&self, on: bool) {
        if on {
            self.set_command(CMD_BUS_MASTER, 0);
        } else {
            self.set_command(0, CMD_BUS_MASTER);
        }
    }
}

/// Enables MSI-X on the function being attached with up to `want`
/// vectors, and returns a line for each, in table order.
pub fn msix_lines(
    cx: &mut AttachCx<'_>,
    want: u16,
) -> Result<Vec<bus::IrqLine>, crate::msi::MsiError> {
    let child = cx
        .bus_data_arc::<PciChild>()
        .ok_or(crate::msi::MsiError::NoCapability)?;
    let hws = crate::msi::enable_msix(&child, want)?;
    Ok(hws
        .into_iter()
        .filter_map(|hw| {
            cx.irq_line(Resource::Irq {
                domain: &crate::msi::PCI_MSI,
                hw,
                trigger: irq::Trigger::Edge,
                polarity: irq::Polarity::High,
            })
            .ok()
        })
        .collect())
}

/// `pciN`.
#[derive(Debug)]
pub struct PciBus {
    data: Arc<PciBusData>,
    domain: u32,
    bus: u8,
}

fn resources(f: &Function, data: &PciBusData) -> Vec<Resource> {
    let mut out = Vec::new();
    for b in &f.bars {
        match b.kind {
            BarKind::Memory32 | BarKind::Memory64 => out.push(Resource::Memory {
                base: b.base,
                len: b.size,
                prefetchable: b.prefetchable,
            }),
            BarKind::Io if cfg!(target_arch = "x86_64") => {
                if let (Ok(base), Ok(len)) = (u16::try_from(b.base), u16::try_from(b.size)) {
                    out.push(Resource::IoPort { base, len });
                }
            }
            BarKind::Io => {}
        }
    }
    if let pin @ 1..=4 = f.header.interrupt_pin
        && let Some((_, _, r)) = data
            .intx
            .iter()
            .find(|(slot, p, _)| *slot == f.addr.slot && *p == pin - 1)
    {
        out.push(*r);
    }
    out
}

fn acpi_path(data: &PciBusData, a: PciAddress) -> Option<String> {
    let slot = u32::from(a.slot) << 16;
    data.adr_paths
        .iter()
        .find(|(adr, _)| *adr == slot | u32::from(a.function) || *adr == slot | 0xffff)
        .map(|(_, p)| p.clone())
}

impl Driver for PciBus {
    const NAME: &'static str = "pci";
    const IDS: Ids = Ids::PciBus;

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::PciBus { acpi: true, .. } => "ACPI PCI bus".into(),
            _ => "PCI bus".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let Ident::PciBus { domain, bus, .. } = *cx.ident() else {
            return Err(AttachError::Decline);
        };
        let data = cx
            .bus_data_arc::<PciBusData>()
            .ok_or(AttachError::Device("no configuration space"))?;
        Ok(Self { data, domain, bus })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        for f in scan(self.data.cfg, self.domain, self.bus, self.data.depth) {
            let res = resources(&f, &self.data);
            let path = acpi_path(&self.data, f.addr);
            let ident = Ident::Pci(f.header.ident, f.addr, path);
            let child = PciChild {
                cfg: self.data.cfg,
                function: f,
                bus: Arc::clone(&self.data),
            };
            cx.add_child(ident, Some(Arc::new(child)), res);
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<PciBus>();
