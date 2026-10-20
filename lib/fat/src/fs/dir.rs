// SPDX-License-Identifier: BSD-2-Clause
//! Directories: walking entries with their long names, lookup, readdir,
//! and making, removing, and rewriting entries as FreeBSD's msdosfs does.
//!
//! A file is named by where its short entry lies ([`Loc`]: the cluster
//! holding it, or 0 for FAT12/16's fixed root, and the offset there). A
//! directory's own node is its `.` entry (its start cluster, offset 0), as
//! in FreeBSD's `deget`; the root has none.
//!
//! Behavior reference: sys/fs/msdosfs/msdosfs_lookup.c
//! (`msdosfs_lookup_ino`, `createde`, `uniqdosname`, `removede`,
//! `dosdirempty`), msdosfs_vnops.c (`msdosfs_readdir`, `msdosfs_mkdir`).

use alloc::vec::Vec;

use super::conv::{self, Conv};
use super::time::FatTime;
use super::{Alloc, BlockIo, CLUST_FIRST, Error, Fat, FatType, Fill, Mode, ROOT};

/// An entry's size.
pub const DIRENT: u64 = 32;
/// `ATTR_*`.
pub const ATTR_READONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_WIN95: u8 = 0x0f;
const SLOT_EMPTY: u8 = 0x00;
const SLOT_DELETED: u8 = 0xe5;
/// The root directory's location (`MSDOSFSROOT`, `MSDOSFSROOT_OFS`).
pub const ROOT_LOC: Loc = Loc {
    dirclust: ROOT,
    diroffset: 0x1fff_ffff,
};

/// Where a short entry lies (FreeBSD's `de_dirclust`, `de_diroffset`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Loc {
    pub dirclust: u32,
    pub diroffset: u32,
}

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
}

/// A decoded short entry (`struct direntry`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: [u8; 11],
    pub attr: u8,
    pub lcase: u8,
    /// Created (with hundredths), modified (2-second steps), and the date
    /// of last access.
    pub ctime: FatTime,
    pub mtime: FatTime,
    pub adate: u16,
    pub start: u32,
    pub size: u32,
}

impl Entry {
    fn parse(b: &[u8], fat32: bool) -> Self {
        let u16_at = |at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
        let mut name = [0; 11];
        name.copy_from_slice(&b[..11]);
        let high = if fat32 {
            u32::from(u16_at(20)) << 16
        } else {
            0
        };
        Self {
            name,
            attr: b[11],
            lcase: b[12],
            ctime: FatTime {
                date: u16_at(16),
                time: u16_at(14),
                hundredths: b[13],
            },
            mtime: FatTime {
                date: u16_at(24),
                time: u16_at(22),
                hundredths: 0,
            },
            adate: u16_at(18),
            start: high | u32::from(u16_at(26)),
            size: u32::from_le_bytes([b[28], b[29], b[30], b[31]]),
        }
    }

    /// Encodes into 32 directory bytes (FreeBSD's `DE_EXTERNALIZE`).
    pub fn write(&self, b: &mut [u8], fat32: bool) {
        b[..11].copy_from_slice(&self.name);
        b[11] = self.attr;
        b[12] = self.lcase;
        b[13] = self.ctime.hundredths;
        b[14..16].copy_from_slice(&self.ctime.time.to_le_bytes());
        b[16..18].copy_from_slice(&self.ctime.date.to_le_bytes());
        b[18..20].copy_from_slice(&self.adate.to_le_bytes());
        let high = if fat32 {
            u16::try_from(self.start >> 16).unwrap_or(0)
        } else {
            0
        };
        b[20..22].copy_from_slice(&high.to_le_bytes());
        b[22..24].copy_from_slice(&self.mtime.time.to_le_bytes());
        b[24..26].copy_from_slice(&self.mtime.date.to_le_bytes());
        b[26..28].copy_from_slice(
            &u16::try_from(self.start & 0xffff)
                .unwrap_or(0)
                .to_le_bytes(),
        );
        b[28..32].copy_from_slice(&self.size.to_le_bytes());
    }

    #[must_use]
    pub fn kind(&self) -> Kind {
        if self.attr & ATTR_DIRECTORY != 0 {
            Kind::Directory
        } else {
            Kind::File
        }
    }
}

/// A named entry found in a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    /// The Unix name: the long name if it checks out, else the short one.
    pub name: Vec<u8>,
    pub entry: Entry,
    pub loc: Loc,
    /// The short entry's byte offset in the directory.
    pub diroff: u64,
    /// Long-name slots before it.
    pub slots: u32,
}

/// One slot as a directory walk sees it.
struct Slot<'a> {
    bytes: &'a [u8],
    diroff: u64,
    loc: Loc,
}

impl<B: BlockIo> Fat<B> {
    fn fat32(&self) -> bool {
        self.geo.fat_type == FatType::Fat32
    }

    /// The root directory's first cluster on FAT32 (`ROOT` elsewhere).
    fn root_start(&self) -> u32 {
        if self.fat32() {
            self.geo.root_cluster
        } else {
            ROOT
        }
    }

    /// Whether `start` names the root directory.
    #[must_use]
    pub fn is_root(&self, start: u32) -> bool {
        start == ROOT || (self.fat32() && start == self.geo.root_cluster)
    }

    /// Calls `f` with each slot of the directory starting at cluster
    /// `start` (`ROOT` for the root) from byte `from` on, until it returns
    /// true.
    fn walk(
        &self,
        cx: &B::Cx,
        start: u32,
        from: u64,
        f: &mut dyn FnMut(&Slot<'_>) -> bool,
    ) -> Result<(), Error> {
        let g = &self.geo;
        if start == ROOT && !self.fat32() {
            let len = usize::try_from(g.root_bytes).map_err(|_| Error::Io)?;
            let mut stop = false;
            self.io.read(cx, g.root_start, len, &mut |b| {
                let mut off = from;
                while off < g.root_bytes && !stop {
                    let at = usize::try_from(off).unwrap_or(0);
                    let loc = Loc {
                        dirclust: ROOT,
                        diroffset: u32::try_from(off).unwrap_or(0),
                    };
                    stop = f(&Slot {
                        bytes: &b[at..at + 32],
                        diroff: off,
                        loc,
                    });
                    off += DIRENT;
                }
            })?;
            return Ok(());
        }
        let start = if start == ROOT {
            self.root_start()
        } else {
            start
        };
        let cl = u64::from(g.cluster);
        let mut index = from / cl;
        let Some(mut cn) = self.chain_nth(cx, start, index)? else {
            return Ok(());
        };
        loop {
            let mut stop = false;
            let base = index * cl;
            self.io
                .read(cx, g.cluster_offset(cn), g.cluster as usize, &mut |b| {
                    let mut off = from.saturating_sub(base);
                    while off < cl && !stop {
                        let at = usize::try_from(off).unwrap_or(0);
                        stop = f(&Slot {
                            bytes: &b[at..at + 32],
                            diroff: base + off,
                            loc: Loc {
                                dirclust: cn,
                                diroffset: u32::try_from(off).unwrap_or(0),
                            },
                        });
                        off += DIRENT;
                    }
                })?;
            if stop {
                return Ok(());
            }
            let next = self.fat_get(cx, cn)?;
            if g.is_eof(next) {
                return Ok(());
            }
            if next < CLUST_FIRST || next > g.max_cluster {
                return Err(Error::Corrupt("directory chain"));
            }
            cn = next;
            index += 1;
        }
    }

    /// Calls `f` with each named entry of directory `start` from byte
    /// `from`, with its long name when one checks out, until it returns
    /// true. Volume labels are left out.
    pub fn entries(
        &self,
        cx: &B::Cx,
        start: u32,
        from: u64,
        f: &mut dyn FnMut(&Name) -> bool,
    ) -> Result<(), Error> {
        let fat32 = self.fat32();
        // The long name being gathered: its checksum, the next slot
        // number expected, and the pieces from the last back.
        let mut chk: Option<u8> = None;
        let mut want = 0u8;
        let mut pieces: Vec<Vec<u8>> = Vec::new();
        let mut first_slot = 0u64;
        self.walk(cx, start, from, &mut |s| {
            let b = s.bytes;
            match b[0] {
                SLOT_EMPTY => return true,
                SLOT_DELETED => {
                    chk = None;
                    return false;
                }
                _ => {}
            }
            if b[11] == ATTR_WIN95 {
                let cnt = b[0] & 0x3f;
                if b[0] & 0x40 != 0
                    && cnt >= 1
                    && usize::from(cnt) <= conv::WIN_MAXLEN.div_ceil(conv::WIN_CHARS)
                {
                    chk = Some(b[13]);
                    want = cnt;
                    pieces.clear();
                    first_slot = s.diroff;
                } else if chk != Some(b[13]) || cnt != want || cnt == 0 {
                    chk = None;
                    return false;
                }
                match conv::win2unix_piece(b) {
                    Some(p) if chk.is_some() => {
                        pieces.push(p);
                        want -= 1;
                    }
                    _ => chk = None,
                }
                return false;
            }
            if b[11] & ATTR_VOLUME != 0 {
                chk = None;
                return false;
            }
            let entry = Entry::parse(b, fat32);
            let long = match chk {
                Some(c) if want == 0 && c == conv::win_chksum(&entry.name) => {
                    let mut n = Vec::new();
                    for p in pieces.iter().rev() {
                        n.extend_from_slice(p);
                    }
                    Some(n)
                }
                _ => None,
            };
            chk = None;
            let slots = if long.is_some() {
                u32::try_from((s.diroff - first_slot) / DIRENT).unwrap_or(0)
            } else {
                0
            };
            let name = long.unwrap_or_else(|| conv::dos2unixfn(&entry.name, entry.lcase));
            f(&Name {
                name,
                entry,
                loc: s.loc,
                diroff: s.diroff,
                slots,
            })
        })
    }

    /// The entry `name` names in directory `start` (FreeBSD's
    /// `msdosfs_lookup_ino`): its long name matches, ignoring case, or
    /// (when it needs no generation number) its short name does.
    pub fn lookup(&self, cx: &B::Cx, start: u32, name: &[u8]) -> Result<Option<Name>, Error> {
        let (dos, conv) = conv::unix2dosfn(name, 0);
        if conv == Conv::Invalid {
            return Ok(None);
        }
        let olddos = conv != Conv::LongNumbered;
        let mut found = None;
        self.entries(cx, start, 0, &mut |n| {
            let long_ok = n.slots > 0 && conv::win_matches(&n.name, name);
            if long_ok || (olddos && n.entry.name == dos) {
                found = Some(n.clone());
                return true;
            }
            false
        })?;
        Ok(found)
    }

    /// The short entry at `loc`.
    pub fn entry_at(&self, cx: &B::Cx, loc: Loc) -> Result<Entry, Error> {
        let (at, len, within) = self.slot_block(loc);
        let mut e = None;
        let fat32 = self.fat32();
        self.io.read(cx, at, len, &mut |b| {
            e = Some(Entry::parse(&b[within..within + 32], fat32))
        })?;
        e.ok_or(Error::Io)
    }

    /// The block holding the entry at `loc` (a cluster, or the fixed
    /// root), and the entry's offset in it.
    fn slot_block(&self, loc: Loc) -> (u64, usize, usize) {
        let g = &self.geo;
        if loc.dirclust == ROOT {
            (
                g.root_start,
                usize::try_from(g.root_bytes).unwrap_or(0),
                loc.diroffset as usize,
            )
        } else {
            (
                g.cluster_offset(loc.dirclust),
                g.cluster as usize,
                loc.diroffset as usize,
            )
        }
    }

    /// Rewrites the short entry at `loc` (FreeBSD's `deupdat`), now if
    /// `mode` says so.
    pub fn write_entry(&self, cx: &B::Cx, loc: Loc, e: &Entry, mode: Mode) -> Result<(), Error> {
        let (at, len, within) = self.slot_block(loc);
        let fat32 = self.fat32();
        self.io.modify(cx, at, len, Fill::Read, mode, &mut |b| {
            e.write(&mut b[within..within + 32], fat32)
        })
    }

    /// The location of the slot at byte `diroff` of directory `start`.
    fn loc_of(&self, cx: &B::Cx, start: u32, diroff: u64) -> Result<Option<Loc>, Error> {
        if start == ROOT && !self.fat32() {
            return Ok((diroff < self.geo.root_bytes).then(|| Loc {
                dirclust: ROOT,
                diroffset: u32::try_from(diroff).unwrap_or(0),
            }));
        }
        let start = if start == ROOT {
            self.root_start()
        } else {
            start
        };
        let cl = u64::from(self.geo.cluster);
        Ok(self.chain_nth(cx, start, diroff / cl)?.map(|cn| Loc {
            dirclust: cn,
            diroffset: u32::try_from(diroff % cl).unwrap_or(0),
        }))
    }

    /// Whether directory `start` holds nothing but `.` and `..` (FreeBSD's
    /// `dosdirempty`).
    pub fn dir_empty(&self, cx: &B::Cx, start: u32) -> Result<bool, Error> {
        let mut empty = true;
        self.walk(cx, start, 0, &mut |s| {
            let b = s.bytes;
            match b[0] {
                SLOT_EMPTY => return true,
                SLOT_DELETED => return false,
                _ => {}
            }
            if b[11] & ATTR_VOLUME != 0 {
                return false;
            }
            if &b[..11] != b".          " && &b[..11] != b"..         " {
                empty = false;
                return true;
            }
            false
        })?;
        Ok(empty)
    }

    /// A short name for `name` no entry of directory `start` has
    /// (FreeBSD's `uniqdosname`): generation numbers from 1 on.
    fn uniq_dos_name(&self, cx: &B::Cx, start: u32, name: &[u8]) -> Result<[u8; 11], Error> {
        for generation in 1.. {
            let (dn, conv) = conv::unix2dosfn(name, generation);
            if conv == Conv::Invalid {
                return Err(if generation == 1 {
                    Error::BadName
                } else {
                    Error::Exists
                });
            }
            let mut taken = false;
            self.walk(cx, start, 0, &mut |s| {
                if s.bytes[0] == SLOT_EMPTY {
                    return true;
                }
                if s.bytes[11] & ATTR_VOLUME == 0 && s.bytes[..11] == dn {
                    taken = true;
                    return true;
                }
                false
            })?;
            if !taken {
                return Ok(dn);
            }
        }
        Err(Error::Exists)
    }

    /// Where a new entry needing `need` slots goes in directory `start`
    /// (FreeBSD's `msdosfs_lookup_ino` for `CREATE`): the short entry's
    /// offset, the end of the first run of free slots that long, or past
    /// the end.
    fn free_slots(&self, cx: &B::Cx, start: u32, need: u64) -> Result<u64, Error> {
        let (mut count, mut at, mut end) = (0u64, 0u64, 0u64);
        let mut done = false;
        self.walk(cx, start, 0, &mut |s| {
            end = s.diroff + DIRENT;
            match s.bytes[0] {
                SLOT_EMPTY | SLOT_DELETED => {
                    if count < need {
                        count += 1;
                        at = s.diroff;
                    }
                    if s.bytes[0] == SLOT_EMPTY {
                        end = s.diroff;
                        done = true;
                        return true;
                    }
                }
                _ => {
                    if count < need {
                        count = 0;
                    }
                }
            }
            false
        })?;
        let _ = done;
        if count == 0 {
            count = 1;
            at = end;
        }
        if need > count {
            at += DIRENT * (need - count);
        }
        Ok(at)
    }

    /// The directory's size in bytes: its chain's, or the fixed root's.
    pub fn dir_size(&self, cx: &B::Cx, start: u32) -> Result<u64, Error> {
        if start == ROOT && !self.fat32() {
            return Ok(self.geo.root_bytes);
        }
        let start = if start == ROOT {
            self.root_start()
        } else {
            start
        };
        Ok(self.chain_len(cx, start)? * u64::from(self.geo.cluster))
    }

    /// Makes an entry `name` in directory `start` describing `e` (whose
    /// name field is replaced), with long-name slots unless the name is
    /// its own short name (FreeBSD's `createde`); the directory grows if
    /// need be (but FAT12/16's root cannot: `NoSpace`). The blocks are
    /// written now. Returns where the short entry went.
    pub fn create_entry(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        start: u32,
        name: &[u8],
        e: &Entry,
    ) -> Result<Name, Error> {
        let (_, conv) = conv::unix2dosfn(name, 0);
        let slots = match conv {
            Conv::Invalid => return Err(Error::BadName),
            Conv::Exact => 0,
            Conv::Long | Conv::LongNumbered => {
                let n = conv::win_slots(name);
                if n == 0 {
                    return Err(Error::NameTooLong);
                }
                u64::try_from(n).unwrap_or(0)
            }
        };
        let mut e = e.clone();
        e.name = self.uniq_dos_name(cx, start, name)?;
        e.lcase = 0;
        let at = self.free_slots(cx, start, slots + 1)?;
        let size = self.dir_size(cx, start)?;
        if at + DIRENT > size {
            if start == ROOT && !self.fat32() {
                return Err(Error::NoSpace);
            }
            let dstart = if start == ROOT {
                self.root_start()
            } else {
                start
            };
            let more = (at + DIRENT - size).div_ceil(u64::from(self.geo.cluster));
            let last_index = size / u64::from(self.geo.cluster) - 1;
            let last = self
                .chain_nth(cx, dstart, last_index)?
                .ok_or(Error::Corrupt("directory chain"))?;
            self.extend(
                cx,
                a,
                last,
                u32::try_from(more).map_err(|_| Error::NoSpace)?,
                true,
            )?;
        }
        let chksum = conv::win_chksum(&e.name);
        let loc = self
            .loc_of(cx, start, at)?
            .ok_or(Error::Corrupt("directory chain"))?;
        self.write_entry(cx, loc, &e, Mode::Sync)?;
        for cnt in 1..=slots {
            let off = at - cnt * DIRENT;
            let l = self
                .loc_of(cx, start, off)?
                .ok_or(Error::Corrupt("directory chain"))?;
            let slot = conv::unix2winfn(name, usize::try_from(cnt).unwrap_or(1), chksum);
            let (bat, len, within) = self.slot_block(l);
            self.io
                .modify(cx, bat, len, Fill::Read, Mode::Sync, &mut |b| {
                    b[within..within + 32].copy_from_slice(&slot);
                })?;
        }
        Ok(Name {
            name: name.to_vec(),
            entry: e,
            loc,
            diroff: at,
            slots: u32::try_from(slots).unwrap_or(0),
        })
    }

    /// Removes the entry `n` of directory `start`, and its long-name slots
    /// (FreeBSD's `removede`). The blocks are written now.
    pub fn remove_entry(&self, cx: &B::Cx, start: u32, n: &Name) -> Result<(), Error> {
        for k in 0..=u64::from(n.slots) {
            let Some(off) = n.diroff.checked_sub(k * DIRENT) else {
                break;
            };
            let l = self
                .loc_of(cx, start, off)?
                .ok_or(Error::Corrupt("directory chain"))?;
            let (at, len, within) = self.slot_block(l);
            let mut stop = false;
            self.io
                .modify(cx, at, len, Fill::Read, Mode::Sync, &mut |b| {
                    if k > 0 && b[within + 11] != ATTR_WIN95 {
                        stop = true;
                        return;
                    }
                    b[within] = SLOT_DELETED;
                })?;
            if stop {
                break;
            }
        }
        Ok(())
    }

    /// Writes a new directory's first cluster: `.` and `..` (`parent`, 0
    /// for the root), with `e`'s times (FreeBSD's `msdosfs_mkdir`), now.
    pub fn init_dir(&self, cx: &B::Cx, cluster: u32, parent: u32, e: &Entry) -> Result<(), Error> {
        let fat32 = self.fat32();
        let parent = if self.is_root(parent) { ROOT } else { parent };
        let g = &self.geo;
        self.io.modify(
            cx,
            g.cluster_offset(cluster),
            g.cluster as usize,
            Fill::Zero,
            Mode::Sync,
            &mut |b| {
                let mut dot = e.clone();
                dot.name = *b".          ";
                dot.attr = ATTR_DIRECTORY;
                dot.lcase = 0;
                dot.size = 0;
                dot.start = cluster;
                dot.write(&mut b[..32], fat32);
                let mut dotdot = dot.clone();
                dotdot.name = *b"..         ";
                dotdot.start = parent;
                dotdot.write(&mut b[32..64], fat32);
            },
        )
    }

    /// Points a directory's `..` (in its first cluster) at `parent`.
    pub fn set_dotdot(&self, cx: &B::Cx, dir: u32, parent: u32) -> Result<(), Error> {
        let loc = Loc {
            dirclust: dir,
            diroffset: 32,
        };
        let mut e = self.entry_at(cx, loc)?;
        e.start = if self.is_root(parent) { ROOT } else { parent };
        self.write_entry(cx, loc, &e, Mode::Sync)
    }
}
