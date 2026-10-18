// SPDX-License-Identifier: BSD-2-Clause
//! A function's header: identity, type, and bridge bus numbers.

use devtree::ids::PciIdent;
use devtree::strings::PciAddress;

use crate::config::ConfigSpace;

/// Vendor ID (word) and device ID (word).
pub const VENDOR: u16 = 0x00;
/// Command (word) and status (word).
pub const COMMAND: u16 = 0x04;
/// Revision (byte) and class code (3 bytes).
pub const CLASS: u16 = 0x08;
/// Cache line, latency, header type, BIST.
pub const HEADER: u16 = 0x0c;
/// The first BAR.
pub const BAR0: u16 = 0x10;
/// Type 1: primary, secondary, and subordinate bus numbers.
pub const BUSES: u16 = 0x18;
/// Type 0: subsystem vendor and device.
pub const SUBSYSTEM: u16 = 0x2c;
/// Type 0: expansion ROM BAR.
pub const ROM: u16 = 0x30;
/// Capabilities pointer.
pub const CAP_PTR: u16 = 0x34;
/// Interrupt line and pin.
pub const INTERRUPT: u16 = 0x3c;

/// Command: I/O space decoding.
pub const CMD_IO: u16 = 1 << 0;
/// Command: memory space decoding.
pub const CMD_MEMORY: u16 = 1 << 1;
/// Command: bus mastering (DMA).
pub const CMD_BUS_MASTER: u16 = 1 << 2;
/// Command: INTx disabled.
pub const CMD_INTX_DISABLE: u16 = 1 << 10;
/// Status: a capability list is present.
pub const STATUS_CAP_LIST: u16 = 1 << 4;

/// The layout of the rest of the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderType {
    /// An endpoint (six BARs).
    Device,
    /// A PCI-to-PCI bridge (two BARs, bus numbers, windows).
    Bridge,
    /// A CardBus bridge.
    CardBus,
    /// Reserved: the function is ignored.
    Unknown(u8),
}

impl HeaderType {
    /// How many BARs the header has.
    #[must_use]
    pub fn bars(self) -> u8 {
        match self {
            Self::Device => 6,
            Self::Bridge => 2,
            Self::CardBus => 1,
            Self::Unknown(_) => 0,
        }
    }
}

/// What the header says about a present function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Vendor, device, subsystem, class, revision.
    pub ident: PciIdent,
    /// The header layout.
    pub kind: HeaderType,
    /// Function 0 only: other functions may exist.
    pub multifunction: bool,
    /// The interrupt pin (1 = INTA .. 4 = INTD), or 0.
    pub interrupt_pin: u8,
}

/// Bus numbers of a bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BridgeBuses {
    /// The bus it is on.
    pub primary: u8,
    /// The bus behind it.
    pub secondary: u8,
    /// The highest bus behind it.
    pub subordinate: u8,
}

/// Reads the header of `a`, or `None` if no function is there.
pub fn read(cfg: &dyn ConfigSpace, a: PciAddress) -> Option<Header> {
    let id = cfg.read32(a, VENDOR);
    let vendor = id as u16;
    if vendor == 0xffff || vendor == 0 {
        return None;
    }
    let class = cfg.read32(a, CLASS);
    let header = cfg.read8(a, HEADER + 2);
    let kind = match header & 0x7f {
        0 => HeaderType::Device,
        1 => HeaderType::Bridge,
        2 => HeaderType::CardBus,
        n => HeaderType::Unknown(n),
    };
    let (subvendor, subdevice) = if kind == HeaderType::Device {
        let s = cfg.read32(a, SUBSYSTEM);
        (s as u16, (s >> 16) as u16)
    } else {
        (0, 0)
    };
    Some(Header {
        ident: PciIdent {
            vendor,
            device: (id >> 16) as u16,
            subvendor,
            subdevice,
            class: class >> 8,
            revision: class as u8,
        },
        kind,
        multifunction: header & 0x80 != 0,
        interrupt_pin: match cfg.read8(a, INTERRUPT + 1) {
            p @ 1..=4 => p,
            _ => 0,
        },
    })
}

/// A bridge's bus numbers.
pub fn bridge_buses(cfg: &dyn ConfigSpace, a: PciAddress) -> BridgeBuses {
    let b = cfg.read32(a, BUSES);
    BridgeBuses {
        primary: b as u8,
        secondary: (b >> 8) as u8,
        subordinate: (b >> 16) as u8,
    }
}
