// SPDX-License-Identifier: BSD-2-Clause
//! The SCSI a USB disk needs (T10 SPC-4, SBC-3), and the Bulk-Only
//! Transport's wrappers (USB Mass Storage Class BBB 1.0): command blocks
//! built, and what the device returns parsed. Device data is untrusted:
//! parsing returns `None`, never panics.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

/// Operation codes.
pub mod op {
    /// TEST UNIT READY.
    pub const TEST_UNIT_READY: u8 = 0x00;
    /// REQUEST SENSE.
    pub const REQUEST_SENSE: u8 = 0x03;
    /// INQUIRY.
    pub const INQUIRY: u8 = 0x12;
    /// MODE SENSE(6).
    pub const MODE_SENSE_6: u8 = 0x1a;
    /// READ CAPACITY(10).
    pub const READ_CAPACITY_10: u8 = 0x25;
    /// READ(10).
    pub const READ_10: u8 = 0x28;
    /// WRITE(10).
    pub const WRITE_10: u8 = 0x2a;
    /// SYNCHRONIZE CACHE(10).
    pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
    /// READ(16).
    pub const READ_16: u8 = 0x88;
    /// WRITE(16).
    pub const WRITE_16: u8 = 0x8a;
    /// SERVICE ACTION IN(16), for READ CAPACITY(16).
    pub const SERVICE_ACTION_IN_16: u8 = 0x9e;
}

/// A command descriptor block, up to 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cdb {
    bytes: [u8; 16],
    len: u8,
}

impl Cdb {
    fn new(len: u8) -> Self {
        Self {
            bytes: [0; 16],
            len,
        }
    }

    /// The command's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// TEST UNIT READY.
    #[must_use]
    pub fn test_unit_ready() -> Self {
        Self::new(6)
    }

    /// REQUEST SENSE for `len` bytes.
    #[must_use]
    pub fn request_sense(len: u8) -> Self {
        let mut c = Self::new(6);
        c.bytes[0] = op::REQUEST_SENSE;
        c.bytes[4] = len;
        c
    }

    /// Standard INQUIRY for `len` bytes.
    #[must_use]
    pub fn inquiry(len: u8) -> Self {
        let mut c = Self::new(6);
        c.bytes[0] = op::INQUIRY;
        c.bytes[4] = len;
        c
    }

    /// MODE SENSE(6) of every page, for `len` bytes (the header says
    /// whether the medium is write-protected).
    #[must_use]
    pub fn mode_sense(len: u8) -> Self {
        let mut c = Self::new(6);
        c.bytes[0] = op::MODE_SENSE_6;
        c.bytes[2] = 0x3f;
        c.bytes[4] = len;
        c
    }

    /// READ CAPACITY(10).
    #[must_use]
    pub fn read_capacity_10() -> Self {
        let mut c = Self::new(10);
        c.bytes[0] = op::READ_CAPACITY_10;
        c
    }

    /// READ CAPACITY(16), for 32 bytes.
    #[must_use]
    pub fn read_capacity_16() -> Self {
        let mut c = Self::new(16);
        c.bytes[0] = op::SERVICE_ACTION_IN_16;
        c.bytes[1] = 0x10;
        c.bytes[13] = 32;
        c
    }

    /// READ or WRITE of `blocks` blocks at `lba`: the 10-byte form when it
    /// reaches, else the 16-byte one.
    #[must_use]
    pub fn rw(write: bool, lba: u64, blocks: u32) -> Self {
        match (u32::try_from(lba), u16::try_from(blocks)) {
            (Ok(l), Ok(n)) if u64::from(l) + u64::from(n) <= 1 << 32 => {
                let mut c = Self::new(10);
                c.bytes[0] = if write { op::WRITE_10 } else { op::READ_10 };
                c.bytes[2..6].copy_from_slice(&l.to_be_bytes());
                c.bytes[7..9].copy_from_slice(&n.to_be_bytes());
                c
            }
            _ => {
                let mut c = Self::new(16);
                c.bytes[0] = if write { op::WRITE_16 } else { op::READ_16 };
                c.bytes[2..10].copy_from_slice(&lba.to_be_bytes());
                c.bytes[10..14].copy_from_slice(&blocks.to_be_bytes());
                c
            }
        }
    }

    /// SYNCHRONIZE CACHE(10) of the whole medium.
    #[must_use]
    pub fn synchronize_cache() -> Self {
        let mut c = Self::new(10);
        c.bytes[0] = op::SYNCHRONIZE_CACHE_10;
        c
    }
}

/// Peripheral device types (SPC-4 Table 50).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceType {
    /// Direct access (a disk).
    Direct,
    /// CD/DVD.
    Cdrom,
    /// Optical memory.
    Optical,
    /// Simplified direct access (RBC).
    Simplified,
    /// Anything else.
    Other(u8),
}

/// Standard INQUIRY data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inquiry {
    /// The peripheral device type.
    pub device_type: DeviceType,
    /// The qualifier says nothing is there (the LUN is not supported).
    pub absent: bool,
    /// The medium is removable.
    pub removable: bool,
    /// The SPC version (`ANSI VERSION`).
    pub version: u8,
    /// T10 vendor identification (8 bytes).
    pub vendor: [u8; 8],
    /// Product identification (16 bytes).
    pub product: [u8; 16],
    /// Product revision level (4 bytes).
    pub revision: [u8; 4],
}

impl Inquiry {
    /// Parses at least 36 bytes of INQUIRY data.
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 36 {
            return None;
        }
        let device_type = match b[0] & 0x1f {
            0x00 => DeviceType::Direct,
            0x05 => DeviceType::Cdrom,
            0x07 => DeviceType::Optical,
            0x0e => DeviceType::Simplified,
            t => DeviceType::Other(t),
        };
        Some(Self {
            device_type,
            absent: b[0] >> 5 == 3,
            removable: b[1] & 0x80 != 0,
            version: b[2],
            vendor: b[8..16].try_into().ok()?,
            product: b[16..32].try_into().ok()?,
            revision: b[32..36].try_into().ok()?,
        })
    }
}

/// The capacity: the last block's address and the block size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// Blocks.
    pub blocks: u64,
    /// Bytes per block.
    pub block_size: u32,
}

impl Capacity {
    /// Parses READ CAPACITY(10) data. `None` also when the device says
    /// to ask READ CAPACITY(16) (the last LBA is `0xffffffff`).
    #[must_use]
    pub fn parse_10(b: &[u8]) -> Option<Self> {
        let last = u32::from_be_bytes(b.get(0..4)?.try_into().ok()?);
        let block_size = u32::from_be_bytes(b.get(4..8)?.try_into().ok()?);
        (last != u32::MAX).then_some(Self {
            blocks: u64::from(last) + 1,
            block_size,
        })
    }

    /// Parses READ CAPACITY(16) data.
    #[must_use]
    pub fn parse_16(b: &[u8]) -> Option<Self> {
        let last = u64::from_be_bytes(b.get(0..8)?.try_into().ok()?);
        let block_size = u32::from_be_bytes(b.get(8..12)?.try_into().ok()?);
        Some(Self {
            blocks: last.checked_add(1)?,
            block_size,
        })
    }
}

/// Sense keys this driver acts on.
pub mod key {
    /// No sense.
    pub const NO_SENSE: u8 = 0x0;
    /// Recovered error: the command succeeded.
    pub const RECOVERED_ERROR: u8 = 0x1;
    /// Not ready (spinning up; no medium).
    pub const NOT_READY: u8 = 0x2;
    /// Medium error.
    pub const MEDIUM_ERROR: u8 = 0x3;
    /// Illegal request (an unsupported command).
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    /// Unit attention (reset, medium changed).
    pub const UNIT_ATTENTION: u8 = 0x6;
    /// Data protect (write-protected).
    pub const DATA_PROTECT: u8 = 0x7;
}

/// Sense data, either format (SPC-4 §4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense {
    /// The sense key.
    pub key: u8,
    /// Additional sense code.
    pub asc: u8,
    /// Additional sense code qualifier.
    pub ascq: u8,
}

impl Sense {
    /// Parses fixed (0x70/0x71) or descriptor (0x72/0x73) sense data.
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        match b.first()? & 0x7f {
            0x70 | 0x71 => Some(Self {
                key: b.get(2)? & 0xf,
                asc: *b.get(12).unwrap_or(&0),
                ascq: *b.get(13).unwrap_or(&0),
            }),
            0x72 | 0x73 => Some(Self {
                key: b.get(1)? & 0xf,
                asc: *b.get(2).unwrap_or(&0),
                ascq: *b.get(3).unwrap_or(&0),
            }),
            _ => None,
        }
    }

    /// Medium not present (ASC 0x3a).
    #[must_use]
    pub fn no_medium(&self) -> bool {
        self.key == key::NOT_READY && self.asc == 0x3a
    }
}

/// Bulk-Only Transport (BBB 1.0 §5).
pub mod bbb {
    use super::Cdb;

    /// A command block wrapper's size.
    pub const CBW_LEN: usize = 31;
    /// A command status wrapper's size.
    pub const CSW_LEN: usize = 13;
    const CBW_SIGNATURE: u32 = 0x4342_5355;
    const CSW_SIGNATURE: u32 = 0x5342_5355;

    /// The class requests: Bulk-Only Mass Storage Reset and Get Max LUN.
    pub const RESET: u8 = 0xff;
    /// Get Max LUN.
    pub const GET_MAX_LUN: u8 = 0xfe;

    /// A command block wrapper for `cdb` to `lun`, moving `len` bytes in
    /// direction `is_in`, tagged `tag`.
    #[must_use]
    pub fn cbw(tag: u32, len: u32, is_in: bool, lun: u8, cdb: &Cdb) -> [u8; CBW_LEN] {
        let mut w = [0u8; CBW_LEN];
        w[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        w[4..8].copy_from_slice(&tag.to_le_bytes());
        w[8..12].copy_from_slice(&len.to_le_bytes());
        w[12] = if is_in { 0x80 } else { 0 };
        w[13] = lun & 0xf;
        let c = cdb.as_bytes();
        w[14] = u8::try_from(c.len()).unwrap_or(16);
        w[15..15 + c.len()].copy_from_slice(c);
        w
    }

    /// A command's status.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Status {
        /// Passed.
        Passed,
        /// Failed: ask REQUEST SENSE.
        Failed,
        /// A phase error: reset recovery.
        Phase,
    }

    /// A command status wrapper.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Csw {
        /// Its tag (the CBW's).
        pub tag: u32,
        /// The bytes of the data phase not moved.
        pub residue: u32,
        /// The status.
        pub status: Status,
    }

    impl Csw {
        /// Parses a CSW; `None` if it is not one (a short or unsigned
        /// reply calls for reset recovery).
        #[must_use]
        pub fn parse(b: &[u8]) -> Option<Self> {
            if b.len() < CSW_LEN {
                return None;
            }
            let word = |at: usize| Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?));
            if word(0)? != CSW_SIGNATURE {
                return None;
            }
            Some(Self {
                tag: word(4)?,
                residue: word(8)?,
                status: match b[12] {
                    0 => Status::Passed,
                    1 => Status::Failed,
                    _ => Status::Phase,
                },
            })
        }
    }
}

/// Trims an ASCII identification field as FreeBSD prints it.
#[must_use]
pub fn ident(b: &[u8]) -> &str {
    let s = core::str::from_utf8(b).unwrap_or("");
    s.trim_matches(|c: char| c == ' ' || c == '\0')
}
