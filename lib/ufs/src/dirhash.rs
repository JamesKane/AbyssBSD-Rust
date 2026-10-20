// SPDX-License-Identifier: BSD-2-Clause
//! Directory hashing (FreeBSD's `ufs_dirhash`): for a large directory,
//! an in-memory index of its names and of the free space in each chunk,
//! so lookups and inserts touch one chunk instead of scanning them all.
//!
//! The index hangs off the directory's in-core [`Inode`](crate::Inode),
//! which every directory operation takes by `&mut`, so the engine keeps it
//! exact as entries come and go. It is never written: it is rebuilt from
//! the directory after the vnode is reclaimed.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// The size at which a directory gets an index (FreeBSD's
/// `vfs.ufs.dirhash_minsize`).
pub const MIN_SIZE: u64 = 2560;

/// A directory's index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirHash {
    /// Each live name's inode, and the byte offset of its entry.
    pub(crate) names: BTreeMap<Box<[u8]>, (u32, u64)>,
    /// Each chunk's largest free slot: the most bytes a new entry there
    /// could take.
    pub(crate) free: Vec<u16>,
}

impl DirHash {
    /// The first chunk with a slot of at least `need` bytes.
    pub(crate) fn chunk_with(&self, need: usize) -> Option<u64> {
        self.free
            .iter()
            .position(|&f| usize::from(f) >= need)
            .map(crate::len64)
    }

    /// The live names indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}
