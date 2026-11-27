// SPDX-License-Identifier: BSD-2-Clause
//! The interface a platform implements (ADR-0020, DRIVER-MODEL.md §5.5).
//!
//! A platform is what one SoC or firmware family's firmware says outside
//! the standard ACPI objects: vendor tables in AML, vendor `_DSD` property
//! names, changes to the `_OSI` answers (ADR-0027), and (later)
//! corrections. Each lives in its own crate under `platform/`,
//! implements [`PlatformSource`], and is listed in `kernel/table`'s
//! `PLATFORMS`. The ACPI bus runs every source whose [`PlatformSource::applies`]
//! accepts the firmware, merges what they extract, and names no vendor
//! itself.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use aml::{Host, Interpreter, NodeId};
use devtree::graph::EdgeKind;
use devtree::platform::Description;

/// The firmware's identity: the root table's header and the SMBIOS system
/// information (ADR-0027).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FirmwareIdentity {
    /// The OEM ID.
    pub oem_id: [u8; 6],
    /// The OEM table ID.
    pub oem_table_id: [u8; 8],
    /// The OEM revision.
    pub oem_revision: u32,
    /// The SMBIOS system maker, product and family, empty without SMBIOS.
    pub system: SystemIdentity,
}

/// The SMBIOS system information structure's strings (type 1), as the
/// loader puts them in the kernel environment (`smbios.system.maker`,
/// `.product`, `.family`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemIdentity {
    /// The manufacturer (`Apple Inc.`).
    pub maker: String,
    /// The product name (`MacPro6,1`).
    pub product: String,
    /// The family (`MacPro`).
    pub family: String,
}

/// What a platform changes in the `_OSI` answers, before the DSDT loads
/// (ADR-0027 §2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OsiChanges {
    /// Stop answering true to every vendor (operating system) string,
    /// keeping the feature group strings: ACPICA's
    /// `ACPI_DISABLE_ALL_VENDOR_STRINGS`.
    pub drop_vendor_strings: bool,
    /// Strings to answer true to.
    pub add: &'static [&'static str],
    /// The FreeBSD tunable that turns the change off when it is 0.
    pub tunable: Option<&'static str>,
}

impl OsiChanges {
    /// No change.
    pub const NONE: Self = Self {
        drop_vendor_strings: false,
        add: &[],
        tunable: None,
    };

    /// Whether it changes nothing.
    #[must_use]
    pub fn is_none(&self) -> bool {
        !self.drop_vendor_strings && self.add.is_empty()
    }
}

/// A device property a platform states for firmware that should have
/// given it in the device's `_DSD` and did not (ADR-0027 §3): what a
/// device's driver reads through the same accessor as its `_DSD`
/// properties, which win over these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceProperty {
    /// The device's `_HID`.
    pub hid: &'static str,
    /// The property's name: vendor-prefixed (`apple,fan-floor-top`), or a
    /// generic devicetree name.
    pub name: &'static str,
    /// The value.
    pub value: PropertyValue,
    /// The firmware defect or omission it addresses (DRIVER-MODEL.md §5.5).
    pub why: &'static str,
}

/// A device property's value: `_DSD`'s own value set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyValue {
    /// An integer.
    Integer(u64),
    /// A string.
    String(&'static str),
    /// A package of integers.
    Integers(&'static [u64]),
    /// A package of strings.
    Strings(&'static [&'static str]),
}

/// The `_OSI` strings after every platform's changes, and what happened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OsiOutcome {
    /// The strings `_OSI` answers true to.
    pub interfaces: Vec<String>,
    /// The platforms whose change was applied.
    pub applied: Vec<&'static str>,
    /// The platforms whose change its tunable turned off, with the tunable.
    pub vetoed: Vec<(&'static str, &'static str)>,
    /// A vendor string one platform added while another dropped the vendor
    /// strings: (adder, string). The string is still added; this is a
    /// diagnostic, which the corpus checks fail on.
    pub conflicts: Vec<(&'static str, &'static str)>,
}

/// Applies every platform's `_OSI` changes for firmware `id` to `base`
/// (FreeBSD's defaults, [`aml::FREEBSD_OSI`]): the drops first, then the
/// additions, so the result does not depend on the platforms' order.
/// `enabled` reads a tunable: `Some(false)` when it is set to 0.
pub fn osi_interfaces(
    base: &[&str],
    platforms: &[&dyn PlatformSource],
    id: &FirmwareIdentity,
    enabled: impl Fn(&str) -> Option<bool>,
) -> OsiOutcome {
    let mut out = OsiOutcome::default();
    let mut changes = Vec::new();
    for p in platforms {
        let c = p.osi(id);
        if c.is_none() {
            continue;
        }
        if let Some(t) = c.tunable
            && enabled(t) == Some(false)
        {
            out.vetoed.push((p.name(), t));
            continue;
        }
        out.applied.push(p.name());
        changes.push((p.name(), c));
    }
    let is_feature = |s: &str| aml::OSI_FEATURE_GROUPS.contains(&s);
    let drop = changes.iter().any(|(_, c)| c.drop_vendor_strings);
    out.interfaces = base
        .iter()
        .filter(|s| !drop || is_feature(s))
        .map(|s| String::from(*s))
        .collect();
    for (name, c) in &changes {
        for &s in c.add {
            if drop && !is_feature(s) && !c.drop_vendor_strings {
                out.conflicts.push((name, s));
            }
            if !out.interfaces.iter().any(|i| i == s) {
                out.interfaces.push(String::from(s));
            }
        }
    }
    out
}

/// An entry a platform's extractor could not read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Malformed {
    /// The platform.
    pub source: &'static str,
    /// The table (its object name, or a section of the description).
    pub table: &'static str,
    /// The device carrying the table.
    pub carrier: NodeId,
    /// The entry's position, or `None` for the table as a whole.
    pub entry: Option<usize>,
    /// What is wrong.
    pub why: String,
}

/// A name in a platform's table that resolves to nothing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unresolved {
    /// The device carrying the table.
    pub carrier: NodeId,
    /// The table.
    pub table: &'static str,
    /// The name, as written.
    pub name: String,
}

/// What a platform's tables describe.
#[derive(Clone, Debug, Default)]
pub struct Extracted {
    /// The entries, in the common types.
    pub description: Description<NodeId>,
    /// Entries that could not be read.
    pub malformed: Vec<Malformed>,
    /// Names that resolve to nothing.
    pub unresolved: Vec<Unresolved>,
}

/// One platform: an SoC or firmware family's conventions.
pub trait PlatformSource: Sync {
    /// The platform's name in diagnostics and edge origins (`"cix-sky1"`).
    fn name(&self) -> &'static str;

    /// Whether this firmware is this platform's: by its identity, or by a
    /// namespace object among the present `devices`.
    fn applies(
        &self,
        id: &FirmwareIdentity,
        interp: &mut Interpreter,
        host: &mut dyn Host,
        devices: &[NodeId],
    ) -> bool;

    /// The platform's vendor tables, read from the present `devices` into
    /// the common types. Empty if it has none.
    fn extract(
        &self,
        interp: &mut Interpreter,
        host: &mut dyn Host,
        devices: &[NodeId],
    ) -> Extracted {
        let _ = (interp, host, devices);
        Extracted::default()
    }

    /// What it changes in the `_OSI` answers for firmware `id`, applied
    /// before the DSDT loads, since firmware's `_INI` methods read the
    /// answers (ADR-0027 §2). Nothing by default.
    fn osi(&self, id: &FirmwareIdentity) -> OsiChanges {
        let _ = id;
        OsiChanges::NONE
    }

    /// The device properties it states for firmware `id` (ADR-0027 §3).
    /// None by default.
    fn properties(&self, id: &FirmwareIdentity) -> &'static [DeviceProperty] {
        let _ = id;
        &[]
    }

    /// The vendor `_DSD` reference properties its firmware uses, and the
    /// edge each makes (the generic names are `devtree::dsd`'s).
    fn reference_kinds(&self) -> &'static [(&'static str, EdgeKind)] {
        &[]
    }

    /// The `_DSD` reference properties that name a register window
    /// another device owns (a CRU or syscon), which the referring device
    /// leases a field of (DRIVER-MODEL.md §9.3, `RegisterRegion`).
    fn register_windows(&self) -> &'static [&'static str] {
        &[]
    }
}

/// The edge a `_DSD` reference property makes, by the generic table and
/// then by each applying platform's names; `None` if nobody knows it.
#[must_use]
pub fn reference_kind(platforms: &[&dyn PlatformSource], name: &str) -> Option<EdgeKind> {
    devtree::dsd::reference_kind(name).or_else(|| {
        platforms.iter().find_map(|p| {
            p.reference_kinds()
                .iter()
                .find(|(n, _)| *n == name)
                .map(|&(_, k)| k)
        })
    })
}

#[cfg(test)]
mod tests;
