// SPDX-License-Identifier: BSD-2-Clause
//! PCI Express memory-mapped configuration space table (PCI Firmware
//! Specification 3.3 §4.1.2), listing the ECAM windows.
//!
//! Behavior reference: `sys/dev/pci/pci_host_generic_acpi.c`,
//! `sys/x86/pci/pci_bus.c`.

use crate::{Error, HEADER_LEN, Sdt, u16_at, u64_at};

const ENTRIES: usize = HEADER_LEN + 8;
const ENTRY_LEN: usize = 16;

/// A parsed MCFG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mcfg<'a> {
    entries: &'a [u8],
}

/// Parses an MCFG table. The allocation list must be a whole number of
/// entries.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table, [`Error::BadLength`] if the
/// list is missing or ragged.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Mcfg<'a>, Error> {
    if t.signature() != *b"MCFG" {
        return Err(Error::BadSignature);
    }
    let entries = t.bytes.get(ENTRIES..).ok_or(Error::BadLength(*b"MCFG"))?;
    if entries.len() % ENTRY_LEN != 0 {
        return Err(Error::BadLength(*b"MCFG"));
    }
    Ok(Mcfg { entries })
}

/// One ECAM window: configuration space for buses `start_bus..=end_bus` of
/// one PCI segment group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EcamRegion {
    /// Physical address of the window; bus 0's space is at this address
    /// even when `start_bus` is not 0.
    pub base: u64,
    /// PCI segment group number.
    pub segment: u16,
    /// First decoded bus.
    pub start_bus: u8,
    /// Last decoded bus (at least `start_bus`).
    pub end_bus: u8,
}

impl EcamRegion {
    /// Physical address of the configuration space of a function, or `None`
    /// if the bus is outside this window or the device or function is out of
    /// range.
    #[must_use]
    pub fn function_address(&self, bus: u8, device: u8, function: u8) -> Option<u64> {
        if bus < self.start_bus || bus > self.end_bus || device > 31 || function > 7 {
            return None;
        }
        let off = (u64::from(bus) << 20) | (u64::from(device) << 15) | (u64::from(function) << 12);
        self.base.checked_add(off)
    }

    /// The number of buses decoded (0 for an inverted range, which
    /// [`Mcfg::regions`] never returns).
    #[must_use]
    pub fn bus_count(&self) -> u16 {
        (u16::from(self.end_bus) + 1).saturating_sub(u16::from(self.start_bus))
    }
}

impl<'a> Mcfg<'a> {
    /// The ECAM windows. An entry whose end bus precedes its start bus
    /// yields an error; the remaining entries are still returned.
    pub fn regions(&self) -> impl Iterator<Item = Result<EcamRegion, Error>> + 'a {
        self.entries
            .as_chunks::<ENTRY_LEN>()
            .0
            .iter()
            .map(|c| region(c))
    }
}

fn region(e: &[u8]) -> Result<EcamRegion, Error> {
    let bad = Error::Malformed("MCFG allocation");
    let r = EcamRegion {
        base: u64_at(e, 0).ok_or(bad)?,
        segment: u16_at(e, 8).ok_or(bad)?,
        start_bus: *e.get(10).ok_or(bad)?,
        end_bus: *e.get(11).ok_or(bad)?,
    };
    if r.end_bus < r.start_bus {
        return Err(Error::Malformed("MCFG bus range"));
    }
    Ok(r)
}
