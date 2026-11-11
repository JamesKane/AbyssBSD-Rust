// SPDX-License-Identifier: BSD-2-Clause
//! Ranges more than one party touches (DRIVER-MODEL.md §8.1).
//!
//! - **Firmware-shared**: the bytes AML reads and writes through the
//!   namespace's `SystemMemory` and `SystemIO` operation regions (their
//!   fields' extents). AML touches them whenever a method runs, so a
//!   driver granted an overlapping range races firmware. They
//!   are recorded in the resource space before any child attaches, and a
//!   driver is granted one only if it asks knowingly.
//! - **Declared-shared**: the memory ranges of devices the platform
//!   description says overlap another device's on purpose
//!   ([`devtree::platform::SharedRange`]). Both devices may be granted
//!   the overlap.
//!
//! What the namespace says statically is all that is known: a region a
//! method creates while it runs exists only during that call, and is not
//! recorded. FreeBSD records neither kind.

use alloc::vec::Vec;

use aml::host::RegionSpace;
use aml::resource::{AddressKind, Resource};
use aml::{Interpreter, NodeId};
use bus::{AddressSpace, Range};

/// Bytes of an operation region AML reads or writes directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareRegion {
    /// The `OperationRegion`.
    pub region: NodeId,
    /// The bytes its fields touch.
    pub range: Range,
}

/// The bytes AML reads and writes through `SystemMemory` and `SystemIO`
/// operation regions, by region in definition order: the extents of the
/// fields declared on each, widened to their access units, with adjacent
/// extents merged. A region is often far larger than its fields (an 8 MiB
/// region over a whole chipset block, with fields in a few registers),
/// and AML reaches its contents only through fields.
#[must_use]
pub fn firmware_regions(interp: &Interpreter) -> Vec<FirmwareRegion> {
    let mut out: Vec<FirmwareRegion> = Vec::new();
    for n in interp.nodes() {
        let Some(f) = interp.field_info(n) else {
            continue;
        };
        let (Some(region), Some((offset, len))) = (f.region, f.extent) else {
            continue;
        };
        let Some(r) = interp.region_info(region) else {
            continue;
        };
        let space = match r.space {
            RegionSpace::SystemMemory => AddressSpace::Memory,
            RegionSpace::SystemIo => AddressSpace::IoPort,
            _ => continue,
        };
        // An access past the region's end fails; it touches nothing.
        if offset >= r.len {
            continue;
        }
        out.push(FirmwareRegion {
            region,
            range: Range {
                space,
                base: r.base.saturating_add(offset),
                len: len.min(r.len - offset),
            },
        });
    }
    // Merge each region's overlapping and adjacent extents, keeping the
    // regions in the order their first field appeared.
    let mut merged: Vec<FirmwareRegion> = Vec::new();
    let mut order: Vec<NodeId> = Vec::new();
    for f in &out {
        if !order.contains(&f.region) {
            order.push(f.region);
        }
    }
    for region in order {
        let mut mine: Vec<Range> = out
            .iter()
            .filter(|f| f.region == region)
            .map(|f| f.range)
            .collect();
        mine.sort();
        let mut cur: Option<Range> = None;
        for r in mine {
            cur = match cur {
                Some(c) if r.base <= c.end() => Some(Range {
                    len: c.end().max(r.end()) - c.base,
                    ..c
                }),
                Some(c) => {
                    merged.push(FirmwareRegion { region, range: c });
                    Some(r)
                }
                None => Some(r),
            };
        }
        if let Some(c) = cur {
            merged.push(FirmwareRegion { region, range: c });
        }
    }
    merged
}

/// The memory and I/O ranges a device's `_CRS` consumes, in order: what
/// its driver may be granted. Ranges a bridge produces stay with the
/// bridge's driver.
#[must_use]
pub fn consumed(crs: &[Resource]) -> Vec<Range> {
    let mem = |base: u64, len: u64| Range {
        space: AddressSpace::Memory,
        base,
        len,
    };
    let io = |base: u64, len: u64| Range {
        space: AddressSpace::IoPort,
        base,
        len,
    };
    let mut out = Vec::new();
    for r in crs {
        match r {
            Resource::Io(p) if p.length > 0 => out.push(io(p.min.into(), p.length.into())),
            Resource::FixedIo { base, length } if *length > 0 => {
                out.push(io((*base).into(), (*length).into()));
            }
            Resource::Memory24(m) | Resource::Memory32(m) | Resource::FixedMemory32(m)
                if m.length > 0 =>
            {
                out.push(mem(m.min.into(), m.length.into()));
            }
            Resource::Address(a) if a.consumer && a.length > 0 => match a.kind {
                AddressKind::Memory { .. } => out.push(mem(a.min, a.length)),
                AddressKind::Io { .. }
                    if u16::try_from(a.min).is_ok() && u16::try_from(a.length).is_ok() =>
                {
                    out.push(io(a.min, a.length));
                }
                _ => {}
            },
            _ => {}
        }
    }
    out
}

/// The memory windows a bridge's `_CRS` produces, at their bus addresses
/// (`_MIN`, as FreeBSD's `AcpiResourceToAddress64` gives `Minimum`):
/// (start, length). A PCI domain reserves them, since a request to one of
/// them reaches a device there rather than memory (docs/IOMMU.md §4.6).
#[must_use]
pub fn windows(crs: &[Resource]) -> Vec<(u64, u64)> {
    crs.iter()
        .filter_map(|r| match r {
            Resource::Address(a)
                if !a.consumer && a.length > 0 && matches!(a.kind, AddressKind::Memory { .. }) =>
            {
                Some((a.min, a.length))
            }
            _ => None,
        })
        .collect()
}

/// The system-resource devices' ranges as `acpi0` takes them (FreeBSD's
/// `acpi_sysres_attach`, in device order): a range inside one already
/// taken is dropped, one overlapping it widens it, and the rest are
/// added. `acpi0` holds each and manages them in its rmans.
#[must_use]
pub fn system_resource_ranges(ranges: impl IntoIterator<Item = Range>) -> Vec<Range> {
    let mut out: Vec<Range> = Vec::new();
    for r in ranges {
        if r.len == 0 {
            continue;
        }
        let (start, end) = (r.base, r.end() - 1);
        let mut placed = false;
        for b in out.iter_mut().filter(|b| b.space == r.space) {
            let (bs, be) = (b.base, b.end() - 1);
            if start >= bs && end <= be {
                placed = true;
                break;
            }
            let mut grown = false;
            if start < bs && end >= bs {
                b.len += bs - start;
                b.base = start;
                grown = true;
            }
            if start <= be && end > be {
                b.len += end - be;
                grown = true;
            }
            if grown {
                placed = true;
                break;
            }
        }
        if !placed {
            out.push(r);
        }
    }
    out
}

/// A range a host bridge's `_CRS` describes, as FreeBSD's
/// `pci_host_generic_acpi` reads them: every memory or I/O address
/// descriptor (and fixed memory), at its PCI address (`_MIN` to `_MAX`)
/// and its CPU address (`_MIN` plus `_TRA`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRange {
    /// I/O space on the PCI side (otherwise memory).
    pub io: bool,
    /// The first PCI address.
    pub pci_min: u64,
    /// The last PCI address.
    pub pci_max: u64,
    /// The CPU address of `pci_min`.
    pub cpu_min: u64,
}

/// A host bridge's ranges, in `_CRS` order.
#[must_use]
pub fn host_ranges(crs: &[Resource]) -> Vec<HostRange> {
    crs.iter()
        .filter_map(|r| match r {
            Resource::Address(a) if a.max >= a.min => {
                let io = match a.kind {
                    AddressKind::Memory { .. } => false,
                    AddressKind::Io { .. } => true,
                    _ => return None,
                };
                Some(HostRange {
                    io,
                    pci_min: a.min,
                    pci_max: a.max,
                    cpu_min: a.min.wrapping_add(a.translation),
                })
            }
            Resource::FixedMemory32(m) if m.length > 0 => Some(HostRange {
                io: false,
                pci_min: m.min.into(),
                pci_max: u64::from(m.min) + u64::from(m.length) - 1,
                cpu_min: m.min.into(),
            }),
            _ => None,
        })
        .collect()
}

/// What a device's range overlaps.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Party {
    /// An operation region AML touches.
    Firmware(NodeId),
    /// Another device's range, not declared shared.
    Device(NodeId),
    /// Another device's range, inside a range the description declares
    /// shared.
    Declared(NodeId),
}

/// One overlap of a device's consumed range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overlap {
    /// The device.
    pub device: NodeId,
    /// Its range.
    pub range: Range,
    /// What else covers part of it.
    pub with: Party,
}

/// Whether a device is a system-resource device (`PNP0C01`, `PNP0C02`):
/// ranges firmware reserves for the platform, which FreeBSD's
/// `acpi_sysres` claims and other devices' ranges lie inside by design.
/// They have no driver of their own, so their overlaps mean nothing.
#[must_use]
pub fn system_resource(ident: &devtree::ids::AcpiIdent) -> bool {
    let sysres = |id: &str| id == "PNP0C01" || id == "PNP0C02";
    ident.hid.as_deref().is_some_and(sysres) || ident.cids.iter().any(|c| sysres(c))
}

/// The overlaps among `devices`' consumed ranges (`ranges`, by device)
/// and the firmware regions: each device against each region, and each
/// pair of devices once (the earlier device first). `declared` are the
/// declared-shared ranges.
#[must_use]
pub fn overlaps(
    ranges: &[(NodeId, Vec<Range>)],
    regions: &[FirmwareRegion],
    declared: &[Range],
) -> Vec<Overlap> {
    let mut out = Vec::new();
    for (k, (device, mine)) in ranges.iter().enumerate() {
        for &range in mine {
            for f in regions {
                let o = Overlap {
                    device: *device,
                    range,
                    with: Party::Firmware(f.region),
                };
                if f.range.intersection(&range).is_some() && !out.contains(&o) {
                    out.push(o);
                }
            }
            for (other, theirs) in &ranges[k + 1..] {
                for both in theirs.iter().filter_map(|t| t.intersection(&range)) {
                    let with = if declared.iter().any(|d| d.contains(&both)) {
                        Party::Declared(*other)
                    } else {
                        Party::Device(*other)
                    };
                    out.push(Overlap {
                        device: *device,
                        range,
                        with,
                    });
                }
            }
        }
    }
    out
}

/// The declared-shared ranges: the memory ranges each declared device
/// consumes (`ranges`, by device).
#[must_use]
pub fn declared(
    shared: &[devtree::platform::SharedRange<NodeId>],
    ranges: &[(NodeId, Vec<Range>)],
) -> Vec<Range> {
    let mut out = Vec::new();
    for s in shared {
        for (_, mine) in ranges.iter().filter(|(d, _)| *d == s.device) {
            for r in mine.iter().filter(|r| r.space == AddressSpace::Memory) {
                if !out.contains(r) {
                    out.push(*r);
                }
            }
        }
    }
    out
}
