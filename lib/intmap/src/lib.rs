// SPDX-License-Identifier: BSD-2-Clause
//! A hash map keyed by integers (inode numbers, disk offsets, ids): open
//! addressing with linear probing, Fibonacci hashing, and deletion by
//! shifting back, so lookups touch one or two adjacent slots where an
//! ordered tree walks a dozen nodes. FreeBSD keeps such tables as hash
//! chains (`inodedep_hashtbl`, `vfs_hash`); this is the same idea without
//! a list node per entry.
//!
//! The interface is `BTreeMap`'s, keys by reference, so a map that never
//! asks for a range or an order changes type and nothing else. Iteration
//! order is unspecified.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

/// A key: any value that is one integer.
pub trait IntKey: Copy + Eq {
    fn bits(self) -> u64;
}

impl IntKey for u32 {
    fn bits(self) -> u64 {
        u64::from(self)
    }
}

impl IntKey for u64 {
    fn bits(self) -> u64 {
        self
    }
}

/// The map.
#[derive(Clone)]
pub struct IntMap<K, V> {
    /// One table, or [`SHARDS`] once the map is large, picked by a key's
    /// top hash bits: each grows on its own, so no allocation grows past
    /// a shard's (a kernel heap has no large contiguous run to give).
    tables: Vec<Table<K, V>>,
    len: usize,
}

/// One open-addressed table.
#[derive(Clone)]
struct Table<K, V> {
    slots: Vec<Option<(K, V)>>,
    len: usize,
}

impl<K: IntKey, V> Default for IntMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: IntKey + core::fmt::Debug, V: core::fmt::Debug> core::fmt::Debug for IntMap<K, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Slots in a table's first allocation.
const FIRST: usize = 16;
/// Tables a large map is split into, and the size at which it splits.
const SHARD_BITS: u32 = 8;
const SHARDS: usize = 1 << SHARD_BITS;
const SPLIT: usize = 8192;

/// A key's hash: Fibonacci hashing, whose top bits are the well mixed ones.
fn hash<K: IntKey>(key: K) -> u64 {
    key.bits().wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

impl<K: IntKey, V> Table<K, V> {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
        }
    }

    /// Where hash `h` lands in a table of `n` slots (a power of two): the
    /// bits below those that pick the shard.
    fn home(h: u64, n: usize) -> usize {
        let bits = n.trailing_zeros();
        usize::try_from((h << SHARD_BITS) >> (64 - bits)).unwrap_or(0)
    }

    /// The slot holding `key`, if any.
    fn find(&self, key: K, h: u64) -> Option<usize> {
        let n = self.slots.len();
        if n == 0 {
            return None;
        }
        let mut i = Self::home(h, n);
        loop {
            match &self.slots[i] {
                None => return None,
                Some((k, _)) if *k == key => return Some(i),
                Some(_) => i = (i + 1) & (n - 1),
            }
        }
    }

    /// Room for one more at most three quarters full.
    fn reserve_one(&mut self) {
        let n = self.slots.len();
        if (self.len + 1) * 4 <= n * 3 {
            return;
        }
        let bigger = (n * 2).max(FIRST);
        let old = core::mem::take(&mut self.slots);
        self.slots.resize_with(bigger, || None);
        for (k, v) in old.into_iter().flatten() {
            let i = self.free_slot(hash(k));
            self.slots[i] = Some((k, v));
        }
    }

    /// The first empty slot on hash `h`'s probe path.
    fn free_slot(&self, h: u64) -> usize {
        let n = self.slots.len();
        let mut i = Self::home(h, n);
        while self.slots[i].is_some() {
            i = (i + 1) & (n - 1);
        }
        i
    }

    /// Puts `key` (not there) in, returning its slot.
    fn put(&mut self, key: K, h: u64, value: V) -> usize {
        self.reserve_one();
        let i = self.free_slot(h);
        self.slots[i] = Some((key, value));
        self.len += 1;
        i
    }

    fn remove(&mut self, key: K, h: u64) -> Option<V> {
        let n = self.slots.len();
        let mut hole = self.find(key, h)?;
        let (_, value) = self.slots[hole].take()?;
        self.len -= 1;
        // Shift back what follows in the cluster, so no probe path breaks
        // at the hole (no tombstones).
        let mut i = (hole + 1) & (n - 1);
        while let Some((k, _)) = &self.slots[i] {
            let home = Self::home(hash(*k), n);
            // Whether `home` lies cyclically in (hole, i]: then it stays.
            let stays = if hole <= i {
                hole < home && home <= i
            } else {
                hole < home || home <= i
            };
            if !stays {
                self.slots[hole] = self.slots[i].take();
                hole = i;
            }
            i = (i + 1) & (n - 1);
        }
        Some(value)
    }
}

impl<K: IntKey, V> IntMap<K, V> {
    /// An empty map (no table until the first insert).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tables: Vec::new(),
            len: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The table hash `h` belongs in.
    fn shard(&self, h: u64) -> usize {
        if self.tables.len() == SHARDS {
            usize::try_from(h >> (64 - SHARD_BITS)).unwrap_or(0)
        } else {
            0
        }
    }

    /// Past [`SPLIT`] entries in one table, splits it into [`SHARDS`].
    fn maybe_split(&mut self) {
        if self.tables.len() == SHARDS || self.len < SPLIT {
            return;
        }
        let old = core::mem::take(&mut self.tables);
        self.tables.resize_with(SHARDS, Table::new);
        for t in old {
            for (k, v) in t.slots.into_iter().flatten() {
                let h = hash(k);
                let s = self.shard(h);
                self.tables[s].put(k, h, v);
            }
        }
    }

    /// Where `key` is: its table and slot.
    fn find(&self, key: K) -> Option<(usize, usize)> {
        let h = hash(key);
        let s = self.shard(h);
        Some((s, self.tables.get(s)?.find(key, h)?))
    }

    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.find(*key).is_some()
    }

    #[must_use]
    pub fn get(&self, key: &K) -> Option<&V> {
        let (s, i) = self.find(*key)?;
        self.tables[s].slots[i].as_ref().map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let (s, i) = self.find(*key)?;
        self.tables[s].slots[i].as_mut().map(|(_, v)| v)
    }

    /// The place for `key`, filled or not.
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        Entry { map: self, key }
    }

    /// Puts `key` (not there) in: its table and slot.
    fn put(&mut self, key: K, value: V) -> (usize, usize) {
        self.maybe_split();
        if self.tables.is_empty() {
            self.tables.push(Table::new());
        }
        let h = hash(key);
        let s = self.shard(h);
        let i = self.tables[s].put(key, h, value);
        self.len += 1;
        (s, i)
    }

    /// Puts `value` under `key`, returning the value it replaces.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if let Some(old) = self.get_mut(&key) {
            return Some(core::mem::replace(old, value));
        }
        self.put(key, value);
        None
    }

    /// The value under `key`, put there by `make` if there was none.
    pub fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> &mut V {
        let (s, i) = match self.find(key) {
            Some(at) => at,
            None => self.put(key, make()),
        };
        match &mut self.tables[s].slots[i] {
            Some((_, v)) => v,
            None => unreachable!("a slot just found or filled"),
        }
    }

    /// Takes the value under `key` out.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let h = hash(*key);
        let s = self.shard(h);
        let v = self.tables.get_mut(s)?.remove(*key, h)?;
        self.len -= 1;
        Some(v)
    }

    /// Each key and value, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.into_iter()
    }

    /// Each value, in no particular order.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// Keeps only what `keep` says to.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let gone: Vec<K> = self
            .tables
            .iter_mut()
            .flat_map(|t| t.slots.iter_mut().flatten())
            .filter_map(|(k, v)| (!keep(k, v)).then_some(*k))
            .collect();
        for k in gone {
            self.remove(&k);
        }
    }
}

/// A key and value as an iterator yields them.
type Pair<'a, K, V> = (&'a K, &'a V);

impl<'a, K: IntKey, V> IntoIterator for &'a IntMap<K, V> {
    type Item = Pair<'a, K, V>;
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        Iter {
            tables: self.tables.iter(),
            slots: [].iter(),
        }
    }
}

/// The entries of a map ([`IntMap::iter`]).
pub struct Iter<'a, K, V> {
    tables: core::slice::Iter<'a, Table<K, V>>,
    slots: core::slice::Iter<'a, Option<(K, V)>>,
}

impl<K, V> core::fmt::Debug for Iter<'_, K, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Iter")
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = Pair<'a, K, V>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((k, v)) = self.slots.by_ref().flatten().next() {
                return Some((k, v));
            }
            self.slots = self.tables.next()?.slots.iter();
        }
    }
}

/// A place in a map ([`IntMap::entry`]).
pub struct Entry<'a, K, V> {
    map: &'a mut IntMap<K, V>,
    key: K,
}

impl<K: core::fmt::Debug, V> core::fmt::Debug for Entry<'_, K, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Entry({:?})", self.key)
    }
}

impl<'a, K: IntKey, V> Entry<'a, K, V> {
    /// Its value, put there by `make` if there was none.
    pub fn or_insert_with(self, make: impl FnOnce() -> V) -> &'a mut V {
        self.map.get_or_insert_with(self.key, make)
    }

    /// Its value, `value` put there if there was none.
    pub fn or_insert(self, value: V) -> &'a mut V {
        self.or_insert_with(|| value)
    }

    /// Its value, a default one put there if there was none.
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::collections::BTreeMap;

    #[test]
    fn agrees_with_an_ordered_map() {
        let mut m: IntMap<u64, u64> = IntMap::new();
        let mut model = BTreeMap::new();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for step in 0..400_000u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Few distinct keys, so removals hit and clusters form.
            let key = x % 40_000;
            match x % 3 {
                0 => assert_eq!(m.insert(key, step), model.insert(key, step)),
                1 => assert_eq!(m.remove(&key), model.remove(&key)),
                _ => assert_eq!(m.get(&key), model.get(&key)),
            }
            assert_eq!(m.len(), model.len());
        }
        for (k, v) in &model {
            assert_eq!(m.get(k), Some(v));
        }
        let mut seen: alloc::vec::Vec<u64> = m.iter().map(|(k, _)| *k).collect();
        seen.sort_unstable();
        assert_eq!(seen, model.keys().copied().collect::<alloc::vec::Vec<_>>());
    }

    #[test]
    fn retain_and_default() {
        let mut m: IntMap<u32, u32> = IntMap::new();
        for i in 0..1000 {
            *m.entry(i).or_default() += i;
        }
        m.retain(|k, _| k % 2 == 0);
        assert_eq!(m.len(), 500);
        assert!(m.contains_key(&998) && !m.contains_key(&999));
    }
}
