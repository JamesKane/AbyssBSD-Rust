// SPDX-License-Identifier: BSD-2-Clause
//! The SCSI a USB disk needs (T10 SPC-4, SBC-3), and the Bulk-Only
//! Transport's wrappers (USB Mass Storage Class BBB 1.0) and USB Attached
//! SCSI's information units (T10 UAS-2): command blocks built, and what
//! the device returns parsed. Device data is untrusted:
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
    /// REPORT LUNS.
    pub const REPORT_LUNS: u8 = 0xa0;
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

    /// INQUIRY for vital product data page `page`, for `len` bytes.
    #[must_use]
    pub fn inquiry_vpd(page: u8, len: u8) -> Self {
        let mut c = Self::inquiry(len);
        c.bytes[1] = 1;
        c.bytes[2] = page;
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

    /// REPORT LUNS of every logical unit, for `len` bytes.
    #[must_use]
    pub fn report_luns(len: u32) -> Self {
        let mut c = Self::new(12);
        c.bytes[0] = op::REPORT_LUNS;
        c.bytes[6..10].copy_from_slice(&len.to_be_bytes());
        c
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

/// Vital product data pages (SPC-4 §7.8).
pub mod vpd {
    /// Supported VPD pages.
    pub const SUPPORTED_PAGES: u8 = 0x00;
    /// Unit serial number.
    pub const UNIT_SERIAL: u8 = 0x80;
    /// The buffer FreeBSD's CAM reads either page into: a 4-byte header
    /// and 251 bytes.
    pub const LEN: u8 = 255;

    /// The page's payload, as long as its header says and the data
    /// allows.
    #[must_use]
    pub fn payload(b: &[u8], page: u8) -> Option<&[u8]> {
        if *b.get(1)? != page {
            return None;
        }
        let len = usize::from(*b.get(3)?);
        b.get(4..).map(|p| &p[..len.min(p.len())])
    }

    /// Whether a supported-pages list names `page`.
    #[must_use]
    pub fn supports(b: &[u8], page: u8) -> bool {
        payload(b, SUPPORTED_PAGES).is_some_and(|l| l.contains(&page))
    }

    /// The unit serial number, trimmed of the spaces devices pad it
    /// with; `None` for none or all spaces (SPC-5 says that means no
    /// serial number), as FreeBSD's CAM reads it.
    #[must_use]
    pub fn serial(b: &[u8]) -> Option<&[u8]> {
        let s = payload(b, UNIT_SERIAL)?;
        let start = s.iter().position(|&c| c != b' ')?;
        let end = s.iter().rposition(|&c| c != b' ')? + 1;
        Some(&s[start..end])
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

/// The LUNs in REPORT LUNS data (SPC-4 §6.33), in SAM's single-level
/// peripheral and flat forms; entries in other forms are skipped.
pub fn report_luns(b: &[u8]) -> impl Iterator<Item = u16> + '_ {
    let len = b
        .get(..4)
        .and_then(|l| l.try_into().ok())
        .map_or(0, u32::from_be_bytes);
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    let list = b.get(8..).unwrap_or(&[]);
    let list = &list[..len.min(list.len())];
    list.as_chunks::<8>().0.iter().filter_map(|e| {
        if e[2..] != [0; 6] {
            return None;
        }
        match e[0] >> 6 {
            0 if e[0] == 0 => Some(u16::from(e[1])),
            1 => Some(u16::from_be_bytes([e[0] & 0x3f, e[1]])),
            _ => None,
        }
    })
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

/// SCSI status codes (SAM-5 §5.3).
pub mod status {
    /// Good.
    pub const GOOD: u8 = 0x00;
    /// Check condition: the sense data says why.
    pub const CHECK_CONDITION: u8 = 0x02;
    /// Busy.
    pub const BUSY: u8 = 0x08;
    /// Task set full: too many commands queued.
    pub const TASK_SET_FULL: u8 = 0x28;
}

/// USB Attached SCSI (T10 UAS-2): its information units and the pipe
/// usage descriptor that says which endpoint is which.
pub mod uas {
    use super::{Cdb, Sense};

    /// The pipe usage descriptor's type, after each endpoint's.
    pub const PIPE_USAGE: u8 = 0x24;

    /// What an endpoint carries.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Pipe {
        /// Command and task management IUs, host to device.
        Command,
        /// Sense, response, and ready IUs, device to host.
        Status,
        /// Data in.
        DataIn,
        /// Data out.
        DataOut,
    }

    impl Pipe {
        /// From a pipe usage descriptor's body (after length and type).
        #[must_use]
        pub fn parse(body: &[u8]) -> Option<Self> {
            match body.first()? {
                1 => Some(Self::Command),
                2 => Some(Self::Status),
                3 => Some(Self::DataIn),
                4 => Some(Self::DataOut),
                _ => None,
            }
        }
    }

    /// The IU identifiers.
    const IU_COMMAND: u8 = 0x01;
    const IU_SENSE: u8 = 0x03;
    const IU_RESPONSE: u8 = 0x04;
    const IU_TASK: u8 = 0x05;
    const IU_READ_READY: u8 = 0x06;
    const IU_WRITE_READY: u8 = 0x07;

    /// A command IU's size with a CDB of up to 16 bytes.
    pub const COMMAND_LEN: usize = 32;
    /// A task management IU's size.
    pub const TASK_LEN: usize = 16;
    /// The most a status pipe read needs: a sense IU with 252 bytes of
    /// sense data.
    pub const STATUS_LEN: usize = 16 + 252;

    /// A LUN in SAM's single-level form: peripheral addressing below 256,
    /// flat addressing above.
    fn lun_bytes(lun: u16) -> [u8; 8] {
        let mut b = [0u8; 8];
        let [hi, lo] = lun.to_be_bytes();
        b[0] = if lun < 256 { 0 } else { 0x40 | (hi & 0x3f) };
        b[1] = lo;
        b
    }

    /// A command IU: `cdb` to `lun` as a simple task, tagged `tag`.
    #[must_use]
    pub fn command(tag: u16, lun: u16, cdb: &Cdb) -> [u8; COMMAND_LEN] {
        let mut u = [0u8; COMMAND_LEN];
        u[0] = IU_COMMAND;
        u[2..4].copy_from_slice(&tag.to_be_bytes());
        // Byte 4: priority 0, task attribute SIMPLE (0); byte 6: no
        // additional CDB.
        u[8..16].copy_from_slice(&lun_bytes(lun));
        let c = cdb.as_bytes();
        u[16..16 + c.len()].copy_from_slice(c);
        u
    }

    /// Task management functions.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Function {
        /// Abort one task, by tag.
        AbortTask,
        /// Abort every task on the LUN from this initiator.
        AbortTaskSet,
        /// Reset the logical unit.
        LogicalUnitReset,
        /// Ask whether a task is still in the task set.
        QueryTask,
    }

    impl Function {
        fn code(self) -> u8 {
            match self {
                Self::AbortTask => 0x01,
                Self::AbortTaskSet => 0x02,
                Self::LogicalUnitReset => 0x08,
                Self::QueryTask => 0x80,
            }
        }
    }

    /// A task management IU, tagged `tag`, about task `managed` (for
    /// the functions that name one).
    #[must_use]
    pub fn task(tag: u16, lun: u16, f: Function, managed: u16) -> [u8; TASK_LEN] {
        let mut u = [0u8; TASK_LEN];
        u[0] = IU_TASK;
        u[2..4].copy_from_slice(&tag.to_be_bytes());
        u[4] = f.code();
        u[6..8].copy_from_slice(&managed.to_be_bytes());
        u[8..16].copy_from_slice(&lun_bytes(lun));
        u
    }

    /// A task management function's response code (UAS-2 §6.2.5).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Response {
        /// Function complete.
        Complete,
        /// Function succeeded (a query found the task).
        Succeeded,
        /// Anything else: invalid IU, not supported, failed, bad LUN,
        /// overlapped tag.
        Failed(u8),
    }

    /// What the status pipe returns.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Status {
        /// The command ended: its SCSI status, and the sense data if the
        /// device sent any (autosense).
        Sense {
            /// The task's tag.
            tag: u16,
            /// The SCSI status.
            status: u8,
            /// The sense data.
            sense: Option<Sense>,
        },
        /// A task management function ended.
        Response {
            /// The function's tag.
            tag: u16,
            /// How.
            code: Response,
        },
        /// Without streams: the device is ready to send the data in of
        /// command `tag`.
        ReadReady(u16),
        /// Without streams: the device is ready for command `tag`'s data
        /// out.
        WriteReady(u16),
    }

    impl Status {
        /// Parses a status pipe IU; `None` if it is not one.
        #[must_use]
        pub fn parse(b: &[u8]) -> Option<Self> {
            let tag = u16::from_be_bytes(b.get(2..4)?.try_into().ok()?);
            match *b.first()? {
                IU_SENSE => {
                    let status = *b.get(6)?;
                    let len = usize::from(u16::from_be_bytes(b.get(14..16)?.try_into().ok()?));
                    let data = b.get(16..)?;
                    let sense = Sense::parse(data.get(..len.min(data.len()))?);
                    Some(Self::Sense { tag, status, sense })
                }
                IU_RESPONSE => {
                    let code = match *b.get(7)? {
                        0x00 => Response::Complete,
                        0x08 => Response::Succeeded,
                        c => Response::Failed(c),
                    };
                    Some(Self::Response { tag, code })
                }
                IU_READ_READY => Some(Self::ReadReady(tag)),
                IU_WRITE_READY => Some(Self::WriteReady(tag)),
                _ => None,
            }
        }

        /// Its tag.
        #[must_use]
        pub fn tag(&self) -> u16 {
            match *self {
                Self::Sense { tag, .. } | Self::Response { tag, .. } => tag,
                Self::ReadReady(t) | Self::WriteReady(t) => t,
            }
        }
    }
}

/// Trims an ASCII identification field as FreeBSD prints it.
#[must_use]
pub fn ident(b: &[u8]) -> &str {
    let s = core::str::from_utf8(b).unwrap_or("");
    s.trim_matches(|c: char| c == ' ' || c == '\0')
}
