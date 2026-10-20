// SPDX-License-Identifier: BSD-2-Clause
//! File operations over the engine, as FreeBSD's msdosfs performs them:
//! making files and directories, removing them, renaming, and sizing a
//! file's cluster chain. Directory blocks are written synchronously, as
//! FreeBSD's msdosfs does without `async`.
//!
//! Behavior reference: sys/fs/msdosfs/msdosfs_vnops.c (`msdosfs_create`,
//! `msdosfs_mkdir`, `msdosfs_remove`, `msdosfs_rmdir`, `msdosfs_rename`),
//! msdosfs_denode.c (`detrunc`, `deextend`).

use super::dir::{ATTR_ARCHIVE, ATTR_DIRECTORY, Entry, Kind, Name};
use super::time::{self, FatTime};
use super::{Alloc, BlockIo, Error, Fat, Mode, ROOT};

/// The end-of-chain mark (`CLUST_EOFE`).
const EOF: u32 = 0xffff_ffff;

/// A new entry's fields at `now` (FreeBSD's `DETIMES` with `DE_ACCESS`,
/// `DE_CREATE`, and `DE_UPDATE`).
#[must_use]
pub fn new_entry(kind: Kind, now: (i64, u32)) -> Entry {
    let t = time::to_fat(now.0, now.1);
    Entry {
        name: [b' '; 11],
        attr: if kind == Kind::Directory {
            ATTR_DIRECTORY
        } else {
            ATTR_ARCHIVE
        },
        lcase: 0,
        ctime: t,
        mtime: FatTime { hundredths: 0, ..t },
        adate: t.date,
        start: 0,
        size: 0,
    }
}

impl<B: BlockIo> Fat<B> {
    /// Makes a file or directory `name` in directory `dir` (FreeBSD's
    /// `msdosfs_create`, `msdosfs_mkdir`): a directory gets its first
    /// cluster, with `.` and `..`, before the entry names it.
    pub fn create(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        dir: u32,
        name: &[u8],
        kind: Kind,
        now: (i64, u32),
    ) -> Result<Name, Error> {
        let mut e = new_entry(kind, now);
        if kind == Kind::Directory {
            let (cn, _) = self.cluster_alloc(cx, a, 0, 1, EOF)?;
            e.start = cn;
            if let Err(err) = self.init_dir(cx, cn, dir, &e) {
                let _ = self.free_chain(cx, a, cn);
                return Err(err);
            }
        }
        match self.create_entry(cx, a, dir, name, &e) {
            Ok(n) => Ok(n),
            Err(err) => {
                if kind == Kind::Directory {
                    let _ = self.free_chain(cx, a, e.start);
                }
                Err(err)
            }
        }
    }

    /// Removes file `n` from directory `dir` (FreeBSD's `msdosfs_remove`);
    /// the caller frees its clusters ([`Self::resize`] to 0) once nothing
    /// uses it.
    pub fn remove(&self, cx: &B::Cx, dir: u32, n: &Name) -> Result<(), Error> {
        self.remove_entry(cx, dir, n)
    }

    /// Removes empty directory `n` from `dir` and frees its clusters
    /// (FreeBSD's `msdosfs_rmdir`).
    pub fn rmdir(&self, cx: &B::Cx, a: &mut Alloc, dir: u32, n: &Name) -> Result<(), Error> {
        if !self.dir_empty(cx, n.entry.start)? {
            return Err(Error::NotEmpty);
        }
        self.remove_entry(cx, dir, n)?;
        self.free_chain(cx, a, n.entry.start)
    }

    /// Moves `from` (in `from_dir`) to `to_name` in `to_dir`, replacing
    /// `target` there if given (already checked: same kind, an empty
    /// directory), as FreeBSD's `msdosfs_rename` does: the new entry keeps
    /// the old one's fields, a moved directory's `..` points at its new
    /// parent, and the old entry goes. A replaced file's clusters are the
    /// caller's to free once nothing uses it; a replaced directory's go
    /// now. Returns the new entry.
    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        from_dir: u32,
        from: &Name,
        to_dir: u32,
        to_name: &[u8],
        target: Option<&Name>,
    ) -> Result<Name, Error> {
        if let Some(t) = target {
            self.remove_entry(cx, to_dir, t)?;
            if t.entry.kind() == Kind::Directory {
                self.free_chain(cx, a, t.entry.start)?;
            }
        }
        // The old entry goes first, so a rename to another case of the
        // same name finds its short name free.
        self.remove_entry(cx, from_dir, from)?;
        let n = match self.create_entry(cx, a, to_dir, to_name, &from.entry) {
            Ok(n) => n,
            Err(err) => {
                // Put the old entry back where it can go.
                let _ = self.create_entry(cx, a, from_dir, &from.name, &from.entry);
                return Err(err);
            }
        };
        if from.entry.kind() == Kind::Directory && from_dir != to_dir {
            self.set_dotdot(cx, from.entry.start, to_dir)?;
        }
        Ok(n)
    }

    /// Sizes the file whose entry is `e` to `size` bytes: its cluster
    /// chain grows (zeroed if `clear`) or shrinks (FreeBSD's `deextend`,
    /// `detrunc`). Sets `e`'s start and size; the caller writes the entry.
    pub fn resize(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        e: &mut Entry,
        size: u64,
        clear: bool,
    ) -> Result<(), Error> {
        let size32 = u32::try_from(size).map_err(|_| Error::TooBig)?;
        let have = if e.start == 0 {
            0
        } else {
            self.chain_len(cx, e.start)?
        };
        let need = self.geo.clusters_for(size);
        if need > have {
            let more = u32::try_from(need - have).map_err(|_| Error::NoSpace)?;
            let last = if have == 0 {
                0
            } else {
                self.chain_nth(cx, e.start, have - 1)?
                    .ok_or(Error::Corrupt("cluster chain"))?
            };
            let first = self.extend(cx, a, last, more, clear)?;
            if have == 0 {
                e.start = first;
            }
        } else if need < have {
            self.truncate_chain(cx, a, e.start, need)?;
            if need == 0 {
                e.start = 0;
            }
        }
        e.size = size32;
        Ok(())
    }

    /// Writes `data` at `off` of the file whose entry is `e`, growing it
    /// (for tools; the kernel writes file data through its page cache).
    pub fn write_data(
        &self,
        cx: &B::Cx,
        a: &mut Alloc,
        e: &mut Entry,
        off: u64,
        data: &[u8],
    ) -> Result<(), Error> {
        let end = off + super::len64(data.len());
        if end > u64::from(e.size) {
            self.resize(cx, a, e, end, true)?;
        }
        let mut done = 0usize;
        while done < data.len() {
            let at = off + super::len64(done);
            let (disk, run) = self
                .bmap(cx, e.start, at)?
                .ok_or(Error::Corrupt("cluster chain"))?;
            let n = usize::try_from(run)
                .unwrap_or(usize::MAX)
                .min(data.len() - done);
            self.io.write_data(cx, disk, &data[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    /// Updates file `n`'s entry in place (times, attributes, size, start).
    pub fn update(&self, cx: &B::Cx, n: &Name, mode: Mode) -> Result<(), Error> {
        self.write_entry(cx, n.loc, &n.entry, mode)
    }

    /// Whether `dir` is the root (start `ROOT`, or FAT32's root cluster).
    #[must_use]
    pub fn root_of(&self, dir: u32) -> u32 {
        if self.is_root(dir) { ROOT } else { dir }
    }
}
