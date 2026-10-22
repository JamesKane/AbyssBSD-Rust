// SPDX-License-Identifier: BSD-2-Clause
//! Memory objects: the pages behind a mapping (`vm_object`).
//!
//! An object holds pages by page index. Anonymous memory starts with no
//! pages and gains a zeroed one at its first fault. Copy-on-write stacks
//! objects into shadow chains: a *shadow* holds the pages written since
//! it was made, and finds every other page in its *backing* object, at an
//! offset. A page belongs to exactly one object, which frees it when the
//! object goes away; the page tables only ever map pages of objects that a
//! live map entry reaches.
//!
//! When a backing object is reachable only through its one shadow (the
//! other sharers exited or wrote their own copies), [`Object::collapse`]
//! folds it into the shadow, so chains do not grow with every `fork`.
//!
//! An object may instead have a *pager*, which supplies its pages: a file
//! (FreeBSD's vnode object). Its pages are filled on first touch and kept,
//! shared by every mapping of the file; private mappings shadow it as
//! usual. A pager object has a size, and has no backing object.
//!
//! A pager object may write its pages back (a file on a disk): it tracks
//! which pages are dirty, and [`Object::sync`] writes them. Filling a page
//! may sleep, so a fault never fills under its map lock: [`Object::lookup`]
//! with `fill` false reports [`Found::Fill`], and the caller fills with
//! [`Object::fill`] after letting the lock go, then looks again.
//!
//! Each object has its own spin lock, held for page lookups and inserts.
//! Locks are taken top-down along a chain (shadow before backing), never
//! the other way.
//!
//! Behavior reference: sys/vm/vm_object.c (shadow chains and collapse).

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use sync::SpinLock;

use crate::Pfn;

/// What objects need from the physical allocator.
pub trait PageOps: Sync {
    /// A zeroed page, or `None` when memory is exhausted.
    fn alloc_zeroed(&self) -> Option<Pfn>;
    /// A new page holding a copy of `from`, or `None` when memory is
    /// exhausted.
    fn alloc_copy(&self, from: Pfn) -> Option<Pfn>;
    /// A new page that `fill` writes (all of it), or `None` when memory is
    /// exhausted.
    fn alloc_filled(&self, fill: &mut dyn FnMut(&mut [u8])) -> Option<Pfn>;
    /// Returns a page that no page table maps any more.
    fn free(&self, pfn: Pfn);
    /// Calls `f` with page `pfn`'s contents.
    fn access(&self, pfn: Pfn, f: &mut dyn FnMut(&mut [u8]));
}

/// A pager could not move a page (an I/O error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PagerError;

/// Where a pager object's pages come from, and where dirty ones go.
pub trait Pager: Send + Sync {
    /// The object's size in pages; a page past it does not exist.
    fn size(&self) -> PageIndex;
    /// Writes page `index`'s contents into `page` (a whole page, zero
    /// past the end of the data). May sleep.
    fn fill(&self, index: PageIndex, page: &mut [u8]) -> Result<(), PagerError>;
    /// The most pages one fill may read (a cluster: FreeBSD's
    /// `MAXPHYS` worth); 1 reads page by page.
    fn max_run(&self) -> PageIndex {
        1
    }
    /// Fills the pages from `index` on into `buf`, whole pages, as many
    /// as it holds (at most [`Self::max_run`], none past the end): one
    /// transfer where the data is contiguous. May sleep.
    fn fill_run(&self, index: PageIndex, buf: &mut [u8]) -> Result<(), PagerError> {
        for (i, page) in (index..).zip(buf.as_chunks_mut::<PAGE_BYTES>().0) {
            self.fill(i, page)?;
        }
        Ok(())
    }
    /// Whether pages written through mappings or the page cache go back
    /// to the pager (then the object tracks which are dirty).
    fn writes_back(&self) -> bool {
        false
    }
    /// Writes page `index` back. May sleep.
    fn write_back(&self, _index: PageIndex, _page: &[u8]) -> Result<(), PagerError> {
        Ok(())
    }
    /// Writes the pages from `index` on back, from `buf` (whole pages, at
    /// most [`Self::max_run`]): one transfer where their blocks are
    /// contiguous (a cluster, as FreeBSD's `cluster_write`). May sleep.
    fn write_back_run(&self, index: PageIndex, buf: &[u8]) -> Result<(), PagerError> {
        for (i, page) in (index..).zip(buf.as_chunks::<PAGE_BYTES>().0) {
            self.write_back(i, page)?;
        }
        Ok(())
    }
    /// Starts filling `pages` pages from `index` on (read-ahead), without
    /// waiting: `None` if this pager cannot, or not now. May sleep.
    fn start_run(&self, _index: PageIndex, _pages: PageIndex) -> Option<Box<dyn PendingFill>> {
        None
    }
}

/// A fill [`Pager::start_run`] started.
pub trait PendingFill: Send {
    /// Waits for the data: the run's pages, whole. May sleep.
    fn wait(&mut self) -> Result<Vec<u8>, PagerError>;
    /// Runs `insert` if the data still stands for what the pager holds
    /// (nothing written or moved since the fill started), keeping it so
    /// until `insert` returns; returns whether it ran. `insert` must not
    /// allocate. May sleep.
    fn commit(&self, insert: &mut dyn FnMut()) -> bool;
}

/// A read-ahead in flight.
struct Ahead {
    index: PageIndex,
    pages: PageIndex,
    fill: Box<dyn PendingFill>,
}

/// Why a page lookup found nothing to map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupError {
    /// No memory to fill a pager's page.
    OutOfMemory,
    /// Past the end of a pager object (`SIGBUS`).
    PastEnd,
    /// The pager could not read the page (`SIGBUS`, or `EIO` to a read).
    Io,
}

/// A page index within an object.
pub type PageIndex = u64;

const PAGE_BYTES: usize = 4096;

/// Pages dirty in every object, for throttling writers.
static DIRTY: AtomicUsize = AtomicUsize::new(0);

/// Every pager object, for reclaiming clean pages (dead ones are pruned
/// as reclaiming passes).
static PAGED: SpinLock<Vec<Weak<Object>>> = SpinLock::new(Vec::new());

/// Pages dirty in all objects together.
#[must_use]
pub fn dirty_total() -> usize {
    DIRTY.load(Ordering::Relaxed)
}

/// Frees up to `want` clean pages of pager objects nothing maps (only
/// their file system holds them) and nobody is reading or writing
/// through, oldest objects first; returns how many it freed. Never waits
/// for a lock: a caller may hold one.
pub fn reclaim_clean(want: usize) -> usize {
    let objects: Vec<Arc<Object>> = {
        let Some(mut list) = PAGED.try_lock() else {
            return 0;
        };
        list.retain(|w| w.strong_count() > 0);
        list.iter().filter_map(Weak::upgrade).collect()
    };
    let mut freed = 0;
    for o in &objects {
        if freed >= want {
            break;
        }
        freed += o.drop_clean(want - freed);
    }
    freed
}

/// A memory object.
pub struct Object {
    inner: SpinLock<Inner>,
    pages_ops: &'static dyn PageOps,
    /// Only one mapping (possibly clipped into several entries of one map)
    /// reaches this object, so unmapping a range may free its pages
    /// (`OBJ_ONEMAPPING`). Cleared for good once shared.
    one_mapping: AtomicBool,
    pager: Option<Arc<dyn Pager>>,
    /// Page-cache transfers and write-backs under way, which hold page
    /// frames outside the lock: reclaiming leaves the object alone.
    io: AtomicUsize,
    /// A pager object's file system still holds its reference (see
    /// [`Object::disown`]).
    owned: AtomicBool,
    /// Read-ahead: the page a sequential fill would ask next, and how many
    /// pages the next fill reads.
    ra_next: AtomicU64,
    ra_window: AtomicU64,
    /// The next window, read ahead while the last is used.
    ahead: SpinLock<Option<Ahead>>,
}

/// A transfer through an object's pages: its frames stay put until the
/// guard goes.
#[derive(Debug)]
pub struct IoGuard<'a>(&'a Object);

impl Drop for IoGuard<'_> {
    fn drop(&mut self) {
        self.0.io.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Inner {
    pages: BTreeMap<PageIndex, Pfn>,
    /// Pages written since they were last written back (only for pagers
    /// that write back).
    dirty: BTreeSet<PageIndex>,
    /// Pages a shared mapping has mapped writable: writing one back
    /// leaves it dirty, since writes through that mapping are not seen
    /// (no page table is searched to take the write right back).
    mapped_writable: BTreeSet<PageIndex>,
    /// The object this one shadows, and where this one's index 0 falls in
    /// it.
    backing: Option<(Arc<Object>, PageIndex)>,
}

/// Where a page lookup found the page.
#[derive(Clone, Debug)]
pub enum Found {
    /// In the object asked (the top of the chain): it may be written.
    Top(Pfn),
    /// In an object it shadows: it may only be read until copied up.
    Backing(Pfn),
    /// Nowhere in the chain: the page reads as zeros.
    Absent,
    /// A pager object of the chain should have it but has not read it
    /// yet: fill it (with no map lock held) and look again.
    Fill(Arc<Object>, PageIndex),
}

impl Object {
    /// An empty anonymous object.
    #[must_use]
    pub fn anonymous(ops: &'static dyn PageOps) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(Inner {
                pages: BTreeMap::new(),
                dirty: BTreeSet::new(),
                mapped_writable: BTreeSet::new(),
                backing: None,
            }),
            pages_ops: ops,
            one_mapping: AtomicBool::new(true),
            pager: None,
            owned: AtomicBool::new(false),
            io: AtomicUsize::new(0),
            ra_next: AtomicU64::new(0),
            ra_window: AtomicU64::new(1),
            ahead: SpinLock::new(None),
        })
    }

    /// An empty object shadowing `backing` from page `offset` on.
    #[must_use]
    pub fn shadow(backing: Arc<Self>, offset: PageIndex) -> Arc<Self> {
        let ops = backing.pages_ops;
        Arc::new(Self {
            inner: SpinLock::new(Inner {
                pages: BTreeMap::new(),
                dirty: BTreeSet::new(),
                mapped_writable: BTreeSet::new(),
                backing: Some((backing, offset)),
            }),
            pages_ops: ops,
            one_mapping: AtomicBool::new(true),
            pager: None,
            owned: AtomicBool::new(false),
            io: AtomicUsize::new(0),
            ra_next: AtomicU64::new(0),
            ra_window: AtomicU64::new(1),
            ahead: SpinLock::new(None),
        })
    }

    /// An object whose pages `pager` supplies. It counts as shared from
    /// the start: unmapping never frees its pages, which stay cached.
    ///
    /// Its file system holds a reference until it calls [`Self::disown`]
    /// (when the file's vnode goes): knowing which reference is the file
    /// system's is how [`reclaim_clean`] tells an object nothing maps.
    #[must_use]
    pub fn with_pager(ops: &'static dyn PageOps, pager: Arc<dyn Pager>) -> Arc<Self> {
        let o = Arc::new(Self {
            inner: SpinLock::new(Inner {
                pages: BTreeMap::new(),
                dirty: BTreeSet::new(),
                mapped_writable: BTreeSet::new(),
                backing: None,
            }),
            pages_ops: ops,
            one_mapping: AtomicBool::new(false),
            pager: Some(pager),
            owned: AtomicBool::new(true),
            io: AtomicUsize::new(0),
            ra_next: AtomicU64::new(0),
            ra_window: AtomicU64::new(1),
            ahead: SpinLock::new(None),
        });
        PAGED.lock().push(Arc::downgrade(&o));
        o
    }

    /// Holds this object's page frames in place for a transfer through
    /// them (see [`reclaim_clean`]): take it before looking pages up.
    pub fn io(&self) -> IoGuard<'_> {
        self.io.fetch_add(1, Ordering::SeqCst);
        IoGuard(self)
    }

    /// The file system lets go of its reference (it drops it next): any
    /// reference left is a mapping's or a shadow's.
    pub fn disown(&self) {
        self.owned.store(false, Ordering::SeqCst);
    }

    /// Frees up to `want` of this object's clean pages, if nothing maps it
    /// and no transfer is under way. `self` is a reference of the caller's
    /// own, beside the file system's if it has not disowned the object.
    pub fn drop_clean(self: &Arc<Self>, want: usize) -> usize {
        let Some(mut inner) = self.inner.try_lock() else {
            return 0;
        };
        // Ours and the file system's: no map entry or shadow reaches it,
        // and a new one finds the pages gone when it looks (under this
        // lock). A transfer takes its guard before it looks.
        let expected = 1 + usize::from(self.owned.load(Ordering::SeqCst));
        if Arc::strong_count(self) > expected || self.io.load(Ordering::SeqCst) != 0 {
            return 0;
        }
        let Inner { pages, dirty, .. } = &mut *inner;
        let doomed: Vec<PageIndex> = pages
            .keys()
            .filter(|i| !dirty.contains(i))
            .take(want)
            .copied()
            .collect();
        let freed: Vec<Pfn> = doomed.iter().filter_map(|i| pages.remove(i)).collect();
        drop(inner);
        let n = freed.len();
        for pfn in freed {
            self.pages_ops.free(pfn);
        }
        n
    }

    /// Whether only one mapping reaches this object.
    #[must_use]
    pub fn one_mapping(&self) -> bool {
        self.one_mapping.load(Ordering::Relaxed)
    }

    /// Records that a second mapping (a `fork`) reaches this object.
    pub fn set_shared(&self) {
        self.one_mapping.store(false, Ordering::Relaxed);
    }

    /// Finds page `index` along the chain, filling it from a pager object
    /// that should have it. With `fold` (the caller's mapping alone
    /// reaches this object), first folds away what only this object still
    /// reaches, as [`Self::collapse`] does, if the look at this object
    /// shows there is some: a fault in a shared chain then pays nothing
    /// for it.
    ///
    /// With `fill` false, a page a pager object of the chain has not read
    /// is reported as [`Found::Fill`] instead of read here.
    pub fn lookup(
        self: &Arc<Self>,
        index: PageIndex,
        fold: bool,
        fill: bool,
    ) -> Result<Found, LookupError> {
        if self.pager.is_some() {
            if let Some(pfn) = self.page_or_fill(index, fill)? {
                return Ok(Found::Top(pfn));
            }
            return Ok(Found::Fill(Arc::clone(self), index));
        } else {
            let inner = self.inner.lock();
            if let Some(&pfn) = inner.pages.get(&index) {
                return Ok(Found::Top(pfn));
            }
            let foldable = fold
                && inner
                    .backing
                    .as_ref()
                    .is_some_and(|(b, _)| Arc::strong_count(b) == 1 && b.pager.is_none());
            drop(inner);
            if foldable {
                self.collapse();
                return self.lookup(index, false, fill);
            }
        }
        let mut next = self.backing();
        let mut index = index;
        while let Some((object, off)) = next {
            index += off;
            if let Some(pfn) = object.page_or_fill(index, fill)? {
                return Ok(Found::Backing(pfn));
            }
            if object.pager.is_some() {
                return Ok(Found::Fill(object, index));
            }
            next = object.backing();
        }
        Ok(Found::Absent)
    }

    /// Page `index` along the chain if some object already holds it: no
    /// filling, no allocating (for prefaulting).
    #[must_use]
    pub fn resident_page(&self, index: PageIndex) -> Option<Pfn> {
        if let Some(&pfn) = self.inner.lock().pages.get(&index) {
            return Some(pfn);
        }
        let mut next = self.backing();
        let mut index = index;
        while let Some((object, off)) = next {
            index += off;
            if let Some(&pfn) = object.inner.lock().pages.get(&index) {
                return Some(pfn);
            }
            next = object.backing();
        }
        None
    }

    /// This object's page `index`: present, or (with `fill`) filled now if
    /// this is a pager object.
    fn page_or_fill(&self, index: PageIndex, fill: bool) -> Result<Option<Pfn>, LookupError> {
        if let Some(&pfn) = self.inner.lock().pages.get(&index) {
            return Ok(Some(pfn));
        }
        let Some(pager) = &self.pager else {
            return Ok(None);
        };
        if index >= pager.size() {
            return Err(LookupError::PastEnd);
        }
        if !fill {
            return Ok(None);
        }
        if let Some(pfn) = self.take_ahead(pager.as_ref(), index)? {
            return Ok(Some(pfn));
        }
        let run = self.run_at(pager.as_ref(), index);
        if run == pager.max_run() {
            // A stream: the next window goes out before this one is waited
            // for, so the disk has it in hand by the time it is asked.
            self.start_ahead(pager.as_ref(), index + run);
        }
        if run == 1 {
            // Fill outside the lock; a racing fill of the same page loses.
            let mut failed = false;
            let new = self
                .pages_ops
                .alloc_filled(&mut |page| failed = pager.fill(index, page).is_err())
                .ok_or(LookupError::OutOfMemory)?;
            if failed {
                self.pages_ops.free(new);
                return Err(LookupError::Io);
            }
            return Ok(Some(self.insert_filled(index, new)));
        }
        // A cluster: one transfer into a buffer, copied into pages (a
        // copy costs far less than the transfer saved).
        let mut buf = alloc::vec![0u8; usize::try_from(run).unwrap_or(1) * PAGE_BYTES];
        pager
            .fill_run(index, &mut buf)
            .map_err(|_| LookupError::Io)?;
        let mut first = None;
        for (i, chunk) in (index..).zip(buf.as_chunks::<PAGE_BYTES>().0) {
            let Some(new) = self
                .pages_ops
                .alloc_filled(&mut |page| page.copy_from_slice(chunk))
            else {
                // The page asked for is in; read-ahead may stop short.
                if first.is_none() {
                    return Err(LookupError::OutOfMemory);
                }
                break;
            };
            let pfn = self.insert_filled(i, new);
            first.get_or_insert(pfn);
        }
        first.map(Some).ok_or(LookupError::OutOfMemory)
    }

    /// Starts reading `max_run` pages from `index` on ahead, unless one is
    /// already going there; never past the end or over a page held.
    fn start_ahead(&self, pager: &dyn Pager, index: PageIndex) {
        if self.ahead.lock().as_ref().is_some_and(|a| a.index == index) {
            return;
        }
        let end = index.saturating_add(pager.max_run()).min(pager.size());
        let end = self
            .inner
            .lock()
            .pages
            .range(index..end)
            .next()
            .map_or(end, |(&i, _)| i);
        if end <= index {
            return;
        }
        if let Some(fill) = pager.start_run(index, end - index) {
            // One left unused (the reader went elsewhere) is dropped.
            let old = self.ahead.lock().replace(Ahead {
                index,
                pages: end - index,
                fill,
            });
            drop(old);
        }
    }

    /// Page `index` from the read-ahead in flight there, if one is: the
    /// next window is started, then this one waited for and put in.
    /// `Ok(None)` when there is none, or its data no longer stands.
    fn take_ahead(&self, pager: &dyn Pager, index: PageIndex) -> Result<Option<Pfn>, LookupError> {
        let taken = {
            let mut ahead = self.ahead.lock();
            if ahead.as_ref().is_none_or(|a| a.index != index) {
                return Ok(None);
            }
            ahead.take()
        };
        let Some(Ahead {
            pages, mut fill, ..
        }) = taken
        else {
            return Ok(None);
        };
        self.start_ahead(pager, index + pages);
        self.ra_window.store(pages, Ordering::Relaxed);
        self.ra_next.store(index + pages, Ordering::Relaxed);
        let Ok(buf) = fill.wait() else {
            // Read again, synchronously, to report the error.
            return Ok(None);
        };
        // Pages made outside every lock (making one may write others
        // back); put in only if the data still stands.
        let mut made = Vec::new();
        for chunk in buf.as_chunks::<PAGE_BYTES>().0 {
            match self
                .pages_ops
                .alloc_filled(&mut |page| page.copy_from_slice(chunk))
            {
                Some(new) => made.push(new),
                None => break,
            }
        }
        let mut first = None;
        let committed = fill.commit(&mut || {
            for (i, &new) in (index..).zip(&made) {
                let pfn = self.insert_filled(i, new);
                first.get_or_insert(pfn);
            }
        });
        if !committed {
            for new in made {
                self.pages_ops.free(new);
            }
            return Ok(None);
        }
        Ok(first)
    }

    /// Inserts a page just filled at `index`, unless a racing fill got
    /// there first (then that page wins and this one is freed).
    fn insert_filled(&self, index: PageIndex, new: Pfn) -> Pfn {
        let mut inner = self.inner.lock();
        if let Some(&pfn) = inner.pages.get(&index) {
            drop(inner);
            self.pages_ops.free(new);
            return pfn;
        }
        inner.pages.insert(index, new);
        new
    }

    /// How many pages to fill from `index` on: the read-ahead window when
    /// fills come in order (doubling each time, up to the pager's run),
    /// one page otherwise; never past the end or over a page held.
    fn run_at(&self, pager: &dyn Pager, index: PageIndex) -> PageIndex {
        let max = pager.max_run().max(1);
        let window = if self.ra_next.load(Ordering::Relaxed) == index {
            self.ra_window
                .load(Ordering::Relaxed)
                .saturating_mul(2)
                .min(max)
        } else {
            1
        };
        let end = index.saturating_add(window).min(pager.size());
        let inner = self.inner.lock();
        let end = inner
            .pages
            .range(index + 1..end)
            .next()
            .map_or(end, |(&i, _)| i);
        drop(inner);
        let run = end.saturating_sub(index).max(1);
        self.ra_window.store(run, Ordering::Relaxed);
        self.ra_next.store(index + run, Ordering::Relaxed);
        run
    }

    /// The page at `index` of this object (not the chain), creating it if
    /// it is missing: zeroed, or a copy of `below` (the page the chain
    /// supplied). Returns `None` when memory is exhausted.
    ///
    /// If another fault inserted the page meanwhile, that page wins and the
    /// new one is freed.
    pub fn get_or_insert(&self, index: PageIndex, below: Option<Pfn>) -> Option<Pfn> {
        if let Some(&pfn) = self.inner.lock().pages.get(&index) {
            return Some(pfn);
        }
        // Allocate (and zero or copy) outside the lock.
        self.insert_new(index, below)
    }

    /// [`Self::get_or_insert`] for a page the caller just found missing
    /// here: no first look.
    pub fn insert_new(&self, index: PageIndex, below: Option<Pfn>) -> Option<Pfn> {
        let new = match below {
            Some(from) => self.pages_ops.alloc_copy(from)?,
            None => self.pages_ops.alloc_zeroed()?,
        };
        let mut inner = self.inner.lock();
        if let Some(&pfn) = inner.pages.get(&index) {
            drop(inner);
            self.pages_ops.free(new);
            return Some(pfn);
        }
        inner.pages.insert(index, new);
        Some(new)
    }

    /// Frees the pages in `[start, end)`, which no page table maps any
    /// more (an unmapped range of an object nothing else shares).
    pub fn free_range(&self, start: PageIndex, end: PageIndex) {
        let mut inner = self.inner.lock();
        let tail = inner.pages.split_off(&start);
        let mut keep = tail;
        let mut doomed = keep.split_off(&end);
        core::mem::swap(&mut doomed, &mut keep);
        inner.pages.append(&mut keep);
        drop(inner);
        for pfn in doomed.into_values() {
            self.pages_ops.free(pfn);
        }
    }

    /// Page `index` of this object, read from its pager if need be (may
    /// sleep: call with no map lock held).
    pub fn fill(&self, index: PageIndex) -> Result<Pfn, LookupError> {
        self.page_or_fill(index, true)?.ok_or(LookupError::PastEnd)
    }

    /// Page `index` of this object, if it holds it.
    #[must_use]
    pub fn page_here(&self, index: PageIndex) -> Option<Pfn> {
        self.inner.lock().pages.get(&index).copied()
    }

    /// Page `index` of this object to write into: held, read from the
    /// pager, or (for an anonymous object, or past the pager's data) a new
    /// zeroed page.
    pub fn page_for_write(&self, index: PageIndex) -> Result<Pfn, LookupError> {
        match self.page_or_fill(index, true) {
            Ok(Some(pfn)) => Ok(pfn),
            Ok(None) | Err(LookupError::PastEnd) => self
                .get_or_insert(index, None)
                .ok_or(LookupError::OutOfMemory),
            Err(e) => Err(e),
        }
    }

    /// Page `index` of this object to overwrite or to start from zeros:
    /// held, or a new zeroed page, never read from the pager.
    pub fn page_for_overwrite(&self, index: PageIndex) -> Result<Pfn, LookupError> {
        if let Some(pfn) = self.page_here(index) {
            return Ok(pfn);
        }
        self.get_or_insert(index, None)
            .ok_or(LookupError::OutOfMemory)
    }

    /// Calls `f` with page `pfn`'s contents (a page of this object).
    pub fn access(&self, pfn: Pfn, f: &mut dyn FnMut(&mut [u8])) {
        self.pages_ops.access(pfn, f);
    }

    /// Whether a pager supplies this object's pages.
    #[must_use]
    pub fn has_pager(&self) -> bool {
        self.pager.is_some()
    }

    /// Whether written pages must go back to the pager.
    #[must_use]
    pub fn tracks_dirty(&self) -> bool {
        self.pager.as_ref().is_some_and(|p| p.writes_back())
    }

    /// Records that page `index` was written.
    pub fn mark_dirty(&self, index: PageIndex) {
        if self.tracks_dirty() && self.inner.lock().dirty.insert(index) {
            DIRTY.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records that a shared mapping maps page `index` writable (it was
    /// just marked dirty): it stays dirty for good.
    pub fn mark_mapped_writable(&self, index: PageIndex) {
        if self.tracks_dirty() {
            let mut inner = self.inner.lock();
            if inner.dirty.insert(index) {
                DIRTY.fetch_add(1, Ordering::Relaxed);
            }
            inner.mapped_writable.insert(index);
        }
    }

    /// Whether page `index` was written since it was last written back.
    #[must_use]
    pub fn is_dirty(&self, index: PageIndex) -> bool {
        self.inner.lock().dirty.contains(&index)
    }

    /// How many pages are dirty.
    #[must_use]
    pub fn dirty_pages(&self) -> usize {
        self.inner.lock().dirty.len()
    }

    /// Writes the dirty pages in `[start, end)` back to the pager (each is
    /// clean from just before its write; one that fails is dirty again).
    pub fn sync(&self, start: PageIndex, end: PageIndex) -> Result<(), PagerError> {
        let Some(pager) = &self.pager else {
            return Ok(());
        };
        // The pages are written outside the lock: hold them in place.
        let _io = self.io();
        let todo: Vec<(PageIndex, Pfn)> = {
            let mut inner = self.inner.lock();
            let indexes: Vec<PageIndex> = inner.dirty.range(start..end).copied().collect();
            let mut out = Vec::with_capacity(indexes.len());
            for i in indexes {
                if !inner.mapped_writable.contains(&i) {
                    inner.dirty.remove(&i);
                    DIRTY.fetch_sub(1, Ordering::Relaxed);
                }
                if let Some(&pfn) = inner.pages.get(&i) {
                    out.push((i, pfn));
                }
            }
            out
        };
        // Runs of consecutive pages go back together.
        let max_run = usize::try_from(pager.max_run().max(1)).unwrap_or(1);
        let mut result = Ok(());
        let mut rest = todo.as_slice();
        while let Some(&(first, _)) = rest.first() {
            let len = rest
                .iter()
                .zip(first..)
                .take(max_run)
                .take_while(|&(&(i, _), want)| i == want)
                .count();
            let (run, after) = rest.split_at(len);
            rest = after;
            let mut r = Ok(());
            if let [(i, pfn)] = run {
                self.pages_ops
                    .access(*pfn, &mut |page| r = pager.write_back(*i, page));
            } else {
                let mut buf = Vec::with_capacity(run.len() * PAGE_BYTES);
                for &(_, pfn) in run {
                    self.pages_ops
                        .access(pfn, &mut |page| buf.extend_from_slice(page));
                }
                r = pager.write_back_run(first, &buf);
            }
            if r.is_err() {
                let mut inner = self.inner.lock();
                for &(i, _) in run {
                    if inner.dirty.insert(i) {
                        DIRTY.fetch_add(1, Ordering::Relaxed);
                    }
                }
                result = r;
            }
        }
        result
    }

    /// Cuts the object to `pages` pages: those past it go, if nothing but
    /// the caller holds the object (no mapping can still reach them, since
    /// every map entry and shadow holds a reference). Otherwise they stay,
    /// zeroed, until the object goes (no page table is searched for them
    /// here): what was cut off never reads back if the file grows again.
    pub fn truncate(self: &Arc<Self>, pages: PageIndex) {
        let mut inner = self.inner.lock();
        let cut = inner.dirty.split_off(&pages);
        DIRTY.fetch_sub(cut.len(), Ordering::Relaxed);
        let _ = inner.mapped_writable.split_off(&pages);
        if Arc::strong_count(self) > 1 {
            // Kept for whoever still holds the object, but zeroed: should
            // the file grow again, they must read as zeros, not as what
            // was cut off.
            let kept: Vec<Pfn> = inner.pages.range(pages..).map(|(_, &p)| p).collect();
            drop(inner);
            for pfn in kept {
                self.pages_ops.access(pfn, &mut |p| p.fill(0));
            }
            return;
        }
        let doomed = inner.pages.split_off(&pages);
        drop(inner);
        for pfn in doomed.into_values() {
            self.pages_ops.free(pfn);
        }
    }

    /// Pages this object holds itself.
    #[must_use]
    pub fn resident(&self) -> usize {
        self.inner.lock().pages.len()
    }

    /// The object this one shadows, and the page offset into it.
    #[must_use]
    pub fn backing(&self) -> Option<(Arc<Self>, PageIndex)> {
        self.inner.lock().backing.clone()
    }

    /// How many objects deep the chain below this one is.
    #[must_use]
    pub fn depth(&self) -> usize {
        let mut depth = 0;
        let mut next = self.backing();
        while let Some((o, _)) = next {
            depth += 1;
            next = o.backing();
        }
        depth
    }

    /// Folds the backing object into this one while nothing else reaches
    /// it: its pages that this object does not shadow move up, the rest
    /// are freed, and this object takes over its backing.
    ///
    /// `this` must be the only path to the backing object besides
    /// references that are about to be dropped; the check is the backing
    /// object's reference count, which only this object holds.
    pub fn collapse(&self) {
        loop {
            let mut inner = self.inner.lock();
            let Some((backing, offset)) = inner.backing.take() else {
                return;
            };
            // Ours is the only reference: no other map or shadow can reach
            // it, or take a new reference, since all of those go through
            // an object that holds one.
            // A pager object's absent pages are its pager's, not zeros: it
            // never folds into a shadow.
            if Arc::strong_count(&backing) != 1 || backing.pager.is_some() {
                inner.backing = Some((backing, offset));
                return;
            }
            let mut below = backing.inner.lock();
            let mut freed = alloc::vec::Vec::new();
            for (index, pfn) in core::mem::take(&mut below.pages) {
                // Only the window this object sees moves up; the rest of
                // the backing object is unreachable.
                let moved = index
                    .checked_sub(offset)
                    .filter(|i| !inner.pages.contains_key(i));
                match moved {
                    Some(i) => {
                        inner.pages.insert(i, pfn);
                    }
                    None => freed.push(pfn),
                }
            }
            inner.backing = below.backing.take().map(|(o, off)| (o, off + offset));
            drop(below);
            drop(inner);
            for pfn in freed {
                self.pages_ops.free(pfn);
            }
            // `backing` is empty now and drops here.
        }
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        DIRTY.fetch_sub(inner.dirty.len(), Ordering::Relaxed);
        for pfn in core::mem::take(&mut inner.pages).into_values() {
            self.pages_ops.free(pfn);
        }
        // Long chains would recurse through `Drop`; unlink them one at a
        // time instead, stopping at an object someone else still holds.
        let mut next = inner.backing.take();
        while let Some((object, _)) = next {
            match Arc::try_unwrap(object) {
                Ok(mut o) => next = o.inner.get_mut().backing.take(),
                Err(_) => break,
            }
        }
    }
}

impl core::fmt::Debug for Object {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("Object")
            .field("pages", &inner.pages.len())
            .field("backing", &inner.backing.as_ref().map(|(_, off)| off))
            .finish()
    }
}
