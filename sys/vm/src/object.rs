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
//! Each object has its own spin lock, held for page lookups and inserts.
//! Locks are taken top-down along a chain (shadow before backing), never
//! the other way.
//!
//! Behavior reference: sys/vm/vm_object.c (shadow chains and collapse).

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
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
}

/// Where a pager object's pages come from.
pub trait Pager: Send + Sync {
    /// The object's size in pages; a page past it does not exist.
    fn size(&self) -> PageIndex;
    /// Writes page `index`'s contents into `page` (a whole page, zero
    /// past the end of the data).
    fn fill(&self, index: PageIndex, page: &mut [u8]);
}

/// Why a page lookup found nothing to map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupError {
    /// No memory to fill a pager's page.
    OutOfMemory,
    /// Past the end of a pager object (`SIGBUS`).
    PastEnd,
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
    /// The object this one shadows, and where this one's index 0 falls in
    /// it.
    backing: Option<(Arc<Object>, PageIndex)>,
}

/// Where a page lookup found the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// In the object asked (the top of the chain): it may be written.
    Top(Pfn),
    /// In an object it shadows: it may only be read until copied up.
    Backing(Pfn),
    /// Nowhere in the chain: the page reads as zeros.
    Absent,
}

impl Object {
    /// An empty anonymous object.
    #[must_use]
    pub fn anonymous(ops: &'static dyn PageOps) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(Inner {
                pages: BTreeMap::new(),
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
    /// that should have it.
    pub fn lookup(&self, index: PageIndex) -> Result<Found, LookupError> {
        if let Some(pfn) = self.page_or_fill(index)? {
            return Ok(Found::Top(pfn));
        }
        let mut next = self.backing();
        let mut index = index;
        while let Some((object, off)) = next {
            index += off;
            if let Some(pfn) = object.page_or_fill(index)? {
                return Ok(Found::Backing(pfn));
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

    /// This object's page `index`: present, or filled now if this is a
    /// pager object.
    fn page_or_fill(&self, index: PageIndex) -> Result<Option<Pfn>, LookupError> {
        if let Some(&pfn) = self.inner.lock().pages.get(&index) {
            return Ok(Some(pfn));
        }
        let Some(pager) = &self.pager else {
            return Ok(None);
        };
        if index >= pager.size() {
            return Err(LookupError::PastEnd);
        }
        // Fill outside the lock; a racing fill of the same page loses.
        let new = self
            .pages_ops
            .alloc_filled(&mut |page| pager.fill(index, page))
            .ok_or(LookupError::OutOfMemory)?;
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
