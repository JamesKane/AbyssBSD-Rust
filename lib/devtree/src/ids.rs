// SPDX-License-Identifier: BSD-2-Clause
//! Device identities and match-table entries for each bus.
//!
//! An identity is what a bus knows about a child before any driver runs;
//! an ID is one entry of a driver's `const` match table. IDs carry the rank
//! they give a match and any quirks the driver needs for that device, so
//! per-device special cases are data rather than code (DRIVER-MODEL.md
//! §7.2).

use alloc::string::String;
use alloc::vec::Vec;

use crate::matching::{Matches, Rank};

/// A PCI function's identity, from configuration space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciIdent {
    /// Vendor ID.
    pub vendor: u16,
    /// Device ID.
    pub device: u16,
    /// Subsystem vendor ID.
    pub subvendor: u16,
    /// Subsystem device ID.
    pub subdevice: u16,
    /// The 24-bit class code: base class, subclass, programming interface.
    pub class: u32,
    /// Revision ID.
    pub revision: u8,
}

/// One PCI match-table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciId {
    vendor: Option<u16>,
    device: Option<u16>,
    subsystem: Option<(u16, u16)>,
    /// Class code and mask.
    class: Option<(u32, u32)>,
    rank: Rank,
    /// Driver-defined quirk bits for matching devices.
    pub quirks: u32,
}

impl PciId {
    /// A vendor and device, at [`Rank::DEFAULT`].
    #[must_use]
    pub const fn device(vendor: u16, device: u16) -> Self {
        Self {
            vendor: Some(vendor),
            device: Some(device),
            subsystem: None,
            class: None,
            rank: Rank::DEFAULT,
            quirks: 0,
        }
    }

    /// A full 24-bit class code, at [`Rank::GENERIC`].
    #[must_use]
    pub const fn class(code: u32) -> Self {
        Self {
            vendor: None,
            device: None,
            subsystem: None,
            class: Some((code & 0xff_ffff, 0xff_ffff)),
            rank: Rank::GENERIC,
            quirks: 0,
        }
    }

    /// A base class and subclass, any programming interface, at
    /// [`Rank::GENERIC`].
    #[must_use]
    pub const fn subclass(base: u8, sub: u8) -> Self {
        Self {
            vendor: None,
            device: None,
            subsystem: None,
            class: Some((((base as u32) << 16) | ((sub as u32) << 8), 0xff_ff00)),
            rank: Rank::GENERIC,
            quirks: 0,
        }
    }

    /// Also requires this subsystem.
    #[must_use]
    pub const fn subsystem(mut self, subvendor: u16, subdevice: u16) -> Self {
        self.subsystem = Some((subvendor, subdevice));
        self
    }

    /// With rank `rank` instead of the default.
    #[must_use]
    pub const fn rank(mut self, rank: Rank) -> Self {
        self.rank = rank;
        self
    }

    /// With quirk bits.
    #[must_use]
    pub const fn quirks(mut self, quirks: u32) -> Self {
        self.quirks = quirks;
        self
    }
}

impl Matches<PciIdent> for PciId {
    fn rank_for(&self, i: &PciIdent) -> Option<Rank> {
        let ok = self.vendor.is_none_or(|v| v == i.vendor)
            && self.device.is_none_or(|d| d == i.device)
            && self
                .subsystem
                .is_none_or(|(v, d)| v == i.subvendor && d == i.subdevice)
            && self.class.is_none_or(|(c, m)| i.class & m == c);
        ok.then_some(self.rank)
    }
}

/// A virtio device's identity, from its transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioIdent {
    /// The transport's vendor ID (32 bits on virtio-mmio).
    pub vendor: u32,
    /// The transport's device ID (the PCI device ID on virtio-pci).
    pub device: u16,
    /// The transport's subsystem vendor ID.
    pub subvendor: u16,
    /// The virtio device type (1 network, 2 block, 4 entropy, …).
    pub device_type: u32,
}

/// One virtio match-table entry: a device type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioId {
    device_type: u32,
    rank: Rank,
}

impl VirtioId {
    /// Matches virtio device type `device_type`, at [`Rank::DEFAULT`].
    #[must_use]
    pub const fn new(device_type: u32) -> Self {
        Self {
            device_type,
            rank: Rank::DEFAULT,
        }
    }

    /// With rank `rank` instead of the default.
    #[must_use]
    pub const fn rank(mut self, rank: Rank) -> Self {
        self.rank = rank;
        self
    }
}

impl Matches<VirtioIdent> for VirtioId {
    fn rank_for(&self, i: &VirtioIdent) -> Option<Rank> {
        (i.device_type == self.device_type).then_some(self.rank)
    }
}

/// An ACPI namespace device's identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpiIdent {
    /// `_HID`, as a string (EISA IDs decoded, e.g. `PNP0A08`).
    pub hid: Option<String>,
    /// `_CID`s, in order.
    pub cids: Vec<String>,
    /// `_UID`, as the string firmware gave (or an integer's decimal form).
    pub uid: Option<String>,
}

/// One ACPI match-table entry: a hardware or compatible ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiId {
    id: &'static str,
    rank: Rank,
    /// Driver-defined quirk bits for matching devices.
    pub quirks: u32,
}

impl AcpiId {
    /// Matches `_HID` or any `_CID` equal to `id`, at [`Rank::DEFAULT`].
    #[must_use]
    pub const fn new(id: &'static str) -> Self {
        Self {
            id,
            rank: Rank::DEFAULT,
            quirks: 0,
        }
    }

    /// With rank `rank` instead of the default.
    #[must_use]
    pub const fn rank(mut self, rank: Rank) -> Self {
        self.rank = rank;
        self
    }

    /// With quirk bits.
    #[must_use]
    pub const fn quirks(mut self, quirks: u32) -> Self {
        self.quirks = quirks;
        self
    }
}

impl Matches<AcpiIdent> for AcpiId {
    fn rank_for(&self, i: &AcpiIdent) -> Option<Rank> {
        let hit = i.hid.as_deref() == Some(self.id) || i.cids.iter().any(|c| c == self.id);
        hit.then_some(self.rank)
    }
}
