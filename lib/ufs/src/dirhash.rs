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
use alloc::vec::Vec;

use intmap::IntMap;

/// The size at which a directory gets an index (FreeBSD's
/// `vfs.ufs.dirhash_minsize`).
pub const MIN_SIZE: u64 = 2560;

/// A directory's index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirHash {
    /// Each live name's inode, and the byte offset of its entry.
    pub(crate) names: Names,
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

/// Names to their inode and entry offset, hashed by name (FreeBSD's
/// dirhash hashes names too): a lookup compares one name, not a tree
/// path of them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Names {
    /// A name's hash to the entries whose names have it (nearly always
    /// one).
    by_hash: IntMap<u64, Vec<Entry>>,
    len: usize,
}

/// A name, and its inode and entry offset.
type Entry = (Box<[u8]>, (u32, u64));

/// A name's hash: FNV-1a, 64 bits.
fn hash(name: &[u8]) -> u64 {
    name.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Names {
    pub(crate) fn get(&self, name: &[u8]) -> Option<&(u32, u64)> {
        self.by_hash
            .get(&hash(name))?
            .iter()
            .find(|(n, _)| **n == *name)
            .map(|(_, v)| v)
    }

    pub(crate) fn contains_key(&self, name: &[u8]) -> bool {
        self.get(name).is_some()
    }

    /// Indexes `name` as `v`, in place of what it named.
    pub(crate) fn insert(&mut self, name: Box<[u8]>, v: (u32, u64)) {
        let list = self.by_hash.entry(hash(&name)).or_default();
        match list.iter_mut().find(|(n, _)| *n == name) {
            Some((_, old)) => *old = v,
            None => {
                list.push((name, v));
                self.len += 1;
            }
        }
    }

    pub(crate) fn remove(&mut self, name: &[u8]) {
        let h = hash(name);
        let Some(list) = self.by_hash.get_mut(&h) else {
            return;
        };
        if let Some(i) = list.iter().position(|(n, _)| **n == *name) {
            list.swap_remove(i);
            self.len -= 1;
            if list.is_empty() {
                self.by_hash.remove(&h);
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl PartialEq for Names {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && self
                .by_hash
                .iter()
                .all(|(_, list)| list.iter().all(|(n, v)| other.get(n) == Some(v)))
    }
}

impl Eq for Names {}
