// SPDX-License-Identifier: BSD-2-Clause
//! Core System Resource Table (Microsoft's CSRT specification): resource
//! groups for SoC blocks that have no other description, such as the DMA
//! controllers whose request lines `_CRS` `FixedDMA` descriptors name.
//!
//! A group is a header, vendor-defined shared information, and resource
//! descriptors (each a length, a type, a subtype, and a UID). For a DMA
//! controller group, the shared information this parser interprets
//! ([`ResourceGroup::dma_controller`]) is the layout CIX's firmware uses,
//! as Intel's does: the controller's MMIO base, its interrupt, its channel
//! count, and the range of request lines it serves.

use crate::{Error, HEADER_LEN, Sdt, u16_at, u32_at};

/// A group header's length: length, vendor, subvendor, device, subdevice,
/// revision, reserved, shared information length.
const GROUP_HEADER: usize = 24;
/// A resource descriptor header's length.
const RESOURCE_HEADER: usize = 12;
/// The DMA shared-information layout's length.
const DMA_SHARED_INFO: usize = 28;

/// A parsed CSRT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csrt<'a> {
    groups: &'a [u8],
}

/// Parses a CSRT.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Csrt<'a>, Error> {
    if t.signature() != *b"CSRT" {
        return Err(Error::BadSignature);
    }
    Ok(Csrt {
        groups: t.bytes.get(HEADER_LEN..).unwrap_or(&[]),
    })
}

/// One resource group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceGroup<'a> {
    /// Vendor ID: four ASCII characters, little-endian (`"CIX "`).
    pub vendor: u32,
    /// Subvendor ID.
    pub subvendor: u32,
    /// Device ID.
    pub device: u16,
    /// Subdevice ID.
    pub subdevice: u16,
    /// Revision.
    pub revision: u16,
    /// The vendor-defined shared information.
    pub shared_info: &'a [u8],
    descriptors: &'a [u8],
}

/// One resource descriptor of a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resource<'a> {
    /// Resource type (1 interrupt, 2 timer, 3 DMA, 4 cache).
    pub kind: u16,
    /// Subtype (for DMA: 0 channel, 1 controller).
    pub subtype: u16,
    /// Unique ID within the group.
    pub uid: u32,
    /// The vendor-defined data after the header.
    pub data: &'a [u8],
}

/// A DMA controller, from its group's shared information.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaController {
    /// The controller's MMIO base.
    pub mmio_base: u64,
    /// Its interrupt (GSI).
    pub gsi: u32,
    /// Number of channels.
    pub channels: u8,
    /// DMA address width in bits.
    pub address_width: u8,
    /// The first request line it serves.
    pub base_request_line: u16,
    /// How many request lines it serves.
    pub request_lines: u16,
}

impl DmaController {
    /// Whether it serves request line `line`.
    #[must_use]
    pub fn serves(&self, line: u16) -> bool {
        line >= self.base_request_line
            && u32::from(line) < u32::from(self.base_request_line) + u32::from(self.request_lines)
    }
}

impl<'a> Csrt<'a> {
    /// The resource groups. A group whose length runs past the table, or is
    /// shorter than its header, yields an error and ends the list.
    pub fn groups(&self) -> impl Iterator<Item = Result<ResourceGroup<'a>, Error>> + 'a {
        let mut rest = self.groups;
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let g = group(rest);
            match &g {
                Ok((_, len)) => rest = &rest[*len..],
                Err(_) => rest = &[],
            }
            Some(g.map(|(g, _)| g))
        })
    }
}

fn group(b: &[u8]) -> Result<(ResourceGroup<'_>, usize), Error> {
    let bad = Error::Malformed("CSRT resource group");
    let len = usize::try_from(u32_at(b, 0).ok_or(bad)?).map_err(|_| bad)?;
    let shared = usize::try_from(u32_at(b, 20).ok_or(bad)?).map_err(|_| bad)?;
    if len < GROUP_HEADER || len > b.len() || GROUP_HEADER + shared > len {
        return Err(bad);
    }
    Ok((
        ResourceGroup {
            vendor: u32_at(b, 4).ok_or(bad)?,
            subvendor: u32_at(b, 8).ok_or(bad)?,
            device: u16_at(b, 12).ok_or(bad)?,
            subdevice: u16_at(b, 14).ok_or(bad)?,
            revision: u16_at(b, 16).ok_or(bad)?,
            shared_info: &b[GROUP_HEADER..GROUP_HEADER + shared],
            descriptors: &b[GROUP_HEADER + shared..len],
        },
        len,
    ))
}

impl<'a> ResourceGroup<'a> {
    /// The group's resource descriptors. One whose length is shorter than
    /// its header or runs past the group yields an error and ends the list.
    pub fn resources(&self) -> impl Iterator<Item = Result<Resource<'a>, Error>> + 'a {
        let mut rest = self.descriptors;
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let bad = Error::Malformed("CSRT resource descriptor");
            let r = u32_at(rest, 0)
                .and_then(|l| usize::try_from(l).ok())
                .filter(|&l| l >= RESOURCE_HEADER && l <= rest.len())
                .ok_or(bad)
                .and_then(|len| {
                    let r = Resource {
                        kind: u16_at(rest, 4).ok_or(bad)?,
                        subtype: u16_at(rest, 6).ok_or(bad)?,
                        uid: u32_at(rest, 8).ok_or(bad)?,
                        data: &rest[RESOURCE_HEADER..len],
                    };
                    Ok((r, len))
                });
            match r {
                Ok((r, len)) => {
                    rest = &rest[len..];
                    Some(Ok(r))
                }
                Err(e) => {
                    rest = &[];
                    Some(Err(e))
                }
            }
        })
    }

    /// The group's shared information read as a DMA controller's, or
    /// `None` if it is too short for that layout or names no request
    /// lines.
    #[must_use]
    pub fn dma_controller(&self) -> Option<DmaController> {
        let s = self.shared_info;
        if s.len() < DMA_SHARED_INFO {
            return None;
        }
        let lo = u64::from(u32_at(s, 4)?);
        let hi = u64::from(u32_at(s, 8)?);
        let c = DmaController {
            mmio_base: hi << 32 | lo,
            gsi: u32_at(s, 12)?,
            channels: s[18],
            address_width: s[19],
            base_request_line: u16_at(s, 20)?,
            request_lines: u16_at(s, 22)?,
        };
        (c.request_lines > 0).then_some(c)
    }
}
