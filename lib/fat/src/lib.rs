// SPDX-License-Identifier: BSD-2-Clause
//! FAT32 volumes: formatting a fresh volume populated from an in-memory tree,
//! and reading files back.
//!
//! This is the image builder's FAT, used for EFI System Partitions. Names are
//! 8.3 short names only (no VFAT long names yet), and files are laid out
//! contiguously. The kernel's msdosfs is a separate, full implementation.
//!
//! Reference: Microsoft "FAT: General Overview of On-Disk Format" 1.03.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod fs;
mod name;
mod read;

use alloc::vec::Vec;

pub use name::{NameError, ShortName, VolumeLabel};
pub use read::Volume;

/// Bytes per logical sector. Only 512 is supported.
pub const SECTOR: usize = 512;
const DIR_ENTRY: usize = 32;
const RESERVED_SECTORS: u16 = 32;
const NUM_FATS: u8 = 2;
const ROOT_CLUSTER: u32 = 2;
const FSINFO_SECTOR: u16 = 1;
const BACKUP_BOOT_SECTOR: u16 = 6;
/// FAT32 needs at least this many clusters; fewer means FAT16 by definition.
const MIN_CLUSTERS: u32 = 65_525;
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;

/// Why formatting or reading failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The volume buffer is not a whole number of sectors.
    NotSectorAligned,
    /// The volume is too small to be FAT32, or too large for this builder.
    VolumeSize,
    /// The tree needs more clusters than the volume has.
    NoSpace,
    /// A path component is not a valid 8.3 name.
    Name(NameError),
    /// A path names an existing entry of the wrong kind.
    NotADirectory,
    /// A path names a file that already exists.
    Exists,
    /// A file is 4 GiB or larger.
    FileTooLarge,
    /// The volume does not contain a FAT32 boot sector this crate understands.
    NotFat32,
    /// A cluster chain is broken or loops.
    Corrupt,
    /// The requested path does not exist.
    NotFound,
}

impl From<NameError> for Error {
    fn from(e: NameError) -> Self {
        Self::Name(e)
    }
}

/// A DOS date and time, as stored in directory entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    /// Calendar year, 1980 to 2107.
    pub year: u16,
    /// Month, 1 to 12.
    pub month: u8,
    /// Day of month, 1 to 31.
    pub day: u8,
    /// Hour, 0 to 23.
    pub hour: u8,
    /// Minute, 0 to 59.
    pub minute: u8,
    /// Second, 0 to 59 (stored with two-second resolution).
    pub second: u8,
}

impl Timestamp {
    /// A fixed timestamp for reproducible images.
    pub const EPOCH: Self = Self {
        year: 2026,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
    };

    fn date(self) -> u16 {
        (self.year.saturating_sub(1980).min(127) << 9)
            | (u16::from(self.month.clamp(1, 12)) << 5)
            | u16::from(self.day.clamp(1, 31))
    }

    fn time(self) -> u16 {
        (u16::from(self.hour.min(23)) << 11)
            | (u16::from(self.minute.min(59)) << 5)
            | u16::from(self.second.min(59) / 2)
    }
}

/// A node in the tree to write.
#[derive(Debug)]
pub enum Node<'a> {
    /// A regular file and its contents.
    File(&'a [u8]),
    /// A directory.
    Dir(Dir<'a>),
}

/// A directory: named children, kept in insertion order.
#[derive(Debug, Default)]
pub struct Dir<'a> {
    entries: Vec<(ShortName, Node<'a>)>,
}

impl<'a> Dir<'a> {
    /// An empty directory.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Adds a file at a `/`-separated path, creating parent directories.
    pub fn insert_file(&mut self, path: &str, data: &'a [u8]) -> Result<(), Error> {
        if u32::try_from(data.len()).is_err() {
            return Err(Error::FileTooLarge);
        }
        let (parents, leaf) = match path.rsplit_once('/') {
            Some((p, l)) => (Some(p), l),
            None => (None, path),
        };
        let dir = match parents {
            Some(p) => self.make_dirs(p)?,
            None => self,
        };
        let name = ShortName::new(leaf)?;
        if dir.entries.iter().any(|(n, _)| *n == name) {
            return Err(Error::Exists);
        }
        dir.entries.push((name, Node::File(data)));
        Ok(())
    }

    /// Creates the directories along a `/`-separated path, like `mkdir -p`.
    pub fn make_dirs(&mut self, path: &str) -> Result<&mut Self, Error> {
        let mut dir = self;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let name = ShortName::new(part)?;
            let pos = match dir.entries.iter().position(|(n, _)| *n == name) {
                Some(pos) => pos,
                None => {
                    dir.entries.push((name, Node::Dir(Dir::new())));
                    dir.entries.len() - 1
                }
            };
            dir = match &mut dir.entries[pos].1 {
                Node::Dir(d) => d,
                Node::File(_) => return Err(Error::NotADirectory),
            };
        }
        Ok(dir)
    }

    /// The directory's children.
    pub fn entries(&self) -> impl Iterator<Item = (&ShortName, &Node<'a>)> {
        self.entries.iter().map(|(n, node)| (n, node))
    }
}

/// Parameters that are not derived from the volume size.
#[derive(Clone, Copy, Debug)]
pub struct FormatOptions {
    /// Volume serial number.
    pub volume_id: u32,
    /// Volume label.
    pub label: VolumeLabel,
    /// Sectors preceding the volume on its disk (the partition's first LBA).
    pub hidden_sectors: u32,
    /// Timestamp for every directory entry.
    pub timestamp: Timestamp,
}

/// The on-disk geometry chosen for a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    total_sectors: u32,
    sectors_per_cluster: u8,
    fat_sectors: u32,
    cluster_count: u32,
}

impl Geometry {
    fn for_size(total_sectors: u32) -> Result<Self, Error> {
        // Cluster sizes per the Microsoft recommendation for FAT32, but with
        // 512-byte clusters allowed up to 260 MiB so small ESPs stay FAT32.
        let sectors_per_cluster: u8 = match total_sectors {
            0..=532_480 => 1,
            532_481..=16_777_216 => 8,
            16_777_217..=33_554_432 => 16,
            33_554_433..=67_108_864 => 32,
            _ => 64,
        };
        let spc = u32::from(sectors_per_cluster);
        let data_and_fat = total_sectors
            .checked_sub(u32::from(RESERVED_SECTORS))
            .ok_or(Error::VolumeSize)?;
        let per_fat_divisor = (256 * spc + u32::from(NUM_FATS)) / 2;
        let fat_sectors = data_and_fat.div_ceil(per_fat_divisor);
        let data_sectors = data_and_fat
            .checked_sub(u32::from(NUM_FATS) * fat_sectors)
            .ok_or(Error::VolumeSize)?;
        let cluster_count = data_sectors / spc;
        if cluster_count < MIN_CLUSTERS {
            return Err(Error::VolumeSize);
        }
        Ok(Self {
            total_sectors,
            sectors_per_cluster,
            fat_sectors,
            cluster_count,
        })
    }

    fn cluster_bytes(&self) -> usize {
        usize::from(self.sectors_per_cluster) * SECTOR
    }

    fn first_data_sector(&self) -> u32 {
        u32::from(RESERVED_SECTORS) + u32::from(NUM_FATS) * self.fat_sectors
    }

    fn cluster_offset(&self, cluster: u32) -> usize {
        let sector = self.first_data_sector()
            + (cluster - ROOT_CLUSTER) * u32::from(self.sectors_per_cluster);
        sector as usize * SECTOR
    }
}

/// Formats `volume` as FAT32 and writes `root` into it.
///
/// The whole buffer is overwritten. Output depends only on the arguments, so
/// identical inputs give byte-identical images.
pub fn format(volume: &mut [u8], opts: &FormatOptions, root: &Dir<'_>) -> Result<(), Error> {
    if !volume.len().is_multiple_of(SECTOR) {
        return Err(Error::NotSectorAligned);
    }
    let total = u32::try_from(volume.len() / SECTOR).map_err(|_| Error::VolumeSize)?;
    let geo = Geometry::for_size(total)?;
    volume.fill(0);

    let mut writer = Writer {
        volume,
        geo,
        next_cluster: ROOT_CLUSTER,
        ts: opts.timestamp,
    };
    let root_first = writer.write_dir(root, None, Some(opts.label))?;
    debug_assert_eq!(root_first, ROOT_CLUSTER);
    let used = writer.next_cluster - ROOT_CLUSTER;
    let free = geo.cluster_count - used;
    let next_free = writer.next_cluster;

    let boot = boot_sector(&geo, opts);
    let fsinfo = fsinfo_sector(free, next_free);
    for base in [0, usize::from(BACKUP_BOOT_SECTOR)] {
        volume[base * SECTOR..][..SECTOR].copy_from_slice(&boot);
        volume[(base + usize::from(FSINFO_SECTOR)) * SECTOR..][..SECTOR].copy_from_slice(&fsinfo);
        // The third boot sector only carries the signature.
        volume[(base + 2) * SECTOR + 510] = 0x55;
        volume[(base + 2) * SECTOR + 511] = 0xAA;
    }
    // FAT[0] holds the media byte, FAT[1] the clean-shutdown flags.
    for fat in 0..u32::from(NUM_FATS) {
        let start = (u32::from(RESERVED_SECTORS) + fat * geo.fat_sectors) as usize * SECTOR;
        volume[start..start + 4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
        volume[start + 4..start + 8].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
    }
    Ok(())
}

struct Writer<'v> {
    volume: &'v mut [u8],
    geo: Geometry,
    next_cluster: u32,
    ts: Timestamp,
}

impl Writer<'_> {
    /// Allocates `bytes` worth of contiguous clusters (at least one), chains
    /// them in every FAT, and returns the first.
    fn allocate(&mut self, bytes: usize) -> Result<u32, Error> {
        let clusters = u32::try_from(bytes.div_ceil(self.geo.cluster_bytes()).max(1))
            .map_err(|_| Error::NoSpace)?;
        let first = self.next_cluster;
        let end = first.checked_add(clusters).ok_or(Error::NoSpace)?;
        if end - ROOT_CLUSTER > self.geo.cluster_count {
            return Err(Error::NoSpace);
        }
        for c in first..end {
            let next = if c + 1 == end { END_OF_CHAIN } else { c + 1 };
            self.set_fat(c, next);
        }
        self.next_cluster = end;
        Ok(first)
    }

    fn set_fat(&mut self, cluster: u32, value: u32) {
        for fat in 0..u32::from(NUM_FATS) {
            let start = (u32::from(RESERVED_SECTORS) + fat * self.geo.fat_sectors) as usize
                * SECTOR
                + cluster as usize * 4;
            self.volume[start..start + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    /// Writes a directory and, depth-first, everything beneath it. Returns
    /// the directory's first cluster.
    fn write_dir(
        &mut self,
        dir: &Dir<'_>,
        parent: Option<u32>,
        label: Option<VolumeLabel>,
    ) -> Result<u32, Error> {
        let dots = if parent.is_some() { 2 } else { 0 };
        let count = dir.entries.len() + dots + usize::from(label.is_some());
        let first = self.allocate(count * DIR_ENTRY)?;

        let mut records: Vec<[u8; DIR_ENTRY]> = Vec::with_capacity(count);
        if let Some(label) = label {
            records.push(self.entry(label.as_bytes(), ATTR_VOLUME_ID, 0, 0));
        }
        if let Some(parent) = parent {
            records.push(self.entry(b".          ", ATTR_DIRECTORY, first, 0));
            // ".." refers to the root as cluster 0, not its real cluster.
            let up = if parent == ROOT_CLUSTER { 0 } else { parent };
            records.push(self.entry(b"..         ", ATTR_DIRECTORY, up, 0));
        }
        for (name, node) in &dir.entries {
            let record = match node {
                Node::File(data) => {
                    let size = u32::try_from(data.len()).map_err(|_| Error::FileTooLarge)?;
                    let cluster = if data.is_empty() {
                        0
                    } else {
                        self.write_file(data)?
                    };
                    self.entry(name.as_bytes(), ATTR_ARCHIVE, cluster, size)
                }
                Node::Dir(sub) => {
                    let cluster = self.write_dir(sub, Some(first), None)?;
                    self.entry(name.as_bytes(), ATTR_DIRECTORY, cluster, 0)
                }
            };
            records.push(record);
        }

        let mut at = self.geo.cluster_offset(first);
        for r in &records {
            self.volume[at..at + DIR_ENTRY].copy_from_slice(r);
            at += DIR_ENTRY;
        }
        Ok(first)
    }

    fn write_file(&mut self, data: &[u8]) -> Result<u32, Error> {
        let first = self.allocate(data.len())?;
        let at = self.geo.cluster_offset(first);
        self.volume[at..at + data.len()].copy_from_slice(data);
        Ok(first)
    }

    fn entry(&self, name: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; DIR_ENTRY] {
        let mut e = [0u8; DIR_ENTRY];
        let (date, time) = (self.ts.date(), self.ts.time());
        e[0..11].copy_from_slice(name);
        e[11] = attr;
        e[14..16].copy_from_slice(&time.to_le_bytes());
        e[16..18].copy_from_slice(&date.to_le_bytes());
        e[18..20].copy_from_slice(&date.to_le_bytes());
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[22..24].copy_from_slice(&time.to_le_bytes());
        e[24..26].copy_from_slice(&date.to_le_bytes());
        e[26..28].copy_from_slice(&((cluster & 0xFFFF) as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }
}

fn boot_sector(geo: &Geometry, opts: &FormatOptions) -> [u8; SECTOR] {
    let mut b = [0u8; SECTOR];
    b[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    b[3..11].copy_from_slice(b"ABYSSBSD");
    b[11..13].copy_from_slice(&512u16.to_le_bytes());
    b[13] = geo.sectors_per_cluster;
    b[14..16].copy_from_slice(&RESERVED_SECTORS.to_le_bytes());
    b[16] = NUM_FATS;
    b[21] = 0xF8; // fixed media
    b[24..26].copy_from_slice(&63u16.to_le_bytes()); // sectors per track
    b[26..28].copy_from_slice(&255u16.to_le_bytes()); // heads
    b[28..32].copy_from_slice(&opts.hidden_sectors.to_le_bytes());
    b[32..36].copy_from_slice(&geo.total_sectors.to_le_bytes());
    b[36..40].copy_from_slice(&geo.fat_sectors.to_le_bytes());
    b[44..48].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
    b[48..50].copy_from_slice(&FSINFO_SECTOR.to_le_bytes());
    b[50..52].copy_from_slice(&BACKUP_BOOT_SECTOR.to_le_bytes());
    b[64] = 0x80; // drive number
    b[66] = 0x29; // extended boot signature
    b[67..71].copy_from_slice(&opts.volume_id.to_le_bytes());
    b[71..82].copy_from_slice(opts.label.as_bytes());
    b[82..90].copy_from_slice(b"FAT32   ");
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

fn fsinfo_sector(free: u32, next_free: u32) -> [u8; SECTOR] {
    let mut s = [0u8; SECTOR];
    s[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    s[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    s[488..492].copy_from_slice(&free.to_le_bytes());
    s[492..496].copy_from_slice(&next_free.to_le_bytes());
    s[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    s
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    fn opts() -> FormatOptions {
        FormatOptions {
            volume_id: 0x1234_5678,
            label: VolumeLabel::new("ABYSS").unwrap(),
            hidden_sectors: 2048,
            timestamp: Timestamp::EPOCH,
        }
    }

    #[test]
    fn geometry_small_esp_uses_512_byte_clusters() {
        let g = Geometry::for_size(64 << 11).unwrap();
        assert_eq!(g.sectors_per_cluster, 1);
        assert!(g.cluster_count >= MIN_CLUSTERS);
        // The FATs must be able to address every cluster.
        assert!(g.fat_sectors as usize * SECTOR / 4 >= g.cluster_count as usize + 2);
    }

    #[test]
    fn too_small_for_fat32() {
        assert_eq!(Geometry::for_size(16 << 11), Err(Error::VolumeSize));
    }

    #[test]
    fn write_then_read_back() {
        let big: std::vec::Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut root = Dir::new();
        root.insert_file("EFI/BOOT/BOOTX64.EFI", &big).unwrap();
        root.insert_file("EFI/ABYSS/LOADER.CFG", b"kernel=/abyss/kernel\n")
            .unwrap();
        root.insert_file("EMPTY", b"").unwrap();
        let mut vol = vec![0u8; 64 << 20];
        format(&mut vol, &opts(), &root).unwrap();

        let v = Volume::open(&vol).unwrap();
        assert_eq!(v.read_file("EFI/BOOT/BOOTX64.EFI").unwrap(), big);
        assert_eq!(
            v.read_file("efi/abyss/loader.cfg").unwrap(),
            b"kernel=/abyss/kernel\n"
        );
        assert_eq!(v.read_file("EMPTY").unwrap(), b"");
        assert_eq!(v.read_file("EFI/NOPE").unwrap_err(), Error::NotFound);
        assert_eq!(
            v.label(),
            Some(*VolumeLabel::new("ABYSS").unwrap().as_bytes())
        );
    }

    #[test]
    fn output_is_reproducible() {
        let mut root = Dir::new();
        root.insert_file("A/B.TXT", b"hello").unwrap();
        let mut a = vec![0u8; 40 << 20];
        let mut b = vec![0xffu8; 40 << 20];
        format(&mut a, &opts(), &root).unwrap();
        format(&mut b, &opts(), &root).unwrap();
        assert!(a == b);
    }

    #[test]
    fn tree_conflicts() {
        let mut root = Dir::new();
        root.insert_file("A", b"x").unwrap();
        assert_eq!(root.insert_file("A", b"y"), Err(Error::Exists));
        assert_eq!(root.insert_file("A/B", b"y"), Err(Error::NotADirectory));
        assert_eq!(
            root.insert_file("LONGFILENAME.TXT", b"y"),
            Err(Error::Name(NameError::BaseTooLong))
        );
    }
}
