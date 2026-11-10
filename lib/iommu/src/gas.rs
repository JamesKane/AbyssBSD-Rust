// SPDX-License-Identifier: BSD-2-Clause
//! IOVA allocation: a translation domain's guest address space, with
//! FreeBSD's `iommu_gas` semantics (docs/IOMMU.md §4.6).
//!
//! A [`Space`] holds the entries of one domain in address order: mapped
//! allocations, reserved regions that are never allocated (a host bridge's
//! windows), and identity regions mapped at their own address (IORT RMRs).
//! Two empty placeholders sit at 0 and at the end, so every free gap lies
//! between two entries.
//!
//! - **First fit, lowest first.** [`Space::alloc`] takes the lowest address
//!   that fits the [`Request`]: its size and page offset, alignment,
//!   boundary, and limit.
//! - **Guard pages.** An allocation leaves at least one free page between
//!   it and the entries on either side when it is made, so IOVA 0 is never
//!   handed out and an overrun faults at the next page.
//! - **Logarithmic search.** The entries form an AVL tree keyed by end
//!   address, each node augmented with the largest free gap in its subtree
//!   (FreeBSD's `free_down`), so subtrees without a big enough gap are
//!   skipped whole.
//! - **Fallible memory.** Nodes live in an arena that grows with
//!   `try_reserve` and recycles freed nodes; running out is
//!   [`GasError::NoMemory`], never an abort. [`Space::reserve_nodes`]
//!   grows it ahead of a path that must not allocate.
//!
//! Behavior reference: sys/dev/iommu/iommu_gas.c (`iommu_gas_match_one`,
//! `iommu_gas_find_space`, `iommu_gas_alloc_region`,
//! `iommu_gas_reserve_region_extend`), with the board port's fixes:
//! reserving from 0 terminates (`1464aadaf77`) and a domain releases its
//! reservations as it goes (`cf75a5f809b`).

use alloc::vec::Vec;

use crate::{Iova, PAGE};

/// The size of a 48-bit space (FreeBSD's `MAXADDR_48BIT` + 1).
pub const SPACE_48: u64 = 1 << 48;

const NIL: u32 = u32::MAX;

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An allocation from [`Space::alloc`] (FreeBSD `IOMMU_MAP_ENTRY_MAP`).
    Mapped,
    /// A region never allocated (`IOMMU_MAP_ENTRY_UNMAPPED`).
    Reserved,
    /// A region mapped at its own address (`IOMMU_MAP_ENTRY_RMRR`).
    Identity,
}

/// An entry: `[start, end)`, page aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// First address.
    pub start: Iova,
    /// One past the last address.
    pub end: Iova,
    /// What it is.
    pub kind: Kind,
}

/// Why an operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GasError {
    /// Malformed arguments: misaligned, empty, inverted, or out of the
    /// space (FreeBSD `EINVAL`).
    Invalid,
    /// The region overlaps an entry it may not overlap (`EBUSY`).
    Busy,
    /// No free range satisfies the request (`ENOMEM` from the search).
    NoSpace,
    /// No memory for a tree node.
    NoMemory,
    /// No entry of the right kind starts there.
    NotFound,
    /// The space still holds allocations.
    InUse,
}

/// An allocation request: FreeBSD's `iommu_gas_match_args` with the tag's
/// constraints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// Bytes the device addresses, at least 1.
    pub size: u64,
    /// The buffer's offset in its first page, below [`PAGE`]: the
    /// allocation covers `offset + size` bytes from a page boundary.
    pub offset: u64,
    /// Alignment of the allocation's start, a power of two.
    pub align: u64,
    /// A boundary `[addr, addr + size)` must not cross: a power of two of
    /// at least [`PAGE`] (so entries stay page aligned), or 0 for none.
    pub boundary: u64,
    /// The highest bus address the device can reach, inclusive.
    pub limit: u64,
    /// Whether a request that would cross `boundary` may be cut short at
    /// it instead of moving past it (FreeBSD `IOMMU_MF_CANSPLIT`).
    pub can_split: bool,
}

impl Request {
    /// `size` bytes at a page boundary, with no other constraint.
    #[must_use]
    pub const fn new(size: u64) -> Self {
        Self {
            size,
            offset: 0,
            align: 1,
            boundary: 0,
            limit: u64::MAX,
            can_split: false,
        }
    }

    /// With the buffer at `offset` in its first page.
    #[must_use]
    pub const fn offset(self, offset: u64) -> Self {
        Self { offset, ..self }
    }

    /// With the start aligned to `align`.
    #[must_use]
    pub const fn align(self, align: u64) -> Self {
        Self { align, ..self }
    }

    /// Not crossing a multiple of `boundary`.
    #[must_use]
    pub const fn boundary(self, boundary: u64) -> Self {
        Self { boundary, ..self }
    }

    /// Below `limit`, inclusive.
    #[must_use]
    pub const fn limit(self, limit: u64) -> Self {
        Self { limit, ..self }
    }

    /// Allowed to be cut short at a boundary.
    #[must_use]
    pub const fn can_split(self) -> Self {
        Self {
            can_split: true,
            ..self
        }
    }

    fn validate(&self) -> Result<u64, GasError> {
        if self.size == 0
            || self.offset >= PAGE
            || !self.align.is_power_of_two()
            || (self.boundary != 0 && (!self.boundary.is_power_of_two() || self.boundary < PAGE))
        {
            return Err(GasError::Invalid);
        }
        // The smallest gap that can hold it: the pages plus a guard page
        // on each side.
        self.size
            .checked_add(self.offset)
            .and_then(|n| align_up(n, PAGE))
            .and_then(|n| n.checked_add(2 * PAGE))
            .ok_or(GasError::Invalid)
    }
}

/// A successful allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allocation {
    /// The entry's first page: what [`Space::free`] takes.
    pub start: Iova,
    /// One past the entry's last page.
    pub end: Iova,
    /// The bus address of the buffer: `start + offset`.
    pub addr: Iova,
    /// Bytes covered: the request's size, or less if it was split at a
    /// boundary.
    pub size: u64,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    start: u64,
    end: u64,
    /// `None` for the two placeholders.
    kind: Option<Kind>,
    left: u32,
    right: u32,
    height: u8,
    /// The lowest start in the subtree.
    first: u64,
    /// The highest end in the subtree.
    last: u64,
    /// The largest gap between neighboring entries within the subtree.
    free_down: u64,
}

/// One step of the ordered search.
enum Step {
    Found(u64, u64),
    /// Everything further is above the limit.
    Stop,
    Next,
}

/// A domain's IOVA space `[0, end)`.
#[derive(Debug)]
pub struct Space {
    nodes: Vec<Node>,
    root: u32,
    /// Recycled nodes, chained through `left`.
    free: u32,
    end: u64,
    len: usize,
}

const fn align_up(v: u64, align: u64) -> Option<u64> {
    match v.checked_add(align - 1) {
        Some(x) => Some(x & !(align - 1)),
        None => None,
    }
}

/// Whether `[addr, addr + size)` stays within one `boundary`-aligned block
/// (FreeBSD's `vm_addr_bound_ok`).
const fn bound_ok(addr: u64, size: u64, boundary: u64) -> bool {
    boundary == 0 || (addr ^ (addr + (size - 1))) & !(boundary - 1) == 0
}

/// Places `r` in the free gap `[beg, end)` between two entries, leaving a
/// guard page on each side: FreeBSD's `iommu_gas_match_one`. Returns the
/// allocation's start and the bytes it covers.
fn place(beg: u64, end: u64, r: &Request) -> Option<(u64, u64)> {
    let last = |start: u64, size: u64| start.checked_add(r.offset)?.checked_add(size - 1);
    let mut start = align_up(beg.checked_add(PAGE)?, r.align)?;
    // The last byte it may use.
    let end = end.checked_sub(PAGE + 1)?.min(r.limit);
    let mut size = r.size;
    if last(start, size)? > end {
        return None;
    }
    if !bound_ok(start + r.offset, size, r.boundary) {
        // Move past the boundary it crosses, or cut it there.
        let first = start;
        let b = align_up(start + r.offset + 1, r.boundary)?;
        start = align_up(b, r.align)?;
        let fits = last(start, size).is_some_and(|l| l <= end)
            && bound_ok(start + r.offset, size, r.boundary);
        if !fits {
            if !r.can_split {
                return None;
            }
            size = b - first - r.offset;
            start = first;
        }
    }
    Some((start, size))
}

impl Space {
    /// An empty space `[0, end)`. `end` is page aligned and at least three
    /// pages (one allocation and its guards).
    pub fn new(end: u64) -> Result<Self, GasError> {
        if !end.is_multiple_of(PAGE) || end < 3 * PAGE {
            return Err(GasError::Invalid);
        }
        let mut s = Self {
            nodes: Vec::new(),
            root: NIL,
            free: NIL,
            end,
            len: 0,
        };
        s.reserve_nodes(2)?;
        let a = s.node(0, 0, None)?;
        let b = s.node(end, end, None)?;
        s.insert(a);
        s.insert(b);
        Ok(s)
    }

    /// The end of the space.
    #[must_use]
    pub const fn end(&self) -> Iova {
        Iova::new(self.end)
    }

    /// Entries, placeholders not counted.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Makes room for `n` more entries without allocating later.
    pub fn reserve_nodes(&mut self, n: usize) -> Result<(), GasError> {
        let mut spare = 0;
        let mut f = self.free;
        while f != NIL && spare < n {
            spare += 1;
            f = self.nodes[f as usize].left;
        }
        self.nodes
            .try_reserve(n - spare)
            .map_err(|_| GasError::NoMemory)
    }

    /// A fresh node, from the free list or the arena.
    fn node(&mut self, start: u64, end: u64, kind: Option<Kind>) -> Result<u32, GasError> {
        let n = Node {
            start,
            end,
            kind,
            left: NIL,
            right: NIL,
            height: 1,
            first: start,
            last: end,
            free_down: 0,
        };
        if self.free != NIL {
            let i = self.free;
            self.free = self.nodes[i as usize].left;
            self.nodes[i as usize] = n;
            return Ok(i);
        }
        let i = u32::try_from(self.nodes.len())
            .ok()
            .filter(|&i| i != NIL)
            .ok_or(GasError::NoMemory)?;
        self.nodes.try_reserve(1).map_err(|_| GasError::NoMemory)?;
        self.nodes.push(n);
        Ok(i)
    }

    fn release(&mut self, i: u32) {
        self.nodes[i as usize].left = self.free;
        self.free = i;
    }

    fn n(&self, i: u32) -> &Node {
        &self.nodes[i as usize]
    }

    fn height(&self, i: u32) -> u8 {
        if i == NIL { 0 } else { self.n(i).height }
    }

    fn key(&self, i: u32) -> (u64, u64) {
        (self.n(i).end, self.n(i).start)
    }

    /// Recomputes `i`'s height and augmentation from its children.
    fn update(&mut self, i: u32) {
        let Node {
            start,
            end,
            left,
            right,
            ..
        } = *self.n(i);
        let (mut first, mut last, mut free, mut h) = (start, end, 0, 0);
        if left != NIL {
            let l = self.n(left);
            free = l.free_down.max(start - l.last);
            first = l.first;
            h = l.height;
        }
        if right != NIL {
            let r = self.n(right);
            free = free.max(r.free_down).max(r.first - end);
            last = r.last;
            h = h.max(r.height);
        }
        let n = &mut self.nodes[i as usize];
        n.first = first;
        n.last = last;
        n.free_down = free;
        n.height = h + 1;
    }

    fn rotate_right(&mut self, i: u32) -> u32 {
        let l = self.n(i).left;
        self.nodes[i as usize].left = self.n(l).right;
        self.nodes[l as usize].right = i;
        self.update(i);
        self.update(l);
        l
    }

    fn rotate_left(&mut self, i: u32) -> u32 {
        let r = self.n(i).right;
        self.nodes[i as usize].right = self.n(r).left;
        self.nodes[r as usize].left = i;
        self.update(i);
        self.update(r);
        r
    }

    /// Restores the AVL balance at `i` (whose subtrees are balanced) and
    /// returns the subtree's new root.
    fn balance(&mut self, i: u32) -> u32 {
        let (l, r) = (self.n(i).left, self.n(i).right);
        let (hl, hr) = (self.height(l), self.height(r));
        if hl > hr + 1 {
            if self.height(self.n(l).left) < self.height(self.n(l).right) {
                self.nodes[i as usize].left = self.rotate_left(l);
            }
            return self.rotate_right(i);
        }
        if hr > hl + 1 {
            if self.height(self.n(r).right) < self.height(self.n(r).left) {
                self.nodes[i as usize].right = self.rotate_right(r);
            }
            return self.rotate_left(i);
        }
        self.update(i);
        i
    }

    fn insert(&mut self, new: u32) {
        self.root = self.insert_at(self.root, new);
        if self.n(new).kind.is_some() {
            self.len += 1;
        }
    }

    fn insert_at(&mut self, at: u32, new: u32) -> u32 {
        if at == NIL {
            return new;
        }
        if self.key(new) < self.key(at) {
            let l = self.insert_at(self.n(at).left, new);
            self.nodes[at as usize].left = l;
        } else {
            let r = self.insert_at(self.n(at).right, new);
            self.nodes[at as usize].right = r;
        }
        self.balance(at)
    }

    /// Removes node `i` (which is in the tree) and recycles it.
    fn remove(&mut self, i: u32) {
        let key = self.key(i);
        self.root = self.remove_at(self.root, key);
        if self.n(i).kind.is_some() {
            self.len -= 1;
        }
        self.release(i);
    }

    fn remove_at(&mut self, at: u32, key: (u64, u64)) -> u32 {
        if at == NIL {
            return NIL;
        }
        let k = self.key(at);
        if key < k {
            let l = self.remove_at(self.n(at).left, key);
            self.nodes[at as usize].left = l;
        } else if key > k {
            let r = self.remove_at(self.n(at).right, key);
            self.nodes[at as usize].right = r;
        } else {
            let (l, r) = (self.n(at).left, self.n(at).right);
            if l == NIL {
                return r;
            }
            if r == NIL {
                return l;
            }
            let (r, min) = self.remove_min(r);
            self.nodes[min as usize].left = l;
            self.nodes[min as usize].right = r;
            return self.balance(min);
        }
        self.balance(at)
    }

    /// Detaches the lowest node under `at`: returns the subtree's new root
    /// and the detached node.
    fn remove_min(&mut self, at: u32) -> (u32, u32) {
        let l = self.n(at).left;
        if l == NIL {
            return (self.n(at).right, at);
        }
        let (l, min) = self.remove_min(l);
        self.nodes[at as usize].left = l;
        (self.balance(at), min)
    }

    /// The entry with the lowest end above `x` (the end placeholder if
    /// nothing else).
    fn first_end_above(&self, x: u64) -> u32 {
        let (mut at, mut best) = (self.root, NIL);
        while at != NIL {
            if self.n(at).end > x {
                best = at;
                at = self.n(at).left;
            } else {
                at = self.n(at).right;
            }
        }
        best
    }

    /// The first entry overlapping `[start, end)`, if any.
    fn overlapping(&self, start: u64, end: u64) -> Option<u32> {
        let i = self.first_end_above(start);
        (i != NIL && self.n(i).start < end && self.n(i).start < self.n(i).end).then_some(i)
    }

    /// The non-placeholder entry starting at `start`.
    fn starting_at(&self, start: u64) -> Option<u32> {
        let i = self.first_end_above(start);
        (i != NIL && self.n(i).start == start && self.n(i).kind.is_some()).then_some(i)
    }

    fn entry(&self, i: u32) -> Option<Entry> {
        let n = self.n(i);
        n.kind.map(|kind| Entry {
            start: Iova::new(n.start),
            end: Iova::new(n.end),
            kind,
        })
    }

    /// Allocates the lowest range satisfying `r`: FreeBSD's
    /// `iommu_gas_map` without the mapping.
    pub fn alloc(&mut self, r: &Request) -> Result<Allocation, GasError> {
        let min_free = r.validate()?;
        // A node first, so nothing fails after the search.
        self.reserve_nodes(1)?;
        let Step::Found(start, size) = self.search(self.root, r, min_free) else {
            return Err(GasError::NoSpace);
        };
        // `place` checked that the pages fit below the gap's end.
        let end = start + (size + r.offset).next_multiple_of(PAGE);
        let i = self.node(start, end, Some(Kind::Mapped))?;
        self.insert(i);
        Ok(Allocation {
            start: Iova::new(start),
            end: Iova::new(end),
            addr: Iova::new(start + r.offset),
            size,
        })
    }

    /// The address-ordered search under `at`: FreeBSD's
    /// `iommu_gas_find_space` over the lower region.
    fn search(&self, at: u32, r: &Request, min_free: u64) -> Step {
        if at == NIL || self.n(at).free_down < min_free {
            return Step::Next;
        }
        let n = *self.n(at);
        if n.left != NIL {
            match self.search(n.left, r, min_free) {
                Step::Next => {}
                s => return s,
            }
            let l = self.n(n.left);
            if n.start - l.last >= min_free
                && let Some((s, size)) = place(l.last, n.start, r)
            {
                return Step::Found(s, size);
            }
        }
        if n.end >= r.limit {
            // Every later gap starts above the limit.
            return Step::Stop;
        }
        if n.right != NIL {
            let rt = self.n(n.right);
            if rt.first - n.end >= min_free
                && let Some((s, size)) = place(n.end, rt.first, r)
            {
                return Step::Found(s, size);
            }
            return self.search(n.right, r, min_free);
        }
        Step::Next
    }

    /// Frees the allocation starting at `start` (FreeBSD
    /// `iommu_gas_free_space`).
    pub fn free(&mut self, start: Iova) -> Result<(), GasError> {
        let i = self.starting_at(start.get()).ok_or(GasError::NotFound)?;
        if self.n(i).kind != Some(Kind::Mapped) {
            return Err(GasError::NotFound);
        }
        self.remove(i);
        Ok(())
    }

    fn check_region(&self, start: u64, end: u64) -> Result<(), GasError> {
        if !start.is_multiple_of(PAGE)
            || !end.is_multiple_of(PAGE)
            || start >= end
            || end > self.end
        {
            return Err(GasError::Invalid);
        }
        Ok(())
    }

    fn add(&mut self, start: u64, end: u64, kind: Kind) -> Result<(), GasError> {
        let i = self.node(start, end, Some(kind))?;
        self.insert(i);
        Ok(())
    }

    /// Reserves `[start, end)`, which must overlap nothing (FreeBSD
    /// `iommu_gas_reserve_region`).
    pub fn reserve(&mut self, start: Iova, end: Iova) -> Result<(), GasError> {
        let (start, end) = (start.get(), end.get());
        self.check_region(start, end)?;
        if self.overlapping(start, end).is_some() {
            return Err(GasError::Busy);
        }
        self.add(start, end, Kind::Reserved)
    }

    /// Reserves whatever of `[start, end)` no entry holds yet, as one
    /// reserved entry per free gap (FreeBSD
    /// `iommu_gas_reserve_region_extend`). `end` is clamped to the space.
    pub fn reserve_extend(&mut self, start: Iova, end: Iova) -> Result<(), GasError> {
        let (mut at, end) = (start.get(), end.get().min(self.end));
        if at >= end {
            return Ok(());
        }
        self.check_region(at, end)?;
        while at < end {
            // Every entry before `next` ends at or below `at`, so
            // `[at, next.start)` is free. Each step moves past an entry's
            // end, which is above `at`: the loop advances even from 0,
            // where the empty placeholder sits.
            let next = *self.n(self.first_end_above(at));
            let gap_end = next.start.min(end);
            if at < gap_end {
                self.add(at, gap_end, Kind::Reserved)?;
            }
            at = at.max(next.end);
        }
        Ok(())
    }

    /// Records `[start, end)` as an identity region (an RMR), clipped
    /// where it overlaps identity regions already present at its ends, as
    /// FreeBSD's `iommu_gas_alloc_region` with `IOMMU_MF_RMRR` does.
    /// Returns the range added, for the caller to map, or `None` if it
    /// was already covered. Any other overlap is [`GasError::Busy`],
    /// including an identity region strictly inside it, which FreeBSD
    /// does not handle.
    pub fn identity(&mut self, start: Iova, end: Iova) -> Result<Option<(Iova, Iova)>, GasError> {
        let (mut start, mut end) = (start.get(), end.get());
        self.check_region(start, end)?;
        if let Some(i) = self.overlapping(start, end)
            && self.n(i).start <= start
        {
            if self.n(i).kind != Some(Kind::Identity) {
                return Err(GasError::Busy);
            }
            start = self.n(i).end;
        }
        if start >= end {
            return Ok(None);
        }
        let i = self.first_end_above(end - 1);
        let tail = *self.n(i);
        if tail.start < end && tail.start > start && tail.start < tail.end {
            if tail.kind != Some(Kind::Identity) {
                return Err(GasError::Busy);
            }
            end = tail.start;
        }
        if self.overlapping(start, end).is_some() {
            return Err(GasError::Busy);
        }
        self.add(start, end, Kind::Identity)?;
        Ok(Some((Iova::new(start), Iova::new(end))))
    }

    /// Removes the reserved or identity region starting at `start`
    /// (FreeBSD `iommu_gas_free_region`) and returns its kind.
    pub fn free_region(&mut self, start: Iova) -> Result<Kind, GasError> {
        let i = self.starting_at(start.get()).ok_or(GasError::NotFound)?;
        let kind = self.n(i).kind;
        match kind {
            Some(k @ (Kind::Reserved | Kind::Identity)) => {
                self.remove(i);
                Ok(k)
            }
            _ => Err(GasError::NotFound),
        }
    }

    /// Removes every reserved and identity region, passing each to `f`
    /// first so identity ones can be unmapped: the board port's
    /// `iommu_gas_free_reserved`, run as a domain is freed.
    pub fn release_reserved(&mut self, mut f: impl FnMut(Entry)) {
        let mut at = 0;
        while at < self.end {
            let i = self.first_end_above(at);
            let n = *self.n(i);
            if let Some(Kind::Reserved | Kind::Identity) = n.kind {
                if let Some(e) = self.entry(i) {
                    f(e);
                }
                self.remove(i);
            }
            at = n.end;
        }
    }

    /// Ends the space, which must hold no allocations (FreeBSD
    /// `iommu_gas_fini_domain`); reserved and identity regions are
    /// released first with [`Space::release_reserved`].
    pub fn fini(self) -> Result<(), GasError> {
        if !self.is_empty() {
            return Err(GasError::InUse);
        }
        Ok(())
    }

    /// The entries in address order, placeholders left out.
    #[must_use]
    pub fn iter(&self) -> Iter<'_> {
        let mut it = Iter {
            space: self,
            stack: [NIL; STACK],
            depth: 0,
        };
        it.descend(self.root);
        it
    }

    /// Checks the tree's invariants: order, no overlaps, balance, and the
    /// augmentation (FreeBSD's `iommu_gas_check_free`, behind
    /// `hw.iommu.check_free`).
    pub fn check(&self) -> Result<(), &'static str> {
        let mut prev = None;
        self.check_at(self.root, &mut prev)?;
        let mut count = 0;
        for e in self.iter() {
            count += 1;
            if e.end.get() > self.end {
                return Err("entry past the end");
            }
        }
        if count != self.len {
            return Err("entry count");
        }
        Ok(())
    }

    fn check_at(&self, at: u32, prev: &mut Option<(u64, u64)>) -> Result<u8, &'static str> {
        if at == NIL {
            return Ok(0);
        }
        let n = *self.n(at);
        let hl = self.check_at(n.left, prev)?;
        if n.start > n.end {
            return Err("inverted entry");
        }
        if let Some((ps, pe)) = *prev {
            if (pe, ps) >= (n.end, n.start) {
                return Err("out of order");
            }
            if pe > n.start {
                return Err("overlapping entries");
            }
        }
        *prev = Some((n.start, n.end));
        let hr = self.check_at(n.right, prev)?;
        if hl.abs_diff(hr) > 1 || n.height != hl.max(hr) + 1 {
            return Err("unbalanced");
        }
        let (mut first, mut last, mut free) = (n.start, n.end, 0);
        if n.left != NIL {
            let l = self.n(n.left);
            free = l.free_down.max(n.start - l.last);
            first = l.first;
        }
        if n.right != NIL {
            let r = self.n(n.right);
            free = free.max(r.free_down).max(r.first - n.end);
            last = r.last;
        }
        if (first, last, free) != (n.first, n.last, n.free_down) {
            return Err("stale augmentation");
        }
        Ok(n.height)
    }
}

/// Deep enough for any AVL tree of `u32`-indexed nodes (height below
/// 1.44 log2 n + 2).
const STACK: usize = 64;

/// In-order iteration over a [`Space`]'s entries.
#[derive(Debug)]
pub struct Iter<'a> {
    space: &'a Space,
    stack: [u32; STACK],
    depth: usize,
}

impl Iter<'_> {
    fn descend(&mut self, mut at: u32) {
        while at != NIL && self.depth < STACK {
            self.stack[self.depth] = at;
            self.depth += 1;
            at = self.space.n(at).left;
        }
    }
}

impl Iterator for Iter<'_> {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        while self.depth > 0 {
            self.depth -= 1;
            let at = self.stack[self.depth];
            self.descend(self.space.n(at).right);
            if let Some(e) = self.space.entry(at) {
                return Some(e);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;
