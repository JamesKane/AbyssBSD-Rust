// SPDX-License-Identifier: BSD-2-Clause
//! Capability lists, walked with bounds: a device can link them in a loop.

use alloc::vec::Vec;

use devtree::strings::PciAddress;

use crate::config::ConfigSpace;
use crate::header::{CAP_PTR, COMMAND, STATUS_CAP_LIST};

/// Standard capability IDs.
pub mod id {
    /// Power management.
    pub const PM: u8 = 0x01;
    /// MSI.
    pub const MSI: u8 = 0x05;
    /// Vendor specific (virtio uses it).
    pub const VENDOR: u8 = 0x09;
    /// PCI Express.
    pub const PCIE: u8 = 0x10;
    /// MSI-X.
    pub const MSIX: u8 = 0x11;
}

/// A standard capability: its ID and offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    /// The capability ID.
    pub id: u8,
    /// Its offset in configuration space.
    pub offset: u8,
}

/// An extended (PCIe) capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtCapability {
    /// The capability ID.
    pub id: u16,
    /// Its version.
    pub version: u8,
    /// Its offset in configuration space (0x100 or above).
    pub offset: u16,
}

/// At most this many standard capabilities fit in the 192 bytes after the
/// header.
const MAX_CAPS: usize = 48;
/// At most this many extended capabilities fit in 3840 bytes.
const MAX_EXT_CAPS: usize = 960;

/// The standard capability list.
pub fn list(cfg: &dyn ConfigSpace, a: PciAddress) -> Vec<Capability> {
    let mut out = Vec::new();
    if cfg.read16(a, COMMAND + 2) & STATUS_CAP_LIST == 0 {
        return out;
    }
    let mut at = cfg.read8(a, CAP_PTR) & !3;
    while at >= 0x40 && out.len() < MAX_CAPS {
        let w = cfg.read16(a, u16::from(at));
        if out.iter().any(|c: &Capability| c.offset == at) {
            break;
        }
        out.push(Capability {
            id: w as u8,
            offset: at,
        });
        at = (w >> 8) as u8 & !3;
    }
    out
}

/// The extended capability list (PCIe functions under ECAM only).
pub fn ext_list(cfg: &dyn ConfigSpace, a: PciAddress) -> Vec<ExtCapability> {
    let mut out = Vec::new();
    let mut at: u16 = 0x100;
    while at >= 0x100 && out.len() < MAX_EXT_CAPS {
        let h = cfg.read32(a, at);
        if h == 0 || h == u32::MAX || out.iter().any(|c: &ExtCapability| c.offset == at) {
            break;
        }
        out.push(ExtCapability {
            id: h as u16,
            version: ((h >> 16) & 0xf) as u8,
            offset: at,
        });
        at = ((h >> 20) as u16) & !3;
    }
    out
}

/// The first standard capability with `id`.
#[must_use]
pub fn find(caps: &[Capability], id: u8) -> Option<Capability> {
    caps.iter().copied().find(|c| c.id == id)
}
