// SPDX-License-Identifier: BSD-2-Clause
//! Files from UFS2 (P5.4), as FreeBSD's `loader.efi` reads the kernel
//! from the root partition: every whole disk the firmware offers Block I/O
//! for is searched, in the firmware's order, for GPT partitions of type
//! `freebsd-ufs`, and the first holding the file supplies it.
//!
//! Metadata reads go through a small cache of recent blocks (an indirect
//! block is read again for each block it maps); file data is read run by
//! run (`bmap_run`) straight into the page-aligned destination.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::Write;

use efi::{Allocate, SystemTable, raw};

use crate::kernel::Loaded;
use crate::{Error, memtype};

/// Blocks kept by the metadata cache.
const CACHED: usize = 8;
/// The most bytes one transfer reads.
const MAX_TRANSFER: usize = 1 << 20;

/// One `freebsd-ufs` partition, through its disk's Block I/O.
struct Partition<'a> {
    st: &'a SystemTable,
    bio: *mut raw::BlockIo,
    media: raw::BlockIoMedia,
    first_lba: u64,
    /// Blocks in the partition.
    blocks: u64,
    /// Recent metadata reads: offset, and the bytes.
    cache: RefCell<Vec<(u64, Vec<u8>)>>,
}

impl Partition<'_> {
    /// Reads whole blocks at byte `offset` of the partition into `buf`,
    /// which meets the media's alignment.
    fn read_into(&self, offset: u64, buf: &mut [u8]) -> Result<(), ufs::Error> {
        let bs = u64::from(self.media.block_size);
        let len = buf.len() as u64;
        if !offset.is_multiple_of(bs)
            || !len.is_multiple_of(bs)
            || (offset + len) / bs > self.blocks
        {
            return Err(ufs::Error::Io);
        }
        for (i, chunk) in buf.chunks_mut(MAX_TRANSFER).enumerate() {
            let at = offset + (i * MAX_TRANSFER) as u64;
            // SAFETY: `bio` is the firmware's live protocol for this disk.
            unsafe {
                self.st.read_blocks(
                    self.bio,
                    self.media.media_id,
                    self.first_lba + at / bs,
                    chunk,
                )
            }
            .map_err(|_| ufs::Error::Io)?;
        }
        Ok(())
    }
}

/// A heap buffer of `len` bytes aligned to `align` (a power of two),
/// as the firmware's `io_align` asks.
struct Aligned {
    raw: Vec<u8>,
    start: usize,
    len: usize,
}

impl Aligned {
    fn new(len: usize, align: usize) -> Self {
        let align = align.max(1);
        let raw = alloc::vec![0u8; len + align];
        let start = raw.as_ptr().align_offset(align).min(align);
        Self { raw, start, len }
    }

    fn bytes(&mut self) -> &mut [u8] {
        &mut self.raw[self.start..self.start + self.len]
    }
}

impl ufs::BlockIo for Partition<'_> {
    type Cx = ();

    fn read(
        &self,
        _: &(),
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), ufs::Error> {
        if let Some((_, b)) = self
            .cache
            .borrow()
            .iter()
            .find(|(o, b)| *o == offset && b.len() == len)
        {
            f(b);
            return Ok(());
        }
        let mut buf = Aligned::new(len, self.media.io_align as usize);
        self.read_into(offset, buf.bytes())?;
        let bytes = buf.bytes().to_vec();
        f(&bytes);
        let mut cache = self.cache.borrow_mut();
        if cache.len() == CACHED {
            cache.remove(0);
        }
        cache.push((offset, bytes));
        Ok(())
    }
}

/// Where a file was found, for the log and the root's name.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Origin {
    /// The disk's place in the firmware's order, and the partition's
    /// index in its GPT (1-based, as FreeBSD names `p1`).
    pub(crate) disk: usize,
    pub(crate) partition: u32,
    /// The partition's unique GUID (its `/dev/gptid/` name).
    pub(crate) guid: gpt::Guid,
}

impl core::fmt::Display for Origin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "disk {} partition {}", self.disk, self.partition)
    }
}

/// The UFS2 file systems on the machine's disks, each with where it is.
fn file_systems(st: &SystemTable) -> Vec<(Origin, ufs::Ufs<Partition<'_>>)> {
    let mut out = Vec::new();
    let Ok(handles) = st.handles_with::<raw::BlockIo>() else {
        return out;
    };
    let mut disk = 0;
    for &h in handles.handles() {
        let Ok(bio) = st.handle_protocol::<raw::BlockIo>(h) else {
            continue;
        };
        // SAFETY: firmware returned this protocol and its media.
        let media = unsafe { *(*bio).media };
        if media.logical_partition != 0 || media.media_present == 0 || media.block_size < 512 {
            continue;
        }
        let whole = Partition {
            st,
            bio,
            media,
            first_lba: 0,
            blocks: media.last_block + 1,
            cache: RefCell::new(Vec::new()),
        };
        disk += 1;
        for (index, first_lba, blocks, guid) in ufs_partitions(&whole) {
            let part = Partition {
                first_lba,
                blocks,
                cache: RefCell::new(Vec::new()),
                ..whole
            };
            if let Ok(fs) = ufs::Ufs::open(&(), part, media.block_size) {
                out.push((
                    Origin {
                        disk,
                        partition: index,
                        guid,
                    },
                    fs,
                ));
            }
        }
    }
    out
}

/// The `freebsd-ufs` partitions of a disk's GPT: index, first block,
/// length in blocks, and unique GUID.
fn ufs_partitions(disk: &Partition<'_>) -> Vec<(u32, u64, u64, gpt::Guid)> {
    let bs = disk.media.block_size as usize;
    let mut header = Aligned::new(bs, disk.media.io_align as usize);
    if disk.read_into(bs as u64, header.bytes()).is_err() {
        return Vec::new();
    }
    let Ok(h) = gpt::PrimaryHeader::read(header.bytes()) else {
        return Vec::new();
    };
    let len = h.entries_len.next_multiple_of(bs);
    let mut entries = Aligned::new(len, disk.media.io_align as usize);
    if disk
        .read_into(h.entries_lba * bs as u64, entries.bytes())
        .is_err()
    {
        return Vec::new();
    }
    let Ok(table) = gpt::Table::from_parts(&h, entries.bytes()) else {
        return Vec::new();
    };
    table
        .partitions()
        .filter(|p| p.type_guid == gpt::types::FREEBSD_UFS)
        .map(|p| (p.index + 1, p.first_lba, p.sectors(), p.unique_guid))
        .collect()
}

/// Reads the regular file at `path` from the first UFS2 file system that
/// has it, into fresh pages.
pub(crate) fn read_file(st: &SystemTable, path: &[u8]) -> Option<(Loaded, Origin)> {
    for (origin, fs) in file_systems(st) {
        let Ok(Some((_, ip))) = fs.namei(&(), path) else {
            continue;
        };
        if ip.kind() != ufs::Kind::Regular {
            continue;
        }
        match load(st, &fs, &ip) {
            Ok(l) => return Some((l, origin)),
            Err(_) => continue,
        }
    }
    None
}

/// The root file system for the kernel (`vfs.root.mountfrom`) when it
/// came from the UFS2 file system at `origin`: what that file system's
/// `/etc/fstab` mounts on `/`, as FreeBSD's loader reads it, or else that
/// file system by its partition's GUID.
pub(crate) fn root_mount(st: &SystemTable, origin: Origin) -> String {
    let fallback = || {
        let mut g = String::new();
        let _ = write!(g, "{}", origin.guid);
        format!("ufs:/dev/gptid/{}", g.to_ascii_lowercase())
    };
    let Some(fs) = file_systems(st)
        .into_iter()
        .find(|(o, _)| o.disk == origin.disk && o.partition == origin.partition)
        .map(|(_, fs)| fs)
    else {
        return fallback();
    };
    let Ok(Some((_, ip))) = fs.namei(&(), b"/etc/fstab") else {
        return fallback();
    };
    let mut buf = alloc::vec![0u8; usize::try_from(ip.size).unwrap_or(0).min(64 * 1024)];
    let Ok(n) = fs.read(&(), &ip, 0, &mut buf) else {
        return fallback();
    };
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    fstab_root(text).unwrap_or_else(fallback)
}

/// `type:device` for the `/` line of an fstab (FreeBSD's loader
/// `getrootmount`): the first whose second field is `/`.
pub(crate) fn fstab_root(text: &str) -> Option<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .find_map(|l| {
            let mut f = l.split_ascii_whitespace();
            let (dev, on, kind) = (f.next()?, f.next()?, f.next()?);
            (on == "/").then(|| format!("{kind}:{dev}"))
        })
}

/// The whole of file `ip`, run by run, into pages allocated for it.
fn load(st: &SystemTable, fs: &ufs::Ufs<Partition<'_>>, ip: &ufs::Inode) -> Result<Loaded, Error> {
    let len = usize::try_from(ip.size).map_err(|_| Error::Kernel("file too large"))?;
    let sb = fs.superblock();
    let bsize = u64::from(sb.bsize);
    let fsize = u64::from(sb.fsize);
    // Whole blocks past the end, so each run reads straight in.
    let room = usize::try_from(ip.size.next_multiple_of(bsize))
        .map_err(|_| Error::Kernel("file too large"))?;
    let pages = room.div_ceil(4096).max(1);
    let phys = st
        .allocate_pages(Allocate::Anywhere, memtype::BOOT_DATA, pages)
        .map_err(|s| Error::Efi("allocate file buffer", s))?;
    // SAFETY: freshly allocated pages, identity mapped, `pages * 4096`
    // bytes long.
    let buf = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, pages * 4096) };
    let part = fs.io();
    let blocks = ip.size.div_ceil(bsize);
    let max_run = (MAX_TRANSFER as u64 / bsize).max(1);
    let mut lbn = 0;
    while lbn < blocks {
        let (addr, run) = fs
            .bmap_run(&(), ip, lbn, max_run.min(blocks - lbn))
            .map_err(|_| Error::Kernel("UFS block map"))?;
        let start = usize::try_from(lbn * bsize).map_err(|_| Error::Kernel("file too large"))?;
        // Only the allocated fragments of the file's last block.
        let last = lbn + run - 1;
        let n = usize::try_from((run - 1) * bsize + u64::from(sb.block_size(ip.size, last)))
            .map_err(|_| Error::Kernel("file too large"))?;
        let out = &mut buf[start..start + n];
        if addr == 0 {
            out.fill(0);
        } else {
            part.read_into(addr * fsize, out)
                .map_err(|_| Error::Kernel("UFS read"))?;
        }
        lbn += run;
    }
    Ok(Loaded {
        phys: addr::PhysAddr::new(phys),
        len,
    })
}
