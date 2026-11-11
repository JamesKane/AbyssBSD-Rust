// SPDX-License-Identifier: BSD-2-Clause
//! ISO 9660 (docs/VFS.md §10.7): the engine behind the kernel's cd9660,
//! over a [`BlockIo`] as `lib/ufs` and `lib/fat` are, read-only. It reads
//! what FreeBSD 15.1's cd9660 reads, and names, numbers, and describes
//! files as it does:
//!
//! - The volume descriptors from block 16 (plus the session's start), the
//!   primary one for the volume and a Joliet supplementary one when Rock
//!   Ridge is not used ([`Volume::open`], `iso_mountfs`); High Sierra
//!   volumes too.
//! - Rock Ridge (IEEE P1282 over SUSP): POSIX attributes, names, symbolic
//!   links, device numbers, times, and relocated directories, found by the
//!   root's `SP` and `ER` entries ([`rrip`]).
//! - Plain ISO 9660 and Joliet names as FreeBSD translates them: lower
//!   case, the version dropped, the last of several versions shown
//!   (`isofntrans`, `iso_shipdir`); lookups that ignore case and version
//!   (`isofncmp`). Without `kiconv`, a Joliet character outside Latin-1 is
//!   `?`.
//! - A file's number is where its directory record lies on the volume, a
//!   directory's where its extent starts (`isodirino`).
//!
//! Everything read comes from an untrusted disk: records are checked
//! against their block and the System Use areas against their record, as
//! FreeBSD's later fix of PR 296853 checks them; a malformed Rock Ridge
//! root fails the mount (`EINVAL`).
//!
//! Behavior reference: sys/fs/cd9660 (`cd9660_vfsops.c`,
//! `cd9660_lookup.c`, `cd9660_vnops.c`, `cd9660_node.c`,
//! `cd9660_rrip.c`, `cd9660_util.c`), FreeBSD 15.1.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod name;
pub mod rrip;
#[cfg(test)]
mod tests;

use alloc::vec::Vec;

/// The block size volume descriptors are counted in
/// (`ISO_DEFAULT_BLOCK_SIZE`).
pub const DEFAULT_BLOCK: u32 = 2048;
/// The fixed part of a directory record (`ISO_DIRECTORY_RECORD_SIZE`).
pub const RECORD_MIN: usize = 33;
/// The longest symbolic link target (`MAXPATHLEN`).
pub const MAXPATHLEN: usize = 1024;
/// The longest name (`NAME_MAX`).
pub const NAME_MAX: usize = 255;

/// File types in a mode (`S_IFMT` and its kinds).
pub const S_IFMT: u16 = 0o170_000;
pub const S_IFDIR: u16 = 0o040_000;
pub const S_IFREG: u16 = 0o100_000;
pub const S_IFLNK: u16 = 0o120_000;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device failed (`EIO`).
    Io,
    /// Not a volume this reads, or a structure failing its checks
    /// (`EINVAL`).
    Invalid,
    /// A number naming no record (`ESTALE`).
    Stale,
    /// No such name (`ENOENT`).
    NotFound,
}

/// Where the bytes come from.
pub trait BlockIo {
    /// What every call needs (the kernel's `Sleepable`; `()` elsewhere).
    type Cx: ?Sized;
    /// Calls `f` with the `len` bytes at `offset` (sector multiples).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the device fails.
    fn read(
        &self,
        cx: &Self::Cx,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), Error>;

    /// The host name Rock Ridge's `HOST` components stand for
    /// (`getcredhostname`).
    fn hostname(&self, _cx: &Self::Cx) -> Vec<u8> {
        Vec::new()
    }
}

/// How the volume is read (FreeBSD's `ISOFSMNT_*` and mount options).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Ignore Rock Ridge (`norrip`, `mount_cd9660 -r`).
    pub norrip: bool,
    /// Keep version numbers and case (`gens`, `-g`).
    pub gens: bool,
    /// Read extended attribute records (`extatt`, `-e`).
    pub extatt: bool,
    /// Ignore Joliet (`nojoliet`, `-j`).
    pub nojoliet: bool,
    /// Use a Joliet descriptor marked as not conforming (`brokenjoliet`,
    /// `-b`).
    pub brokenjoliet: bool,
    /// Every file's owner and group (`uid`, `gid`).
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Permission masks for files and directories (`mask`, `dirmask`).
    pub mask: Option<u32>,
    pub dirmask: Option<u32>,
    /// Where the last session starts, in 2048-byte blocks (`ssector`).
    pub ssector: u32,
}

/// What kind of names and attributes the volume has (`enum ISO_FTYPE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FType {
    /// ISO 9660 (or Joliet) names, lower-cased, versions dropped.
    Default,
    /// ISO 9660 names as recorded (`gens` without Rock Ridge).
    Iso9660,
    /// Rock Ridge.
    Rrip,
    /// High Sierra.
    HighSierra,
}

/// A time, as `struct timespec`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Time {
    pub sec: i64,
    pub nsec: i64,
}

/// A file's attributes (FreeBSD's `iso_node` and its `ISO_RRIP_INODE`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inode {
    /// Its number (`i_number`).
    pub ino: u64,
    /// Its extent's first block, and where its data starts (after the
    /// extended attribute record), in logical blocks.
    pub extent: u64,
    pub start: u64,
    pub size: u64,
    /// Mode, with the type (`iso_mode`).
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub links: i16,
    pub rdev: u64,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
    /// A directory's `..`.
    pub parent: u64,
}

impl Inode {
    /// The type bits of its mode.
    #[must_use]
    pub fn kind(&self) -> u16 {
        self.mode & S_IFMT
    }
}

/// How a lookup found a file: the record that names it, for loading its
/// attributes as FreeBSD does from the entry rather than from the file
/// (`cd9660_vget_internal`'s `isodir`), and whether it is a relocated
/// directory, read from its own `.`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub ino: u64,
    pub record: Vec<u8>,
    pub relocated: bool,
}

/// One entry as `readdir` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    pub name: &'a [u8],
    pub ino: u64,
    /// Where reading resumes after it (`d_off`).
    pub next: u64,
}

/// A mounted volume (`struct iso_mnt`).
#[derive(Clone, Debug)]
pub struct Volume {
    pub block_size: u32,
    bshift: u32,
    bmask: u64,
    /// Blocks in the volume, its sessions before included.
    pub volume_space_size: u64,
    root: Vec<u8>,
    root_extent: u64,
    pub ftype: FType,
    rr_skip: usize,
    rr_skip0: usize,
    pub joliet_level: u8,
    fmask: u16,
    dmask: u16,
    uid: Option<u32>,
    gid: Option<u32>,
    extatt: bool,
}

/// Byte `at` of `b`, or 0 past its end.
fn b8(b: &[u8], at: usize) -> u8 {
    b.get(at).copied().unwrap_or(0)
}

/// `isonum_723`: a both-endian 16-bit number, read little-endian.
fn n723(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b8(b, at), b8(b, at + 1)])
}

/// `isonum_733`: a both-endian 32-bit number, read little-endian.
fn n733(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b8(b, at), b8(b, at + 1), b8(b, at + 2), b8(b, at + 3)])
}

/// A length as a 64-bit count.
fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A 64-bit offset as an index, saturating.
fn idx(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// The fields of a directory record.
pub(crate) mod rec {
    use super::{b8, n733};

    pub(crate) fn len(r: &[u8]) -> usize {
        usize::from(b8(r, 0))
    }
    pub(crate) fn ext_attr_len(r: &[u8]) -> u32 {
        u32::from(b8(r, 1))
    }
    pub(crate) fn extent(r: &[u8]) -> u32 {
        n733(r, 2)
    }
    pub(crate) fn size(r: &[u8]) -> u32 {
        n733(r, 10)
    }
    pub(crate) fn date(r: &[u8]) -> [u8; 7] {
        let mut d = [0; 7];
        for (i, b) in d.iter_mut().enumerate() {
            *b = b8(r, 18 + i);
        }
        d
    }
    pub(crate) fn flags(r: &[u8]) -> u8 {
        b8(r, 25)
    }
    pub(crate) fn name(r: &[u8]) -> &[u8] {
        let n = usize::from(b8(r, 32));
        r.get(33..33 + n).unwrap_or(&[])
    }
}

/// `cd9660_tstamp_conv7`: a 7-byte time; `None` (and the epoch) before
/// 1970. FreeBSD computes in `int`, which this follows, wrapping.
#[must_use]
pub fn tstamp7(pi: &[u8; 7], high_sierra: bool) -> (Time, bool) {
    let y = i32::from(pi[0]) + 1900;
    let m = i32::from(pi[1]);
    let d = i32::from(pi[2]);
    let hour = i32::from(pi[3]);
    let minute = i32::from(pi[4]);
    let second = i32::from(pi[5]);
    let tz = if high_sierra {
        0
    } else {
        i32::from(i8::from_ne_bytes([pi[6]]))
    };
    if y < 1970 {
        return (Time::default(), false);
    }
    let days = 367i32
        .wrapping_mul(y - 1960)
        .wrapping_sub(7i32.wrapping_mul(y + (m + 9) / 12) / 4)
        .wrapping_sub(3i32.wrapping_mul((y + (m + 9) / 12 - 1) / 100 + 1) / 4)
        .wrapping_add(275i32.wrapping_mul(m) / 9)
        .wrapping_add(d)
        .wrapping_sub(239);
    let mut crtime = days
        .wrapping_mul(24)
        .wrapping_add(hour)
        .wrapping_mul(60)
        .wrapping_add(minute)
        .wrapping_mul(60)
        .wrapping_add(second);
    // The offset is unreliable on some disks.
    if (-48..=52).contains(&tz) {
        crtime = crtime.wrapping_sub(tz.wrapping_mul(15 * 60));
    }
    (
        Time {
            sec: i64::from(crtime),
            nsec: 0,
        },
        true,
    )
}

/// `cd9660_chars2ui`: decimal digits, unchecked, wrapping.
fn chars2ui(b: &[u8]) -> u32 {
    b.iter().fold(0u32, |rc, &c| {
        rc.wrapping_mul(10)
            .wrapping_add(u32::from(c).wrapping_sub(u32::from(b'0')))
    })
}

/// `cd9660_tstamp_conv17`: a 17-byte time.
#[must_use]
pub fn tstamp17(pi: &[u8]) -> (Time, bool) {
    let get = |r: core::ops::Range<usize>| pi.get(r).unwrap_or(&[]);
    let low = |n: u32| n.to_le_bytes()[0];
    let buf = [
        low(chars2ui(get(0..4)).wrapping_sub(1900)),
        low(chars2ui(get(4..6))),
        low(chars2ui(get(6..8))),
        low(chars2ui(get(8..10))),
        low(chars2ui(get(10..12))),
        low(chars2ui(get(12..14))),
        b8(pi, 16),
    ];
    tstamp7(&buf, false)
}

impl Volume {
    /// Reads the volume on `io`, whose sectors are `sector_size` bytes
    /// (FreeBSD's `iso_mountfs`).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if it is not an ISO 9660 or High Sierra volume
    /// this reads, or its Rock Ridge root is malformed; [`Error::Io`].
    pub fn open<B: BlockIo + ?Sized>(
        io: &B,
        cx: &B::Cx,
        sector_size: u32,
        opts: &Options,
    ) -> Result<Self, Error> {
        if sector_size == 0 || !DEFAULT_BLOCK.is_multiple_of(sector_size) {
            return Err(Error::Invalid);
        }
        let mut high_sierra = false;
        let mut pri: Option<Vec<u8>> = None;
        let mut sup: Option<Vec<u8>> = None;
        let mut joliet_level = 0u8;
        let first = 16 + u64::from(opts.ssector);
        for blk in first..first + 84 {
            let mut vd = Vec::new();
            io.read(
                cx,
                blk * u64::from(DEFAULT_BLOCK),
                idx(u64::from(DEFAULT_BLOCK)),
                &mut |b| {
                    vd.extend_from_slice(b);
                },
            )?;
            if vd.get(1..6) != Some(b"CD001") {
                if vd.get(9..14) != Some(b"CDROM") {
                    return Err(Error::Invalid);
                }
                high_sierra = true;
            }
            match b8(&vd, if high_sierra { 8 } else { 0 }) {
                1 => {
                    if pri.is_none() {
                        pri = Some(vd);
                    }
                }
                2 => {
                    if sup.is_none() {
                        if !opts.nojoliet {
                            joliet_level = match vd.get(88..91) {
                                Some(b"%/@") => 1,
                                Some(b"%/C") => 2,
                                Some(b"%/E") => 3,
                                _ => 0,
                            };
                            if b8(&vd, 7) & 1 != 0 && !opts.brokenjoliet {
                                joliet_level = 0;
                            }
                        }
                        sup = Some(vd);
                    }
                }
                255 => break,
                _ => {}
            }
        }
        let pri = pri.ok_or(Error::Invalid)?;
        let (lbs_at, vss_at, root_at) = if high_sierra {
            (136, 88, 180)
        } else {
            (128, 80, 156)
        };
        let block_size = u32::from(n723(&pri, lbs_at));
        if !(512..=65_536).contains(&block_size)
            || !block_size.is_power_of_two()
            || block_size < sector_size
        {
            return Err(Error::Invalid);
        }
        let root = pri
            .get(root_at..root_at + 34)
            .ok_or(Error::Invalid)?
            .to_vec();
        let mut v = Self {
            block_size,
            bshift: block_size.trailing_zeros(),
            bmask: u64::from(block_size - 1),
            volume_space_size: u64::from(n733(&pri, vss_at)) + u64::from(opts.ssector),
            root_extent: u64::from(rec::extent(&root)),
            root,
            ftype: FType::Default,
            rr_skip: 0,
            rr_skip0: 0,
            joliet_level: 0,
            fmask: 0o7777
                & opts
                    .mask
                    .map_or(0o7777, |m| u16::try_from(m & 0o7777).unwrap_or(0)),
            dmask: 0o7777
                & opts
                    .dirmask
                    .map_or(0o7777, |m| u16::try_from(m & 0o7777).unwrap_or(0)),
            uid: opts.uid,
            gid: opts.gid,
            extatt: opts.extatt,
        };
        let mut norrip = opts.norrip;
        let mut gens = opts.gens;
        if !norrip {
            // Rock Ridge if the root's `.` says so (`cd9660_rrip_offset`).
            let blk = v.root_extent + u64::from(rec::ext_attr_len(&v.root));
            let block = v.block(io, cx, blk)?;
            let dot = v.record_at(&block, 0).ok_or(Error::Invalid)?;
            match rrip::offset(&mut v, io, cx, dot)? {
                Some(skip) => {
                    v.rr_skip = skip;
                    gens = false;
                }
                None => norrip = true,
            }
        }
        v.ftype = if high_sierra {
            FType::HighSierra
        } else {
            match (norrip, gens) {
                (true, true) => FType::Iso9660,
                (false, false) => FType::Rrip,
                _ => FType::Default,
            }
        };
        if v.ftype != FType::Rrip
            && joliet_level > 0
            && let Some(sup) = &sup
        {
            let root = sup.get(156..190).ok_or(Error::Invalid)?.to_vec();
            v.root_extent = u64::from(rec::extent(&root));
            v.root = root;
            v.joliet_level = joliet_level;
        }
        Ok(v)
    }

    /// The root's number (`cd9660_root`'s `isodirino`).
    #[must_use]
    pub fn root_ino(&self) -> u64 {
        self.dirino(&self.root)
    }

    /// The preferred I/O size (`f_iosize`) and the block size of `stat`.
    #[must_use]
    pub fn logical_block_size(&self) -> u32 {
        self.block_size
    }

    /// A directory's number: where its data starts (`isodirino`).
    fn dirino(&self, r: &[u8]) -> u64 {
        (u64::from(rec::extent(r)) + u64::from(rec::ext_attr_len(r))) << self.bshift
    }

    /// Logical block `blk` of the volume.
    fn block<B: BlockIo + ?Sized>(&self, io: &B, cx: &B::Cx, blk: u64) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let at = blk.checked_shl(self.bshift).ok_or(Error::Invalid)?;
        io.read(cx, at, idx(u64::from(self.block_size)), &mut |b| {
            out.extend_from_slice(b);
        })?;
        Ok(out)
    }

    /// The record at `off` in `block`, if it lies whole within it.
    fn record_at<'a>(&self, block: &'a [u8], off: usize) -> Option<&'a [u8]> {
        let r = block.get(off..)?;
        let len = rec::len(r);
        if len < RECORD_MIN || len < RECORD_MIN + usize::from(b8(r, 32)) {
            return None;
        }
        r.get(..len)
    }

    /// Walks directory `dir` from offset `from`, calling `f` with each
    /// record: the offset just past it, where it lies on the volume (in
    /// bytes), and the record. `f` returns whether to go on. A malformed
    /// record ends the walk with `Err(Invalid)`.
    fn walk<B: BlockIo + ?Sized>(
        &self,
        io: &B,
        cx: &B::Cx,
        dir: &Inode,
        from: u64,
        f: &mut RecordFn<'_>,
    ) -> Result<(), Error> {
        let bsize = u64::from(self.block_size);
        let mut off = from;
        let mut block: Option<(u64, Vec<u8>)> = None;
        while off < dir.size {
            let lbn = off >> self.bshift;
            if block.as_ref().is_none_or(|(b, _)| *b != lbn) {
                block = Some((lbn, self.block(io, cx, dir.start + lbn)?));
            }
            let Some((_, data)) = &block else {
                return Err(Error::Io);
            };
            let in_block = off & self.bmask;
            let r = data.get(idx(in_block)..).unwrap_or(&[]);
            let len = rec::len(r);
            if len == 0 {
                // The rest of the block is padding.
                off = (off & !self.bmask) + bsize;
                continue;
            }
            if len < RECORD_MIN
                || in_block + len64(len) > bsize
                || len < RECORD_MIN + usize::from(b8(r, 32))
            {
                return Err(Error::Invalid);
            }
            let at = ((dir.start + lbn) << self.bshift) + in_block;
            off += len64(len);
            if !f(off, at, &r[..len])? {
                break;
            }
        }
        Ok(())
    }

    /// A record's file number: a directory's start, or where it lies.
    fn ino_of(&self, r: &[u8], at: u64) -> u64 {
        if rec::flags(r) & 2 != 0 {
            self.dirino(r)
        } else {
            at
        }
    }

    /// Looks `name` up in directory `dir` (`cd9660_lookup`); never `.`
    /// or `..`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or the device's.
    pub fn lookup<B: BlockIo + ?Sized>(
        &self,
        io: &B,
        cx: &B::Cx,
        dir: &Inode,
        name: &[u8],
    ) -> Result<Found, Error> {
        let rrip = self.ftype == FType::Rrip;
        // A leading `=` asks for an associated file.
        let (assoc, name) = match name.split_first() {
            Some((b'=', rest)) if !rrip => (true, rest),
            _ => (false, name),
        };
        let mut found: Option<Found> = None;
        let mut saved: Option<Found> = None;
        let r = self.walk(io, cx, dir, 0, &mut |_, at, r| {
            if rrip {
                let ino = self.ino_of(r, at);
                let mut i_ino = ino;
                let mut buf = Vec::new();
                rrip::getname(self, io, cx, r, &mut buf, &mut i_ino)?;
                if buf == name {
                    found = Some(Found {
                        ino: i_ino,
                        record: r.to_vec(),
                        relocated: i_ino != ino,
                    });
                    return Ok(false);
                }
                return Ok(true);
            }
            let isoflags = if self.ftype == FType::HighSierra {
                b8(r, 24)
            } else {
                rec::flags(r)
            };
            if (isoflags & 4 != 0) == assoc {
                if name::isofncmp(name, rec::name(r), self.joliet_level) == 0 {
                    // The last of consecutive matches (versions) wins.
                    saved = Some(Found {
                        ino: if isoflags & 2 != 0 {
                            self.dirino(r)
                        } else {
                            at
                        },
                        record: r.to_vec(),
                        relocated: false,
                    });
                } else if saved.is_some() {
                    return Ok(false);
                }
            }
            Ok(true)
        });
        match r {
            // A malformed record ends the search as the directory's end.
            Ok(()) | Err(Error::Invalid) => {}
            Err(e) => return Err(e),
        }
        found.or(saved).ok_or(Error::NotFound)
    }

    /// The record numbered `ino`: where it lies (`cd9660_vget_internal`
    /// without a record).
    fn record_of<B: BlockIo + ?Sized>(
        &self,
        io: &B,
        cx: &B::Cx,
        ino: u64,
    ) -> Result<Vec<u8>, Error> {
        let lbn = ino >> self.bshift;
        if lbn >= self.volume_space_size {
            return Err(Error::Stale);
        }
        let off = idx(ino & self.bmask);
        if off + RECORD_MIN > idx(u64::from(self.block_size)) {
            return Err(Error::Stale);
        }
        let block = self.block(io, cx, lbn)?;
        let r = block.get(off..).ok_or(Error::Stale)?;
        if off + rec::len(r) > block.len() || rec::len(r) < RECORD_MIN {
            return Err(Error::Stale);
        }
        Ok(r[..rec::len(r)].to_vec())
    }

    /// The file numbered `ino`, from the record a lookup found
    /// (`found`), the root's, or the record where the number says
    /// (`cd9660_vget_internal`).
    ///
    /// # Errors
    ///
    /// [`Error::Stale`] for a number naming no record; the device's.
    pub fn load<B: BlockIo + ?Sized>(
        &self,
        io: &B,
        cx: &B::Cx,
        ino: u64,
        found: Option<&Found>,
    ) -> Result<Inode, Error> {
        let (record, relocated) = if ino == self.root_ino() {
            // With Rock Ridge, the root's `.` (`cd9660_root`).
            (self.root.clone(), self.ftype == FType::Rrip)
        } else if let Some(f) = found.filter(|f| f.ino == ino) {
            (f.record.clone(), f.relocated)
        } else {
            (self.record_of(io, cx, ino)?, false)
        };
        let record = if relocated {
            // A relocated directory: its own `.`.
            let block = self.block(io, cx, ino >> self.bshift)?;
            self.record_at(&block, 0).ok_or(Error::Stale)?.to_vec()
        } else {
            record
        };
        let extent = u64::from(rec::extent(&record));
        let mut ip = Inode {
            ino,
            extent,
            start: extent + u64::from(rec::ext_attr_len(&record)),
            size: u64::from(rec::size(&record)),
            ..Inode::default()
        };
        if self.ftype == FType::Rrip {
            rrip::analyze(self, io, cx, &record, &mut ip)?;
        } else {
            let ea = if self.extatt && rec::ext_attr_len(&record) != 0 {
                // The extended attribute record precedes the data.
                Some(self.block(io, cx, extent)?)
            } else {
                None
            };
            self.defattr(&record, &mut ip, ea.as_deref());
            self.deftstamp(&record, &mut ip, ea.as_deref());
        }
        if ip.kind() == S_IFDIR {
            ip.parent = self.dotdot(io, cx, &ip)?;
        }
        Ok(ip)
    }

    /// A directory's `..`: its second record, through Rock Ridge's parent
    /// link.
    fn dotdot<B: BlockIo + ?Sized>(&self, io: &B, cx: &B::Cx, dir: &Inode) -> Result<u64, Error> {
        if dir.ino == self.root_ino() {
            return Ok(dir.ino);
        }
        let mut parent = None;
        let mut n = 0;
        let r = self.walk(io, cx, dir, 0, &mut |_, _, r| {
            n += 1;
            if n < 2 {
                return Ok(true);
            }
            let mut ino = self.dirino(r);
            if self.ftype == FType::Rrip {
                let mut name = Vec::new();
                rrip::getname(self, io, cx, r, &mut name, &mut ino)?;
            }
            parent = Some(ino);
            Ok(false)
        });
        match r {
            Ok(()) | Err(Error::Invalid) => Ok(parent.unwrap_or(self.root_ino())),
            Err(e) => Err(e),
        }
    }

    /// `cd9660_defattr`: the type from the record's flags, and the
    /// permissions and owner from the extended attribute record when it is
    /// read, else `0555` and root.
    pub(crate) fn defattr(&self, r: &[u8], ip: &mut Inode, ea: Option<&[u8]>) {
        ip.mode = if rec::flags(r) & 2 != 0 {
            S_IFDIR
        } else {
            S_IFREG
        };
        // Not 2 for a directory, or `fts` would think it holds none.
        ip.links = 1;
        match ea.filter(|a| b8(a, 180) == 1) {
            Some(a) => {
                let p0 = b8(a, 8);
                let p1 = b8(a, 9);
                for (bit, set) in [
                    (p0 & 0x40, 0o001),
                    (p0 & 0x10, 0o004),
                    (p0 & 0x04, 0o010),
                    (p0 & 0x01, 0o040),
                    (p1 & 0x40, 0o100),
                    (p1 & 0x10, 0o400),
                ] {
                    if bit == 0 {
                        ip.mode |= set;
                    }
                }
                ip.uid = u32::from(n723(a, 0));
                ip.gid = u32::from(n723(a, 4));
            }
            None => {
                ip.mode |= 0o555;
                ip.uid = 0;
                ip.gid = 0;
            }
        }
    }

    /// `cd9660_deftstamp`: the times from the extended attribute record,
    /// else all the record's.
    pub(crate) fn deftstamp(&self, r: &[u8], ip: &mut Inode, ea: Option<&[u8]>) {
        let hs = self.ftype == FType::HighSierra;
        if let Some(a) = ea.filter(|a| !hs && b8(a, 180) == 1) {
            let field = |at: usize| a.get(at..at + 17).unwrap_or(&[]);
            let (t, ok) = tstamp17(field(61));
            ip.atime = if ok { t } else { tstamp17(field(10)).0 };
            let (t, ok) = tstamp17(field(10));
            ip.ctime = if ok { t } else { ip.atime };
            let (t, ok) = tstamp17(field(27));
            ip.mtime = if ok { t } else { ip.ctime };
            return;
        }
        ip.ctime = tstamp7(&rec::date(r), false).0;
        ip.atime = ip.ctime;
        ip.mtime = ip.ctime;
    }

    /// Calls `f` with directory `dir`'s entries from cookie `from` on, as
    /// `cd9660_readdir` reports them, until it returns `false`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a malformed record; the device's.
    pub fn readdir<B: BlockIo + ?Sized>(
        &self,
        io: &B,
        cx: &B::Cx,
        dir: &Inode,
        from: u64,
        f: &mut dyn FnMut(Entry<'_>) -> bool,
    ) -> Result<(), Error> {
        // Without Rock Ridge and `gens`, entries wait to be shipped: of
        // consecutive versions of one name, only the last is shown, and
        // an associated file before its file (`iso_shipdir`).
        let shipping = self.ftype == FType::Default;
        let mut save: Option<(Vec<u8>, u64, u64)> = None;
        let mut assocent: Option<(Vec<u8>, u64, u64)> = None;
        let mut stopped = false;
        let walked = self.walk(io, cx, dir, from, &mut |off, at, r| {
            let mut ino = self.ino_of(r, at);
            let raw = rec::name(r);
            if self.ftype == FType::Rrip {
                let mut name = Vec::new();
                rrip::getname(self, io, cx, r, &mut name, &mut ino)?;
                if name.is_empty() {
                    return Ok(true);
                }
                if !f(Entry {
                    name: &name,
                    ino,
                    next: off,
                }) {
                    stopped = true;
                    return Ok(false);
                }
                return Ok(true);
            }
            let name: Vec<u8> = match raw {
                [0] => b".".to_vec(),
                [1] => b"..".to_vec(),
                _ => name::isofntrans(
                    raw,
                    self.ftype == FType::Iso9660,
                    rec::flags(r) & 4 != 0,
                    self.joliet_level,
                ),
            };
            if !shipping || matches!(raw, [0] | [1]) {
                if !f(Entry {
                    name: &name,
                    ino,
                    next: off,
                }) {
                    stopped = true;
                    return Ok(false);
                }
                return Ok(true);
            }
            if !shipdir(&mut save, &mut assocent, Some((name, ino, off)), f) {
                stopped = true;
                return Ok(false);
            }
            Ok(true)
        });
        walked?;
        if shipping && !stopped {
            shipdir(&mut save, &mut assocent, None, f);
        }
        Ok(())
    }

    /// The target of the symbolic link `ip` (`cd9660_readlink`):
    /// `mounted_on` stands for the volume's root (`VOLROOT`).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] without Rock Ridge or a usable `SL`.
    pub fn readlink<B: BlockIo + ?Sized>(
        &self,
        io: &B,
        cx: &B::Cx,
        ip: &Inode,
        mounted_on: &[u8],
    ) -> Result<Vec<u8>, Error> {
        if self.ftype != FType::Rrip {
            return Err(Error::Invalid);
        }
        let r = self.record_of(io, cx, ip.ino).map_err(|_| Error::Invalid)?;
        rrip::getsymname(self, io, cx, &r, mounted_on)
    }

    /// Where file `ip`'s byte `offset` lies on the volume.
    #[must_use]
    pub fn data_offset(&self, ip: &Inode, offset: u64) -> u64 {
        (ip.start << self.bshift).saturating_add(offset)
    }

    /// Attributes as `getattr` reports them: the masks applied, and the
    /// mount's owner and group if given.
    #[must_use]
    pub fn mode_of(&self, ip: &Inode) -> u16 {
        let mask = if ip.kind() == S_IFDIR {
            self.dmask
        } else {
            self.fmask
        };
        ip.mode & mask & 0o7777
    }

    #[must_use]
    pub fn uid_of(&self, ip: &Inode) -> u32 {
        self.uid.unwrap_or(ip.uid)
    }

    #[must_use]
    pub fn gid_of(&self, ip: &Inode) -> u32 {
        self.gid.unwrap_or(ip.gid)
    }
}

/// An entry waiting to be shipped: its name, number, and next offset.
type Pending = (Vec<u8>, u64, u64);

/// What `readdir` reports entries to.
type EntryFn<'a> = dyn FnMut(Entry<'_>) -> bool + 'a;

/// Ships entry `e` to `f`; `false` when it is full.
fn ship(e: &Pending, f: &mut EntryFn<'_>) -> bool {
    f(Entry {
        name: &e.0,
        ino: e.1,
        next: e.2,
    })
}

/// What [`Volume::walk`] calls with each record: the offset past it,
/// where it lies, and the record; `Ok(false)` stops.
type RecordFn<'a> = dyn FnMut(u64, u64, &[u8]) -> Result<bool, Error> + 'a;

/// `iso_shipdir`: `cur` arrives (or `None`, the end); what waits goes
/// when the name changes. Returns `false` once `f` is full.
fn shipdir(
    save: &mut Option<Pending>,
    assocent: &mut Option<Pending>,
    cur: Option<Pending>,
    f: &mut EntryFn<'_>,
) -> bool {
    let (cname, assoc) = match &cur {
        Some((n, _, _)) if n.len() > 1 && n[0] == b'=' => (&n[1..], true),
        Some((n, _, _)) => (&n[..], false),
        None => (&[][..], false),
    };
    let waiting: Option<&[u8]> = match (&*save, &*assocent) {
        (Some((n, _, _)), _) => Some(n),
        (None, Some((n, _, _))) => Some(&n[1..]),
        _ => None,
    };
    if let Some(sname) = waiting
        && !sname.is_empty()
        && (cur.is_none() || sname != cname)
    {
        if let Some(a) = assocent.take()
            && !ship(&a, f)
        {
            *assocent = Some(a);
            return false;
        }
        if let Some(s) = save.take()
            && !ship(&s, f)
        {
            *save = Some(s);
            return false;
        }
    }
    if let Some(c) = cur {
        if assoc {
            *assocent = Some(c);
        } else {
            *save = Some(c);
        }
    }
    true
}
