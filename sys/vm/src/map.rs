// SPDX-License-Identifier: BSD-2-Clause
//! Address-space maps: which ranges of a process's address space are
//! mapped, to what, with which rights (`vm_map`).
//!
//! A [`VmMap`] is an ordered set of non-overlapping [`Entry`]s, each a
//! page-aligned range backed by an [`Object`] at a page offset. Anonymous
//! entries get their object at the first fault. Operations that cover
//! part of an entry first *clip* it in two at the boundary, so every
//! operation works on whole entries.
//!
//! The map holds no lock and touches no page tables: the kernel wraps it
//! in its address-space lock and applies to the page tables what each
//! operation reports (ranges to unmap, to write-protect, to downgrade).
//!
//! Behavior reference: sys/vm/vm_map.c (entries, clipping, `fork`
//! inheritance, `vm_map_find` first fit).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use crate::object::{Object, PageIndex, PageOps};
use crate::{PAGE_SIZE, VirtAddr};
use entries::Entries;

/// Access rights, with FreeBSD's `PROT_*` values.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Prot(u8);

impl Prot {
    pub const NONE: Self = Self(0);
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);
    pub const EXEC: Self = Self(4);
    pub const RW: Self = Self(3);
    pub const ALL: Self = Self(7);

    /// The rights in `PROT_*` bits, if they are only those.
    #[must_use]
    pub fn from_bits(bits: u32) -> Option<Self> {
        u8::try_from(bits).ok().filter(|b| b & !7 == 0).map(Self)
    }

    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0 as u32
    }

    /// Whether every right in `other` is in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl fmt::Debug for Prot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = |bit: Self, ch| if self.contains(bit) { ch } else { '-' };
        write!(
            f,
            "{}{}{}",
            c(Self::READ, 'r'),
            c(Self::WRITE, 'w'),
            c(Self::EXEC, 'x')
        )
    }
}

/// What a child gets of an entry at `fork` (`minherit(2)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inherit {
    /// The same memory, shared (`MAP_SHARED`).
    Share,
    /// A copy-on-write copy (private mappings, the default).
    Copy,
    /// Nothing: the range is unmapped in the child.
    None,
    /// Fresh zero-filled memory.
    Zero,
}

/// One mapped range.
#[derive(Clone)]
pub struct Entry {
    start: VirtAddr,
    end: VirtAddr,
    pub(crate) prot: Prot,
    max_prot: Prot,
    inherit: Inherit,
    /// The backing object; `None` for anonymous memory not yet touched.
    pub(crate) object: Option<Arc<Object>>,
    /// The object page `start` maps.
    pub(crate) offset: PageIndex,
    /// The object is shared copy-on-write: the first write gives this
    /// entry a shadow of its own.
    pub(crate) needs_copy: bool,
}

impl Entry {
    /// Anonymous private memory over `range`.
    #[must_use]
    pub fn anonymous(range: Range<VirtAddr>, prot: Prot, max_prot: Prot) -> Self {
        Self {
            start: range.start,
            end: range.end,
            prot,
            max_prot,
            inherit: Inherit::Copy,
            object: None,
            offset: 0,
            needs_copy: false,
        }
    }

    /// A private mapping of `object` from page `offset` on over `range`
    /// (a file's text or data): shared until written, then copied.
    #[must_use]
    pub fn private(
        range: Range<VirtAddr>,
        prot: Prot,
        max_prot: Prot,
        object: Arc<Object>,
        offset: PageIndex,
    ) -> Self {
        Self {
            start: range.start,
            end: range.end,
            prot,
            max_prot,
            inherit: Inherit::Copy,
            object: Some(object),
            offset,
            needs_copy: true,
        }
    }

    /// `self`, inherited by a child as `inherit` says.
    #[must_use]
    pub const fn with_inherit(mut self, inherit: Inherit) -> Self {
        self.inherit = inherit;
        self
    }

    #[must_use]
    pub const fn start(&self) -> VirtAddr {
        self.start
    }

    #[must_use]
    pub const fn end(&self) -> VirtAddr {
        self.end
    }

    #[must_use]
    pub fn range(&self) -> Range<VirtAddr> {
        self.start..self.end
    }

    #[must_use]
    pub const fn prot(&self) -> Prot {
        self.prot
    }

    #[must_use]
    pub const fn max_prot(&self) -> Prot {
        self.max_prot
    }

    #[must_use]
    pub const fn inherit(&self) -> Inherit {
        self.inherit
    }

    #[must_use]
    pub fn object(&self) -> Option<&Arc<Object>> {
        self.object.as_ref()
    }

    /// The object page index `va` (inside the entry) maps.
    #[must_use]
    pub const fn index_of(&self, va: VirtAddr) -> PageIndex {
        self.offset + (va.get() - self.start.get()) / PAGE_SIZE
    }

    const fn pages(&self) -> u64 {
        (self.end.get() - self.start.get()) / PAGE_SIZE
    }

    /// Splits off and returns the part from `at` on.
    fn split_at(&mut self, at: VirtAddr) -> Self {
        let mut right = self.clone();
        right.start = at;
        right.offset = self.index_of(at);
        self.end = at;
        right
    }

    /// Releases the pages of a removed entry that nothing else can reach.
    fn release(self) {
        if let Some(object) = &self.object
            && object.one_mapping()
        {
            object.free_range(self.offset, self.offset + self.pages());
        }
    }
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:#x}-{:#x} {:?}/{:?} {:?}{}",
            self.start.get(),
            self.end.get(),
            self.prot,
            self.max_prot,
            self.inherit,
            if self.needs_copy { " cow" } else { "" }
        )
    }
}

/// Why a map operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// An address or length is not page aligned, or the range is empty.
    Misaligned,
    /// The range is outside the map's bounds.
    OutOfRange,
    /// The range overlaps an existing entry.
    Overlap,
    /// No free range is large enough.
    NoSpace,
    /// Part of the range is not mapped.
    NotMapped,
    /// The rights asked exceed an entry's maximum.
    Protection,
}

/// A process address space's map.
pub struct VmMap {
    entries: Entries,
    bounds: Range<VirtAddr>,
    pub(crate) pages: &'static dyn PageOps,
}

/// What [`VmMap::fork`] produced.
#[derive(Debug)]
pub struct Forked {
    /// The child's map.
    pub child: VmMap,
    /// Ranges of the parent that are now copy-on-write: the parent's page
    /// tables must stop allowing writes there.
    pub write_protect: Vec<Range<VirtAddr>>,
}

impl VmMap {
    /// An empty map over `bounds` (page aligned).
    #[must_use]
    pub fn new(bounds: Range<VirtAddr>, pages: &'static dyn PageOps) -> Self {
        Self {
            entries: Entries::default(),
            bounds,
            pages,
        }
    }

    #[must_use]
    pub fn bounds(&self) -> Range<VirtAddr> {
        self.bounds.clone()
    }

    /// The entries, in address order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// The entry containing `va`.
    #[must_use]
    pub fn entry(&self, va: VirtAddr) -> Option<&Entry> {
        self.entries.at_or_before(va.get()).filter(|e| va < e.end)
    }

    pub(crate) fn entry_mut(&mut self, va: VirtAddr) -> Option<&mut Entry> {
        self.entries
            .at_or_before_mut(va.get())
            .filter(|e| va < e.end)
    }

    fn check(&self, range: &Range<VirtAddr>) -> Result<(), MapError> {
        if !range.start.is_aligned(PAGE_SIZE)
            || !range.end.is_aligned(PAGE_SIZE)
            || range.start >= range.end
        {
            return Err(MapError::Misaligned);
        }
        if range.start < self.bounds.start || range.end > self.bounds.end {
            return Err(MapError::OutOfRange);
        }
        Ok(())
    }

    /// Whether any entry overlaps `range`.
    fn overlaps(&self, range: &Range<VirtAddr>) -> bool {
        self.entries
            .before(range.end.get())
            .is_some_and(|e| e.end > range.start)
    }

    /// Whether any entry overlapping `range` has an object: only then can
    /// a page of it be in the page tables (a fault makes the object
    /// before it maps anything), so `munmap` of memory never touched need
    /// not look.
    #[must_use]
    pub fn populated(&self, range: &Range<VirtAddr>) -> bool {
        self.entries
            .at_or_before(range.start.get())
            .into_iter()
            .chain(self.entries.starting_at(range.start.get()))
            .take_while(|e| e.start < range.end)
            .any(|e| e.end > range.start && e.object.is_some())
    }

    /// Whether nothing is mapped in `range`.
    #[must_use]
    pub fn is_free(&self, range: &Range<VirtAddr>) -> bool {
        !self.overlaps(range)
    }

    /// Adds `entry`, which must not overlap any other.
    pub fn insert(&mut self, entry: Entry) -> Result<(), MapError> {
        let range = entry.range();
        self.check(&range)?;
        if self.overlaps(&range) {
            return Err(MapError::Overlap);
        }
        self.entries.insert(entry);
        Ok(())
    }

    /// The lowest free range of `len` bytes aligned to `align` at or above
    /// `hint` (wrapping around to the bottom of the map), as `mmap` without
    /// `MAP_FIXED` places a mapping.
    #[must_use]
    pub fn find_space(&self, len: u64, align: u64, hint: VirtAddr) -> Option<VirtAddr> {
        let align = align.max(PAGE_SIZE);
        if len == 0 || !len.is_multiple_of(PAGE_SIZE) || !align.is_power_of_two() {
            return None;
        }
        let hint = hint.max(self.bounds.start);
        self.first_fit(len, align, hint, self.bounds.end)
            .or_else(|| self.first_fit(len, align, self.bounds.start, self.bounds.end))
    }

    fn first_fit(&self, len: u64, align: u64, from: VirtAddr, limit: VirtAddr) -> Option<VirtAddr> {
        // Start in the gap holding `from`: after the entry before it, if
        // that entry reaches past it.
        let mut cursor = from;
        if let Some(e) = self.entries.before(from.get())
            && e.end > cursor
        {
            cursor = e.end;
        }
        for e in self.entries.starting_at(from.get()) {
            let start = cursor.align_up(align)?;
            if start.get().checked_add(len)? <= e.start.get() {
                return Some(start);
            }
            cursor = cursor.max(e.end);
        }
        let start = cursor.align_up(align)?;
        (start.get().checked_add(len)? <= limit.get()).then_some(start)
    }

    /// Splits the entry containing `at`, if `at` falls inside one.
    fn clip(&mut self, at: VirtAddr) {
        if let Some(e) = self.entry_mut(at)
            && e.start < at
        {
            let right = e.split_at(at);
            self.entries.insert(right);
        }
    }

    /// Unmaps `range`, freeing the pages only it reached. Unmapped holes
    /// inside the range are fine (`munmap` semantics). The caller removes
    /// the range from the page tables first.
    pub fn remove(&mut self, range: Range<VirtAddr>) -> Result<(), MapError> {
        self.check(&range)?;
        self.clip(range.start);
        self.clip(range.end);
        self.entries
            .remove_range(range.start.get(), range.end.get(), Entry::release);
        Ok(())
    }

    /// The entries starting in `range` (after clipping at its ends, those
    /// it covers), to change in place.
    fn range_mut(&mut self, range: &Range<VirtAddr>) -> impl Iterator<Item = &mut Entry> {
        let end = range.end;
        self.entries
            .starting_at_mut(range.start.get())
            .take_while(move |e| e.start < end)
    }

    /// Whether every page of `range` is mapped.
    fn covered(&self, range: &Range<VirtAddr>) -> bool {
        let mut cursor = range.start;
        while cursor < range.end {
            match self.entry(cursor) {
                Some(e) => cursor = e.end,
                None => return false,
            }
        }
        true
    }

    /// Sets the rights of `range`, which must be mapped throughout and
    /// allow them (`mprotect`). The caller then removes from the page
    /// tables whatever rights were dropped; added rights arrive by fault.
    pub fn protect(&mut self, range: Range<VirtAddr>, prot: Prot) -> Result<(), MapError> {
        self.check(&range)?;
        if !self.covered(&range) {
            return Err(MapError::NotMapped);
        }
        if self
            .entries
            .at_or_before(range.start.get())
            .into_iter()
            .chain(self.entries.starting_at(range.start.get()))
            .take_while(|e| e.start < range.end)
            .any(|e| e.end > range.start && !e.max_prot.contains(prot))
        {
            return Err(MapError::Protection);
        }
        self.clip(range.start);
        self.clip(range.end);
        for e in self.range_mut(&range) {
            e.prot = prot;
        }
        Ok(())
    }

    /// Sets what a child inherits of the mapped parts of `range`
    /// (`minherit`; holes are skipped, as FreeBSD does).
    pub fn set_inherit(
        &mut self,
        range: Range<VirtAddr>,
        inherit: Inherit,
    ) -> Result<(), MapError> {
        self.check(&range)?;
        self.clip(range.start);
        self.clip(range.end);
        for e in self.range_mut(&range) {
            e.inherit = inherit;
        }
        Ok(())
    }

    /// The child's map for `fork`, by each entry's inheritance.
    pub fn fork(&mut self) -> Forked {
        let mut child = Self::new(self.bounds.clone(), self.pages);
        let mut write_protect = Vec::new();
        for e in self.entries.iter_mut() {
            // Fold away what only this entry still reaches: the last fork's
            // child has usually exited, and without this every fork whose
            // parent writes after it stacks one more shadow (FreeBSD
            // collapses as references go).
            if let Some(object) = &e.object
                && !e.needs_copy
            {
                object.collapse();
            }
            match e.inherit {
                Inherit::None => {}
                Inherit::Zero => {
                    let fresh = Entry {
                        object: None,
                        offset: 0,
                        needs_copy: false,
                        ..e.clone()
                    };
                    child.entries.insert(fresh);
                }
                Inherit::Share => {
                    // Both must reach the same object: make it now, and if
                    // this entry was still to copy, take that copy first,
                    // so the sharing is of memory nobody else sees.
                    let ops = self.pages;
                    let object = e.object.get_or_insert_with(|| Object::anonymous(ops));
                    if e.needs_copy {
                        *object = Object::shadow(Arc::clone(object), e.offset);
                        e.offset = 0;
                        e.needs_copy = false;
                    }
                    object.set_shared();
                    let shared = e.clone();
                    child.entries.insert(shared);
                }
                Inherit::Copy => {
                    if let Some(object) = &e.object {
                        object.set_shared();
                        e.needs_copy = true;
                        if e.prot.contains(Prot::WRITE) {
                            write_protect.push(e.range());
                        }
                    }
                    let copy = e.clone();
                    child.entries.insert(copy);
                }
            }
        }
        Forked {
            child,
            write_protect,
        }
    }
}

impl Drop for VmMap {
    fn drop(&mut self) {
        self.entries.clear(Entry::release);
    }
}

impl fmt::Debug for VmMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.entries.iter()).finish()
    }
}

mod entries;

#[cfg(test)]
mod tests;
