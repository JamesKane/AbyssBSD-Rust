// SPDX-License-Identifier: BSD-2-Clause
//! The Generic Address Structure (ACPI 6.5 §5.2.3.2).

use crate::{Error, u64_at};

/// Size of a GAS in a table.
pub const GAS_LEN: usize = 12;

/// Which address space a [`Gas`] points into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressSpace {
    /// System memory (MMIO).
    Memory,
    /// x86 I/O ports.
    Io,
    /// PCI configuration space.
    PciConfig,
    /// Anything else.
    Other(u8),
}

/// A register location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gas {
    /// The address space.
    pub space: AddressSpace,
    /// Register width in bits.
    pub bit_width: u8,
    /// Register offset in bits.
    pub bit_offset: u8,
    /// Access size: 0 undefined, 1 byte, 2 word, 3 dword, 4 qword.
    pub access_size: u8,
    /// The address.
    pub address: u64,
}

impl Gas {
    /// Parses the 12 bytes at `off`.
    pub fn parse(b: &[u8], off: usize) -> Result<Self, Error> {
        let raw = b
            .get(off..off + GAS_LEN)
            .ok_or(Error::Malformed("GAS out of bounds"))?;
        Ok(Self {
            space: match raw[0] {
                0 => AddressSpace::Memory,
                1 => AddressSpace::Io,
                2 => AddressSpace::PciConfig,
                s => AddressSpace::Other(s),
            },
            bit_width: raw[1],
            bit_offset: raw[2],
            access_size: raw[3],
            address: u64_at(raw, 4).ok_or(Error::Malformed("GAS address"))?,
        })
    }

    /// Access width in bytes, from `access_size` or else `bit_width`.
    #[must_use]
    pub fn access_bytes(&self) -> Option<u8> {
        match self.access_size {
            1 => Some(1),
            2 => Some(2),
            3 => Some(4),
            4 => Some(8),
            _ => match self.bit_width {
                8 => Some(1),
                16 => Some(2),
                32 => Some(4),
                64 => Some(8),
                _ => None,
            },
        }
    }
}
