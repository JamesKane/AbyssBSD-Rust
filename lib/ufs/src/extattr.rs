// SPDX-License-Identifier: BSD-2-Clause
//! Extended attributes as UFS2 keeps them: records (`struct extattr`) in
//! the inode's extended attribute area, which lives in up to [`NXADDR`]
//! blocks of its own (`di_extb`, `di_extsize`), the last one only the
//! fragments it needs, as a small file's last direct block.
//!
//! A record is its length (`ea_length`, the whole record), namespace,
//! content padding and name length, the name (no NUL) padded to 8 bytes,
//! then the content padded to 8 bytes. A record shorter than its header
//! ends the area: what follows is a zeroed tail.
//!
//! The area is rewritten whole on every change, as FreeBSD's
//! `ffs_close_ea` does: at least as long as it was, its tail zeroed, so it
//! only grows until its last attribute goes, when its blocks are freed
//! (`ffs_truncate` with `IO_EXT`). Its blocks are written now, then the
//! inode, now (`ffs_extwrite` with `IO_SYNC`). Their pointers go through
//! soft updates as a file's direct blocks do (FreeBSD's
//! `softdep_setup_allocext`): each new or moved block is placed in its
//! inode slot, and an inode written before the block's group is rolled
//! back to what it pointed at, `di_extsize` with it; freed blocks wait
//! for the inode to be on the disk without them.
//!
//! Behavior reference: sys/ufs/ufs/extattr.h, sys/ufs/ffs/ffs_vnops.c
//! (`ffs_findextattr`, `ffs_rdextattr`, `ffs_close_ea`, `ffs_extwrite`,
//! `ffs_getextattr`, `ffs_setextattr`, `ffs_deleteextattr`,
//! `ffs_listextattr`), sys/ufs/ffs/ffs_balloc.c (`ffs_balloc_ufs2` with
//! `IO_EXT`), sys/ufs/ffs/ffs_inode.c (`ffs_truncate` with `IO_EXT`).

use alloc::vec::Vec;

use crate::allocate::{Reserve, Summary};
use crate::inode::{Inode, Timestamp};
use crate::softdep::{Slot, Work};
use crate::{BlockIo, EXT_SLOT, Error, Fill, Mode, NXADDR, Ufs, idx, idx64, int64, len64};

/// `EXTATTR_NAMESPACE_USER`.
pub const NAMESPACE_USER: u8 = 1;
/// `EXTATTR_NAMESPACE_SYSTEM`.
pub const NAMESPACE_SYSTEM: u8 = 2;

/// A record's header before the name: length, namespace, content
/// padding, name length (`sizeof(struct extattr) - 1`).
const HEADER: usize = 7;
/// The shortest record (`sizeof(struct extattr)`); anything shorter ends
/// the area.
const MIN_RECORD: usize = 8;

/// 512-byte units (`DEV_BSIZE`), which `di_blocks` counts.
const DEV_BSIZE: u64 = 512;

/// One record of an area.
#[derive(Clone, Copy, Debug)]
struct Record<'a> {
    /// Where it starts, and its length (`ea_length`).
    at: usize,
    len: usize,
    namespace: u8,
    name: &'a [u8],
    content: &'a [u8],
}

/// The bytes before a record's content (`EXTATTR_BASE_LENGTH`).
fn base_length(name_len: usize) -> usize {
    (HEADER + name_len).next_multiple_of(8)
}

/// The records of `area`, checked as FreeBSD's `ffs_rdextattr` checks
/// them: one shorter than a header ends the area, one past its end is
/// corrupt; here also one whose name or content overruns it.
fn records(area: &[u8]) -> Result<Vec<Record<'_>>, Error> {
    let bad = Error::Integrity("extended attribute record");
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(rest) = area.get(at..).filter(|r| r.len() >= 4) {
        let len = idx(u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]));
        if len < MIN_RECORD {
            break;
        }
        let rec = rest.get(..len).ok_or(bad)?;
        let (namespace, pad, name_len) = (rec[4], usize::from(rec[5]), usize::from(rec[6]));
        let base = base_length(name_len);
        if base + pad > len {
            return Err(bad);
        }
        out.push(Record {
            at,
            len,
            namespace,
            name: &rec[HEADER..HEADER + name_len],
            content: &rec[base..len - pad],
        });
        at += len;
    }
    Ok(out)
}

/// How much of `area` its records fill: past that is a zeroed tail.
fn used(area: &[u8]) -> Result<usize, Error> {
    Ok(records(area)?.last().map_or(0, |r| r.at + r.len))
}

/// The record setting `name` in `namespace` to `value`, laid out as
/// FreeBSD's `ffs_setextattr` lays it (`EXTATTR_SET_LENFIELD` and the
/// paddings).
fn record(namespace: u8, name: &[u8], value: &[u8]) -> Result<Vec<u8>, Error> {
    let name_len = u8::try_from(name.len()).map_err(|_| Error::NameTooLong)?;
    let base = base_length(name.len());
    let content = value.len().next_multiple_of(8);
    let total = u32::try_from(base + content).map_err(|_| Error::Invalid)?;
    let pad = u8::try_from(content - value.len()).map_err(|_| Error::Invalid)?;
    let mut r = Vec::new();
    r.try_reserve_exact(base + content)
        .map_err(|_| Error::NoSpace)?;
    r.extend_from_slice(&total.to_le_bytes());
    r.extend_from_slice(&[namespace, pad, name_len]);
    r.extend_from_slice(name);
    r.resize(base, 0);
    r.extend_from_slice(value);
    r.resize(base + content, 0);
    Ok(r)
}

/// The content of attribute `name` in `namespace` of `area` (FreeBSD's
/// `ffs_findextattr`).
pub fn find(area: &[u8], namespace: u8, name: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    Ok(records(area)?
        .iter()
        .find(|r| r.namespace == namespace && r.name == name)
        .map(|r| r.content.to_vec()))
}

/// The names in `namespace` of `area`, in record order.
pub fn names(area: &[u8], namespace: u8) -> Result<Vec<Vec<u8>>, Error> {
    Ok(records(area)?
        .iter()
        .filter(|r| r.namespace == namespace)
        .map(|r| r.name.to_vec())
        .collect())
}

/// Sets attribute `name` in `namespace` of `area` (its records only) to
/// `value`: an existing one is rewritten where it is, the records after
/// it moving to fit; a new one goes at the end (FreeBSD's
/// `ffs_setextattr`).
pub fn set(area: &mut Vec<u8>, namespace: u8, name: &[u8], value: &[u8]) -> Result<(), Error> {
    let new = record(namespace, name, value)?;
    let old = records(area)?
        .iter()
        .find(|r| r.namespace == namespace && r.name == name)
        .map(|r| (r.at, r.len));
    match old {
        Some((at, len)) => {
            area.splice(at..at + len, new);
        }
        None => {
            area.try_reserve(new.len()).map_err(|_| Error::NoSpace)?;
            area.extend_from_slice(&new);
        }
    }
    Ok(())
}

/// Removes attribute `name` in `namespace` from `area` (its records
/// only), the records after it moving up (FreeBSD's
/// `ffs_deleteextattr`); false if there is none.
pub fn remove(area: &mut Vec<u8>, namespace: u8, name: &[u8]) -> Result<bool, Error> {
    let old = records(area)?
        .iter()
        .find(|r| r.namespace == namespace && r.name == name)
        .map(|r| (r.at, r.len));
    let Some((at, len)) = old else {
        return Ok(false);
    };
    area.drain(at..at + len);
    Ok(true)
}

impl<B: BlockIo> Ufs<B> {
    /// The most an area may hold (`UFS_NXADDR * fs_bsize`).
    fn ext_limit(&self) -> u64 {
        len64(NXADDR) * u64::from(self.sb.bsize)
    }

    /// Inode `ip`'s extended attribute records, without the zeroed tail
    /// (FreeBSD's `ffs_rdextattr` over `ffs_extread`). A file with no area
    /// has none; an area larger than the format allows is `EFBIG`.
    pub fn read_extattrs(&self, cx: &B::Cx, ip: &Inode) -> Result<Vec<u8>, Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let size = u64::from(ip.extsize);
        if size > self.ext_limit() {
            return Err(Error::TooBig);
        }
        let mut out = Vec::new();
        out.try_reserve_exact(idx64(size))
            .map_err(|_| Error::NoSpace)?;
        out.resize(idx64(size), 0);
        for (lbn, &addr) in ip.extb.iter().enumerate() {
            let start = len64(lbn) * bsize;
            if start >= size || addr == 0 {
                continue;
            }
            if addr >= sb.size {
                return Err(Error::Corrupt("extended attribute block address"));
            }
            let n = idx64((size - start).min(bsize));
            let space = idx(sb.block_size(size, len64(lbn)));
            let to = &mut out[idx64(start)..idx64(start) + n];
            self.io.read(cx, addr * fsize, space, &mut |b| {
                to.copy_from_slice(&b[..n])
            })?;
        }
        let n = used(&out)?;
        out.truncate(n);
        Ok(out)
    }

    /// The content of inode `ip`'s attribute `name` in `namespace`, if it
    /// has one (FreeBSD's `ffs_getextattr`).
    pub fn get_extattr(
        &self,
        cx: &B::Cx,
        ip: &Inode,
        namespace: u8,
        name: &[u8],
    ) -> Result<Option<Vec<u8>>, Error> {
        find(&self.read_extattrs(cx, ip)?, namespace, name)
    }

    /// The names of inode `ip`'s attributes in `namespace`, in the area's
    /// order (FreeBSD's `ffs_listextattr`).
    pub fn list_extattr(
        &self,
        cx: &B::Cx,
        ip: &Inode,
        namespace: u8,
    ) -> Result<Vec<Vec<u8>>, Error> {
        names(&self.read_extattrs(cx, ip)?, namespace)
    }

    /// Sets file `ino`'s attribute `name` in `namespace` to `value`
    /// (FreeBSD's `ffs_setextattr`): a value longer than the area may be
    /// is `EINVAL`, an area that would grow past it `ENOSPC`. The inode
    /// is written, its change time `now`.
    #[allow(clippy::too_many_arguments)]
    pub fn set_extattr(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        namespace: u8,
        name: &[u8],
        value: &[u8],
        reserve: Reserve,
        now: Timestamp,
    ) -> Result<(), Error> {
        if name.is_empty() || len64(value.len()) > self.ext_limit() {
            return Err(Error::Invalid);
        }
        let old = self.read_extattrs(cx, ip)?;
        let mut area = old.clone();
        set(&mut area, namespace, name, value)?;
        if len64(area.len()) > self.ext_limit() {
            return Err(Error::NoSpace);
        }
        self.commit_extattrs(cx, sum, ino, ip, &old, &area, reserve, now)
    }

    /// Removes file `ino`'s attribute `name` in `namespace` (FreeBSD's
    /// `ffs_deleteextattr`); false if it has none, and nothing changes.
    /// The inode is written, its change time `now`.
    #[allow(clippy::too_many_arguments)]
    pub fn delete_extattr(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        namespace: u8,
        name: &[u8],
        now: Timestamp,
    ) -> Result<bool, Error> {
        let old = self.read_extattrs(cx, ip)?;
        let mut area = old.clone();
        if !remove(&mut area, namespace, name)? {
            return Ok(false);
        }
        self.commit_extattrs(cx, sum, ino, ip, &old, &area, Reserve::Allowed, now)?;
        Ok(true)
    }

    /// Writes `area` as file `ino`'s records, which were `old` (FreeBSD's
    /// `ffs_close_ea` committing): over the whole area, which grows if it
    /// must, its tail zeroed; with no records left its blocks go. Out of
    /// space partway, the area keeps what it got, holding `old`.
    #[allow(clippy::too_many_arguments)]
    fn commit_extattrs(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        old: &[u8],
        area: &[u8],
        reserve: Reserve,
        now: Timestamp,
    ) -> Result<(), Error> {
        if area.is_empty() {
            ip.ctime = now;
            return self.ext_truncate(cx, sum, ino, ip);
        }
        let before = (ip.extsize, ip.extb);
        let want = len64(area.len()).max(u64::from(ip.extsize));
        let grown = self.ext_grow(cx, sum, ino, ip, want, reserve);
        if grown.is_err() && (ip.extsize, ip.extb) == before {
            return grown;
        }
        let contents = if grown.is_ok() { area } else { old };
        self.ext_write(cx, ip, contents)?;
        ip.ctime = now;
        self.write_inode(cx, ino, ip, Mode::Sync)?;
        grown
    }

    /// Gives file `ino`'s area the space for its first `want` bytes, a
    /// block at a time (`ffs_extwrite`'s calls of `ffs_balloc_ufs2` with
    /// `IO_EXT`).
    fn ext_grow(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        want: u64,
        reserve: Reserve,
    ) -> Result<(), Error> {
        let bsize = u64::from(self.sb.bsize);
        for lbn in 0..want.div_ceil(bsize) {
            let len = (want - lbn * bsize).min(bsize);
            self.ext_balloc(cx, sum, ino, ip, lbn, len, reserve)?;
            let end = lbn * bsize + len;
            if end > u64::from(ip.extsize) {
                ip.extsize = u32::try_from(end).map_err(|_| Error::TooBig)?;
            }
        }
        Ok(())
    }

    /// Area block `lbn` gets space for its first `len` bytes (FreeBSD's
    /// `ffs_balloc_ufs2` with `IO_EXT`): a fragment ending the area grows
    /// to a whole block first if the area now reaches past it; a block
    /// that is the area's last grows its fragments (in place, or moved),
    /// or a new one is allocated. New pointers go to the tracker in the
    /// inode's extended attribute slots.
    #[allow(clippy::too_many_arguments)]
    fn ext_balloc(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
        lbn: u64,
        len: u64,
        reserve: Reserve,
    ) -> Result<(), Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let prev = |ip: &Inode, i: u64| if i == 0 { 0 } else { ip.extb[idx64(i) - 1] };
        let place = |new: u64, i: u64, old: (u64, u64), free_old: Option<(u64, u64)>| {
            self.soft(cx, |d| {
                let slot = Slot::Inode {
                    ino,
                    index: EXT_SLOT + idx64(i),
                };
                d.place(new, slot, old.0, old.1, free_old);
            });
        };
        let size = u64::from(ip.extsize);
        let last = size / bsize;
        if last < lbn {
            let osize = u64::from(sb.block_size(size, last));
            let old = ip.extb[idx64(last)];
            if osize < bsize && size % bsize != 0 && old != 0 {
                let pref = self.blkpref(sum, ino, ip, last, int64(last), prev(ip, last));
                let (new, free_old) =
                    self.realloccg(cx, sum, ino, old, pref, osize, bsize, reserve, true, false)?;
                ip.blocks += (bsize - osize) / DEV_BSIZE;
                ip.extb[idx64(last)] = new;
                ip.extsize = u32::try_from((last + 1) * bsize).map_err(|_| Error::TooBig)?;
                place(new, last, (old, osize), free_old);
            }
        }
        let size = u64::from(ip.extsize);
        let nb = ip.extb[idx64(lbn)];
        if nb != 0 && size >= (lbn + 1) * bsize {
            return Ok(());
        }
        let pref = self.blkpref(sum, ino, ip, lbn, int64(lbn), prev(ip, lbn));
        if nb != 0 {
            let osize = sb.fragroundup(size % bsize);
            let nsize = sb.fragroundup(len);
            if nsize <= osize {
                return Ok(());
            }
            let (new, free_old) =
                self.realloccg(cx, sum, ino, nb, pref, osize, nsize, reserve, true, false)?;
            ip.blocks += (nsize - osize) / DEV_BSIZE;
            ip.extb[idx64(lbn)] = new;
            place(new, lbn, (nb, osize), free_old);
            return Ok(());
        }
        let nsize = if size < (lbn + 1) * bsize {
            sb.fragroundup(len)
        } else {
            bsize
        };
        let new = self.alloc(cx, sum, ino, pref, nsize, reserve)?;
        ip.blocks += nsize / DEV_BSIZE;
        ip.extb[idx64(lbn)] = new;
        place(new, lbn, (0, 0), None);
        Ok(())
    }

    /// Writes `contents` over inode `ip`'s whole area, zeros past it, each
    /// block now (`ffs_extwrite` with `IO_SYNC`); with soft updates, the
    /// pointers to new blocks wait for these writes.
    fn ext_write(&self, cx: &B::Cx, ip: &Inode, contents: &[u8]) -> Result<(), Error> {
        let sb = &self.sb;
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let size = u64::from(ip.extsize);
        for lbn in 0..size.div_ceil(bsize) {
            let addr = ip.extb[idx64(lbn)];
            if addr == 0 {
                return Err(Error::Corrupt("hole in an extended attribute area"));
            }
            let at = addr * fsize;
            let space = idx(sb.block_size(size, lbn));
            let start = idx64(lbn * bsize).min(contents.len());
            let part = &contents[start..(start + space).min(contents.len())];
            self.io
                .modify(cx, at, space, Fill::Zero, Mode::Sync, &mut |b| {
                    b[..part.len()].copy_from_slice(part);
                    b[part.len()..].fill(0);
                    self.soft(cx, |d| d.contents(addr, at));
                })?;
        }
        Ok(())
    }

    /// Frees file `ino`'s extended attribute area (FreeBSD's
    /// `ffs_truncate` with `IO_EXT`): the inode loses its pointers and is
    /// written, then the blocks go; with soft updates they go once the
    /// inode is on the disk without them.
    pub fn ext_truncate(
        &self,
        cx: &B::Cx,
        sum: &mut Summary,
        ino: u32,
        ip: &mut Inode,
    ) -> Result<(), Error> {
        let sb = &self.sb;
        let fsize = u64::from(sb.fsize);
        if ip.extsize == 0 && ip.extb == [0; NXADDR] {
            return Ok(());
        }
        let size = u64::from(ip.extsize);
        let bsize = u64::from(sb.bsize);
        // Pointers past the size are not the file's (fsck counts only
        // those within it): they are cleared, not freed.
        let mut frees: Vec<(u64, u64)> = ip
            .extb
            .iter()
            .enumerate()
            .filter(|&(i, &bn)| bn != 0 && len64(i) * bsize < size)
            .map(|(i, &bn)| (bn, u64::from(sb.block_size(size, len64(i)))))
            .collect();
        if frees.iter().any(|&(bn, _)| bn >= sb.size) {
            return Err(Error::Corrupt("extended attribute block address"));
        }
        let released: u64 = frees.iter().map(|&(_, n)| n / DEV_BSIZE).sum();
        ip.extb = [0; NXADDR];
        ip.extsize = 0;
        ip.blocks = ip.blocks.saturating_sub(released);
        if self.soft {
            let mut moved = Vec::new();
            self.soft(cx, |d| {
                for i in 0..NXADDR {
                    moved.extend(d.cancel_slot(Slot::Inode {
                        ino,
                        index: EXT_SLOT + i,
                    }));
                }
                for &(bn, _) in &frees {
                    moved.extend(d.forget_buffer(bn * fsize, ino));
                }
            });
            for &(bn, n) in &frees {
                self.io.forget(cx, bn * fsize, idx64(n));
            }
            frees.extend(moved);
            let Some(id) = self.soft(cx, |d| {
                let id = d.defer(Work::Free(frees));
                d.wait_copy(id, ino);
                id
            }) else {
                return Err(Error::ReadOnly);
            };
            let r = self.write_inode(cx, ino, ip, Mode::Delayed);
            self.soft(cx, |d| d.arm(id));
            return r;
        }
        self.write_inode(cx, ino, ip, Mode::Sync)?;
        for (bn, n) in frees {
            self.blkfree(cx, sum, bn, n)?;
        }
        Ok(())
    }
}
