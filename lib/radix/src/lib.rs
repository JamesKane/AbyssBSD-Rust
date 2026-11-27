// SPDX-License-Identifier: BSD-2-Clause
//! An ordered map from `u64` keys to values, as a radix tree of 16-way
//! nodes: what FreeBSD keeps a memory object's pages in (`pctrie`,
//! sys/kern/subr_pctrie.c, 16 slots a node on 64-bit machines), so finding
//! a page, or the place for a new one, is a few dependent loads instead of
//! an ordered tree's key comparisons at every level. Keys are page indices,
//! dense from zero for most objects: the tree is as tall as the largest key
//! needs (four levels for 65,536 pages) and grows at the root.
//!
//! The interface is the part of `BTreeMap`'s the object code uses, so the
//! map changes type and little else; iteration yields keys by value.
//! Splitting at a key and appending a map with keys above move whole
//! subtrees, as the B-tree's do, so unmapping a range costs the height of
//! the tree, not its size.
//!
//! Unlike `pctrie`, a node's path is not compressed: a lone key far from
//! zero costs a node per level on the way.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use core::ops::{Bound, RangeBounds};

#[cfg(test)]
mod tests;

/// Key bits a level takes.
const BITS: u32 = 4;
/// Slots a node has.
const WIDTH: usize = 1 << BITS;
/// Levels a 64-bit key can need.
const MAX_HEIGHT: u32 = u64::BITS / BITS;

type Kids<V> = [Option<Box<Node<V>>>; WIDTH];

/// A node: the values of 16 consecutive keys (level 1), or the subtrees of
/// 16 consecutive spans. `n` counts the values below it; a node that holds
/// none is removed.
enum Node<V> {
    Leaf { vals: [Option<V>; WIDTH], n: usize },
    Branch { kids: Kids<V>, n: usize },
}

impl<V> Node<V> {
    const fn leaf() -> Self {
        Self::Leaf {
            vals: [const { None }; WIDTH],
            n: 0,
        }
    }

    const fn branch() -> Self {
        Self::Branch {
            kids: [const { None }; WIDTH],
            n: 0,
        }
    }

    const fn n(&self) -> usize {
        match self {
            Self::Leaf { n, .. } | Self::Branch { n, .. } => *n,
        }
    }
}

/// The slot `key` takes in a node of `level` (1 for a leaf).
#[expect(clippy::cast_possible_truncation, reason = "masked to four bits first")]
const fn digit(key: u64, level: u32) -> usize {
    ((key >> (BITS * (level - 1))) & (WIDTH as u64 - 1)) as usize
}

/// Whether a tree of `height` levels reaches `key`.
const fn covers(height: u32, key: u64) -> bool {
    height >= MAX_HEIGHT || key >> (BITS * height) == 0
}

/// Keys a child of a node at `level` (2 or more) spans.
const fn child_span(level: u32) -> u64 {
    1 << (BITS * (level - 1))
}

/// The ordered map.
pub struct RadixMap<V> {
    root: Option<Box<Node<V>>>,
    /// Levels below and including the root; 0 when empty.
    height: u32,
}

impl<V> Default for RadixMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> RadixMap<V> {
    /// An empty map.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            root: None,
            height: 0,
        }
    }

    /// How many keys it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |r| r.n())
    }

    /// Whether it holds none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// The value at `key`.
    #[must_use]
    pub fn get(&self, key: &u64) -> Option<&V> {
        let key = *key;
        if !covers(self.height, key) {
            return None;
        }
        let mut node = self.root.as_deref()?;
        let mut level = self.height;
        loop {
            match node {
                Node::Branch { kids, .. } => {
                    node = kids[digit(key, level)].as_deref()?;
                    level -= 1;
                }
                Node::Leaf { vals, .. } => return vals[digit(key, level)].as_ref(),
            }
        }
    }

    /// The value at `key`, to change.
    pub fn get_mut(&mut self, key: &u64) -> Option<&mut V> {
        let key = *key;
        if !covers(self.height, key) {
            return None;
        }
        let mut node = self.root.as_deref_mut()?;
        let mut level = self.height;
        loop {
            match node {
                Node::Branch { kids, .. } => {
                    node = kids[digit(key, level)].as_deref_mut()?;
                    level -= 1;
                }
                Node::Leaf { vals, .. } => return vals[digit(key, level)].as_mut(),
            }
        }
    }

    /// Whether it holds `key`.
    #[must_use]
    pub fn contains_key(&self, key: &u64) -> bool {
        self.get(key).is_some()
    }

    /// Puts `value` at `key`: the value it replaces, if any.
    pub fn insert(&mut self, key: u64, value: V) -> Option<V> {
        if let Some(old) = self.get_mut(&key) {
            return Some(core::mem::replace(old, value));
        }
        self.insert_new(key, value);
        None
    }

    /// Puts `value` at `key`, which holds none.
    fn insert_new(&mut self, key: u64, value: V) -> &mut V {
        self.grow_to(key);
        let mut level = self.height;
        let mut slot = &mut self.root;
        loop {
            let node = slot.get_or_insert_with(|| {
                Box::new(if level == 1 {
                    Node::leaf()
                } else {
                    Node::branch()
                })
            });
            let d = digit(key, level);
            match &mut **node {
                Node::Leaf { vals, n } => {
                    *n += 1;
                    return vals[d].insert(value);
                }
                Node::Branch { kids, n } => {
                    *n += 1;
                    slot = &mut kids[d];
                    level -= 1;
                }
            }
        }
    }

    /// Makes the tree tall enough to reach `key`.
    fn grow_to(&mut self, key: u64) {
        if self.root.is_none() {
            self.height = 1;
            while !covers(self.height, key) {
                self.height += 1;
            }
            return;
        }
        while !covers(self.height, key) {
            self.wrap_root();
        }
    }

    /// One level more: the root becomes the first child of a new root.
    fn wrap_root(&mut self) {
        let old = self.root.take();
        let n = old.as_ref().map_or(0, |r| r.n());
        let mut kids = [const { None }; WIDTH];
        kids[0] = old;
        self.root = Some(Box::new(Node::Branch { kids, n }));
        self.height += 1;
    }

    /// Takes out the value at `key`.
    pub fn remove(&mut self, key: &u64) -> Option<V> {
        let key = *key;
        if !covers(self.height, key) {
            return None;
        }
        let v = remove_in(&mut self.root, self.height, key);
        if self.root.is_none() {
            self.height = 0;
        }
        v
    }

    /// The place for `key`, held or not.
    pub fn entry(&mut self, key: u64) -> Entry<'_, V> {
        if self.contains_key(&key) {
            match self.get_mut(&key) {
                Some(value) => Entry::Occupied(OccupiedEntry { value }),
                // `contains_key` found it, under this same `&mut self`.
                None => unreachable!("radix: a key just found is gone"),
            }
        } else {
            Entry::Vacant(VacantEntry { map: self, key })
        }
    }

    /// The keys in `range` with their values, in key order.
    pub fn range<R: RangeBounds<u64>>(&self, range: R) -> Range<'_, V> {
        let (from, end) = bounds(&range);
        let mut r = Range {
            stack: [const { None }; MAX_HEIGHT as usize],
            depth: 0,
            from,
            end,
        };
        if let Some(root) = self.root.as_deref()
            && covers(self.height, from)
            && end.is_none_or(|e| from < e)
        {
            r.push(root, 0, self.height);
        }
        r
    }

    /// The keys in `range` with their values to change, in key order.
    pub fn range_mut<R: RangeBounds<u64>>(&mut self, range: R) -> RangeMut<'_, V> {
        let (from, end) = bounds(&range);
        let mut r = RangeMut {
            stack: [const { None }; MAX_HEIGHT as usize],
            depth: 0,
            from,
            end,
        };
        let height = self.height;
        if let Some(root) = self.root.as_deref_mut()
            && covers(height, from)
            && end.is_none_or(|e| from < e)
        {
            r.push(root, 0, height);
        }
        r
    }

    /// Every key with its value, in key order.
    pub fn iter(&self) -> Range<'_, V> {
        self.range(..)
    }

    /// The values, in key order.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.range(..).map(|(_, v)| v)
    }

    /// The values to change, in key order.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.range_mut(..).map(|(_, v)| v)
    }

    /// The values, taken, in key order.
    pub fn into_values(self) -> impl Iterator<Item = V> {
        self.into_iter().map(|(_, v)| v)
    }

    /// Takes out the keys from `key` on, as a map of their own.
    pub fn split_off(&mut self, key: &u64) -> Self {
        let key = *key;
        if key == 0 {
            return core::mem::take(self);
        }
        let mut out = Self::new();
        if !covers(self.height, key) {
            return out;
        }
        let height = self.height;
        let Some(root) = self.root.as_deref_mut() else {
            return out;
        };
        out.root = split_node(root, height, key);
        if root.n() == 0 {
            self.root = None;
            self.height = 0;
        }
        if out.root.is_some() {
            out.height = height;
        }
        out
    }

    /// Moves every key of `other` here; where both hold one, `other`'s
    /// value wins.
    pub fn append(&mut self, other: &mut Self) {
        let mut other = core::mem::take(other);
        if other.root.is_none() {
            return;
        }
        if self.root.is_none() {
            *self = other;
            return;
        }
        while self.height < other.height {
            self.wrap_root();
        }
        while other.height < self.height {
            other.wrap_root();
        }
        merge(&mut self.root, other.root.take());
    }
}

/// Takes the value at `key` out of the subtree in `slot` (at `level`), and
/// the subtree itself if that empties it.
fn remove_in<V>(slot: &mut Option<Box<Node<V>>>, level: u32, key: u64) -> Option<V> {
    let node = slot.as_deref_mut()?;
    let d = digit(key, level);
    let (v, empty) = match node {
        Node::Leaf { vals, n } => {
            let v = vals[d].take()?;
            *n -= 1;
            (v, *n == 0)
        }
        Node::Branch { kids, n } => {
            let v = remove_in(&mut kids[d], level - 1, key)?;
            *n -= 1;
            (v, *n == 0)
        }
    };
    if empty {
        *slot = None;
    }
    Some(v)
}

/// Moves the keys from `key` on out of `node` (at `level`, reaching
/// `key`) into a node of their own, if there are any.
fn split_node<V>(node: &mut Node<V>, level: u32, key: u64) -> Option<Box<Node<V>>> {
    let d = digit(key, level);
    match node {
        Node::Leaf { vals, n } => {
            let mut moved_vals = [const { None }; WIDTH];
            let mut moved = 0;
            for (from, to) in vals[d..].iter_mut().zip(&mut moved_vals[d..]) {
                if let Some(v) = from.take() {
                    *to = Some(v);
                    moved += 1;
                }
            }
            *n -= moved;
            (moved > 0).then(|| {
                Box::new(Node::Leaf {
                    vals: moved_vals,
                    n: moved,
                })
            })
        }
        Node::Branch { kids, n } => {
            let mut moved_kids: Kids<V> = [const { None }; WIDTH];
            let mut moved = 0;
            if key.is_multiple_of(child_span(level)) {
                // `key` is where the child begins: all of it goes.
                if let Some(c) = kids[d].take() {
                    moved += c.n();
                    moved_kids[d] = Some(c);
                }
            } else if let Some(c) = kids[d].as_deref_mut() {
                if let Some(part) = split_node(c, level - 1, key) {
                    moved += part.n();
                    moved_kids[d] = Some(part);
                }
                if c.n() == 0 {
                    kids[d] = None;
                }
            }
            for (from, to) in kids[d + 1..].iter_mut().zip(&mut moved_kids[d + 1..]) {
                if let Some(c) = from.take() {
                    moved += c.n();
                    *to = Some(c);
                }
            }
            *n -= moved;
            (moved > 0).then(|| {
                Box::new(Node::Branch {
                    kids: moved_kids,
                    n: moved,
                })
            })
        }
    }
}

/// Puts the subtree `src` into `dst`, both at one level; `src`'s values
/// win.
fn merge<V>(dst: &mut Option<Box<Node<V>>>, src: Option<Box<Node<V>>>) {
    let Some(src) = src else {
        return;
    };
    let Some(d) = dst.as_deref_mut() else {
        *dst = Some(src);
        return;
    };
    match (d, *src) {
        (Node::Leaf { vals, n }, Node::Leaf { vals: from, .. }) => {
            for (to, v) in vals.iter_mut().zip(from) {
                if let Some(v) = v {
                    if to.is_none() {
                        *n += 1;
                    }
                    *to = Some(v);
                }
            }
        }
        (Node::Branch { kids, n }, Node::Branch { kids: from, .. }) => {
            for (to, c) in kids.iter_mut().zip(from) {
                merge(to, c);
            }
            *n = kids.iter().flatten().map(|c| c.n()).sum();
        }
        // Both trees are of one height, so a level's nodes are of one kind.
        _ => unreachable!("radix: a leaf and a branch at one level"),
    }
}

/// `range` as its first key and the key past its last (`None`: no end).
fn bounds<R: RangeBounds<u64>>(range: &R) -> (u64, Option<u64>) {
    let from = match range.start_bound() {
        Bound::Included(&a) => a,
        // Past the last key: an empty range.
        Bound::Excluded(&a) => match a.checked_add(1) {
            Some(a) => a,
            None => return (0, Some(0)),
        },
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&b) => b.checked_add(1),
        Bound::Excluded(&b) => Some(b),
        Bound::Unbounded => None,
    };
    (from, end)
}

/// The first slot of a node at `level` beginning at `base` that holds
/// keys from `from` on.
fn first_slot(from: u64, base: u64, level: u32) -> usize {
    if from > base { digit(from, level) } else { 0 }
}

/// A node being walked: the slots left, and where the node begins.
enum Frame<L, B> {
    Leaf { slots: L, base: u64 },
    Branch { slots: B, base: u64, level: u32 },
}

type LeafSlots<'a, V> = core::iter::Enumerate<core::slice::Iter<'a, Option<V>>>;
type BranchSlots<'a, V> = core::iter::Enumerate<core::slice::Iter<'a, Option<Box<Node<V>>>>>;
type LeafSlotsMut<'a, V> = core::iter::Enumerate<core::slice::IterMut<'a, Option<V>>>;
type BranchSlotsMut<'a, V> = core::iter::Enumerate<core::slice::IterMut<'a, Option<Box<Node<V>>>>>;

/// [`RadixMap::range`]'s iterator.
pub struct Range<'a, V> {
    stack: [Option<Frame<LeafSlots<'a, V>, BranchSlots<'a, V>>>; MAX_HEIGHT as usize],
    depth: usize,
    from: u64,
    end: Option<u64>,
}

impl<'a, V> Range<'a, V> {
    fn push(&mut self, node: &'a Node<V>, base: u64, level: u32) {
        let skip = first_slot(self.from, base, level);
        let frame = match node {
            Node::Leaf { vals, .. } => {
                let mut slots = vals.iter().enumerate();
                if skip > 0 {
                    slots.nth(skip - 1);
                }
                Frame::Leaf { slots, base }
            }
            Node::Branch { kids, .. } => {
                let mut slots = kids.iter().enumerate();
                if skip > 0 {
                    slots.nth(skip - 1);
                }
                Frame::Branch { slots, base, level }
            }
        };
        self.stack[self.depth] = Some(frame);
        self.depth += 1;
    }
}

impl<'a, V> Iterator for Range<'a, V> {
    type Item = (u64, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        while self.depth > 0 {
            let child = match &mut self.stack[self.depth - 1] {
                Some(Frame::Leaf { slots, base }) => match slots.next() {
                    Some((i, Some(v))) => {
                        let key = *base + i as u64;
                        if self.end.is_some_and(|e| key >= e) {
                            self.depth = 0;
                            return None;
                        }
                        return Some((key, v));
                    }
                    Some((_, None)) => continue,
                    None => None,
                },
                Some(Frame::Branch { slots, base, level }) => match slots.next() {
                    Some((i, Some(c))) => {
                        let at = *base + i as u64 * child_span(*level);
                        if self.end.is_some_and(|e| at >= e) {
                            self.depth = 0;
                            return None;
                        }
                        Some((&**c, at, *level - 1))
                    }
                    Some((_, None)) => continue,
                    None => None,
                },
                None => None,
            };
            match child {
                Some((c, at, level)) => self.push(c, at, level),
                None => {
                    self.depth -= 1;
                    self.stack[self.depth] = None;
                }
            }
        }
        None
    }
}

/// [`RadixMap::range_mut`]'s iterator.
pub struct RangeMut<'a, V> {
    stack: [Option<Frame<LeafSlotsMut<'a, V>, BranchSlotsMut<'a, V>>>; MAX_HEIGHT as usize],
    depth: usize,
    from: u64,
    end: Option<u64>,
}

impl<'a, V> RangeMut<'a, V> {
    fn push(&mut self, node: &'a mut Node<V>, base: u64, level: u32) {
        let skip = first_slot(self.from, base, level);
        let frame = match node {
            Node::Leaf { vals, .. } => {
                let mut slots = vals.iter_mut().enumerate();
                if skip > 0 {
                    slots.nth(skip - 1);
                }
                Frame::Leaf { slots, base }
            }
            Node::Branch { kids, .. } => {
                let mut slots = kids.iter_mut().enumerate();
                if skip > 0 {
                    slots.nth(skip - 1);
                }
                Frame::Branch { slots, base, level }
            }
        };
        self.stack[self.depth] = Some(frame);
        self.depth += 1;
    }
}

impl<'a, V> Iterator for RangeMut<'a, V> {
    type Item = (u64, &'a mut V);

    fn next(&mut self) -> Option<Self::Item> {
        while self.depth > 0 {
            let child = match &mut self.stack[self.depth - 1] {
                Some(Frame::Leaf { slots, base }) => match slots.next() {
                    Some((i, Some(v))) => {
                        let key = *base + i as u64;
                        if self.end.is_some_and(|e| key >= e) {
                            self.depth = 0;
                            return None;
                        }
                        return Some((key, v));
                    }
                    Some((_, None)) => continue,
                    None => None,
                },
                Some(Frame::Branch { slots, base, level }) => match slots.next() {
                    Some((i, Some(c))) => {
                        let at = *base + i as u64 * child_span(*level);
                        if self.end.is_some_and(|e| at >= e) {
                            self.depth = 0;
                            return None;
                        }
                        Some((&mut **c, at, *level - 1))
                    }
                    Some((_, None)) => continue,
                    None => None,
                },
                None => None,
            };
            match child {
                Some((c, at, level)) => self.push(c, at, level),
                None => {
                    self.depth -= 1;
                    self.stack[self.depth] = None;
                }
            }
        }
        None
    }
}

/// A node [`IntoIter`] is emptying: the next slot to look at, and where
/// the node begins. The node stays boxed, so a frame is a few words and
/// the iterator fits a kernel stack.
struct Owned<V> {
    node: Box<Node<V>>,
    next: usize,
    base: u64,
    level: u32,
}

/// The map's iterator by value, in key order.
pub struct IntoIter<V> {
    stack: [Option<Owned<V>>; MAX_HEIGHT as usize],
    depth: usize,
}

impl<V> Iterator for IntoIter<V> {
    type Item = (u64, V);

    fn next(&mut self) -> Option<Self::Item> {
        while self.depth > 0 {
            let Some(top) = &mut self.stack[self.depth - 1] else {
                self.depth -= 1;
                continue;
            };
            let child = match &mut *top.node {
                Node::Leaf { vals, .. } => {
                    while let Some(slot) = vals.get_mut(top.next) {
                        let key = top.base + top.next as u64;
                        top.next += 1;
                        if let Some(v) = slot.take() {
                            return Some((key, v));
                        }
                    }
                    None
                }
                Node::Branch { kids, .. } => {
                    let mut found = None;
                    while let Some(slot) = kids.get_mut(top.next) {
                        let at = top.base + top.next as u64 * child_span(top.level);
                        top.next += 1;
                        if let Some(c) = slot.take() {
                            found = Some((c, at, top.level - 1));
                            break;
                        }
                    }
                    found
                }
            };
            match child {
                Some((node, base, level)) => {
                    self.stack[self.depth] = Some(Owned {
                        node,
                        next: 0,
                        base,
                        level,
                    });
                    self.depth += 1;
                }
                None => {
                    self.depth -= 1;
                    self.stack[self.depth] = None;
                }
            }
        }
        None
    }
}

impl<V> IntoIterator for RadixMap<V> {
    type Item = (u64, V);
    type IntoIter = IntoIter<V>;

    fn into_iter(mut self) -> IntoIter<V> {
        let mut it = IntoIter {
            stack: [const { None }; MAX_HEIGHT as usize],
            depth: 0,
        };
        if let Some(node) = self.root.take() {
            it.stack[0] = Some(Owned {
                node,
                next: 0,
                base: 0,
                level: self.height,
            });
            it.depth = 1;
        }
        it
    }
}

/// A place in the map ([`RadixMap::entry`]).
pub enum Entry<'a, V> {
    Vacant(VacantEntry<'a, V>),
    Occupied(OccupiedEntry<'a, V>),
}

/// A key the map does not hold.
pub struct VacantEntry<'a, V> {
    map: &'a mut RadixMap<V>,
    key: u64,
}

impl<'a, V> VacantEntry<'a, V> {
    /// The key.
    #[must_use]
    pub const fn key(&self) -> &u64 {
        &self.key
    }

    /// Puts `value` there.
    pub fn insert(self, value: V) -> &'a mut V {
        self.map.insert_new(self.key, value)
    }
}

/// A key the map holds, with its value.
pub struct OccupiedEntry<'a, V> {
    value: &'a mut V,
}

impl<'a, V> OccupiedEntry<'a, V> {
    /// The value.
    #[must_use]
    pub fn get(&self) -> &V {
        self.value
    }

    /// The value, to change.
    pub fn get_mut(&mut self) -> &mut V {
        self.value
    }

    /// The value, for as long as the map is borrowed.
    pub fn into_mut(self) -> &'a mut V {
        self.value
    }
}

impl<V: core::fmt::Debug> core::fmt::Debug for RadixMap<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

macro_rules! opaque_debug {
    ($($t:ident),*) => {$(
        impl<V> core::fmt::Debug for $t<'_, V> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct(stringify!($t)).finish_non_exhaustive()
            }
        }
    )*};
}
opaque_debug!(Range, RangeMut, Entry, VacantEntry, OccupiedEntry);

impl<V> core::fmt::Debug for IntoIter<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IntoIter").finish_non_exhaustive()
    }
}
