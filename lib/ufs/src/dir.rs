// SPDX-License-Identifier: BSD-2-Clause
//! Directory blocks (`struct direct`): entries packed in 512-byte chunks
//! that none crosses, each with its inode, record length, type, and name.

use crate::softdep::{Gone, Id, Removal};
use crate::{Error, count32};

/// A directory chunk (`DIRBLKSIZ`).
pub const DIRBLKSIZ: usize = 512;
/// The longest name (`UFS_MAXNAMLEN`).
pub const MAXNAMLEN: usize = 255;
/// The header before the name: inode, record length, type, name length.
const HEADER: usize = 8;

/// One directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirEntry<'a> {
    pub ino: u32,
    /// `DT_*`.
    pub kind: u8,
    pub name: &'a [u8],
    /// The byte offset of this entry in the directory, and of the next.
    pub offset: u64,
    pub next: u64,
}

/// The space an entry with an `n`-byte name needs (`DIRSIZ`).
#[must_use]
pub const fn dirsiz(n: usize) -> usize {
    (HEADER + n + 1 + 3) & !3
}

/// Calls `f` with each live entry of directory block `block`, which
/// starts at byte `base` of the directory, from chunk offset `skip` on,
/// until it returns false. A malformed entry is an error (FreeBSD's
/// `ufs_dirbadentry`), as is a block that is not whole chunks.
pub(crate) fn entries(
    block: &[u8],
    base: u64,
    skip: usize,
    f: &mut dyn FnMut(DirEntry<'_>) -> bool,
) -> Result<(), Error> {
    if !block.len().is_multiple_of(DIRBLKSIZ) {
        return Err(Error::Corrupt("directory size"));
    }
    let mut at = skip;
    while at < block.len() {
        let room = DIRBLKSIZ - at % DIRBLKSIZ;
        let e = &block[at..at + room];
        let ino = u32::from_le_bytes([e[0], e[1], e[2], e[3]]);
        let reclen = usize::from(u16::from_le_bytes([e[4], e[5]]));
        let kind = e[6];
        let namlen = usize::from(e[7]);
        if reclen == 0 || reclen % 4 != 0 || reclen > room {
            return Err(Error::Corrupt("directory entry length"));
        }
        let next = at + reclen;
        if ino != 0 {
            if namlen > MAXNAMLEN || dirsiz(namlen) > reclen {
                return Err(Error::Corrupt("directory entry name length"));
            }
            let name = &e[HEADER..HEADER + namlen];
            if name.contains(&0) || e[HEADER + namlen] != 0 {
                return Err(Error::Corrupt("directory entry name"));
            }
            let entry = DirEntry {
                ino,
                kind,
                name,
                offset: base + crate::len64(at),
                next: base + crate::len64(next),
            };
            if !f(entry) {
                return Ok(());
            }
        }
        at = next;
    }
    Ok(())
}

/// A slot found while scanning a directory: where its block is, where
/// the entry is in the block, and its fields.
struct Slot {
    /// The entry's byte offset in the directory.
    offset: u64,
    addr: u64,
    bsz: usize,
    at: usize,
    ino: u32,
    reclen: usize,
    namlen: usize,
    /// The entry before it in its chunk, if any.
    prev: Option<usize>,
}

/// Writes an entry at `at` of block bytes `b`.
fn put_entry(b: &mut [u8], at: usize, ino: u32, reclen: usize, kind: u8, name: &[u8]) {
    b[at..at + 4].copy_from_slice(&ino.to_le_bytes());
    b[at + 4..at + 6].copy_from_slice(&reclen16(reclen).to_le_bytes());
    b[at + 6] = kind;
    // Names are checked to fit (at most `MAXNAMLEN`).
    b[at + 7] = u8::try_from(name.len()).unwrap_or(u8::MAX);
    b[at + HEADER..at + HEADER + name.len()].copy_from_slice(name);
    let end = at + dirsiz(name.len());
    b[at + HEADER + name.len()..end].fill(0);
}

/// The `DT_*` type of a file of `mode`.
#[must_use]
pub fn dtype_of(mode: u16) -> u8 {
    match mode & 0o170_000 {
        0o010_000 => 1,
        0o020_000 => 2,
        0o040_000 => 4,
        0o060_000 => 6,
        0o100_000 => 8,
        0o120_000 => 10,
        0o140_000 => 12,
        0o160_000 => 14,
        _ => 0,
    }
}

/// A record length (at most `DIRBLKSIZ`) as stored.
fn reclen16(len: usize) -> u16 {
    u16::try_from(len).unwrap_or(u16::MAX)
}

fn check_name(name: &[u8]) -> Result<(), Error> {
    if name.len() > MAXNAMLEN {
        return Err(Error::NameTooLong);
    }
    if name.is_empty() || name.contains(&0) || name.contains(&b'/') {
        return Err(Error::Corrupt("directory entry name"));
    }
    Ok(())
}

impl<B: crate::BlockIo> crate::Ufs<B> {
    /// Calls `f` with every entry (free ones too) of directory `dir`, in
    /// order, until it returns true; returns the slot it stopped at.
    fn scan(
        &self,
        cx: &B::Cx,
        dir: &crate::Inode,
        f: impl FnMut(&Slot, &[u8]) -> bool,
    ) -> Result<Option<Slot>, Error> {
        self.scan_range(cx, dir, 0, dir.size, f)
    }

    /// [`Self::scan`] over chunk `c` alone.
    fn scan_chunk(
        &self,
        cx: &B::Cx,
        dir: &crate::Inode,
        c: u64,
        f: impl FnMut(&Slot, &[u8]) -> bool,
    ) -> Result<Option<Slot>, Error> {
        let chunk = crate::len64(DIRBLKSIZ);
        self.scan_range(cx, dir, c * chunk, (c + 1) * chunk, f)
    }

    /// [`Self::scan`] over the chunks in bytes `[from, to)` of `dir`
    /// (chunk boundaries).
    fn scan_range(
        &self,
        cx: &B::Cx,
        dir: &crate::Inode,
        from: u64,
        to: u64,
        mut f: impl FnMut(&Slot, &[u8]) -> bool,
    ) -> Result<Option<Slot>, Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let to = to.min(dir.size);
        let mut lbn = from / bsize;
        while lbn * bsize < to {
            let bsz = crate::idx(sb.block_size(dir.size, lbn));
            let addr = self.bmap(cx, dir, lbn)?;
            if addr == 0 {
                return Err(Error::Corrupt("hole in a directory"));
            }
            let base = lbn * bsize;
            let limit = crate::idx64(to - base).min(bsz);
            let start = crate::idx64(from.saturating_sub(base));
            let mut found = Ok(None);
            self.io
                .read(cx, addr * u64::from(sb.fsize), bsz, &mut |b| {
                    let mut at = start;
                    let mut prev = None;
                    while at < limit {
                        if at.is_multiple_of(DIRBLKSIZ) {
                            prev = None;
                        }
                        let room = DIRBLKSIZ - at % DIRBLKSIZ;
                        let reclen = usize::from(u16::from_le_bytes([b[at + 4], b[at + 5]]));
                        if reclen == 0 || reclen % 4 != 0 || reclen > room {
                            found = Err(Error::Corrupt("directory entry length"));
                            return;
                        }
                        let slot = Slot {
                            offset: base + crate::len64(at),
                            addr,
                            bsz,
                            at,
                            ino: u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]),
                            reclen,
                            namlen: usize::from(b[at + 7]),
                            prev,
                        };
                        let name_end = (at + HEADER + slot.namlen).min(at + reclen);
                        if f(&slot, &b[at + HEADER..name_end]) {
                            found = Ok(Some(slot));
                            return;
                        }
                        prev = Some(at);
                        at += reclen;
                    }
                })?;
            if let Some(s) = found? {
                return Ok(Some(s));
            }
            lbn += 1;
        }
        Ok(None)
    }

    /// The live entry `name` of directory `dir`: in its chunk alone if
    /// the directory has an index.
    fn find_entry(
        &self,
        cx: &B::Cx,
        dir: &crate::Inode,
        name: &[u8],
    ) -> Result<Option<Slot>, Error> {
        if let Some(h) = &dir.dirhash {
            let Some(&(_, offset)) = h.names.get(name) else {
                return Ok(None);
            };
            let c = offset / crate::len64(DIRBLKSIZ);
            return self.scan_chunk(cx, dir, c, |s, n| s.ino != 0 && n == name);
        }
        self.scan(cx, dir, |s, n| s.ino != 0 && n == name)
    }

    /// The largest free slot of chunk `c` of `dir`.
    fn chunk_free(&self, cx: &B::Cx, dir: &crate::Inode, c: u64) -> Result<u16, Error> {
        let mut most = 0;
        self.scan_chunk(cx, dir, c, |s, _| {
            let free = if s.ino == 0 {
                s.reclen
            } else {
                s.reclen - dirsiz(s.namlen)
            };
            most = most.max(free);
            false
        })?;
        Ok(reclen16(most))
    }

    /// Indexes directory `dir` (FreeBSD's `ufsdirhash_build`) if it has
    /// no index yet and holds at least `min` bytes (the kernel passes
    /// [`crate::dirhash::MIN_SIZE`]).
    pub fn dirhash_build(&self, cx: &B::Cx, dir: &mut crate::Inode, min: u64) -> Result<(), Error> {
        if dir.dirhash.is_some() || dir.size < min {
            return Ok(());
        }
        let mut h = crate::dirhash::DirHash::default();
        let chunks = dir.size.div_ceil(crate::len64(DIRBLKSIZ));
        h.free = alloc::vec![0; crate::idx64(chunks)];
        self.scan(cx, dir, |s, n| {
            let c = crate::idx64(s.offset / crate::len64(DIRBLKSIZ));
            let free = if s.ino == 0 {
                s.reclen
            } else {
                s.reclen - dirsiz(s.namlen)
            };
            if let Some(f) = h.free.get_mut(c) {
                *f = (*f).max(reclen16(free));
            }
            if s.ino != 0 {
                h.names.insert(n.into(), (s.ino, s.offset));
            }
            false
        })?;
        dir.dirhash = Some(alloc::boxed::Box::new(h));
        Ok(())
    }

    /// Adds entry `name` for inode `ino` of type `kind` to directory
    /// `dir_ino` (FreeBSD's `ufs_direnter`): in a free slot or the slack
    /// of an entry if one is big enough, else in a new chunk at the end.
    /// The block is written now (with soft updates, delayed, the entry
    /// waiting as `entry` says); the caller writes the directory's inode
    /// (its size may grow) and the new inode first.
    #[allow(clippy::too_many_arguments)]
    pub fn dir_enter(
        &self,
        cx: &B::Cx,
        sum: &mut crate::Summary,
        dir_ino: u32,
        dir: &mut crate::Inode,
        name: &[u8],
        ino: u32,
        kind: u8,
        reserve: crate::Reserve,
        entry: Option<Id>,
    ) -> Result<(), Error> {
        check_name(name)?;
        let need = dirsiz(name.len());
        let mut exists = false;
        let fits = |s: &Slot| {
            let free = if s.ino == 0 {
                s.reclen
            } else {
                s.reclen - dirsiz(s.namlen)
            };
            free >= need
        };
        let slot = match &dir.dirhash {
            // Indexed: the name is known not to be there, and the free map
            // names a chunk with room.
            Some(h) => {
                if h.names.contains_key(name) {
                    return Err(Error::Exists);
                }
                match h.chunk_with(need) {
                    Some(c) => self.scan_chunk(cx, dir, c, |s, _| fits(s))?,
                    None => None,
                }
            }
            None => self.scan(cx, dir, |s, n| {
                if s.ino != 0 && n == name {
                    exists = true;
                    return true;
                }
                fits(s)
            })?,
        };
        if exists {
            return Err(Error::Exists);
        }
        let sb = &self.sb;
        let fsize = u64::from(sb.fsize);
        let bsize = u64::from(sb.bsize);
        if let Some(s) = slot {
            let at = s.addr * fsize;
            let lbn = s.offset / bsize;
            self.io
                .modify(cx, at, s.bsz, crate::Fill::Read, self.ordered(), &mut |b| {
                    let off = if s.ino == 0 {
                        put_entry(b, s.at, ino, s.reclen, kind, name);
                        s.at
                    } else {
                        let used = dirsiz(s.namlen);
                        b[s.at + 4..s.at + 6].copy_from_slice(&reclen16(used).to_le_bytes());
                        put_entry(b, s.at + used, ino, s.reclen - used, kind, name);
                        s.at + used
                    };
                    if let Some(id) = entry {
                        self.soft(cx, |d| {
                            d.place_entry(
                                id,
                                dir_ino,
                                lbn,
                                at,
                                count32(off),
                                (0, 0),
                                Gone::Link,
                                &Removal::default(),
                            );
                        });
                    }
                })?;
            if dir.dirhash.is_some() {
                let offset = if s.ino == 0 {
                    s.offset
                } else {
                    s.offset + crate::len64(dirsiz(s.namlen))
                };
                let c = s.offset / crate::len64(DIRBLKSIZ);
                let free = self.chunk_free(cx, dir, c)?;
                if let Some(h) = dir.dirhash.as_mut() {
                    h.names.insert(name.into(), (ino, offset));
                    if let Some(f) = h.free.get_mut(crate::idx64(c)) {
                        *f = free;
                    }
                }
            }
            return Ok(());
        }
        // A new chunk at the end.
        let offset = dir.size;
        let addr = self.balloc(
            cx,
            sum,
            dir_ino,
            dir,
            offset,
            crate::len64(DIRBLKSIZ),
            reserve,
            false,
            true,
        )?;
        dir.size = offset + crate::len64(DIRBLKSIZ);
        let lbn = offset / bsize;
        let bsz = crate::idx(sb.block_size(dir.size, lbn));
        let within = crate::idx64(offset % bsize);
        let at = addr * fsize;
        self.io
            .modify(cx, at, bsz, crate::Fill::Read, self.ordered(), &mut |b| {
                b[within..within + DIRBLKSIZ].fill(0);
                put_entry(b, within, ino, DIRBLKSIZ, kind, name);
                self.soft(cx, |d| {
                    // The block's new space is its contents.
                    d.contents(addr, at);
                    if let Some(id) = entry {
                        d.place_entry(
                            id,
                            dir_ino,
                            lbn,
                            at,
                            count32(within),
                            (0, 0),
                            Gone::Link,
                            &Removal::default(),
                        );
                    }
                });
            })?;
        if let Some(h) = dir.dirhash.as_mut() {
            h.names.insert(name.into(), (ino, offset));
            h.free.push(reclen16(DIRBLKSIZ - need));
        }
        Ok(())
    }

    /// Removes entry `name` from directory `dir_ino` (FreeBSD's
    /// `ufs_dirremove`): the first entry of a chunk is marked free, any
    /// other joins the one before it. The block is written now; with soft
    /// updates, delayed, the links the entry held (`rmdir`: of a directory
    /// removed from this one) dropping once it is out. Returns the inode
    /// the entry named, and with soft updates what the removal waits for.
    pub fn dir_remove(
        &self,
        cx: &B::Cx,
        dir_ino: u32,
        dir: &mut crate::Inode,
        name: &[u8],
        rmdir: bool,
    ) -> Result<(u32, Removal), Error> {
        let s = self.find_entry(cx, dir, name)?.ok_or(Error::NotFound)?;
        let fsize = u64::from(self.sb.fsize);
        let at = s.addr * fsize;
        let lbn = s.offset / u64::from(self.sb.bsize);
        let mut removal = Removal::default();
        self.io
            .modify(cx, at, s.bsz, crate::Fill::Read, self.ordered(), &mut |b| {
                match s.prev {
                    None => b[s.at..s.at + 4].fill(0),
                    Some(p) => {
                        let len = usize::from(u16::from_le_bytes([b[p + 4], b[p + 5]])) + s.reclen;
                        b[p + 4..p + 6].copy_from_slice(&reclen16(len).to_le_bytes());
                    }
                }
                self.soft(cx, |d| {
                    let gone = if rmdir {
                        Gone::Dir(dir_ino)
                    } else {
                        Gone::Link
                    };
                    removal = d.remove_entry(dir_ino, lbn, at, count32(s.at), s.ino, gone);
                });
            })?;
        if dir.dirhash.is_some() {
            let c = s.offset / crate::len64(DIRBLKSIZ);
            let free = self.chunk_free(cx, dir, c)?;
            if let Some(h) = dir.dirhash.as_mut() {
                h.names.remove(name);
                if let Some(f) = h.free.get_mut(crate::idx64(c)) {
                    *f = free;
                }
            }
        }
        Ok((s.ino, removal))
    }

    /// Points entry `name` of directory `dir_ino` at inode `ino` of type
    /// `kind` (FreeBSD's `ufs_dirrewrite`: a rename over an existing name,
    /// or a moved directory's `..`). The block is written now; with soft
    /// updates, delayed, the change waiting as `entry` says, and what the
    /// entry held (`gone`) dropping once the change is out and `also` is.
    /// Returns the inode it named before.
    #[allow(clippy::too_many_arguments)]
    pub fn dir_rewrite(
        &self,
        cx: &B::Cx,
        dir_ino: u32,
        dir: &mut crate::Inode,
        name: &[u8],
        ino: u32,
        kind: u8,
        entry: Option<Id>,
        gone: Gone,
        also: &Removal,
    ) -> Result<u32, Error> {
        let s = self.find_entry(cx, dir, name)?.ok_or(Error::NotFound)?;
        let fsize = u64::from(self.sb.fsize);
        let at = s.addr * fsize;
        let lbn = s.offset / u64::from(self.sb.bsize);
        self.io
            .modify(cx, at, s.bsz, crate::Fill::Read, self.ordered(), &mut |b| {
                let old_type = b[s.at + 6];
                b[s.at..s.at + 4].copy_from_slice(&ino.to_le_bytes());
                b[s.at + 6] = kind;
                if let Some(id) = entry {
                    self.soft(cx, |d| {
                        let old = (s.ino, old_type);
                        d.place_entry(id, dir_ino, lbn, at, count32(s.at), old, gone, also);
                    });
                }
            })?;
        if let Some(h) = dir.dirhash.as_mut() {
            h.names.insert(name.into(), (ino, s.offset));
        }
        Ok(s.ino)
    }

    /// Gives new directory `ino` its first chunk, with `.` and `..`
    /// (`parent`) (FreeBSD's `ufs_mkdir`), returning its address. The
    /// block is written now (delayed with soft updates); the caller writes
    /// the inode.
    pub fn dir_init(
        &self,
        cx: &B::Cx,
        sum: &mut crate::Summary,
        ino: u32,
        ip: &mut crate::Inode,
        parent: u32,
        reserve: crate::Reserve,
    ) -> Result<u64, Error> {
        let addr = self.balloc(
            cx,
            sum,
            ino,
            ip,
            0,
            crate::len64(DIRBLKSIZ),
            reserve,
            false,
            true,
        )?;
        ip.size = crate::len64(DIRBLKSIZ);
        let bsz = crate::idx(self.sb.block_size(ip.size, 0));
        let at = addr * u64::from(self.sb.fsize);
        self.io
            .modify(cx, at, bsz, crate::Fill::Zero, self.ordered(), &mut |b| {
                put_entry(b, 0, ino, dirsiz(1), 4, b".");
                put_entry(b, dirsiz(1), parent, DIRBLKSIZ - dirsiz(1), 4, b"..");
                self.soft(cx, |d| d.contents(addr, at));
            })?;
        Ok(addr)
    }

    /// Whether directory `dir` holds nothing but `.` and `..` (FreeBSD's
    /// `ufs_dirempty`).
    pub fn dir_empty(&self, cx: &B::Cx, dir: &crate::Inode) -> Result<bool, Error> {
        let other = self.scan(cx, dir, |s, n| s.ino != 0 && n != b"." && n != b"..")?;
        Ok(other.is_none())
    }
}
