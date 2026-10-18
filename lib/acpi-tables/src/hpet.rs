// SPDX-License-Identifier: BSD-2-Clause
//! IA-PC High Precision Event Timer table (IA-PC HPET Specification 1.0a
//! §3.2.4).
//!
//! Behavior reference: `sys/dev/acpica/acpi_hpet.c`.

use crate::raw::Fields;
use crate::{Error, Gas, HEADER_LEN, Sdt};

const BLOCK_ID: usize = HEADER_LEN;
const BASE: usize = HEADER_LEN + 4;
const NUMBER: usize = HEADER_LEN + 16;
const MIN_TICK: usize = HEADER_LEN + 17;
const PAGE_PROTECTION: usize = HEADER_LEN + 19;

/// A parsed HPET table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hpet {
    /// The event timer block ID.
    pub block_id: BlockId,
    /// Where the timer block's registers are.
    pub base: Gas,
    /// The HPET sequence number.
    pub number: u8,
    /// The minimum periodic tick, in main counter ticks, that avoids lost
    /// interrupts.
    pub min_tick: u16,
    /// Page protection and OEM attributes.
    pub page_protection: PageProtection,
}

/// Parses an HPET table.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table, [`Error::BadLength`] if the
/// fields are missing.
pub fn parse(t: &Sdt<'_>) -> Result<Hpet, Error> {
    if t.signature() != *b"HPET" {
        return Err(Error::BadSignature);
    }
    let f = Fields::new(t.bytes, Error::BadLength(*b"HPET"));
    Ok(Hpet {
        block_id: BlockId(f.u32(BLOCK_ID)?),
        base: f.gas(BASE)?,
        number: f.u8(NUMBER)?,
        min_tick: f.u16(MIN_TICK)?,
        page_protection: PageProtection(f.u8(PAGE_PROTECTION)?),
    })
}

/// The event timer block ID, a copy of the timer's capabilities register
/// low half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockId(pub u32);

impl BlockId {
    /// Hardware revision ID.
    #[must_use]
    pub const fn hardware_revision(self) -> u8 {
        self.0.to_le_bytes()[0]
    }

    /// The number of comparators in the first timer block.
    #[must_use]
    pub const fn comparators(self) -> u8 {
        (self.0.to_le_bytes()[1] & 0x1f) + 1
    }

    /// The main counter is 64 bits wide (`COUNT_SIZE_CAP`).
    #[must_use]
    pub const fn counter_64bit(self) -> bool {
        self.0 & (1 << 13) != 0
    }

    /// Legacy replacement interrupt routing is supported.
    #[must_use]
    pub const fn legacy_replacement(self) -> bool {
        self.0 & (1 << 15) != 0
    }

    /// The PCI vendor ID of the first timer block.
    #[must_use]
    pub const fn vendor_id(self) -> u16 {
        let b = self.0.to_le_bytes();
        u16::from_le_bytes([b[2], b[3]])
    }
}

/// Page protection: how much of the address space around the registers
/// is guaranteed free of other devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageProtection(pub u8);

impl PageProtection {
    /// The protected region size in bytes, if stated (4 KiB or 64 KiB).
    #[must_use]
    pub const fn protected_bytes(self) -> Option<u32> {
        match self.0 & 0xf {
            1 => Some(4096),
            2 => Some(65_536),
            _ => None,
        }
    }

    /// OEM attributes, bits 7:4.
    #[must_use]
    pub const fn oem_attributes(self) -> u8 {
        self.0 >> 4
    }
}
