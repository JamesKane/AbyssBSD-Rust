// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/efi` (`sys/efiio.h`, `sys/efi.h`): the requests `efivar(3)`,
//! `efibootmgr(8)`, `efitable(8)`, and `efirtc` users send, and how UEFI
//! status codes become error numbers.
//!
//! Behavior reference: sys/dev/efidev/efirt.c (`efi_status2err`).

use crate::errno::Errno;
use crate::ioctl::{IOC_IN, IOC_OUT};
use crate::layout::{Arches, ConstValue};

/// `efi_guid_t`: a GUID in its UEFI encoding.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EfiGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl EfiGuid {
    /// The GUID's 16 bytes as they lie in memory.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0; 16];
        b[0..4].copy_from_slice(&self.data1.to_ne_bytes());
        b[4..6].copy_from_slice(&self.data2.to_ne_bytes());
        b[6..8].copy_from_slice(&self.data3.to_ne_bytes());
        b[8..].copy_from_slice(&self.data4);
        b
    }

    /// A GUID from its 16 bytes in memory.
    #[must_use]
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        Self {
            data1: u32::from_ne_bytes([b[0], b[1], b[2], b[3]]),
            data2: u16::from_ne_bytes([b[4], b[5]]),
            data3: u16::from_ne_bytes([b[6], b[7]]),
            data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        }
    }
}

/// `struct efi_tm`, UEFI's `EFI_TIME`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EfiTm {
    pub tm_year: u16,
    pub tm_mon: u8,
    pub tm_mday: u8,
    pub tm_hour: u8,
    pub tm_min: u8,
    pub tm_sec: u8,
    pub pad1: u8,
    pub tm_nsec: u32,
    /// Minutes from UTC, or 2047 (`EFI_UNSPECIFIED_TIMEZONE`).
    pub tm_tz: i16,
    pub tm_dst: u8,
    pub pad2: u8,
}

/// `sizeof(struct efi_tm)`.
pub const EFI_TM_LEN: usize = 16;
const _: () = assert!(size_of::<EfiTm>() == EFI_TM_LEN);

impl EfiTm {
    /// The structure's bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; EFI_TM_LEN] {
        let mut b = [0; EFI_TM_LEN];
        b[0..2].copy_from_slice(&self.tm_year.to_ne_bytes());
        b[2] = self.tm_mon;
        b[3] = self.tm_mday;
        b[4] = self.tm_hour;
        b[5] = self.tm_min;
        b[6] = self.tm_sec;
        b[7] = self.pad1;
        b[8..12].copy_from_slice(&self.tm_nsec.to_ne_bytes());
        b[12..14].copy_from_slice(&self.tm_tz.to_ne_bytes());
        b[14] = self.tm_dst;
        b[15] = self.pad2;
        b
    }

    /// A structure from its bytes, or `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let b: &[u8; EFI_TM_LEN] = b.get(..EFI_TM_LEN)?.try_into().ok()?;
        Some(Self {
            tm_year: u16::from_ne_bytes([b[0], b[1]]),
            tm_mon: b[2],
            tm_mday: b[3],
            tm_hour: b[4],
            tm_min: b[5],
            tm_sec: b[6],
            pad1: b[7],
            tm_nsec: u32::from_ne_bytes([b[8], b[9], b[10], b[11]]),
            tm_tz: i16::from_ne_bytes([b[12], b[13]]),
            tm_dst: b[14],
            pad2: b[15],
        })
    }
}

/// `struct efi_get_table_ioctl`, `EFIIOC_GET_TABLE`'s argument.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EfiGetTableIoctl {
    /// The caller's buffer (a user address), or null for the length only.
    pub buf: u64,
    /// The table's GUID.
    pub guid: EfiGuid,
    /// Set to the table's length.
    pub table_len: u64,
    /// The buffer's length.
    pub buf_len: u64,
}

/// `struct efi_var_ioctl`, the argument of `EFIIOC_VAR_*`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EfiVarIoctl {
    /// The name, UCS-2 (a user address).
    pub name: u64,
    /// The name buffer's size in bytes.
    pub namesize: u64,
    /// The variable's vendor GUID.
    pub vendor: EfiGuid,
    /// `EFI_VARIABLE_*` attributes.
    pub attrib: u32,
    /// The data (a user address).
    pub data: u64,
    /// The data's size in bytes.
    pub datasize: u64,
}

/// `struct efi_waketime_ioctl`, the argument of `EFIIOC_*_WAKETIME`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EfiWaketimeIoctl {
    pub waketime: EfiTm,
    pub enabled: u8,
    pub pending: u8,
}

/// `sizeof(struct efi_get_table_ioctl)`.
pub const EFI_GET_TABLE_IOCTL_LEN: usize = 40;
/// `sizeof(struct efi_var_ioctl)`.
pub const EFI_VAR_IOCTL_LEN: usize = 56;
/// `sizeof(struct efi_waketime_ioctl)`.
pub const EFI_WAKETIME_IOCTL_LEN: usize = 20;
const _: () = assert!(size_of::<EfiGetTableIoctl>() == EFI_GET_TABLE_IOCTL_LEN);
const _: () = assert!(size_of::<EfiVarIoctl>() == EFI_VAR_IOCTL_LEN);
const _: () = assert!(size_of::<EfiWaketimeIoctl>() == EFI_WAKETIME_IOCTL_LEN);

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(v)
}

fn guid_at(b: &[u8], at: usize) -> EfiGuid {
    let mut g = [0; 16];
    g.copy_from_slice(&b[at..at + 16]);
    EfiGuid::from_bytes(&g)
}

impl EfiVarIoctl {
    /// The structure in an ioctl argument, or `None` if the argument is
    /// not one.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != EFI_VAR_IOCTL_LEN {
            return None;
        }
        Some(Self {
            name: u64_at(b, 0),
            namesize: u64_at(b, 8),
            vendor: guid_at(b, 16),
            attrib: u32::from_ne_bytes([b[32], b[33], b[34], b[35]]),
            data: u64_at(b, 40),
            datasize: u64_at(b, 48),
        })
    }

    /// Writes the structure back over `b` (an argument [`from_bytes`]
    /// accepted), leaving the padding as it was.
    ///
    /// [`from_bytes`]: Self::from_bytes
    pub fn store(&self, b: &mut [u8]) {
        b[0..8].copy_from_slice(&self.name.to_ne_bytes());
        b[8..16].copy_from_slice(&self.namesize.to_ne_bytes());
        b[16..32].copy_from_slice(&self.vendor.to_bytes());
        b[32..36].copy_from_slice(&self.attrib.to_ne_bytes());
        b[40..48].copy_from_slice(&self.data.to_ne_bytes());
        b[48..56].copy_from_slice(&self.datasize.to_ne_bytes());
    }
}

impl EfiGetTableIoctl {
    /// The structure in an ioctl argument, or `None` if the argument is
    /// not one.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != EFI_GET_TABLE_IOCTL_LEN {
            return None;
        }
        Some(Self {
            buf: u64_at(b, 0),
            guid: guid_at(b, 8),
            table_len: u64_at(b, 24),
            buf_len: u64_at(b, 32),
        })
    }

    /// Writes the structure back over `b`.
    pub fn store(&self, b: &mut [u8]) {
        b[0..8].copy_from_slice(&self.buf.to_ne_bytes());
        b[8..24].copy_from_slice(&self.guid.to_bytes());
        b[24..32].copy_from_slice(&self.table_len.to_ne_bytes());
        b[32..40].copy_from_slice(&self.buf_len.to_ne_bytes());
    }
}

// Every length is a few dozen bytes, well within `IOCPARM_MASK`.
#[allow(clippy::cast_possible_truncation)]
const fn ioc(dir: u32, num: u32, len: usize) -> u32 {
    dir | ((len as u32) << 16) | (b'E' as u32) << 8 | num
}

pub const EFIIOC_GET_TABLE: u32 = ioc(IOC_IN | IOC_OUT, 1, EFI_GET_TABLE_IOCTL_LEN);
pub const EFIIOC_GET_TIME: u32 = ioc(IOC_OUT, 2, EFI_TM_LEN);
pub const EFIIOC_SET_TIME: u32 = ioc(IOC_IN, 3, EFI_TM_LEN);
pub const EFIIOC_VAR_GET: u32 = ioc(IOC_IN | IOC_OUT, 4, EFI_VAR_IOCTL_LEN);
pub const EFIIOC_VAR_NEXT: u32 = ioc(IOC_IN | IOC_OUT, 5, EFI_VAR_IOCTL_LEN);
pub const EFIIOC_VAR_SET: u32 = ioc(IOC_IN | IOC_OUT, 6, EFI_VAR_IOCTL_LEN);
pub const EFIIOC_GET_WAKETIME: u32 = ioc(IOC_OUT, 7, EFI_WAKETIME_IOCTL_LEN);
pub const EFIIOC_SET_WAKETIME: u32 = ioc(IOC_IN, 8, EFI_WAKETIME_IOCTL_LEN);

/// The configuration tables `EFIIOC_GET_TABLE` copies (`copy_table`'s
/// list): `EFI_TABLE_ESRT`, `EFI_PROPERTIES_TABLE`, and
/// `EFI_MEMORY_ATTRIBUTES_TABLE`.
pub const EFI_TABLE_ESRT: EfiGuid = EfiGuid {
    data1: 0xb122_a263,
    data2: 0x3661,
    data3: 0x4f68,
    data4: [0x99, 0x29, 0x78, 0xf8, 0xb0, 0xd6, 0x21, 0x80],
};
pub const EFI_PROPERTIES_TABLE: EfiGuid = EfiGuid {
    data1: 0x880a_aca3,
    data2: 0x4adc,
    data3: 0x4a04,
    data4: [0x90, 0x79, 0xb7, 0x47, 0x34, 0x08, 0x25, 0xe5],
};
pub const EFI_MEMORY_ATTRIBUTES_TABLE: EfiGuid = EfiGuid {
    data1: 0xdcfa_911d,
    data2: 0x26eb,
    data3: 0x469f,
    data4: [0xa2, 0x20, 0x38, 0xb7, 0xdc, 0x46, 0x12, 0x20],
};

/// `ESRT_FIRMWARE_RESOURCE_VERSION`.
pub const ESRT_FIRMWARE_RESOURCE_VERSION: u64 = 1;
/// `sizeof(struct efi_esrt_table)`: the header before the entries.
pub const EFI_ESRT_TABLE_LEN: u64 = 16;
/// `sizeof(struct efi_esrt_entry_v1)`.
pub const EFI_ESRT_ENTRY_V1_LEN: u64 = 40;
/// `sizeof(struct efi_prop_table)`.
pub const EFI_PROP_TABLE_LEN: u64 = 16;
/// `sizeof(struct efi_memory_attribute_table)`: the header before the
/// descriptors.
pub const EFI_MEMORY_ATTRIBUTE_TABLE_LEN: u64 = 16;
/// The largest table `EFIIOC_GET_TABLE` copies (`EFI_TABLE_ALLOC_MAX`).
pub const EFI_TABLE_ALLOC_MAX: u64 = 0x80_0000;

/// A table `EFIIOC_GET_TABLE` knows how to size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnownTable {
    /// `EFI_TABLE_ESRT`.
    Esrt,
    /// `EFI_PROPERTIES_TABLE`.
    Properties,
    /// `EFI_MEMORY_ATTRIBUTES_TABLE`.
    MemoryAttributes,
}

impl KnownTable {
    /// The table `guid` names, if `EFIIOC_GET_TABLE` copies it.
    #[must_use]
    pub fn of(guid: EfiGuid) -> Option<Self> {
        match guid {
            g if g == EFI_TABLE_ESRT => Some(Self::Esrt),
            g if g == EFI_PROPERTIES_TABLE => Some(Self::Properties),
            g if g == EFI_MEMORY_ATTRIBUTES_TABLE => Some(Self::MemoryAttributes),
            _ => None,
        }
    }

    /// Bytes of the table's header, which [`KnownTable::length`] reads.
    #[must_use]
    pub const fn header_len(self) -> u64 {
        match self {
            Self::Esrt => EFI_ESRT_TABLE_LEN,
            Self::Properties => EFI_PROP_TABLE_LEN,
            Self::MemoryAttributes => EFI_MEMORY_ATTRIBUTE_TABLE_LEN,
        }
    }

    /// The whole table's length from its header (`get_table_length`).
    ///
    /// # Errors
    ///
    /// `ENODEV` for an ESRT of another version, `EINVAL` for a memory
    /// attributes table other than version 2, `ENOMEM` for a table over
    /// [`EFI_TABLE_ALLOC_MAX`], as FreeBSD.
    pub fn length(self, header: &[u8; 16]) -> Result<u64, Errno> {
        let u32_at = |at: usize| {
            u64::from(u32::from_ne_bytes([
                header[at],
                header[at + 1],
                header[at + 2],
                header[at + 3],
            ]))
        };
        match self {
            Self::Esrt => {
                if u64_at(header, 8) != ESRT_FIRMWARE_RESOURCE_VERSION {
                    return Err(Errno::ENODEV);
                }
                let count = u32_at(0);
                if count > EFI_TABLE_ALLOC_MAX / EFI_ESRT_ENTRY_V1_LEN {
                    return Err(Errno::ENOMEM);
                }
                Ok(EFI_ESRT_TABLE_LEN + count * EFI_ESRT_ENTRY_V1_LEN)
            }
            Self::Properties => {
                let len = u32_at(4);
                if len > EFI_TABLE_ALLOC_MAX {
                    return Err(Errno::ENOMEM);
                }
                Ok(len)
            }
            Self::MemoryAttributes => {
                if u32_at(0) != 2 {
                    return Err(Errno::EINVAL);
                }
                // Both factors are 32-bit, so the product fits.
                let len = EFI_MEMORY_ATTRIBUTE_TABLE_LEN + u32_at(8) * u32_at(4);
                if len > EFI_TABLE_ALLOC_MAX {
                    return Err(Errno::ENOMEM);
                }
                Ok(len)
            }
        }
    }
}

/// `efi_status2err`: the error number for each UEFI status code, by
/// code (the status without its two high bits).
const STATUS_ERRNO: [Option<Errno>; 25] = [
    None,                       // EFI_SUCCESS
    Some(Errno::ENOEXEC),       // EFI_LOAD_ERROR
    Some(Errno::EINVAL),        // EFI_INVALID_PARAMETER
    Some(Errno::ENOSYS),        // EFI_UNSUPPORTED
    Some(Errno::EMSGSIZE),      // EFI_BAD_BUFFER_SIZE
    Some(Errno::EOVERFLOW),     // EFI_BUFFER_TOO_SMALL
    Some(Errno::EBUSY),         // EFI_NOT_READY
    Some(Errno::EIO),           // EFI_DEVICE_ERROR
    Some(Errno::EROFS),         // EFI_WRITE_PROTECTED
    Some(Errno::EAGAIN),        // EFI_OUT_OF_RESOURCES
    Some(Errno::EIO),           // EFI_VOLUME_CORRUPTED
    Some(Errno::ENOSPC),        // EFI_VOLUME_FULL
    Some(Errno::ENXIO),         // EFI_NO_MEDIA
    Some(Errno::ESTALE),        // EFI_MEDIA_CHANGED
    Some(Errno::ENOENT),        // EFI_NOT_FOUND
    Some(Errno::EACCES),        // EFI_ACCESS_DENIED
    Some(Errno::ETIMEDOUT),     // EFI_NO_RESPONSE
    Some(Errno::EADDRNOTAVAIL), // EFI_NO_MAPPING
    Some(Errno::ETIMEDOUT),     // EFI_TIMEOUT
    Some(Errno::EDOOFUS),       // EFI_NOT_STARTED
    Some(Errno::EALREADY),      // EFI_ALREADY_STARTED
    Some(Errno::ECANCELED),     // EFI_ABORTED
    Some(Errno::EPROTO),        // EFI_ICMP_ERROR
    Some(Errno::EPROTO),        // EFI_TFTP_ERROR
    Some(Errno::EPROTO),        // EFI_PROTOCOL_ERROR
];

/// `efi_status_to_errno`: what a UEFI status returns. The two high bits
/// (error, and reserved for OEMs) are dropped first, so a warning maps as
/// the error with its code does, and a code past the table is `EDOOFUS`.
///
/// # Errors
///
/// The error number for any status but `EFI_SUCCESS`.
pub fn status_to_errno(status: u64) -> Result<(), Errno> {
    let code = status & 0x3fff_ffff_ffff_ffff;
    let errno = usize::try_from(code)
        .ok()
        .and_then(|c| STATUS_ERRNO.get(c))
        .map_or(Some(Errno::EDOOFUS), |e| *e);
    errno.map_or(Ok(()), Err)
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("EFIIOC_GET_TABLE", EFIIOC_GET_TABLE),
    c("EFIIOC_GET_TIME", EFIIOC_GET_TIME),
    c("EFIIOC_SET_TIME", EFIIOC_SET_TIME),
    c("EFIIOC_VAR_GET", EFIIOC_VAR_GET),
    c("EFIIOC_VAR_NEXT", EFIIOC_VAR_NEXT),
    c("EFIIOC_VAR_SET", EFIIOC_VAR_SET),
    c("EFIIOC_GET_WAKETIME", EFIIOC_GET_WAKETIME),
    c("EFIIOC_SET_WAKETIME", EFIIOC_SET_WAKETIME),
    ConstValue {
        c_name: "ESRT_FIRMWARE_RESOURCE_VERSION",
        header: "sys/efi.h",
        value: ESRT_FIRMWARE_RESOURCE_VERSION as i64,
        arches: Arches::All,
    },
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/efiio.h",
        value: value as i64,
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERR: u64 = 1 << 63;

    #[test]
    fn statuses_map_as_freebsd() {
        assert_eq!(status_to_errno(0), Ok(()));
        assert_eq!(status_to_errno(ERR | 2), Err(Errno::EINVAL));
        assert_eq!(status_to_errno(ERR | 3), Err(Errno::ENOSYS));
        assert_eq!(status_to_errno(ERR | 5), Err(Errno::EOVERFLOW));
        assert_eq!(status_to_errno(ERR | 8), Err(Errno::EROFS));
        assert_eq!(status_to_errno(ERR | 9), Err(Errno::EAGAIN));
        assert_eq!(status_to_errno(ERR | 14), Err(Errno::ENOENT));
        assert_eq!(status_to_errno(ERR | 15), Err(Errno::EACCES));
        assert_eq!(status_to_errno(ERR | 24), Err(Errno::EPROTO));
        // Past the table, and with the OEM bit.
        assert_eq!(status_to_errno(ERR | 25), Err(Errno::EDOOFUS));
        assert_eq!(status_to_errno(ERR | 1 << 62 | 14), Err(Errno::ENOENT));
        // A warning is its code's error, as in efi_status_to_errno.
        assert_eq!(status_to_errno(1), Err(Errno::ENOEXEC));
        assert_eq!(status_to_errno(u64::MAX), Err(Errno::EDOOFUS));
    }

    #[test]
    fn var_ioctl_round_trips_at_freebsds_offsets() {
        let mut b = [0xa5u8; EFI_VAR_IOCTL_LEN];
        let v = EfiVarIoctl {
            name: 0x1000,
            namesize: 18,
            vendor: EFI_TABLE_ESRT,
            attrib: 7,
            data: 0x2000,
            datasize: 4,
        };
        v.store(&mut b);
        assert_eq!(EfiVarIoctl::from_bytes(&b), Some(v));
        assert_eq!(&b[0..8], &0x1000u64.to_ne_bytes());
        assert_eq!(&b[32..36], &7u32.to_ne_bytes());
        // The padding after `attrib` is untouched.
        assert_eq!(&b[36..40], &[0xa5; 4]);
        assert_eq!(&b[48..56], &4u64.to_ne_bytes());
        assert_eq!(EfiVarIoctl::from_bytes(&b[1..]), None);
        assert_eq!(core::mem::offset_of!(EfiVarIoctl, vendor), 16);
        assert_eq!(core::mem::offset_of!(EfiVarIoctl, data), 40);
    }

    #[test]
    fn table_ioctl_round_trips() {
        let mut b = [0u8; EFI_GET_TABLE_IOCTL_LEN];
        let t = EfiGetTableIoctl {
            buf: 0x4000,
            guid: EFI_MEMORY_ATTRIBUTES_TABLE,
            table_len: 9,
            buf_len: 64,
        };
        t.store(&mut b);
        assert_eq!(EfiGetTableIoctl::from_bytes(&b), Some(t));
        assert_eq!(core::mem::offset_of!(EfiGetTableIoctl, table_len), 24);
    }

    #[test]
    fn guid_bytes_are_the_uefi_encoding() {
        let b = EFI_TABLE_ESRT.to_bytes();
        assert_eq!(b[..4], 0xb122_a263u32.to_le_bytes());
        assert_eq!(EfiGuid::from_bytes(&b), EFI_TABLE_ESRT);
    }

    #[test]
    fn time_round_trips() {
        let t = EfiTm {
            tm_year: 2026,
            tm_mon: 11,
            tm_mday: 10,
            tm_hour: 8,
            tm_min: 30,
            tm_sec: 5,
            tm_nsec: 7,
            tm_tz: 2047,
            ..EfiTm::default()
        };
        assert_eq!(EfiTm::from_bytes(&t.to_bytes()), Some(t));
        assert_eq!(EfiTm::from_bytes(&[0; 15]), None);
    }

    #[test]
    fn requests_encode_freebsds_values() {
        // _IOWR('E', 4, struct efi_var_ioctl) on LP64.
        assert_eq!(EFIIOC_VAR_GET, 0xc038_4504);
        assert_eq!(EFIIOC_GET_TIME, 0x4010_4502);
        assert_eq!(EFIIOC_GET_TABLE, 0xc028_4501);
        assert_eq!(EFIIOC_SET_WAKETIME, 0x8014_4508);
    }

    #[test]
    fn table_lengths_are_bounded() {
        let mut h = [0u8; 16];
        // ESRT: two entries, version 1.
        h[0..4].copy_from_slice(&2u32.to_ne_bytes());
        h[8..16].copy_from_slice(&1u64.to_ne_bytes());
        assert_eq!(KnownTable::Esrt.length(&h), Ok(16 + 2 * 40));
        h[8..16].copy_from_slice(&2u64.to_ne_bytes());
        assert_eq!(KnownTable::Esrt.length(&h), Err(Errno::ENODEV));
        h[8..16].copy_from_slice(&1u64.to_ne_bytes());
        h[0..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert_eq!(KnownTable::Esrt.length(&h), Err(Errno::ENOMEM));
        // Memory attributes: version 2, u32::MAX descriptors of u32::MAX.
        let mut m = [0u8; 16];
        m[0..4].copy_from_slice(&2u32.to_ne_bytes());
        m[4..8].copy_from_slice(&u32::MAX.to_ne_bytes());
        m[8..12].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert_eq!(KnownTable::MemoryAttributes.length(&m), Err(Errno::ENOMEM));
        m[4..8].copy_from_slice(&3u32.to_ne_bytes());
        m[8..12].copy_from_slice(&48u32.to_ne_bytes());
        assert_eq!(KnownTable::MemoryAttributes.length(&m), Ok(16 + 3 * 48));
        m[0..4].copy_from_slice(&1u32.to_ne_bytes());
        assert_eq!(KnownTable::MemoryAttributes.length(&m), Err(Errno::EINVAL));
        assert_eq!(
            KnownTable::of(EFI_PROPERTIES_TABLE),
            Some(KnownTable::Properties)
        );
        assert_eq!(KnownTable::of(EfiGuid::default()), None);
    }
}
