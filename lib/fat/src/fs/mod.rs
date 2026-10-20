// SPDX-License-Identifier: BSD-2-Clause
//! FAT12/16/32 with VFAT long names (docs/VFS.md §10.4): the engine behind
//! the kernel's msdosfs, over [`BlockIo`] as `lib/ufs` is.
//!
//! [`Fat`] holds the geometry, read from the boot sector with FreeBSD's
//! checks; [`Alloc`] the allocation state (the free-cluster map, its
//! count, and the next-free hint), which the caller locks. Files are named
//! by where their directory entry is ([`Loc`]); file data maps cluster by
//! cluster ([`Fat::bmap`]). Names, times, and the entries a file system
//! writes follow FreeBSD's msdosfs, so a FreeBSD system sees what it would
//! have written.
//!
//! Behavior reference: sys/fs/msdosfs (`mountmsdosfs`, `msdosfs_fat.c`,
//! `msdosfs_lookup.c`, `msdosfs_denode.c`, `msdosfs_conv.c`).

pub mod conv;
pub mod dir;
pub mod ops;
mod table;
pub mod time;

#[cfg(test)]
mod tests;

pub use dir::{Entry, Kind, Loc, Name};
pub use table::Alloc;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Io,
    /// Not a FAT file system this engine handles.
    NotFat,
    /// A structure failed its checks.
    Corrupt(&'static str),
    ReadOnly,
    NoSpace,
    Exists,
    NotFound,
    NotEmpty,
    /// A name FAT cannot hold (`EINVAL`).
    BadName,
    NameTooLong,
    /// A file of 4 GiB or more (`EFBIG`).
    TooBig,
}

/// How a changed block goes to the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Sync,
    Delayed,
}

/// What a changed block starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Read,
    Zero,
}

/// Where the bytes come from (the same shape as `lib/ufs`'s).
pub trait BlockIo {
    /// What every call needs (the kernel's `Sleepable`; `()` elsewhere).
    type Cx: ?Sized;
    /// Calls `f` with the `len` bytes at `offset` (sector multiples).
    fn read(
        &self,
        cx: &Self::Cx,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), Error>;
    /// Changes the `len` metadata bytes at `offset` with `f`.
    fn modify(
        &self,
        _cx: &Self::Cx,
        _offset: u64,
        _len: usize,
        _fill: Fill,
        _mode: Mode,
        _f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), Error> {
        Err(Error::ReadOnly)
    }
    /// Writes file data straight to the device.
    fn write_data(&self, _cx: &Self::Cx, _offset: u64, _data: &[u8]) -> Result<(), Error> {
        Err(Error::ReadOnly)
    }
    /// Freed clusters: cached metadata over them must never be written.
    fn forget(&self, _cx: &Self::Cx, _offset: u64, _len: usize) {}
    /// Writes what is delayed and makes it durable.
    fn flush(&self, _cx: &Self::Cx) -> Result<(), Error> {
        Ok(())
    }
}

impl<T: BlockIo + ?Sized> BlockIo for &T {
    type Cx = T::Cx;

    fn read(
        &self,
        cx: &T::Cx,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), Error> {
        (**self).read(cx, offset, len, f)
    }

    fn modify(
        &self,
        cx: &T::Cx,
        offset: u64,
        len: usize,
        fill: Fill,
        mode: Mode,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), Error> {
        (**self).modify(cx, offset, len, fill, mode, f)
    }

    fn write_data(&self, cx: &T::Cx, offset: u64, data: &[u8]) -> Result<(), Error> {
        (**self).write_data(cx, offset, data)
    }

    fn forget(&self, cx: &T::Cx, offset: u64, len: usize) {
        (**self).forget(cx, offset, len);
    }

    fn flush(&self, cx: &T::Cx) -> Result<(), Error> {
        (**self).flush(cx)
    }
}

/// An image in memory.
impl BlockIo for [u8] {
    type Cx = ();

    fn read(&self, _: &(), offset: u64, len: usize, f: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
        let start = usize::try_from(offset).map_err(|_| Error::Io)?;
        f(self
            .get(start..start.checked_add(len).ok_or(Error::Io)?)
            .ok_or(Error::Io)?);
        Ok(())
    }
}

/// The FAT variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

/// `CLUST_FIRST`: the first data cluster.
pub const CLUST_FIRST: u32 = 2;
/// The start cluster that means "the root directory" in a `..` entry
/// (`MSDOSFSROOT`), and "no data" in a file's entry.
pub const ROOT: u32 = 0;

/// A FAT file system's geometry (FreeBSD's `msdosfsmount` fields).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub fat_type: FatType,
    pub bytes_per_sec: u32,
    /// Bytes per cluster.
    pub cluster: u32,
    pub fats: u32,
    /// Where the first FAT starts, and each FAT's size, in bytes.
    pub fat_start: u64,
    pub fat_bytes: u64,
    /// FAT12/16's fixed root directory: where, and how many bytes.
    pub root_start: u64,
    pub root_bytes: u64,
    /// FAT32's root directory cluster.
    pub root_cluster: u32,
    /// Where cluster 2 starts.
    pub data_start: u64,
    /// The highest cluster number (`pm_maxcluster`).
    pub max_cluster: u32,
    /// The FSInfo sector's byte offset, if there is one.
    pub fsinfo: Option<u64>,
    /// Whether every FAT copy is written (else only `active`).
    pub mirror: bool,
    pub active: u32,
    /// The FSInfo next-free hint, as read.
    pub next_free_hint: u32,
    /// The bytes per FAT block read at once (FreeBSD's `pm_fatblocksize`).
    pub fat_block: u32,
    /// The volume serial number, and the volume's total bytes.
    pub serial: u32,
    pub bytes: u64,
}

impl Geometry {
    /// The mask of a FAT entry.
    #[must_use]
    pub fn mask(&self) -> u32 {
        match self.fat_type {
            FatType::Fat12 => 0x0fff,
            FatType::Fat16 => 0xffff,
            FatType::Fat32 => 0x0fff_ffff,
        }
    }

    /// Whether `cn` ends a chain (`MSDOSFSEOF`).
    #[must_use]
    pub fn is_eof(&self, cn: u32) -> bool {
        (cn | !self.mask()) & 0xffff_fff8 == 0xffff_fff8
    }

    /// The byte offset of cluster `cn`.
    #[must_use]
    pub fn cluster_offset(&self, cn: u32) -> u64 {
        self.data_start + u64::from(cn - CLUST_FIRST) * u64::from(self.cluster)
    }

    /// Clusters needed for `bytes`.
    #[must_use]
    pub fn clusters_for(&self, bytes: u64) -> u64 {
        bytes.div_ceil(u64::from(self.cluster))
    }
}

/// A mounted FAT file system: its geometry and the device under it.
#[derive(Debug)]
pub struct Fat<B> {
    io: B,
    geo: Geometry,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl<B: BlockIo> Fat<B> {
    /// Reads the boot sector and checks it as FreeBSD's `mountmsdosfs`
    /// does, then reads FSInfo. `size` is the device's size in bytes.
    pub fn open(cx: &B::Cx, io: B, size: u64) -> Result<Self, Error> {
        let mut boot = alloc::vec![0u8; 512];
        io.read(cx, 0, 512, &mut |b| boot.copy_from_slice(b))
            .map_err(|_| Error::NotFat)?;
        let b = &boot;
        if b[510] != 0x55 || b[511] != 0xaa {
            return Err(Error::NotFat);
        }
        let bytes_per_sec = u32::from(u16_at(b, 11));
        let sec_per_clust = u32::from(b[13]);
        let res_sectors = u32::from(u16_at(b, 14));
        let fats = u32::from(b[16]);
        let root_ents = u32::from(u16_at(b, 17));
        let sectors16 = u32::from(u16_at(b, 19));
        let mut fat_secs = u32::from(u16_at(b, 22));
        let huge = if sectors16 == 0 {
            u32_at(b, 32)
        } else {
            sectors16
        };
        let pow2 = |x: u32| x != 0 && x.is_power_of_two();
        if bytes_per_sec < 512
            || !pow2(bytes_per_sec)
            || !pow2(sec_per_clust)
            || huge == 0
            || fats == 0
        {
            return Err(Error::NotFat);
        }
        let mut fat_type = None;
        let (mut mirror, mut active, mut root_cluster, mut fsinfo_sec) = (true, 0, 0, 0);
        if root_ents == 0 {
            // FAT32.
            if fat_secs != 0 || u16_at(b, 42) != 0 {
                return Err(Error::NotFat);
            }
            fat_type = Some(FatType::Fat32);
            fat_secs = u32_at(b, 36);
            let ext = u16_at(b, 40);
            if ext & 0x80 != 0 {
                mirror = false;
                active = u32::from(ext & 0x0f);
            }
            root_cluster = u32_at(b, 44);
            fsinfo_sec = u32::from(u16_at(b, 48));
        }
        if fat_secs == 0 || sec_per_clust * bytes_per_sec > 64 * 1024 {
            return Err(Error::NotFat);
        }
        let bps = u64::from(bytes_per_sec);
        if u64::from(huge) * bps > size {
            return Err(Error::Corrupt("volume past the end of the device"));
        }
        let fat_start = u64::from(res_sectors) * bps;
        let fat_bytes = u64::from(fat_secs) * bps;
        let (root_start, root_bytes, data_start) = if fat_type == Some(FatType::Fat32) {
            (0, 0, fat_start + u64::from(fats) * fat_bytes)
        } else {
            let rs = fat_start + u64::from(fats) * fat_bytes;
            // In 512-byte blocks, as FreeBSD rounds it.
            let rb = (u64::from(root_ents) * 32).div_ceil(512) * 512;
            (rs, rb, rs + rb)
        };
        let total = u64::from(huge) * bps;
        if total <= data_start {
            return Err(Error::NotFat);
        }
        let cluster = sec_per_clust * bytes_per_sec;
        let mut max_cluster =
            u32::try_from((total - data_start) / u64::from(cluster) + 1).unwrap_or(u32::MAX);
        let fat_type = fat_type.unwrap_or(if max_cluster < 0x0ff6 {
            FatType::Fat12
        } else {
            FatType::Fat16
        });
        // The clusters the FAT can describe.
        let (mult, div) = match fat_type {
            FatType::Fat12 => (3, 2),
            FatType::Fat16 => (2, 1),
            FatType::Fat32 => (4, 1),
        };
        let mask = match fat_type {
            FatType::Fat12 => 0x0fff,
            FatType::Fat16 => 0xffff,
            FatType::Fat32 => 0x0fff_ffffu32,
        };
        let capacity = u32::try_from(fat_bytes / mult * div)
            .unwrap_or(u32::MAX)
            .min(0xffff_fff6 & mask);
        if max_cluster >= capacity {
            max_cluster = capacity - 1;
        }
        let fat_block = match fat_type {
            FatType::Fat12 => 3 * 512u32,
            _ => 4096,
        }
        .next_multiple_of(bytes_per_sec);
        let mut geo = Geometry {
            fat_type,
            bytes_per_sec,
            cluster,
            fats,
            fat_start,
            fat_bytes,
            root_start,
            root_bytes,
            root_cluster,
            data_start,
            max_cluster,
            fsinfo: None,
            mirror,
            active,
            next_free_hint: CLUST_FIRST,
            fat_block,
            serial: if fat_type == FatType::Fat32 {
                u32_at(b, 67)
            } else {
                u32_at(b, 39)
            },
            bytes: total,
        };
        if fsinfo_sec != 0 {
            let at = u64::from(fsinfo_sec) * bps;
            let mut info = alloc::vec![0u8; usize::try_from(bps).unwrap_or(512)];
            io.read(cx, at, info.len(), &mut |b| info.copy_from_slice(b))?;
            if &info[0..4] == b"RRaA"
                && &info[484..488] == b"rrAa"
                && info[508..512] == [0, 0, 0x55, 0xaa]
            {
                geo.fsinfo = Some(at);
                let nxt = u32_at(&info, 492);
                geo.next_free_hint = if nxt > max_cluster || nxt < CLUST_FIRST {
                    CLUST_FIRST
                } else {
                    nxt
                };
            }
        }
        Ok(Self { io, geo })
    }

    #[must_use]
    pub fn geometry(&self) -> &Geometry {
        &self.geo
    }

    #[must_use]
    pub fn io(&self) -> &B {
        &self.io
    }

    /// The disk byte offset of byte `off` of a file whose clusters start at
    /// `start`, and the bytes contiguous from there to the end of its
    /// cluster (FreeBSD's `pcbmap`); `None` past the chain's end.
    pub fn bmap(&self, cx: &B::Cx, start: u32, off: u64) -> Result<Option<(u64, u64)>, Error> {
        let cl = u64::from(self.geo.cluster);
        let Some(cn) = self.chain_nth(cx, start, off / cl)? else {
            return Ok(None);
        };
        let within = off % cl;
        Ok(Some((self.geo.cluster_offset(cn) + within, cl - within)))
    }

    /// The `n`th cluster of the chain starting at `start`.
    pub fn chain_nth(&self, cx: &B::Cx, start: u32, n: u64) -> Result<Option<u32>, Error> {
        let mut cn = start;
        if cn < CLUST_FIRST || cn > self.geo.max_cluster {
            return Ok(None);
        }
        for _ in 0..n {
            let next = self.fat_get(cx, cn)?;
            if self.geo.is_eof(next) {
                return Ok(None);
            }
            if next < CLUST_FIRST || next > self.geo.max_cluster {
                return Err(Error::Corrupt("cluster chain"));
            }
            cn = next;
        }
        Ok(Some(cn))
    }

    /// The number of clusters in the chain starting at `start`.
    pub fn chain_len(&self, cx: &B::Cx, start: u32) -> Result<u64, Error> {
        if start < CLUST_FIRST {
            return Ok(0);
        }
        let mut n = 1;
        let mut cn = start;
        loop {
            let next = self.fat_get(cx, cn)?;
            if self.geo.is_eof(next) {
                return Ok(n);
            }
            if next < CLUST_FIRST
                || next > self.geo.max_cluster
                || n > u64::from(self.geo.max_cluster)
            {
                return Err(Error::Corrupt("cluster chain"));
            }
            cn = next;
            n += 1;
        }
    }
}
