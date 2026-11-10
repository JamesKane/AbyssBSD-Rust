// SPDX-License-Identifier: BSD-2-Clause
//! The platform description (DRIVER-MODEL.md §5.5): what a platform's
//! firmware says outside the standard ACPI objects, as typed entries.
//!
//! Entries come from vendor tables in AML (the Sky1's `CLKT`, `RSTL`,
//! `RSNL`, `DLKL`, and `RSVL`, which `sys/acpi` extracts at graph build)
//! and, where firmware is wrong or silent, from `const` data selected by
//! the firmware's identity. Either way a consumer sees the same types:
//! the clock and reset lines a device's driver asks for by name, the
//! names of its resources, its device links, and the memory firmware
//! reserves for it.
//!
//! `N` identifies a device: a namespace node at graph build, a path in
//! `const` data.

use alloc::string::String;
use alloc::vec::Vec;

/// A clock a device consumes, served by a clock provider.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClockLine<N> {
    /// The consuming device.
    pub consumer: N,
    /// The name its driver asks for (`"apb_pclk"`); `None` for the
    /// device's only, unnamed clock.
    pub name: Option<String>,
    /// The device serving it.
    pub provider: N,
    /// The provider's clock identifier (an SCMI clock ID on the Sky1).
    pub id: u32,
}

/// A reset line a device consumes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResetLine<N> {
    /// The consuming device.
    pub consumer: N,
    /// The name its driver asks for (`"i2c_reset"`), if any.
    pub name: Option<String>,
    /// The reset controller.
    pub controller: N,
    /// The line's index in the controller.
    pub index: u32,
}

/// A kind of `_CRS` resource a name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourceKind {
    /// A memory range.
    Memory,
    /// An interrupt.
    Interrupt,
}

/// A name for one of a device's resources (devicetree's `reg-names` and
/// `interrupt-names`): the `index`th resource of its kind in `_CRS`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResourceName<N> {
    /// The device whose `_CRS` it is.
    pub device: N,
    /// Which resources `index` counts.
    pub kind: ResourceKind,
    /// The position among the device's resources of that kind.
    pub index: u32,
    /// The name (`"rcsu"`, `"xhci"`).
    pub name: String,
}

/// A device link: the consumer may not attach before the supplier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceLink<N> {
    /// The device depended on.
    pub supplier: N,
    /// The dependent device.
    pub consumer: N,
}

/// Memory firmware sets aside for one device (devicetree's
/// `memory-region`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReservedMemory<N> {
    /// The device it is for.
    pub device: N,
    /// The physical base.
    pub base: u64,
    /// The length in bytes.
    pub size: u64,
    /// Whether the kernel must not map it (`"no-map"`): only the device
    /// touches it.
    pub no_map: bool,
}

/// A platform's description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Description<N> {
    /// Clock lines by consumer.
    pub clocks: Vec<ClockLine<N>>,
    /// Reset lines by consumer.
    pub resets: Vec<ResetLine<N>>,
    /// Resource names.
    pub resource_names: Vec<ResourceName<N>>,
    /// Device links.
    pub links: Vec<DeviceLink<N>>,
    /// Reserved memory.
    pub reserved: Vec<ReservedMemory<N>>,
}

impl<N> Default for Description<N> {
    fn default() -> Self {
        Self {
            clocks: Vec::new(),
            resets: Vec::new(),
            resource_names: Vec::new(),
            links: Vec::new(),
            reserved: Vec::new(),
        }
    }
}

impl<N: PartialEq> Description<N> {
    /// Whether nothing is described.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clocks.is_empty()
            && self.resets.is_empty()
            && self.resource_names.is_empty()
            && self.links.is_empty()
            && self.reserved.is_empty()
    }

    /// The clock `consumer`'s driver asks for by `name` (`None`: its
    /// unnamed clock, or its only one).
    pub fn clock(&self, consumer: &N, name: Option<&str>) -> Option<&ClockLine<N>> {
        let mut mine = self.clocks.iter().filter(|c| &c.consumer == consumer);
        match name {
            Some(n) => mine.find(|c| c.name.as_deref() == Some(n)),
            None => mine
                .clone()
                .find(|c| c.name.is_none())
                .or_else(|| only(mine)),
        }
    }

    /// The reset line `consumer`'s driver asks for by `name` (`None`: its
    /// only one).
    pub fn reset(&self, consumer: &N, name: Option<&str>) -> Option<&ResetLine<N>> {
        let mut mine = self.resets.iter().filter(|r| &r.consumer == consumer);
        match name {
            Some(n) => mine.find(|r| r.name.as_deref() == Some(n)),
            None => only(mine),
        }
    }

    /// The position among `device`'s resources of `kind` that `name`
    /// names.
    pub fn resource(&self, device: &N, kind: ResourceKind, name: &str) -> Option<u32> {
        self.resource_names
            .iter()
            .find(|r| &r.device == device && r.kind == kind && r.name == name)
            .map(|r| r.index)
    }
}

/// The single item of `it`, if it has exactly one.
fn only<T>(mut it: impl Iterator<Item = T>) -> Option<T> {
    let first = it.next()?;
    it.next().is_none().then_some(first)
}
