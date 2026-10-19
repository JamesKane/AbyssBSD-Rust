// SPDX-License-Identifier: BSD-2-Clause
//! tarfs: a ustar archive in memory, read-only, as FreeBSD's tarfs mounts
//! one (here the memory disk the loader preloaded).
//!
//! Directories the archive implies but does not list are made with mode
//! 0755. Hard links share their target's vnode. Devices, FIFOs, and other
//! entry types are skipped. File data is read in place.
//!
//! Behavior reference: sys/fs/tarfs/tarfs_vfsops.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::time::Timespec;
use ustar::{Archive, Kind};

use crate::tree::{Builder, Leaf};
use crate::{FileSystem, Meta, RegularFile, Uio, Vnode};

/// A file's contents in the archive.
struct MemFile {
    data: &'static [u8],
}

impl RegularFile for MemFile {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn read(&self, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let Some(rest) = self.data.get(start..) else {
            return Ok(0);
        };
        uio.copy_out(rest)
    }

    fn resident(&self) -> Option<&'static [u8]> {
        Some(self.data)
    }
}

/// Why an archive could not be mounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountError {
    /// The archive is malformed.
    Archive(ustar::Error),
    /// An entry's path cannot be placed (a file where a directory must
    /// be, or a name too long).
    Path(Errno),
}

/// The tree in `archive`, as file system `fs`.
pub fn mount(archive: Archive<'static>, fs: Arc<FileSystem>) -> Result<Arc<Vnode>, MountError> {
    let zero = Timespec::default();
    let mut b = Builder::new(fs, Meta::root(0o755, zero));
    let mut path = Vec::new();
    for e in archive.entries() {
        let e = e.map_err(MountError::Archive)?;
        path.clear();
        e.path_parts(|p| path.extend_from_slice(p));
        let mtime = Timespec {
            tv_sec: i64::try_from(e.mtime).unwrap_or(i64::MAX),
            tv_nsec: 0,
        };
        let meta = Meta {
            perm: u16::try_from(e.mode & 0o7777).unwrap_or(0),
            uid: e.uid,
            gid: e.gid,
            mtime,
        };
        let implied = Meta::root(0o755, mtime);
        let r = match e.kind {
            Kind::Directory => b.dir(&path, meta).map(drop),
            Kind::File => b
                .leaf(
                    &path,
                    meta,
                    Leaf::Regular(Arc::new(MemFile { data: e.data })),
                    implied,
                )
                .map(drop),
            Kind::Symlink(target) => b
                .leaf(&path, meta, Leaf::Symlink(target.into()), implied)
                .map(drop),
            Kind::HardLink(target) => match b.find(target) {
                Some(t) => b.link(&path, t, implied),
                None => Err(Errno::ENOENT),
            },
            Kind::Other(_) => Ok(()),
        };
        r.map_err(MountError::Path)?;
    }
    Ok(b.finish())
}
