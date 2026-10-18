// SPDX-License-Identifier: BSD-2-Clause
//! GUID Partition Table (UEFI 2.10 §5.3): writing a fresh table and reading
//! one back with full checksum validation.
//!
//! Only 512-byte logical sectors are supported for now; 4Kn disks come with
//! the storage stack.

#![no_std]
#![forbid(unsafe_code)]

use crc::crc32;
pub use guid::Guid;

/// Logical sector size handled by this crate.
pub const SECTOR: usize = 512;
/// Partition entries in a table, the UEFI minimum array size.
pub const ENTRY_COUNT: u32 = 128;
/// Size of one partition entry.
pub const ENTRY_SIZE: u32 = 128;
/// Sectors occupied by the partition entry array.
const ENTRY_SECTORS: u64 = (ENTRY_COUNT as u64 * ENTRY_SIZE as u64) / SECTOR as u64;
const HEADER_SIZE: u32 = 92;
const SIGNATURE: &[u8; 8] = b"EFI PART";
const REVISION_1_0: u32 = 0x0001_0000;
/// Partition names are at most 36 UTF-16 code units.
pub const NAME_UNITS: usize = 36;

/// Well-known partition type GUIDs.
pub mod types {
    use super::Guid;

    /// EFI System Partition.
    pub const EFI_SYSTEM: Guid = Guid::must_parse("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
    /// FreeBSD UFS / UFS2.
    pub const FREEBSD_UFS: Guid = Guid::must_parse("516E7CB6-6ECF-11D6-8FF8-00022D09712B");
    /// FreeBSD swap.
    pub const FREEBSD_SWAP: Guid = Guid::must_parse("516E7CB5-6ECF-11D6-8FF8-00022D09712B");
}

/// One partition to create.
#[derive(Clone, Copy, Debug)]
pub struct PartitionSpec<'a> {
    /// What the partition holds (see [`types`]).
    pub type_guid: Guid,
    /// This partition's own identity.
    pub unique_guid: Guid,
    /// First sector, inclusive.
    pub first_lba: u64,
    /// Last sector, inclusive.
    pub last_lba: u64,
    /// UEFI and type-specific attribute bits.
    pub attributes: u64,
    /// Human-readable label (the GPT "partition name").
    pub name: &'a str,
}

/// A partition read back from a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// Index in the entry array.
    pub index: u32,
    /// What the partition holds.
    pub type_guid: Guid,
    /// This partition's own identity.
    pub unique_guid: Guid,
    /// First sector, inclusive.
    pub first_lba: u64,
    /// Last sector, inclusive.
    pub last_lba: u64,
    /// Attribute bits.
    pub attributes: u64,
    /// Name in UTF-16, padded with zeros.
    pub name: [u16; NAME_UNITS],
}

impl Partition {
    /// Number of sectors in the partition.
    pub const fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }

    /// Writes the name, decoded from UTF-16, into `f`.
    pub fn write_name(&self, f: &mut impl core::fmt::Write) -> core::fmt::Result {
        let units = self.name.iter().copied().take_while(|&u| u != 0);
        for c in char::decode_utf16(units) {
            f.write_char(c.unwrap_or(char::REPLACEMENT_CHARACTER))?;
        }
        Ok(())
    }
}

/// Why a table could not be written or read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The buffer is not a whole number of sectors, or too small for two
    /// tables and any usable space.
    DiskSize,
    /// More partitions than [`ENTRY_COUNT`].
    TooManyPartitions,
    /// A partition lies outside the usable range or ends before it starts.
    OutOfRange { index: usize },
    /// Two partitions share sectors.
    Overlap { first: usize, second: usize },
    /// A name is longer than [`NAME_UNITS`] UTF-16 code units.
    NameTooLong { index: usize },
    /// No `EFI PART` signature in the primary header.
    Signature,
    /// The header's own CRC does not match.
    HeaderCrc,
    /// The partition entry array's CRC does not match.
    EntriesCrc,
    /// A header field holds a value this crate does not support.
    Unsupported,
}

/// The range of sectors partitions may occupy on a disk of `sectors` sectors.
pub fn usable_range(sectors: u64) -> Option<(u64, u64)> {
    let first = 2 + ENTRY_SECTORS;
    let last = sectors.checked_sub(2 + ENTRY_SECTORS)?;
    (first <= last).then_some((first, last))
}

/// Rounds `lba` up to a multiple of `align` sectors.
pub const fn align_up(lba: u64, align: u64) -> u64 {
    lba.div_ceil(align) * align
}

/// Writes a protective MBR, primary and backup GPT headers, and both entry
/// arrays into `disk`, which is the whole disk image.
///
/// Sectors outside those structures are left untouched.
pub fn write(disk: &mut [u8], disk_guid: Guid, parts: &[PartitionSpec<'_>]) -> Result<(), Error> {
    if !disk.len().is_multiple_of(SECTOR) {
        return Err(Error::DiskSize);
    }
    let sectors = u64::try_from(disk.len() / SECTOR).map_err(|_| Error::DiskSize)?;
    let (first_usable, last_usable) = usable_range(sectors).ok_or(Error::DiskSize)?;
    validate(parts, first_usable, last_usable)?;

    let mut entries = [0u8; (ENTRY_COUNT * ENTRY_SIZE) as usize];
    for (i, p) in parts.iter().enumerate() {
        let e = &mut entries[i * ENTRY_SIZE as usize..][..ENTRY_SIZE as usize];
        e[0..16].copy_from_slice(&p.type_guid.to_uefi_bytes());
        e[16..32].copy_from_slice(&p.unique_guid.to_uefi_bytes());
        e[32..40].copy_from_slice(&p.first_lba.to_le_bytes());
        e[40..48].copy_from_slice(&p.last_lba.to_le_bytes());
        e[48..56].copy_from_slice(&p.attributes.to_le_bytes());
        for (j, unit) in p.name.encode_utf16().enumerate() {
            e[56 + 2 * j..][..2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let entries_crc = crc32(&entries);

    write_protective_mbr(disk, sectors);

    let backup_entries_lba = sectors - 1 - ENTRY_SECTORS;
    let tables = [
        // (header LBA, alternate header LBA, entry array LBA)
        (1, sectors - 1, 2),
        (sectors - 1, 1, backup_entries_lba),
    ];
    for (my_lba, alt_lba, entries_lba) in tables {
        let header = Header {
            my_lba,
            alternate_lba: alt_lba,
            first_usable,
            last_usable,
            disk_guid,
            entries_lba,
            entries_crc,
        };
        sector_mut(disk, my_lba)[..HEADER_SIZE as usize].copy_from_slice(&header.encode());
        let start = lba_offset(entries_lba);
        disk[start..start + entries.len()].copy_from_slice(&entries);
    }
    Ok(())
}

fn validate(parts: &[PartitionSpec<'_>], first: u64, last: u64) -> Result<(), Error> {
    if parts.len() > ENTRY_COUNT as usize {
        return Err(Error::TooManyPartitions);
    }
    for (i, p) in parts.iter().enumerate() {
        if p.first_lba < first || p.last_lba > last || p.first_lba > p.last_lba {
            return Err(Error::OutOfRange { index: i });
        }
        if p.name.encode_utf16().count() > NAME_UNITS {
            return Err(Error::NameTooLong { index: i });
        }
        for (j, q) in parts.iter().enumerate().skip(i + 1) {
            if p.first_lba <= q.last_lba && q.first_lba <= p.last_lba {
                return Err(Error::Overlap {
                    first: i,
                    second: j,
                });
            }
        }
    }
    Ok(())
}

fn write_protective_mbr(disk: &mut [u8], sectors: u64) {
    let mbr = sector_mut(disk, 0);
    let entry = &mut mbr[446..462];
    entry.fill(0);
    entry[1..4].copy_from_slice(&[0x00, 0x02, 0x00]); // CHS of LBA 1
    entry[4] = 0xEE; // GPT protective
    entry[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    let size = u32::try_from(sectors - 1).unwrap_or(u32::MAX);
    entry[12..16].copy_from_slice(&size.to_le_bytes());
    mbr[462..510].fill(0);
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
}

struct Header {
    my_lba: u64,
    alternate_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: Guid,
    entries_lba: u64,
    entries_crc: u32,
}

impl Header {
    fn encode(&self) -> [u8; HEADER_SIZE as usize] {
        let mut h = [0u8; HEADER_SIZE as usize];
        h[0..8].copy_from_slice(SIGNATURE);
        h[8..12].copy_from_slice(&REVISION_1_0.to_le_bytes());
        h[12..16].copy_from_slice(&HEADER_SIZE.to_le_bytes());
        // 16..20 header CRC, filled in below; 20..24 reserved.
        h[24..32].copy_from_slice(&self.my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&self.alternate_lba.to_le_bytes());
        h[40..48].copy_from_slice(&self.first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&self.last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&self.disk_guid.to_uefi_bytes());
        h[72..80].copy_from_slice(&self.entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&ENTRY_COUNT.to_le_bytes());
        h[84..88].copy_from_slice(&ENTRY_SIZE.to_le_bytes());
        h[88..92].copy_from_slice(&self.entries_crc.to_le_bytes());
        let crc = crc32(&h);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    }
}

/// A validated table borrowed from a disk image.
#[derive(Debug)]
pub struct Table<'a> {
    /// The disk's GUID.
    pub disk_guid: Guid,
    /// First sector partitions may use.
    pub first_usable: u64,
    /// Last sector partitions may use.
    pub last_usable: u64,
    entries: &'a [u8],
    entry_size: usize,
}

impl<'a> Table<'a> {
    /// Reads and validates the primary table of `disk`.
    pub fn read(disk: &'a [u8]) -> Result<Self, Error> {
        if disk.len() < lba_offset(2) {
            return Err(Error::DiskSize);
        }
        let h = &disk[SECTOR..SECTOR + HEADER_SIZE as usize];
        if &h[0..8] != SIGNATURE {
            return Err(Error::Signature);
        }
        if le32(h, 12) != HEADER_SIZE {
            return Err(Error::Unsupported);
        }
        let mut copy = [0u8; HEADER_SIZE as usize];
        copy.copy_from_slice(h);
        copy[16..20].fill(0);
        if crc32(&copy) != le32(h, 16) {
            return Err(Error::HeaderCrc);
        }
        let entries_lba = le64(h, 72);
        let count = usize::try_from(le32(h, 80)).map_err(|_| Error::Unsupported)?;
        let entry_size = usize::try_from(le32(h, 84)).map_err(|_| Error::Unsupported)?;
        if entry_size < 128 || !entry_size.is_power_of_two() {
            return Err(Error::Unsupported);
        }
        let start = usize::try_from(entries_lba)
            .ok()
            .and_then(|lba| lba.checked_mul(SECTOR))
            .ok_or(Error::DiskSize)?;
        let len = count.checked_mul(entry_size).ok_or(Error::Unsupported)?;
        let entries = disk.get(start..start + len).ok_or(Error::DiskSize)?;
        if crc32(entries) != le32(h, 88) {
            return Err(Error::EntriesCrc);
        }
        let mut guid = [0u8; 16];
        guid.copy_from_slice(&h[56..72]);
        Ok(Self {
            disk_guid: Guid::from_uefi_bytes(guid),
            first_usable: le64(h, 40),
            last_usable: le64(h, 48),
            entries,
            entry_size,
        })
    }

    /// The used partition entries, in table order.
    pub fn partitions(&self) -> impl Iterator<Item = Partition> + '_ {
        self.entries
            .chunks_exact(self.entry_size)
            .zip(0u32..)
            .filter_map(|(e, index)| {
                let mut type_bytes = [0u8; 16];
                type_bytes.copy_from_slice(&e[0..16]);
                let type_guid = Guid::from_uefi_bytes(type_bytes);
                if type_guid == Guid::NIL {
                    return None;
                }
                let mut unique = [0u8; 16];
                unique.copy_from_slice(&e[16..32]);
                let mut name = [0u16; NAME_UNITS];
                for (j, unit) in name.iter_mut().enumerate() {
                    *unit = u16::from_le_bytes([e[56 + 2 * j], e[57 + 2 * j]]);
                }
                Some(Partition {
                    index,
                    type_guid,
                    unique_guid: Guid::from_uefi_bytes(unique),
                    first_lba: le64(e, 32),
                    last_lba: le64(e, 40),
                    attributes: le64(e, 48),
                    name,
                })
            })
    }
}

fn lba_offset(lba: u64) -> usize {
    // Callers only pass LBAs inside a buffer that already exists in memory.
    usize::try_from(lba).expect("LBA fits in usize") * SECTOR
}

fn sector_mut(disk: &mut [u8], lba: u64) -> &mut [u8] {
    let start = lba_offset(lba);
    &mut disk[start..start + SECTOR]
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{string::String, vec};

    fn disk(mib: usize) -> std::vec::Vec<u8> {
        vec![0u8; mib << 20]
    }

    fn esp(first: u64, last: u64) -> PartitionSpec<'static> {
        PartitionSpec {
            type_guid: types::EFI_SYSTEM,
            unique_guid: Guid::v8_from_bytes([1; 16]),
            first_lba: first,
            last_lba: last,
            attributes: 0,
            name: "efi",
        }
    }

    #[test]
    fn round_trip() {
        let mut d = disk(64);
        let disk_guid = Guid::v8_from_bytes([7; 16]);
        let ufs = PartitionSpec {
            type_guid: types::FREEBSD_UFS,
            unique_guid: Guid::v8_from_bytes([2; 16]),
            first_lba: 67584,
            last_lba: 100000,
            attributes: 0,
            name: "rootfs",
        };
        write(&mut d, disk_guid, &[esp(2048, 67583), ufs]).unwrap();

        let t = Table::read(&d).unwrap();
        assert_eq!(t.disk_guid, disk_guid);
        assert_eq!(t.first_usable, 34);
        assert_eq!(t.last_usable, (64 << 11) - 34);
        let parts: std::vec::Vec<_> = t.partitions().collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].type_guid, types::EFI_SYSTEM);
        assert_eq!(parts[0].sectors(), 65536);
        assert_eq!(parts[1].index, 1);
        let mut name = String::new();
        parts[1].write_name(&mut name).unwrap();
        assert_eq!(name, "rootfs");
    }

    #[test]
    fn protective_mbr_and_backup_header() {
        let mut d = disk(8);
        write(&mut d, Guid::NIL, &[esp(2048, 4096)]).unwrap();
        assert_eq!(&d[510..512], &[0x55, 0xAA]);
        assert_eq!(d[446 + 4], 0xEE);
        let last = d.len() - SECTOR;
        assert_eq!(&d[last..last + 8], SIGNATURE);
        // The backup header points back at the primary.
        assert_eq!(le64(&d[last..], 32), 1);
    }

    #[test]
    fn detects_corruption() {
        let mut d = disk(8);
        write(&mut d, Guid::NIL, &[esp(2048, 4096)]).unwrap();
        d[2 * SECTOR + 40] ^= 1;
        assert_eq!(Table::read(&d).unwrap_err(), Error::EntriesCrc);
        d[2 * SECTOR + 40] ^= 1;
        d[SECTOR + 40] ^= 1;
        assert_eq!(Table::read(&d).unwrap_err(), Error::HeaderCrc);
    }

    #[test]
    fn rejects_bad_layouts() {
        let mut d = disk(8);
        assert_eq!(
            write(&mut d, Guid::NIL, &[esp(10, 100)]),
            Err(Error::OutOfRange { index: 0 })
        );
        assert_eq!(
            write(&mut d, Guid::NIL, &[esp(2048, 3000), esp(3000, 4000)]),
            Err(Error::Overlap {
                first: 0,
                second: 1
            })
        );
        let long = PartitionSpec {
            name: "0123456789012345678901234567890123456",
            ..esp(2048, 3000)
        };
        assert_eq!(
            write(&mut d, Guid::NIL, &[long]),
            Err(Error::NameTooLong { index: 0 })
        );
        let mut odd = vec![0u8; 1000];
        assert_eq!(write(&mut odd, Guid::NIL, &[]), Err(Error::DiskSize));
    }
}
