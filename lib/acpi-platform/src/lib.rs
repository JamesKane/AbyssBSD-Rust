// SPDX-License-Identifier: BSD-2-Clause
//! The interface a platform implements (ADR-0020, DRIVER-MODEL.md §5.5).
//!
//! A platform is what one SoC family's firmware says outside the standard
//! ACPI objects: vendor tables in AML, vendor `_DSD` property names, and
//! (later) corrections. Each lives in its own crate under `platform/`,
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

/// The firmware's identity, from the root table's header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FirmwareIdentity {
    /// The OEM ID.
    pub oem_id: [u8; 6],
    /// The OEM table ID.
    pub oem_table_id: [u8; 8],
    /// The OEM revision.
    pub oem_revision: u32,
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

/// One platform: an SoC family's firmware conventions.
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

    /// The vendor `_DSD` reference properties its firmware uses, and the
    /// edge each makes (the generic names are `devtree::dsd`'s).
    fn reference_kinds(&self) -> &'static [(&'static str, EdgeKind)] {
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
