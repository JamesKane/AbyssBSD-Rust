// SPDX-License-Identifier: BSD-2-Clause
//! Directory hashing (FreeBSD's `ufs_dirhash`): for a large directory,
//! an in-memory index of its names and of the free space in each chunk,
//! so lookups and inserts touch one chunk instead of scanning them all.
//!
//! The index hangs off the directory's in-core [`Inode`](crate::Inode),
//! which every directory operation takes by `&mut`, so the engine keeps it
//! exact as entries come and go. It is never written: it is rebuilt from
//! the directory after the vnode is reclaimed.

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
/// path of them. The names are kept in one buffer, and a hash with one
/// name (nearly all) holds it in the table itself: an insert allocates
/// nothing, where a boxed name in a list per hash was two allocations.
#[derive(Clone, Debug, Default)]
pub(crate) struct Names {
    by_hash: IntMap<u64, Bucket>,
    /// The names' bytes, end to end; `dead` of them belong to removed
    /// names, and the buffer is packed again once they are half of it.
    arena: Vec<u8>,
    dead: usize,
    len: usize,
}

/// A name (its bytes in the buffer), and its inode and entry offset.
#[derive(Clone, Copy, Debug)]
struct Rec {
    at: u32,
    n: u16,
    v: (u32, u64),
}

/// The names with one hash.
#[derive(Clone, Debug)]
enum Bucket {
    One(Rec),
    Many(Vec<Rec>),
}

impl Bucket {
    fn recs(&self) -> &[Rec] {
        match self {
            Self::One(r) => core::slice::from_ref(r),
            Self::Many(v) => v,
        }
    }
}

/// A name's hash: FNV-1a, 64 bits.
fn fnv(name: &[u8]) -> u64 {
    name.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(not(test))]
fn hash(name: &[u8]) -> u64 {
    fnv(name)
}

/// Tested with eight hashes, so names share them.
#[cfg(test)]
fn hash(name: &[u8]) -> u64 {
    fnv(name) & 7
}

impl Names {
    fn name(&self, r: &Rec) -> &[u8] {
        let at = r.at as usize;
        &self.arena[at..at + usize::from(r.n)]
    }

    pub(crate) fn get(&self, name: &[u8]) -> Option<&(u32, u64)> {
        let b = self.by_hash.get(&hash(name))?;
        b.recs().iter().find(|r| self.name(r) == name).map(|r| &r.v)
    }

    pub(crate) fn contains_key(&self, name: &[u8]) -> bool {
        self.get(name).is_some()
    }

    /// Indexes `name` as `v`, in place of what it named.
    pub(crate) fn insert(&mut self, name: &[u8], v: (u32, u64)) {
        let h = hash(name);
        let n = u16::try_from(name.len()).unwrap_or(u16::MAX);
        if let Some(b) = self.by_hash.get(&h)
            && let Some(i) = b.recs().iter().position(|r| self.name(r) == name)
        {
            match self.by_hash.get_mut(&h) {
                Some(Bucket::One(r)) => r.v = v,
                Some(Bucket::Many(list)) => list[i].v = v,
                None => {}
            }
            return;
        }
        let at = u32::try_from(self.arena.len()).unwrap_or(u32::MAX);
        self.arena.extend_from_slice(name);
        let rec = Rec { at, n, v };
        match self.by_hash.get_mut(&h) {
            Some(b @ Bucket::One(_)) => {
                let Bucket::One(old) = *b else { unreachable!() };
                *b = Bucket::Many(alloc::vec![old, rec]);
            }
            Some(Bucket::Many(list)) => list.push(rec),
            None => {
                self.by_hash.insert(h, Bucket::One(rec));
            }
        }
        self.len += 1;
    }

    pub(crate) fn remove(&mut self, name: &[u8]) {
        let h = hash(name);
        let Some(b) = self.by_hash.get(&h) else {
            return;
        };
        let Some(i) = b.recs().iter().position(|r| self.name(r) == name) else {
            return;
        };
        match self.by_hash.get_mut(&h) {
            Some(Bucket::One(_)) => {
                self.by_hash.remove(&h);
            }
            Some(Bucket::Many(list)) => {
                list.swap_remove(i);
                if let [only] = list[..] {
                    self.by_hash.insert(h, Bucket::One(only));
                }
            }
            None => {}
        }
        self.len -= 1;
        self.dead += name.len();
        if self.dead > 4096 && self.dead * 2 > self.arena.len() {
            self.pack();
        }
    }

    /// Copies the live names into a new buffer.
    fn pack(&mut self) {
        let mut arena = Vec::with_capacity(self.arena.len() - self.dead);
        let old = core::mem::take(&mut self.arena);
        let mut buckets = core::mem::take(&mut self.by_hash);
        let mut moved = IntMap::new();
        let mut relocate = |r: &mut Rec| {
            let at = r.at as usize;
            let new = u32::try_from(arena.len()).unwrap_or(u32::MAX);
            arena.extend_from_slice(&old[at..at + usize::from(r.n)]);
            r.at = new;
        };
        let keys: Vec<u64> = buckets.iter().map(|(k, _)| *k).collect();
        for k in keys {
            if let Some(mut b) = buckets.remove(&k) {
                match &mut b {
                    Bucket::One(r) => relocate(r),
                    Bucket::Many(list) => list.iter_mut().for_each(&mut relocate),
                }
                moved.insert(k, b);
            }
        }
        self.by_hash = moved;
        self.arena = arena;
        self.dead = 0;
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
            && self.by_hash.iter().all(|(_, b)| {
                b.recs()
                    .iter()
                    .all(|r| other.get(self.name(r)) == Some(&r.v))
            })
    }
}

impl Eq for Names {}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeMap;
    use std::format;
    use std::vec::Vec;

    use super::Names;

    /// The index against a map, through inserts, overwrites and removes
    /// enough to pack the buffer, with every name sharing one of eight
    /// hashes.
    #[test]
    fn names_agree_with_a_map() {
        let mut n = Names::default();
        let mut m: BTreeMap<Vec<u8>, (u32, u64)> = BTreeMap::new();
        let mut seed = 0x2545_f491_u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for step in 0..60_000u32 {
            let name = format!(
                "entry-{}-{}",
                rnd() % 900,
                "x".repeat((rnd() % 40) as usize)
            )
            .into_bytes();
            match rnd() % 3 {
                0 | 1 => {
                    let v = (step, u64::from(rnd()));
                    n.insert(&name, v);
                    m.insert(name.clone(), v);
                }
                _ => {
                    n.remove(&name);
                    m.remove(&name);
                }
            }
            assert_eq!(n.len(), m.len());
            assert_eq!(n.get(&name), m.get(&name));
            if step % 5000 == 0 {
                for (k, v) in &m {
                    assert_eq!(n.get(k), Some(v));
                }
            }
        }
        let copy = n.clone();
        assert!(copy == n);
    }
}
