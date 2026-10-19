// SPDX-License-Identifier: BSD-2-Clause
//! A map's entries in address order: a list of short sorted chunks.
//!
//! `BTreeMap` was the obvious choice, but a map whose size sits at a node
//! boundary (eleven entries, say) split a node on every `mmap` and merged
//! it back on every `munmap`, which cost more than the rest of both calls.
//! Here a chunk splits in half when it outgrows [`MAX`] and merges with a
//! neighbor only when it falls below [`MIN`], so no size makes successive
//! inserts and removes undo each other. A lookup is two binary searches;
//! an insert or remove moves at most a chunk's entries.

use alloc::vec::Vec;

use super::Entry;

/// The most entries a chunk holds before it splits.
const MAX: usize = 32;
/// A chunk with fewer entries than this merges with a neighbor, when the
/// two fit in half a chunk.
const MIN: usize = MAX / 4;

/// Non-overlapping entries ordered by start address.
#[derive(Default)]
pub(super) struct Entries {
    /// Each non-empty and sorted; each chunk's entries all start below
    /// the next chunk's.
    chunks: Vec<Vec<Entry>>,
}

/// Where an entry is, or would go: chunk and index. A chunk index equal to
/// the number of chunks means the end.
#[derive(Clone, Copy)]
struct Pos(usize, usize);

impl Entries {
    /// The position of the first entry starting at or above `key`.
    fn locate(&self, key: u64) -> Pos {
        let c = self
            .chunks
            .partition_point(|chunk| chunk.last().is_some_and(|e| e.start.get() < key));
        match self.chunks.get(c) {
            Some(chunk) => Pos(c, chunk.partition_point(|e| e.start.get() < key)),
            None => Pos(c, 0),
        }
    }

    /// The position just before `pos`, if there is one.
    fn prev(&self, Pos(c, i): Pos) -> Option<Pos> {
        if i > 0 {
            Some(Pos(c, i - 1))
        } else {
            let c = c.checked_sub(1)?;
            Some(Pos(c, self.chunks[c].len() - 1))
        }
    }

    fn get(&self, Pos(c, i): Pos) -> &Entry {
        &self.chunks[c][i]
    }

    /// The entries in order.
    pub(super) fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.chunks.iter().flatten()
    }

    /// The entries in order, to change in place (not their ranges).
    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Entry> {
        self.chunks.iter_mut().flatten()
    }

    /// The last entry starting below `key`.
    pub(super) fn before(&self, key: u64) -> Option<&Entry> {
        self.prev(self.locate(key)).map(|p| self.get(p))
    }

    /// The last entry starting at or below `key`.
    pub(super) fn at_or_before(&self, key: u64) -> Option<&Entry> {
        match key.checked_add(1) {
            Some(k) => self.before(k),
            None => self.chunks.last().and_then(|c| c.last()),
        }
    }

    /// [`Self::at_or_before`], to change in place.
    pub(super) fn at_or_before_mut(&mut self, key: u64) -> Option<&mut Entry> {
        let pos = match key.checked_add(1) {
            Some(k) => self.locate(k),
            None => Pos(self.chunks.len(), 0),
        };
        let Pos(c, i) = self.prev(pos)?;
        Some(&mut self.chunks[c][i])
    }

    /// The entries starting at or above `key`, in order.
    pub(super) fn starting_at(&self, key: u64) -> impl Iterator<Item = &Entry> {
        let Pos(c, i) = self.locate(key);
        self.chunks
            .get(c..)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .flat_map(move |(k, chunk)| &chunk[if k == 0 { i } else { 0 }..])
    }

    /// [`Self::starting_at`], to change in place (not their ranges).
    pub(super) fn starting_at_mut(&mut self, key: u64) -> impl Iterator<Item = &mut Entry> {
        let Pos(c, i) = self.locate(key);
        self.chunks
            .get_mut(c..)
            .unwrap_or_default()
            .iter_mut()
            .enumerate()
            .flat_map(move |(k, chunk)| &mut chunk[if k == 0 { i } else { 0 }..])
    }

    /// Adds `entry`, which overlaps no other.
    pub(super) fn insert(&mut self, entry: Entry) {
        let key = entry.start.get();
        if self.chunks.is_empty() {
            let mut chunk = Vec::with_capacity(MAX + 1);
            chunk.push(entry);
            self.chunks.push(chunk);
            return;
        }
        let Pos(c, i) = self.locate(key);
        // Past every chunk's last entry: the end of the last chunk.
        let (c, i) = if c == self.chunks.len() {
            (c - 1, self.chunks[c - 1].len())
        } else {
            (c, i)
        };
        let chunk = &mut self.chunks[c];
        chunk.insert(i, entry);
        if chunk.len() > MAX {
            let mut right = Vec::with_capacity(MAX + 1);
            right.extend(chunk.drain(chunk.len() / 2..));
            self.chunks.insert(c + 1, right);
        }
    }

    /// Removes the entries starting in `[from, to)`, handing each to
    /// `gone`.
    pub(super) fn remove_range(&mut self, from: u64, to: u64, mut gone: impl FnMut(Entry)) {
        loop {
            let Pos(c, i) = self.locate(from);
            let Some(chunk) = self.chunks.get_mut(c) else {
                return;
            };
            let j = chunk.partition_point(|e| e.start.get() < to);
            if i == j {
                return;
            }
            let rest = j < chunk.len();
            for e in chunk.drain(i..j) {
                gone(e);
            }
            self.rebalance(c);
            if rest {
                return;
            }
        }
    }

    /// Drops chunk `c` if it is empty, or merges it with a neighbor if it
    /// is short and they fit in half a chunk together.
    fn rebalance(&mut self, c: usize) {
        let len = self.chunks[c].len();
        if len == 0 {
            self.chunks.remove(c);
            return;
        }
        if len >= MIN {
            return;
        }
        let fits = |n: usize| len + n <= MAX / 2;
        if self.chunks.get(c + 1).is_some_and(|n| fits(n.len())) {
            let next = self.chunks.remove(c + 1);
            self.chunks[c].extend(next);
        } else if c > 0 && fits(self.chunks[c - 1].len()) {
            let this = self.chunks.remove(c);
            self.chunks[c - 1].extend(this);
        }
    }

    /// Removes every entry, handing each to `gone`.
    pub(super) fn clear(&mut self, gone: impl FnMut(Entry)) {
        core::mem::take(&mut self.chunks)
            .into_iter()
            .flatten()
            .for_each(gone);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::collections::BTreeMap;

    use super::*;
    use crate::VirtAddr;
    use crate::map::Prot;

    fn entry(page: u64) -> Entry {
        Entry::anonymous(
            VirtAddr::new(page << 12)..VirtAddr::new((page + 1) << 12),
            Prot::RW,
            Prot::ALL,
        )
    }

    /// Random inserts and range removes agree with a `BTreeMap`, and the
    /// chunks keep their bounds.
    #[test]
    fn agrees_with_a_btree_map() {
        let mut rng = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut ours = Entries::default();
        let mut model = BTreeMap::new();
        for round in 0..20_000 {
            // Grow for a while, then shrink, then hover.
            let insert = match round / 5000 {
                0 => next() % 4 != 0,
                1 => next() % 4 == 0,
                _ => next() % 2 == 0,
            };
            let page = next() % 2000;
            if insert {
                if model.insert(page, ()).is_none() {
                    ours.insert(entry(page));
                }
            } else {
                let n = next() % 8 + 1;
                let mut removed = Vec::new();
                ours.remove_range(page << 12, (page + n) << 12, |e| {
                    removed.push(e.start.get() >> 12);
                });
                let expect: Vec<u64> = model.range(page..page + n).map(|(&k, _)| k).collect();
                assert_eq!(removed, expect);
                for k in expect {
                    model.remove(&k);
                }
            }
            for chunk in &ours.chunks {
                assert!(!chunk.is_empty() && chunk.len() <= MAX);
            }
            let probe = next() % 2100;
            let key = probe << 12;
            assert_eq!(
                ours.before(key).map(|e| e.start.get() >> 12),
                model.range(..probe).next_back().map(|(&k, _)| k)
            );
            assert_eq!(
                ours.at_or_before(key).map(|e| e.start.get() >> 12),
                model.range(..=probe).next_back().map(|(&k, _)| k)
            );
            assert_eq!(
                ours.starting_at(key)
                    .map(|e| e.start.get() >> 12)
                    .take(3)
                    .collect::<Vec<_>>(),
                model
                    .range(probe..)
                    .map(|(&k, _)| k)
                    .take(3)
                    .collect::<Vec<_>>()
            );
        }
        let all: Vec<u64> = ours.iter().map(|e| e.start.get() >> 12).collect();
        assert_eq!(all, model.keys().copied().collect::<Vec<_>>());
    }

    /// At a chunk boundary, an insert and a remove leave the same chunks
    /// (no split and merge every time).
    #[test]
    fn a_boundary_does_not_thrash() {
        let mut ours = Entries::default();
        for page in 0..MAX as u64 {
            ours.insert(entry(page * 2));
        }
        ours.insert(entry(1));
        let chunks = ours.chunks.len();
        for _ in 0..10 {
            ours.remove_range(1 << 12, 2 << 12, drop);
            assert_eq!(ours.chunks.len(), chunks);
            ours.insert(entry(1));
            assert_eq!(ours.chunks.len(), chunks);
        }
    }
}
