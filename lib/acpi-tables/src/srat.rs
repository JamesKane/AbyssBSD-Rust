// SPDX-License-Identifier: BSD-2-Clause
//! System Resource Affinity Table (ACPI 6.5 §5.2.16): which proximity domain
//! (NUMA node) each processor, memory range, and ITS belongs to.
//!
//! Proximity domains are read as full 32-bit values regardless of table
//! revision, as FreeBSD does.
//!
//! Behavior reference: `sys/dev/acpica/acpi_pxm.c`.

use crate::raw::{Entries, Fields, LenField, Raw};
use crate::{Error, HEADER_LEN, Sdt};

/// Header, then a reserved `u32` (1) and a reserved `u64`.
const ENTRIES: usize = HEADER_LEN + 12;

const SHORT: Error = Error::Malformed("SRAT entry too short");
const WALK: Error = Error::Malformed("SRAT entry length");

/// A parsed SRAT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Srat<'a> {
    bytes: &'a [u8],
}

/// Parses an SRAT.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table, [`Error::BadLength`] if the
/// fixed fields are missing.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Srat<'a>, Error> {
    if t.signature() != *b"SRAT" {
        return Err(Error::BadSignature);
    }
    if t.bytes.len() < ENTRIES {
        return Err(Error::BadLength(*b"SRAT"));
    }
    Ok(Srat { bytes: t.bytes })
}

impl<'a> Srat<'a> {
    /// The affinity structures. A malformed structure length yields one
    /// error and ends the walk; a known structure too short for its fields
    /// yields an error and the walk continues.
    #[must_use]
    pub fn entries(&self) -> SratEntries<'a> {
        SratEntries(Entries::new(self.bytes, ENTRIES, None, LenField::U8, WALK))
    }
}

/// Iterator returned by [`Srat::entries`].
#[derive(Clone, Debug)]
pub struct SratEntries<'a>(Entries<'a>);

impl<'a> Iterator for SratEntries<'a> {
    type Item = Result<SratEntry<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.0.next()?.and_then(decode))
    }
}

/// An affinity structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SratEntry<'a> {
    /// Processor Local APIC/SAPIC Affinity (type 0).
    ProcessorLocalApic(ProcessorLocalApic),
    /// Memory Affinity (type 1).
    MemoryAffinity(MemoryAffinity),
    /// Processor Local x2APIC Affinity (type 2).
    ProcessorLocalX2Apic(ProcessorLocalX2Apic),
    /// GICC Affinity (type 3).
    GiccAffinity(GiccAffinity),
    /// GIC ITS Affinity (type 4).
    GicItsAffinity(GicItsAffinity),
    /// Generic Initiator Affinity (type 5).
    GenericInitiator(GenericInitiator<'a>),
    /// A structure this parser does not decode.
    Other {
        /// The structure type.
        kind: u8,
        /// The whole structure, header included.
        bytes: &'a [u8],
    },
}

/// Processor Local APIC/SAPIC Affinity structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorLocalApic {
    /// Proximity domain, assembled from its low byte and high three bytes.
    pub domain: u32,
    /// Local APIC ID.
    pub apic_id: u8,
    /// The entry is in use.
    pub enabled: bool,
    /// Local SAPIC EID (Itanium; 0 elsewhere).
    pub sapic_eid: u8,
    /// Clock domain.
    pub clock_domain: u32,
}

/// Memory Affinity structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryAffinity {
    /// Proximity domain.
    pub domain: u32,
    /// Physical base address.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// Flags.
    pub flags: MemoryFlags,
}

/// Memory affinity flags (ACPI 6.5 Table 5.65).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryFlags(pub u32);

impl MemoryFlags {
    /// The entry is in use.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.0 & 1 != 0
    }

    /// The memory is hot-pluggable.
    #[must_use]
    pub const fn hot_pluggable(self) -> bool {
        self.0 & 2 != 0
    }

    /// The memory is non-volatile.
    #[must_use]
    pub const fn non_volatile(self) -> bool {
        self.0 & 4 != 0
    }
}

/// Processor Local x2APIC Affinity structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorLocalX2Apic {
    /// Proximity domain.
    pub domain: u32,
    /// x2APIC ID.
    pub x2apic_id: u32,
    /// The entry is in use.
    pub enabled: bool,
    /// Clock domain.
    pub clock_domain: u32,
}

/// GICC Affinity structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GiccAffinity {
    /// Proximity domain.
    pub domain: u32,
    /// ACPI processor UID, matching the MADT GICC entry.
    pub processor_uid: u32,
    /// The entry is in use.
    pub enabled: bool,
    /// Clock domain.
    pub clock_domain: u32,
}

/// GIC ITS Affinity structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicItsAffinity {
    /// Proximity domain.
    pub domain: u32,
    /// ITS ID, matching the MADT GIC ITS entry.
    pub its_id: u32,
}

/// Generic Initiator Affinity structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenericInitiator<'a> {
    /// Device handle type: 0 ACPI (HID + UID), 1 PCI (segment + BDF).
    pub handle_type: u8,
    /// Proximity domain.
    pub domain: u32,
    /// The 16-byte device handle.
    pub handle: &'a [u8],
    /// The entry is in use.
    pub enabled: bool,
    /// The initiator supports architectural transactions.
    pub architectural_transactions: bool,
}

fn decode(raw: Raw<'_>) -> Result<SratEntry<'_>, Error> {
    let f = Fields::new(raw.bytes, SHORT);
    Ok(match raw.kind {
        0 => {
            let hi = [f.u8(9)?, f.u8(10)?, f.u8(11)?];
            SratEntry::ProcessorLocalApic(ProcessorLocalApic {
                domain: u32::from_le_bytes([f.u8(2)?, hi[0], hi[1], hi[2]]),
                apic_id: f.u8(3)?,
                enabled: f.u32(4)? & 1 != 0,
                sapic_eid: f.u8(8)?,
                clock_domain: f.u32(12)?,
            })
        }
        1 => SratEntry::MemoryAffinity(MemoryAffinity {
            domain: f.u32(2)?,
            base: f.u64(8)?,
            length: f.u64(16)?,
            flags: MemoryFlags(f.u32(28)?),
        }),
        2 => SratEntry::ProcessorLocalX2Apic(ProcessorLocalX2Apic {
            domain: f.u32(4)?,
            x2apic_id: f.u32(8)?,
            enabled: f.u32(12)? & 1 != 0,
            clock_domain: f.u32(16)?,
        }),
        3 => SratEntry::GiccAffinity(GiccAffinity {
            domain: f.u32(2)?,
            processor_uid: f.u32(6)?,
            enabled: f.u32(10)? & 1 != 0,
            clock_domain: f.u32(14)?,
        }),
        4 => SratEntry::GicItsAffinity(GicItsAffinity {
            domain: f.u32(2)?,
            its_id: f.u32(8)?,
        }),
        5 => {
            let flags = f.u32(24)?;
            SratEntry::GenericInitiator(GenericInitiator {
                handle_type: f.u8(3)?,
                domain: f.u32(4)?,
                handle: raw.bytes.get(8..24).ok_or(SHORT)?,
                enabled: flags & 1 != 0,
                architectural_transactions: flags & 2 != 0,
            })
        }
        kind => SratEntry::Other {
            kind,
            bytes: raw.bytes,
        },
    })
}
