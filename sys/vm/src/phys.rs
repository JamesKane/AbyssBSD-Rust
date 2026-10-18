// SPDX-License-Identifier: BSD-2-Clause
//! The physical page allocator: a binary buddy system.
//!
//! RAM is described as [`Segment`]s, contiguous frame ranges that each
//! belong to a NUMA domain. Every managed frame has a [`VmPage`] in one
//! array that the caller provides (the kernel carves it out of free memory
//! at boot). Free blocks are naturally aligned runs of `2^order` frames
//! that never cross a segment; each domain keeps one doubly linked free
//! list per order, threaded through the `VmPage`s by index, so the
//! allocator never touches the memory it manages.
//!
//! Page states keep the merge rule sound: only a free block's first page
//! says [`PageState::Free`]; pages absorbed into a larger block become
//! [`PageState::Tail`]. Freeing a block merges it with its buddy while the
//! buddy's first page is free at the same order.
//!
//! Behavior reference: sys/vm/vm_phys.c.

use core::fmt;

use crate::{ORDERS, Order, Pfn};

/// The most segments the allocator tracks.
pub const MAX_SEGMENTS: usize = 64;
/// The most NUMA domains.
pub const MAX_DOMAINS: usize = 8;

const NIL: u32 = u32::MAX;

/// A `VmPage` link as an array index (u32 always fits a usize here).
fn ix(link: u32) -> usize {
    usize::try_from(link).unwrap_or(usize::MAX)
}

/// A NUMA domain (SRAT proximity domains, renumbered densely from 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DomainId(pub u8);

impl DomainId {
    const fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a page is doing, as far as the allocator knows.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageState {
    /// Not managed: in use since boot, or never handed to the allocator.
    Reserved = 0,
    /// The first page of a free block (its order is in [`VmPage::order`]).
    Free = 1,
    /// Inside a free block, but not its first page.
    Tail = 2,
    /// The first page of an allocated block.
    Allocated = 3,
}

/// Per-frame metadata. Sixteen bytes per 4 KiB frame.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VmPage {
    next: u32,
    prev: u32,
    order: u8,
    state: PageState,
    reserved: [u8; 6],
}

impl VmPage {
    /// An unmanaged page.
    pub const RESERVED: Self = Self {
        next: NIL,
        prev: NIL,
        order: 0,
        state: PageState::Reserved,
        reserved: [0; 6],
    };

    /// The page's state.
    #[must_use]
    pub const fn state(&self) -> PageState {
        self.state
    }
}

const _: () = assert!(size_of::<VmPage>() == 16);

/// A contiguous range of RAM frames in one domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The first frame.
    pub start: Pfn,
    /// One past the last frame.
    pub end: Pfn,
    /// The domain the memory belongs to.
    pub domain: DomainId,
}

impl Segment {
    const EMPTY: Self = Self {
        start: Pfn(0),
        end: Pfn(0),
        domain: DomainId(0),
    };

    /// Frames in the segment.
    #[must_use]
    pub const fn pages(&self) -> u64 {
        self.end.0 - self.start.0
    }
}

#[derive(Clone, Copy, Debug)]
struct SegmentSlot {
    seg: Segment,
    /// Index of the segment's first page in the `VmPage` array.
    first: u32,
}

#[derive(Clone, Copy, Debug)]
struct Domain {
    heads: [u32; ORDERS],
    counts: [u64; ORDERS],
    free_pages: u64,
    managed_pages: u64,
}

impl Domain {
    const EMPTY: Self = Self {
        heads: [NIL; ORDERS],
        counts: [0; ORDERS],
        free_pages: 0,
        managed_pages: 0,
    };
}

/// Why the allocator could not be set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// More than [`MAX_SEGMENTS`] segments.
    TooManySegments,
    /// A domain number is not below [`MAX_DOMAINS`].
    BadDomain(DomainId),
    /// Segments are empty, unsorted, or overlapping.
    BadSegment(Segment),
    /// The page array is shorter than [`pages_needed`] says.
    ArrayTooSmall { needed: u64, have: u64 },
}

/// Why a free was refused. Each one is a kernel bug in the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeError {
    /// The frame is not in any segment.
    Unmanaged(Pfn),
    /// The frame is not aligned to the order.
    Misaligned(Pfn, Order),
    /// The block is not allocated (a double free), or was allocated at a
    /// different order.
    NotAllocated(Pfn, Order, PageState),
}

impl fmt::Display for FreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unmanaged(p) => write!(f, "{p:?} is not managed memory"),
            Self::Misaligned(p, o) => write!(f, "{p:?} is not aligned to order {}", o.get()),
            Self::NotAllocated(p, o, s) => {
                write!(f, "{p:?} freed at order {} but it is {s:?}", o.get())
            }
        }
    }
}

/// Frame counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frames handed to the allocator.
    pub managed: u64,
    /// Frames currently free.
    pub free: u64,
}

/// The number of [`VmPage`]s the array must hold for `segments`.
#[must_use]
pub fn pages_needed(segments: &[Segment]) -> u64 {
    segments.iter().map(Segment::pages).sum()
}

/// The buddy allocator.
pub struct PhysAllocator<'a> {
    pages: &'a mut [VmPage],
    segments: [SegmentSlot; MAX_SEGMENTS],
    nsegments: usize,
    domains: [Domain; MAX_DOMAINS],
    ndomains: usize,
}

impl fmt::Debug for PhysAllocator<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhysAllocator")
            .field("segments", &self.nsegments)
            .field("domains", &self.ndomains)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl<'a> PhysAllocator<'a> {
    /// An allocator over `segments` (sorted, disjoint) with all frames
    /// reserved. Hand free memory over with [`PhysAllocator::add_free`].
    pub fn new(pages: &'a mut [VmPage], segments: &[Segment]) -> Result<Self, InitError> {
        if segments.len() > MAX_SEGMENTS {
            return Err(InitError::TooManySegments);
        }
        let needed = pages_needed(segments);
        let have = pages.len() as u64;
        if have < needed || needed >= u64::from(NIL) {
            return Err(InitError::ArrayTooSmall { needed, have });
        }
        let mut slots = [SegmentSlot {
            seg: Segment::EMPTY,
            first: 0,
        }; MAX_SEGMENTS];
        let mut first = 0u32;
        let mut ndomains = 0;
        let mut prev_end = 0;
        for (slot, seg) in slots.iter_mut().zip(segments) {
            if seg.end <= seg.start || seg.start.0 < prev_end {
                return Err(InitError::BadSegment(*seg));
            }
            if seg.domain.index() >= MAX_DOMAINS {
                return Err(InitError::BadDomain(seg.domain));
            }
            prev_end = seg.end.0;
            ndomains = ndomains.max(seg.domain.index() + 1);
            *slot = SegmentSlot { seg: *seg, first };
            // Fits: the total is below NIL, checked above.
            first += u32::try_from(seg.pages()).unwrap_or(0);
        }
        pages[..usize::try_from(needed).unwrap_or(0)].fill(VmPage::RESERVED);
        Ok(Self {
            pages,
            segments: slots,
            nsegments: segments.len(),
            domains: [Domain::EMPTY; MAX_DOMAINS],
            ndomains,
        })
    }

    /// Hands the frames `[start, end)` to the allocator. Frames outside
    /// every segment are ignored; frames already managed must not be
    /// passed again.
    pub fn add_free(&mut self, start: Pfn, end: Pfn) {
        let mut p = start;
        while p < end {
            let Some(s) = self.segment_of(p) else {
                // Skip to the next segment that starts after `p`.
                match self.segments[..self.nsegments]
                    .iter()
                    .find(|s| s.seg.start > p)
                {
                    Some(next) => p = next.seg.start,
                    None => return,
                }
                continue;
            };
            let limit = end.min(self.segments[s].seg.end);
            while p < limit {
                // The largest aligned block at `p` that fits.
                let mut order = Order::ZERO;
                while let Some(up) = order.higher() {
                    if !p.0.is_multiple_of(up.pages()) || p.0 + up.pages() > limit.0 {
                        break;
                    }
                    order = up;
                }
                let i = self.index_in(s, p);
                self.pages[i].state = PageState::Allocated;
                self.pages[i].order = order.get();
                let dom = self.segments[s].seg.domain.index();
                self.domains[dom].managed_pages += order.pages();
                // Cannot fail: the block was just marked allocated.
                let _ = self.free(p, order);
                p = p.add(order.pages());
            }
        }
    }

    /// Allocates a block of `order`, preferring `domain` and falling back
    /// to the others in turn.
    pub fn alloc(&mut self, order: Order, domain: DomainId) -> Option<Pfn> {
        let n = self.ndomains.max(1);
        (0..n)
            .map(|k| (domain.index() + k) % n)
            .find_map(|d| self.alloc_in(order, d))
    }

    fn alloc_in(&mut self, order: Order, d: usize) -> Option<Pfn> {
        self.alloc_in_where(order, d, |_| true)
    }

    /// Allocates a block of `order` lying wholly below frame `limit` (for
    /// devices that cannot address all of memory). Slower than
    /// [`PhysAllocator::alloc`]: it searches the free lists.
    pub fn alloc_below(&mut self, order: Order, limit: Pfn) -> Option<Pfn> {
        let n = self.ndomains.max(1);
        (0..n).find_map(|d| {
            self.alloc_in_where(order, d, |p| p.0.saturating_add(order.pages()) <= limit.0)
        })
    }

    /// Takes the first free block of at least `order` in domain `d` whose
    /// start `ok` accepts, and carves `order` from its low end.
    fn alloc_in_where(&mut self, order: Order, d: usize, ok: impl Fn(Pfn) -> bool) -> Option<Pfn> {
        let (i, mut found) = (order.index()..ORDERS).find_map(|o| {
            let mut cur = self.domains[d].heads[o];
            while cur != NIL {
                let i = ix(cur);
                if ok(self.pfn_at(i)) {
                    return Some((i, o));
                }
                cur = self.pages[i].next;
            }
            None
        })?;
        self.unlink(d, i, found);
        // Split down to the requested order, freeing the upper halves.
        while found > order.index() {
            found -= 1;
            let half = 1usize << found;
            let upper = i + half;
            self.pages[upper].state = PageState::Free;
            self.pages[upper].order = Order(u8::try_from(found).unwrap_or(0)).get();
            self.push(d, upper, found);
        }
        self.pages[i].state = PageState::Allocated;
        self.pages[i].order = order.get();
        self.domains[d].free_pages -= order.pages();
        Some(self.pfn_at(i))
    }

    /// Returns a block that [`PhysAllocator::alloc`] handed out.
    pub fn free(&mut self, pfn: Pfn, order: Order) -> Result<(), FreeError> {
        let s = self.segment_of(pfn).ok_or(FreeError::Unmanaged(pfn))?;
        if !pfn.0.is_multiple_of(order.pages())
            || pfn.0 + order.pages() > self.segments[s].seg.end.0
        {
            return Err(FreeError::Misaligned(pfn, order));
        }
        let mut i = self.index_in(s, pfn);
        let page = self.pages[i];
        if page.state != PageState::Allocated || page.order != order.get() {
            return Err(FreeError::NotAllocated(pfn, order, page.state));
        }
        let d = self.segments[s].seg.domain.index();
        self.domains[d].free_pages += order.pages();

        let seg = self.segments[s].seg;
        let mut p = pfn;
        let mut order = order;
        while let Some(up) = order.higher() {
            let buddy = Pfn(p.0 ^ order.pages());
            if buddy < seg.start || buddy.0 + order.pages() > seg.end.0 {
                break;
            }
            let b = self.index_in(s, buddy);
            if self.pages[b].state != PageState::Free || self.pages[b].order != order.get() {
                break;
            }
            self.unlink(d, b, order.index());
            // The lower of the two heads the merged block.
            let (head, tail) = if b < i { (b, i) } else { (i, b) };
            self.pages[tail].state = PageState::Tail;
            i = head;
            p = Pfn(p.0.min(buddy.0));
            order = up;
        }
        self.pages[i].state = PageState::Free;
        self.pages[i].order = order.get();
        self.push(d, i, order.index());
        Ok(())
    }

    /// Frame counts over all domains.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.domains[..self.ndomains]
            .iter()
            .fold(Stats::default(), |s, d| Stats {
                managed: s.managed + d.managed_pages,
                free: s.free + d.free_pages,
            })
    }

    /// Free blocks per order in `domain`.
    #[must_use]
    pub fn free_blocks(&self, domain: DomainId) -> [u64; ORDERS] {
        self.domains
            .get(domain.index())
            .map_or([0; ORDERS], |d| d.counts)
    }

    /// The segments, in address order.
    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        self.segments[..self.nsegments].iter().map(|s| s.seg)
    }

    /// The state of the page for `pfn`, if it is managed.
    #[must_use]
    pub fn state(&self, pfn: Pfn) -> Option<PageState> {
        let s = self.segment_of(pfn)?;
        Some(self.pages[self.index_in(s, pfn)].state)
    }

    fn segment_of(&self, pfn: Pfn) -> Option<usize> {
        let segs = &self.segments[..self.nsegments];
        let i = segs.partition_point(|s| s.seg.end <= pfn);
        (i < segs.len() && segs[i].seg.start <= pfn).then_some(i)
    }

    fn index_in(&self, s: usize, pfn: Pfn) -> usize {
        let slot = &self.segments[s];
        // Both terms fit: the array length was checked against u32.
        let off = usize::try_from(pfn.0 - slot.seg.start.0).unwrap_or(0);
        ix(slot.first) + off
    }

    fn pfn_at(&self, i: usize) -> Pfn {
        let segs = &self.segments[..self.nsegments];
        let s = segs.partition_point(|s| ix(s.first) <= i) - 1;
        segs[s]
            .seg
            .start
            .add(u64::try_from(i - ix(segs[s].first)).unwrap_or(0))
    }

    fn push(&mut self, d: usize, i: usize, order: usize) {
        let head = self.domains[d].heads[order];
        let idx = u32::try_from(i).unwrap_or(NIL);
        self.pages[i].next = head;
        self.pages[i].prev = NIL;
        if head != NIL {
            self.pages[ix(head)].prev = idx;
        }
        self.domains[d].heads[order] = idx;
        self.domains[d].counts[order] += 1;
    }

    fn unlink(&mut self, d: usize, i: usize, order: usize) {
        let VmPage { next, prev, .. } = self.pages[i];
        if prev == NIL {
            self.domains[d].heads[order] = next;
        } else {
            self.pages[ix(prev)].next = next;
        }
        if next != NIL {
            self.pages[ix(next)].prev = prev;
        }
        self.pages[i].next = NIL;
        self.pages[i].prev = NIL;
        self.domains[d].counts[order] -= 1;
    }
}

/// A per-CPU cache of free single pages in front of the buddy allocator,
/// so that most single-page allocations and frees take no shared lock. It
/// refills and drains [`Self::BATCH`] pages at a time, so a CPU that
/// alternates between allocating and freeing settles in the middle.
///
/// Pages in a magazine count as allocated to the buddy allocator. A double
/// free of a cached page is not caught here, only once it is drained.
///
/// Behavior reference: sys/vm/vm_page.c (the per-CPU page caches).
#[derive(Debug)]
pub struct PageMagazine {
    pfns: [Pfn; PageMagazine::CAPACITY],
    len: usize,
}

impl PageMagazine {
    /// Pages a magazine holds.
    pub const CAPACITY: usize = 64;
    /// Pages moved to or from the buddy allocator at a time.
    pub const BATCH: usize = Self::CAPACITY / 2;

    /// An empty magazine.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pfns: [Pfn(0); Self::CAPACITY],
            len: 0,
        }
    }

    /// Pages held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds no pages.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Takes a cached page.
    pub fn pop(&mut self) -> Option<Pfn> {
        self.len = self.len.checked_sub(1)?;
        Some(self.pfns[self.len])
    }

    /// Caches a free page; gives it back if the magazine is full.
    pub fn push(&mut self, pfn: Pfn) -> Result<(), Pfn> {
        if self.len == Self::CAPACITY {
            return Err(pfn);
        }
        self.pfns[self.len] = pfn;
        self.len += 1;
        Ok(())
    }

    /// Moves up to [`Self::BATCH`] single pages from `a` (from `domain`)
    /// into the magazine. Returns how many.
    pub fn refill(&mut self, a: &mut PhysAllocator<'_>, domain: DomainId) -> usize {
        let want = Self::BATCH.min(Self::CAPACITY - self.len);
        let mut got = 0;
        while got < want {
            let Some(p) = a.alloc(Order::ZERO, domain) else {
                break;
            };
            self.pfns[self.len] = p;
            self.len += 1;
            got += 1;
        }
        got
    }

    /// Returns up to [`Self::BATCH`] pages to `a`: half the magazine when
    /// it is full, or everything (`all`).
    pub fn drain(&mut self, a: &mut PhysAllocator<'_>, all: bool) -> Result<(), FreeError> {
        let n = if all {
            self.len
        } else {
            Self::BATCH.min(self.len)
        };
        for _ in 0..n {
            self.len -= 1;
            a.free(self.pfns[self.len], Order::ZERO)?;
        }
        Ok(())
    }
}

impl Default for PageMagazine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    use super::*;

    fn seg(start: u64, end: u64, d: u8) -> Segment {
        Segment {
            start: Pfn(start),
            end: Pfn(end),
            domain: DomainId(d),
        }
    }

    fn with<R>(segs: &[Segment], f: impl FnOnce(&mut PhysAllocator<'_>) -> R) -> R {
        let n = usize::try_from(pages_needed(segs)).unwrap();
        let mut pages = vec![VmPage::RESERVED; n];
        let mut a = PhysAllocator::new(&mut pages, segs).unwrap();
        f(&mut a)
    }

    #[test]
    fn splits_and_merges_back() {
        with(&[seg(0, 4096, 0)], |a| {
            a.add_free(Pfn(0), Pfn(4096));
            assert_eq!(
                a.stats(),
                Stats {
                    managed: 4096,
                    free: 4096
                }
            );
            assert_eq!(a.free_blocks(DomainId(0))[12], 1);
            let p = a.alloc(Order::ZERO, DomainId(0)).unwrap();
            assert_eq!(a.stats().free, 4095);
            // One block of each order 0..12 is left after the split.
            let blocks = a.free_blocks(DomainId(0));
            assert!(blocks[..12].iter().all(|&n| n == 1), "{blocks:?}");
            assert_eq!(blocks[12], 0);
            a.free(p, Order::ZERO).unwrap();
            assert_eq!(a.free_blocks(DomainId(0))[12], 1);
            assert_eq!(a.stats().free, 4096);
        });
    }

    #[test]
    fn blocks_are_aligned_and_distinct() {
        with(&[seg(3, 1000, 0)], |a| {
            a.add_free(Pfn(3), Pfn(1000));
            let mut got = Vec::new();
            for o in [0u8, 3, 1, 5, 2, 0, 4] {
                let o = Order::new(o).unwrap();
                let p = a.alloc(o, DomainId(0)).unwrap();
                assert_eq!(p.0 % o.pages(), 0, "{p:?} at order {o:?}");
                assert!(p.0 >= 3 && p.0 + o.pages() <= 1000);
                got.push((p.0, p.0 + o.pages(), o));
            }
            got.sort_unstable();
            for w in got.windows(2) {
                assert!(w[0].1 <= w[1].0, "overlap: {w:?}");
            }
            for (p, _, o) in got {
                a.free(Pfn(p), o).unwrap();
            }
            assert_eq!(a.stats().free, 997);
        });
    }

    #[test]
    fn refuses_bad_frees() {
        with(&[seg(0, 64, 0)], |a| {
            a.add_free(Pfn(0), Pfn(64));
            let p = a.alloc(Order::new(1).unwrap(), DomainId(0)).unwrap();
            assert!(matches!(
                a.free(p, Order::ZERO),
                Err(FreeError::NotAllocated(..))
            ));
            assert!(matches!(
                a.free(p.add(1), Order::ZERO),
                Err(FreeError::NotAllocated(..))
            ));
            a.free(p, Order::new(1).unwrap()).unwrap();
            assert!(matches!(
                a.free(p, Order::new(1).unwrap()),
                Err(FreeError::NotAllocated(
                    _,
                    _,
                    PageState::Free | PageState::Tail
                ))
            ));
            assert_eq!(
                a.free(Pfn(64), Order::ZERO),
                Err(FreeError::Unmanaged(Pfn(64)))
            );
            assert!(matches!(
                a.free(Pfn(1), Order::new(1).unwrap()),
                Err(FreeError::Misaligned(..))
            ));
            assert_eq!(a.stats().free, 64);
        });
    }

    #[test]
    fn exhausts_then_falls_back_across_domains() {
        with(&[seg(0, 16, 0), seg(32, 48, 1)], |a| {
            a.add_free(Pfn(0), Pfn(64));
            assert_eq!(a.stats().managed, 32);
            let mut d1 = 0;
            for _ in 0..32 {
                let p = a.alloc(Order::ZERO, DomainId(1)).unwrap();
                if p.0 >= 32 {
                    d1 += 1;
                } else {
                    assert_eq!(d1, 16, "domain 1 is preferred until empty");
                }
            }
            assert_eq!(a.alloc(Order::ZERO, DomainId(0)), None);
        });
    }

    #[test]
    fn segments_bound_blocks_and_holes_stay_reserved() {
        // Two segments that would be buddies if they were one.
        with(&[seg(0, 8, 0), seg(8, 16, 0)], |a| {
            a.add_free(Pfn(0), Pfn(16));
            assert_eq!(a.free_blocks(DomainId(0))[3], 2);
            assert_eq!(a.free_blocks(DomainId(0))[4], 0);
            assert!(a.alloc(Order::new(4).unwrap(), DomainId(0)).is_none());
        });
        with(&[seg(0, 64, 0)], |a| {
            a.add_free(Pfn(0), Pfn(10));
            a.add_free(Pfn(20), Pfn(64));
            assert_eq!(a.state(Pfn(15)), Some(PageState::Reserved));
            assert_eq!(a.stats().free, 54);
            let mut n = 0;
            while let Some(p) = a.alloc(Order::ZERO, DomainId(0)) {
                assert!(!(10..20).contains(&p.0));
                n += 1;
            }
            assert_eq!(n, 54);
        });
    }

    #[test]
    fn rejects_bad_setup() {
        let mut pages = vec![VmPage::RESERVED; 4];
        assert!(matches!(
            PhysAllocator::new(&mut pages, &[seg(0, 8, 0)]),
            Err(InitError::ArrayTooSmall { needed: 8, have: 4 })
        ));
        assert!(matches!(
            PhysAllocator::new(&mut pages, &[seg(2, 4, 0), seg(0, 2, 0)]),
            Err(InitError::BadSegment(_))
        ));
        assert!(matches!(
            PhysAllocator::new(&mut pages, &[seg(0, 2, 9)]),
            Err(InitError::BadDomain(DomainId(9)))
        ));
    }

    /// A deterministic churn: random orders, random frees, and a full
    /// accounting check at the end.
    #[test]
    fn churn_balances() {
        with(&[seg(1, 3000, 0), seg(4096, 9000, 1)], |a| {
            a.add_free(Pfn(0), Pfn(9000));
            let total = a.stats().free;
            let mut rng = 0x1234_5678_u64;
            let mut next = || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            let mut live: Vec<(Pfn, Order)> = Vec::new();
            for _ in 0..20_000 {
                if next() % 3 != 0 || live.is_empty() {
                    let o = Order::new(u8::try_from(next() % 6).unwrap()).unwrap();
                    let d = DomainId(u8::try_from(next() % 2).unwrap());
                    if let Some(p) = a.alloc(o, d) {
                        live.push((p, o));
                    }
                } else {
                    let k = usize::try_from(next()).unwrap() % live.len();
                    let (p, o) = live.swap_remove(k);
                    a.free(p, o).unwrap();
                }
                let used: u64 = live.iter().map(|(_, o)| o.pages()).sum();
                assert_eq!(a.stats().free + used, total);
            }
            for (p, o) in live {
                a.free(p, o).unwrap();
            }
            assert_eq!(a.stats().free, total);
            // Everything merged back into maximal blocks.
            let mut fresh_pages = vec![VmPage::RESERVED; 9000];
            let segs = [seg(1, 3000, 0), seg(4096, 9000, 1)];
            let mut fresh = PhysAllocator::new(&mut fresh_pages, &segs).unwrap();
            fresh.add_free(Pfn(0), Pfn(9000));
            for d in [DomainId(0), DomainId(1)] {
                assert_eq!(a.free_blocks(d), fresh.free_blocks(d));
            }
        });
    }

    #[test]
    fn magazine_batches() {
        with(&[seg(0, 256, 0)], |a| {
            a.add_free(Pfn(0), Pfn(256));
            let total = a.stats().free;
            let mut m = PageMagazine::new();
            assert_eq!(m.pop(), None);
            assert_eq!(m.refill(a, DomainId(0)), PageMagazine::BATCH);
            assert_eq!(a.stats().free, total - PageMagazine::BATCH as u64);
            let mut held: Vec<Pfn> = (0..PageMagazine::BATCH).map(|_| m.pop().unwrap()).collect();
            assert!(m.is_empty());
            for p in held.drain(..) {
                m.push(p).unwrap();
            }
            // Fill to capacity, then the next push is refused.
            m.refill(a, DomainId(0));
            assert_eq!(m.len(), PageMagazine::CAPACITY);
            let extra = a.alloc(Order::ZERO, DomainId(0)).unwrap();
            assert_eq!(m.push(extra), Err(extra));
            m.drain(a, false).unwrap();
            assert_eq!(m.len(), PageMagazine::CAPACITY - PageMagazine::BATCH);
            m.push(extra).unwrap();
            m.drain(a, true).unwrap();
            assert!(m.is_empty());
            assert_eq!(a.stats().free, total);
        });
    }

    #[test]
    fn alloc_below_a_limit() {
        with(&[seg(0, 256, 0)], |a| {
            a.add_free(Pfn(0), Pfn(256));
            // Take the low blocks so the free lists start high.
            let low: Vec<Pfn> = (0..8)
                .map(|_| a.alloc(Order(3), DomainId(0)).unwrap())
                .collect();
            assert!(
                a.alloc_below(Order(3), Pfn(64)).is_none(),
                "nothing free below 64"
            );
            for p in low.iter().take(2) {
                a.free(*p, Order(3)).unwrap();
            }
            let p = a.alloc_below(Order(2), Pfn(64)).unwrap();
            assert!(p.0 + 4 <= 64, "{p:?}");
            assert!(a.alloc_below(Order(0), Pfn(1)).is_none_or(|q| q.0 == 0));
        });
    }
}
