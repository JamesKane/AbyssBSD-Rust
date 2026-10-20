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

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

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
    /// Whether pages written through mappings or the page cache go back
    /// to the pager (then the object tracks which are dirty).
    fn writes_back(&self) -> bool {
        false
    }
    /// Writes page `index` back. May sleep.
    fn write_back(&self, _index: PageIndex, _page: &[u8]) -> Result<(), PagerError> {
        Ok(())
    }
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

/// A memory object.
pub struct Object {
    inner: SpinLock<Inner>,
    pages_ops: &'static dyn PageOps,
    /// Only one mapping (possibly clipped into several entries of one map)
    /// reaches this object, so unmapping a range may free its pages
    /// (`OBJ_ONEMAPPING`). Cleared for good once shared.
    one_mapping: AtomicBool,
    pager: Option<Arc<dyn Pager>>,
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
        })
    }

    /// An object whose pages `pager` supplies. It counts as shared from
    /// the start: unmapping never frees its pages, which stay cached.
    #[must_use]
    pub fn with_pager(ops: &'static dyn PageOps, pager: Arc<dyn Pager>) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(Inner {
                pages: BTreeMap::new(),
                dirty: BTreeSet::new(),
                mapped_writable: BTreeSet::new(),
                backing: None,
            }),
            pages_ops: ops,
            one_mapping: AtomicBool::new(false),
            pager: Some(pager),
        })
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
        let mut inner = self.inner.lock();
        if let Some(&pfn) = inner.pages.get(&index) {
            drop(inner);
            self.pages_ops.free(new);
            return Ok(Some(pfn));
        }
        inner.pages.insert(index, new);
        Ok(Some(new))
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
        if self.tracks_dirty() {
            self.inner.lock().dirty.insert(index);
        }
    }

    /// Records that a shared mapping maps page `index` writable (it was
    /// just marked dirty): it stays dirty for good.
    pub fn mark_mapped_writable(&self, index: PageIndex) {
        if self.tracks_dirty() {
            let mut inner = self.inner.lock();
            inner.dirty.insert(index);
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
        let todo: Vec<(PageIndex, Pfn)> = {
            let mut inner = self.inner.lock();
            let indexes: Vec<PageIndex> = inner.dirty.range(start..end).copied().collect();
            let mut out = Vec::with_capacity(indexes.len());
            for i in indexes {
                if !inner.mapped_writable.contains(&i) {
                    inner.dirty.remove(&i);
                }
                if let Some(&pfn) = inner.pages.get(&i) {
                    out.push((i, pfn));
                }
            }
            out
        };
        let mut result = Ok(());
        for (i, pfn) in todo {
            let mut r = Ok(());
            self.pages_ops
                .access(pfn, &mut |page| r = pager.write_back(i, page));
            if r.is_err() {
                self.inner.lock().dirty.insert(i);
                result = r;
            }
        }
        result
    }

    /// Cuts the object to `pages` pages: those past it go, if nothing but
    /// the caller holds the object (no mapping can still reach them, since
    /// every map entry and shadow holds a reference). Otherwise they stay
    /// until the object goes: a mapping keeps seeing what it saw, as no
    /// page table is searched for them here.
    pub fn truncate(self: &Arc<Self>, pages: PageIndex) {
        let mut inner = self.inner.lock();
        let _ = inner.dirty.split_off(&pages);
        let _ = inner.mapped_writable.split_off(&pages);
        if Arc::strong_count(self) > 1 {
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
