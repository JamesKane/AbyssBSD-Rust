// SPDX-License-Identifier: BSD-2-Clause
//! Reading files from a FAT32 volume held in memory.

use alloc::vec::Vec;

use crate::{ATTR_DIRECTORY, ATTR_VOLUME_ID, DIR_ENTRY, Error, SECTOR, ShortName};

/// Long-name entries carry this attribute combination.
const ATTR_LONG_NAME: u8 = 0x0F;

/// A FAT32 volume borrowed from memory.
#[derive(Debug)]
pub struct Volume<'a> {
    data: &'a [u8],
    sectors_per_cluster: u32,
    fat_start: usize,
    data_start: usize,
    root_cluster: u32,
    cluster_count: u32,
}

#[derive(Clone, Copy)]
struct Entry {
    name: ShortName,
    attr: u8,
    cluster: u32,
    size: u32,
}

impl<'a> Volume<'a> {
    /// Parses the boot sector of `data`.
    pub fn open(data: &'a [u8]) -> Result<Self, Error> {
        let b = data.get(..SECTOR).ok_or(Error::NotFat32)?;
        if b[510..512] != [0x55, 0xAA] || &b[82..90] != b"FAT32   " {
            return Err(Error::NotFat32);
        }
        if u16::from_le_bytes([b[11], b[12]]) != 512 {
            return Err(Error::NotFat32);
        }
        let sectors_per_cluster = u32::from(b[13]);
        let reserved = u32::from(u16::from_le_bytes([b[14], b[15]]));
        let fats = u32::from(b[16]);
        let total = le32(b, 32);
        let fat_sectors = le32(b, 36);
        if sectors_per_cluster == 0 || fats == 0 || fat_sectors == 0 {
            return Err(Error::NotFat32);
        }
        let data_sector = reserved + fats * fat_sectors;
        let cluster_count = total.saturating_sub(data_sector) / sectors_per_cluster;
        Ok(Self {
            data,
            sectors_per_cluster,
            fat_start: reserved as usize * SECTOR,
            data_start: data_sector as usize * SECTOR,
            root_cluster: le32(b, 44),
            cluster_count,
        })
    }

    /// The label stored in the root directory, if any.
    pub fn label(&self) -> Option<[u8; 11]> {
        let root = self.read_chain(self.root_cluster, None).ok()?;
        entries(&root)
            .find(|e| e.attr & ATTR_VOLUME_ID != 0)
            .map(|e| *e.name.as_bytes())
    }

    /// Reads the whole file at a `/`-separated path. Matching ignores case.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, Error> {
        let mut dir = self.root_cluster;
        let mut parts = path.split('/').filter(|p| !p.is_empty()).peekable();
        while let Some(part) = parts.next() {
            let want = ShortName::new(part).map_err(|_| Error::NotFound)?;
            let listing = self.read_chain(dir, None)?;
            let found = entries(&listing)
                .find(|e| e.attr & ATTR_VOLUME_ID == 0 && e.name == want)
                .ok_or(Error::NotFound)?;
            let is_dir = found.attr & ATTR_DIRECTORY != 0;
            if parts.peek().is_none() {
                if is_dir {
                    return Err(Error::NotFound);
                }
                if found.cluster == 0 {
                    return Ok(Vec::new());
                }
                return self.read_chain(found.cluster, Some(found.size as usize));
            }
            if !is_dir {
                return Err(Error::NotADirectory);
            }
            dir = found.cluster;
        }
        Err(Error::NotFound)
    }

    fn fat_entry(&self, cluster: u32) -> Result<u32, Error> {
        let at = self.fat_start + cluster as usize * 4;
        let raw = self.data.get(at..at + 4).ok_or(Error::Corrupt)?;
        Ok(le32(raw, 0) & 0x0FFF_FFFF)
    }

    /// Follows a cluster chain, reading at most `limit` bytes if given.
    fn read_chain(&self, first: u32, limit: Option<usize>) -> Result<Vec<u8>, Error> {
        let cluster_bytes = self.sectors_per_cluster as usize * SECTOR;
        let mut out = Vec::new();
        let mut cluster = first;
        // A chain longer than the volume has clusters must loop.
        for _ in 0..=self.cluster_count {
            if cluster < 2 || cluster >= self.cluster_count + 2 {
                return Err(Error::Corrupt);
            }
            let at = self.data_start + (cluster as usize - 2) * cluster_bytes;
            let chunk = self
                .data
                .get(at..at + cluster_bytes)
                .ok_or(Error::Corrupt)?;
            out.extend_from_slice(chunk);
            if limit.is_some_and(|l| out.len() >= l) {
                break;
            }
            let next = self.fat_entry(cluster)?;
            if next >= 0x0FFF_FFF8 {
                break;
            }
            cluster = next;
        }
        if let Some(l) = limit {
            if out.len() < l {
                return Err(Error::Corrupt);
            }
            out.truncate(l);
        }
        Ok(out)
    }
}

fn entries(listing: &[u8]) -> impl Iterator<Item = Entry> + '_ {
    listing
        .as_chunks::<DIR_ENTRY>()
        .0
        .iter()
        .take_while(|e| e[0] != 0x00)
        .filter(|e| e[0] != 0xE5 && e[11] != ATTR_LONG_NAME)
        .map(|e| {
            let mut name = [0u8; 11];
            name.copy_from_slice(&e[0..11]);
            let hi = u32::from(u16::from_le_bytes([e[20], e[21]]));
            let lo = u32::from(u16::from_le_bytes([e[26], e[27]]));
            Entry {
                name: ShortName::from_raw(name),
                attr: e[11],
                cluster: (hi << 16) | lo,
                size: le32(e, 28),
            }
        })
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
