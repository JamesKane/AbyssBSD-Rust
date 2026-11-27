// SPDX-License-Identifier: BSD-2-Clause
//! The map against `BTreeMap`, operation by operation.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use super::*;

/// xorshift64*: the same sequence on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A key: dense near zero mostly, sometimes far out.
    fn key(&mut self, spread: u64) -> u64 {
        match self.next() % 16 {
            0 => self.next(),
            1 => u64::MAX - self.next() % 64,
            _ => self.next() % spread,
        }
    }
}

fn same(r: &RadixMap<u64>, b: &BTreeMap<u64, u64>) {
    assert_eq!(r.len(), b.len());
    assert_eq!(r.is_empty(), b.is_empty());
    let got: Vec<(u64, u64)> = r.iter().map(|(k, v)| (k, *v)).collect();
    let want: Vec<(u64, u64)> = b.iter().map(|(k, v)| (*k, *v)).collect();
    assert_eq!(got, want);
}

#[test]
fn empty() {
    let r: RadixMap<u64> = RadixMap::new();
    assert_eq!(r.len(), 0);
    assert!(r.get(&0).is_none());
    assert!(r.get(&u64::MAX).is_none());
    assert_eq!(r.iter().count(), 0);
    let (lo, hi) = (5, 3);
    assert_eq!(r.range(lo..hi).count(), 0);
}

#[test]
fn extreme_keys() {
    let mut r = RadixMap::new();
    for k in [0, 15, 16, 255, 256, u64::MAX, u64::MAX - 1, 1 << 63] {
        assert_eq!(r.insert(k, k), None);
    }
    for k in [0, 15, 16, 255, 256, u64::MAX, u64::MAX - 1, 1 << 63] {
        assert_eq!(r.get(&k), Some(&k));
    }
    assert_eq!(
        r.range(u64::MAX..).map(|(k, _)| k).collect::<Vec<_>>(),
        [u64::MAX]
    );
    assert_eq!(r.range(..=0).count(), 1);
    assert_eq!(
        r.range((Bound::Excluded(u64::MAX), Bound::Unbounded))
            .count(),
        0
    );
    let hi = r.split_off(&(1 << 63));
    assert_eq!(hi.len(), 3);
    assert_eq!(r.len(), 5);
    assert_eq!(r.remove(&u64::MAX), None);
    assert_eq!(r.remove(&256), Some(256));
}

#[test]
fn random_against_btreemap() {
    for (seed, spread) in [(1, 64), (2, 4096), (3, 1 << 20), (4, 300)] {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
        let mut r = RadixMap::new();
        let mut b = BTreeMap::new();
        for step in 0..20_000 {
            let k = rng.key(spread);
            match rng.next() % 10 {
                0..=3 => assert_eq!(r.insert(k, step), b.insert(k, step)),
                4 | 5 => assert_eq!(r.remove(&k), b.remove(&k)),
                6 => {
                    let hit = r.get(&k).copied();
                    assert_eq!(hit, b.get(&k).copied());
                    assert_eq!(r.contains_key(&k), hit.is_some());
                }
                7 => match (r.entry(k), b.entry(k)) {
                    (Entry::Occupied(o), std::collections::btree_map::Entry::Occupied(p)) => {
                        assert_eq!(o.get(), p.get());
                    }
                    (Entry::Vacant(v), std::collections::btree_map::Entry::Vacant(w)) => {
                        assert_eq!(*v.key(), k);
                        *v.insert(step) += 1;
                        *w.insert(step) += 1;
                    }
                    _ => panic!("entry disagrees at {k}"),
                },
                8 => {
                    let a = rng.key(spread);
                    let z = a.saturating_add(rng.next() % (spread / 4 + 2));
                    let got: Vec<_> = r.range(a..z).map(|(k, v)| (k, *v)).collect();
                    let want: Vec<_> = b.range(a..z).map(|(k, v)| (*k, *v)).collect();
                    assert_eq!(got, want, "range {a}..{z}");
                    for (_, v) in r.range_mut(a..=z) {
                        *v ^= 1;
                    }
                    for (_, v) in b.range_mut(a..=z) {
                        *v ^= 1;
                    }
                }
                _ => {
                    // Split and put back, as `free_range` takes a window out.
                    let a = rng.key(spread);
                    let z = a.saturating_add(rng.next() % (spread / 2 + 2));
                    let mut tail = r.split_off(&a);
                    let mut far = tail.split_off(&z);
                    let mut btail = b.split_off(&a);
                    let mut bfar = btail.split_off(&z);
                    same(&tail, &btail);
                    same(&far, &bfar);
                    if rng.next().is_multiple_of(2) {
                        r.append(&mut tail);
                        b.append(&mut btail);
                    }
                    r.append(&mut far);
                    b.append(&mut bfar);
                    assert!(far.is_empty());
                }
            }
            if step % 997 == 0 {
                same(&r, &b);
            }
        }
        same(&r, &b);
        let got: Vec<_> = r.into_iter().collect();
        let want: Vec<_> = b.into_iter().collect();
        assert_eq!(got, want);
    }
}

#[test]
fn append_overlapping_takes_the_other() {
    let mut a = RadixMap::new();
    let mut b = RadixMap::new();
    for k in 0..100 {
        a.insert(k, 0);
    }
    for k in 50..5000 {
        b.insert(k * 3, 1);
    }
    let mut ba: BTreeMap<u64, u64> = (0..100).map(|k| (k, 0)).collect();
    let mut bb: BTreeMap<u64, u64> = (50..5000).map(|k| (k * 3, 1)).collect();
    a.append(&mut b);
    ba.append(&mut bb);
    same(&a, &ba);
    assert!(b.is_empty());
}

#[test]
fn values_and_removal_free_nodes() {
    let mut r = RadixMap::new();
    for k in 0..70_000 {
        r.insert(k, k);
    }
    for v in r.values_mut() {
        *v += 1;
    }
    assert_eq!(r.values().copied().sum::<u64>(), (1..=70_000).sum());
    for k in 0..70_000 {
        assert_eq!(r.remove(&k), Some(k + 1));
    }
    assert!(r.is_empty());
    assert_eq!(r.height, 0);
    r.insert(3, 3);
    assert_eq!(r.height, 1);
}
