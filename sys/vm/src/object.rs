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
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use sync::PreemptLock;

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
    /// Where page `pfn`'s bytes lie for as long as it is allocated (a
    /// direct map), so a device can take them without a copy; `None` if
    /// pages have no such fixed place.
    fn page_ptr(&self, _pfn: Pfn) -> Option<core::ptr::NonNull<u8>> {
        None
    }
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
    /// Starts filling `pages` (fresh, whole pages from `index` on) where
    /// they lie, without waiting: `None` if this pager cannot, or not now.
    /// May sleep.
    fn start_run(
        &self,
        _index: PageIndex,
        _pages: &Arc<FreshPages>,
    ) -> Option<Box<dyn PendingFill>> {
        None
    }
    /// Starts writing back the pages from `index` on, whose contents `buf`
    /// holds (whole pages: the pages themselves, lent, or a copy), without
    /// waiting (FreeBSD's `bawrite`). `Err` gives `buf` back when this
    /// pager cannot, or not now: the caller keeps the pages dirty. A write
    /// that fails later marks its pages dirty again through `object`. May
    /// sleep.
    fn start_write_back_run(
        &self,
        _index: PageIndex,
        buf: IoBuf,
        _object: Weak<Object>,
    ) -> Result<(), IoBuf> {
        Err(buf)
    }
}

/// A fill [`Pager::start_run`] started.
pub trait PendingFill: Send {
    /// Waits for the data to be in the run's `pages`, whole (holes and
    /// what lies past the end zeroed). May sleep.
    fn wait(&mut self, pages: &FreshPages) -> Result<(), PagerError>;
    /// Runs `insert` if the data still stands for what the pager holds
    /// (nothing written or moved since the fill started), keeping it so
    /// until `insert` returns; returns whether it ran. `insert` must not
    /// allocate. May sleep.
    fn commit(&self, insert: &mut dyn FnMut()) -> bool;
}

/// A read-ahead in flight.
struct Ahead {
    index: PageIndex,
    pages: Arc<FreshPages>,
    fill: Box<dyn PendingFill>,
}

/// Read-ahead windows kept in flight beyond the one being used: two of
/// [`Pager::max_run`] (FreeBSD's `vfs.read_max`, 64 blocks of 32 KiB).
const AHEAD: usize = 2;

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

/// Every pager object, for reclaiming clean pages. Dead ones are pruned
/// as reclaiming passes and as the list grows: a `Weak` keeps a dead
/// object's allocation, and memory may never run out.
static PAGED: PreemptLock<Vec<Weak<Object>>> = PreemptLock::new(Vec::new());

/// How many objects [`PAGED`] lists, dead ones too.
#[cfg(test)]
pub(crate) fn paged_len() -> usize {
    PAGED.lock().len()
}

/// Pages dirty in all objects together.
#[must_use]
pub fn dirty_total() -> usize {
    DIRTY.load(Ordering::Relaxed)
}

/// What a reclaiming pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
    /// Pages freed.
    pub freed: usize,
    /// Pages looked at (`v_pdpages`'s count), freed or not.
    pub scanned: usize,
}

/// Frees up to `want` clean pages of pager objects nothing maps (only
/// their file system holds them) and nobody is reading or writing
/// through, oldest objects first; returns how many it freed. Never waits
/// for a lock: a caller may hold one. Allocates nothing: it runs when
/// memory has run out.
pub fn reclaim_clean(want: usize) -> usize {
    reclaim_clean_counted(want).freed
}

/// [`reclaim_clean`], also counting the pages it looked at.
pub fn reclaim_clean_counted(want: usize) -> Reclaimed {
    let mut r = Reclaimed::default();
    {
        let Some(mut list) = PAGED.try_lock() else {
            return r;
        };
        list.retain(|w| w.strong_count() > 0);
    }
    // One object at a time, the list's lock let go between them (an
    // object being registered meanwhile only shifts what comes after).
    let mut i = 0;
    while r.freed < want {
        let o = {
            let Some(list) = PAGED.try_lock() else {
                break;
            };
            let Some(w) = list.get(i) else {
                break;
            };
            w.upgrade()
        };
        i += 1;
        if let Some(o) = o {
            let d = o.drop_clean_counted(want - r.freed);
            r.freed += d.freed;
            r.scanned += d.scanned;
        }
    }
    r
}

/// How many pages [`Object::drop_clean`] takes out under the lock at a
/// time.
const DROP_BATCH: usize = 64;

/// A memory object.
pub struct Object {
    inner: PreemptLock<Inner>,
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
    /// The next windows, read ahead while the last is used, in order.
    ahead: PreemptLock<VecDeque<Ahead>>,
    /// A device's memory (`OBJT_DEVICE`): its pages are the device's,
    /// never allocated or freed.
    device: Option<Device>,
    /// Bytes of shared mappings that may be made writable
    /// (`writemappings`), which a write seal must wait out.
    writemappings: AtomicU64,
    /// Pages a mapping may reach (`object->size` of a sized anonymous
    /// object, as POSIX shared memory's); past them a fault is `SIGBUS`.
    limit: AtomicU64,
}

/// How memory is mapped (`vm_memattr_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemAttr {
    /// Normal memory, write-back cacheable.
    #[default]
    WriteBack,
    /// Uncached but write-combining (`VM_MEMATTR_WRITE_COMBINING`):
    /// frame buffers.
    WriteCombining,
    /// Device registers: uncached, no speculation or combining
    /// (`VM_MEMATTR_DEVICE`), for a BAR mapped through `/dev/pci`.
    Device,
}

/// A device object's physical pages.
#[derive(Clone, Copy, Debug)]
struct Device {
    base: Pfn,
    pages: PageIndex,
    attr: MemAttr,
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

/// An object's pages lent for a transfer without copying them (FreeBSD's
/// buffer over its pages, `vfs_busy_pages`): while it lasts the object is
/// held and counted as under I/O, so none of its pages is freed (a page of
/// an object others hold is kept, zeroed if cut off, until the object
/// goes). A page written meanwhile goes out as it is when the device reads
/// it, and is dirty again for a later write.
pub struct PageRun {
    ranges: Vec<(core::ptr::NonNull<u8>, usize)>,
    object: Arc<Object>,
}

// SAFETY: the ranges are pages of `object`, which the run holds; they are
// only read through it (and written by a device reading into them).
unsafe impl Send for PageRun {}
// SAFETY: as above; a shared run only lends addresses.
unsafe impl Sync for PageRun {}

impl PageRun {
    /// Bytes in all.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ranges.iter().map(|&(_, n)| n).sum()
    }

    /// Whether it holds no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes `from..from + len` of the run, as a run of their own.
    #[must_use]
    pub fn slice(&self, from: usize, len: usize) -> Self {
        self.object.io.fetch_add(1, Ordering::SeqCst);
        Self {
            ranges: slice_ranges(&self.ranges, from, len),
            object: Arc::clone(&self.object),
        }
    }

    /// The bytes, if they lie in one range (a run within one page).
    #[must_use]
    pub fn contiguous(&self) -> Option<&[u8]> {
        match self.ranges.as_slice() {
            // SAFETY: `n` bytes of a page the run holds (see the type),
            // never written through it.
            &[(p, n)] => Some(unsafe { core::slice::from_raw_parts(p.as_ptr(), n) }),
            _ => None,
        }
    }

    /// The bytes, copied.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        for &(p, n) in &self.ranges {
            // SAFETY: `n` bytes of a page the run holds (see the type).
            out.extend_from_slice(unsafe { core::slice::from_raw_parts(p.as_ptr(), n) });
        }
        out
    }
}

impl core::fmt::Debug for PageRun {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "PageRun({} bytes in {} ranges)",
            self.len(),
            self.ranges.len()
        )
    }
}

impl Drop for PageRun {
    fn drop(&mut self) {
        self.object.io.fetch_sub(1, Ordering::SeqCst);
    }
}

// SAFETY: each range is part of a page the run's object holds and keeps
// while the run lives.
unsafe impl crate::dma::DmaBuf for PageRun {
    fn ranges(&self) -> usize {
        self.ranges.len()
    }
    fn range(&self, i: usize) -> (*mut u8, usize) {
        let (p, n) = self.ranges[i];
        (p.as_ptr(), n)
    }
    /// Pages of an object: file data only (a payload-only zone, mapped in
    /// place under an IOMMU).
    fn payload_only(&self) -> bool {
        true
    }
}

/// The part `from..from + len` of `ranges` (each within one page).
fn slice_ranges(
    ranges: &[(core::ptr::NonNull<u8>, usize)],
    from: usize,
    len: usize,
) -> Vec<(core::ptr::NonNull<u8>, usize)> {
    let mut out = Vec::new();
    let (mut skip, mut want) = (from, len);
    for &(p, n) in ranges {
        if want == 0 {
            break;
        }
        if skip >= n {
            skip -= n;
            continue;
        }
        let take = (n - skip).min(want);
        // In the same page as `p`, `skip` bytes on.
        out.push((
            core::ptr::NonNull::new(p.as_ptr().wrapping_add(skip)).unwrap_or(p),
            take,
        ));
        want -= take;
        skip = 0;
    }
    out
}

/// Pages allocated for a fill and in no object yet: a device reads into
/// them where they lie (FreeBSD's read cluster over the object's busy
/// pages), and the object takes them after. Those not taken are freed
/// when the last reference goes (a transfer still under way holds one).
pub struct FreshPages {
    pfns: Vec<Pfn>,
    ranges: Vec<(core::ptr::NonNull<u8>, usize)>,
    ops: &'static dyn PageOps,
}

// SAFETY: the pages are allocated to this value alone until it frees or
// gives them up; their bytes are written only by the transfers it lends
// them to, and by `zero` once those are done.
unsafe impl Send for FreshPages {}
// SAFETY: as above; a shared value only lends addresses.
unsafe impl Sync for FreshPages {}

impl FreshPages {
    /// `n` pages from `ops` (their contents left as they are), or `None`
    /// if memory runs out or pages have no fixed place to lend.
    fn alloc(ops: &'static dyn PageOps, n: usize) -> Option<Self> {
        let mut out = Self {
            pfns: Vec::with_capacity(n),
            ranges: Vec::with_capacity(n),
            ops,
        };
        for _ in 0..n {
            let pfn = ops.alloc_filled(&mut |_| {})?;
            out.pfns.push(pfn);
            out.ranges.push((ops.page_ptr(pfn)?, PAGE_BYTES));
        }
        Some(out)
    }

    /// Bytes in all.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ranges.len() * PAGE_BYTES
    }

    /// Whether it holds no pages.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Bytes `from..from + len`, lent for a transfer.
    #[must_use]
    pub fn slice(self: &Arc<Self>, from: usize, len: usize) -> IoBuf {
        IoBuf::Fresh(FreshSlice {
            ranges: slice_ranges(&self.ranges, from, len),
            _pages: Arc::clone(self),
        })
    }

    /// Zeroes bytes `from..from + len`. No transfer into them may be under
    /// way.
    pub fn zero(&self, from: usize, len: usize) {
        for (p, n) in slice_ranges(&self.ranges, from, len) {
            // SAFETY: `n` bytes of a page this value holds (see the type),
            // which no transfer writes now.
            unsafe { core::ptr::write_bytes(p.as_ptr(), 0, n) };
        }
    }

    /// Gives the pages up, in order.
    fn take(&mut self) -> Vec<Pfn> {
        self.ranges.clear();
        core::mem::take(&mut self.pfns)
    }
}

impl Drop for FreshPages {
    fn drop(&mut self) {
        for pfn in self.take() {
            self.ops.free(pfn);
        }
    }
}

impl core::fmt::Debug for FreshPages {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FreshPages({})", self.pfns.len())
    }
}

/// Part of [`FreshPages`], lent for a transfer.
pub struct FreshSlice {
    ranges: Vec<(core::ptr::NonNull<u8>, usize)>,
    _pages: Arc<FreshPages>,
}

// SAFETY: parts of pages the slice's `FreshPages` holds (see there).
unsafe impl Send for FreshSlice {}
// SAFETY: as above.
unsafe impl Sync for FreshSlice {}

impl FreshSlice {
    fn len(&self) -> usize {
        self.ranges.iter().map(|&(_, n)| n).sum()
    }

    fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        for &(p, n) in &self.ranges {
            // SAFETY: `n` bytes of a page the slice holds (see the type).
            out.extend_from_slice(unsafe { core::slice::from_raw_parts(p.as_ptr(), n) });
        }
        out
    }
}

impl core::fmt::Debug for FreshSlice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FreshSlice({} bytes)", self.len())
    }
}

/// Bytes for a transfer: owned, or an object's pages lent ([`PageRun`]).
#[derive(Debug)]
pub enum IoBuf {
    Owned(Vec<u8>),
    Lent(PageRun),
    /// Pages for a fill, not yet in an object.
    Fresh(FreshSlice),
}

impl Default for IoBuf {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

impl From<Vec<u8>> for IoBuf {
    fn from(v: Vec<u8>) -> Self {
        Self::Owned(v)
    }
}

impl IoBuf {
    /// Bytes in all.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Owned(v) => v.len(),
            Self::Lent(r) => r.len(),
            Self::Fresh(f) => f.len(),
        }
    }

    /// Whether it holds no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes `from..from + len`, as a buffer of their own (a lent run's
    /// pages stay lent; owned bytes are copied).
    #[must_use]
    pub fn slice(&self, from: usize, len: usize) -> Self {
        match self {
            Self::Owned(v) => Self::Owned(v[from..from + len].to_vec()),
            Self::Lent(r) => Self::Lent(r.slice(from, len)),
            Self::Fresh(f) => Self::Fresh(FreshSlice {
                ranges: slice_ranges(&f.ranges, from, len),
                _pages: Arc::clone(&f._pages),
            }),
        }
    }

    /// The bytes, owned (a lent run's copied).
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Owned(v) => v,
            Self::Lent(r) => r.to_vec(),
            Self::Fresh(f) => f.to_vec(),
        }
    }
}

// SAFETY: an owned vector's bytes, a lent run's (see `PageRun`), or fresh
// pages' (see `FreshPages`).
unsafe impl crate::dma::DmaBuf for IoBuf {
    fn ranges(&self) -> usize {
        match self {
            Self::Owned(v) => v.ranges(),
            Self::Lent(r) => r.ranges(),
            Self::Fresh(f) => f.ranges.len(),
        }
    }
    fn range(&self, i: usize) -> (*mut u8, usize) {
        match self {
            Self::Owned(v) => v.range(i),
            Self::Lent(r) => r.range(i),
            Self::Fresh(f) => {
                let (p, n) = f.ranges[i];
                (p.as_ptr(), n)
            }
        }
    }
    /// A lent run's or fresh pages are object pages (payload only); owned
    /// bytes are the general heap's.
    fn payload_only(&self) -> bool {
        !matches!(self, Self::Owned(_))
    }
}

/// A page an object holds.
#[derive(Clone, Copy, Debug)]
struct Page {
    pfn: Pfn,
    /// The instruction cache holds nothing older than the page's contents
    /// (synced for an executable mapping since it last changed): another
    /// executable mapping needs no sync. Only for pages of objects whose
    /// written pages go back to a pager (files), and only while the page
    /// is clean and the object has no writable shared mapping
    /// (ARCHITECTURE.md §5.2). A page that leaves takes it along.
    synced: bool,
}

impl Page {
    const fn new(pfn: Pfn) -> Self {
        Self { pfn, synced: false }
    }
}

struct Inner {
    pages: BTreeMap<PageIndex, Page>,
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

/// What [`Object::page_filled`] gave.
#[derive(Clone, Copy, Debug)]
pub enum Filled {
    /// A page the object held: the caller writes into it.
    Held(Pfn),
    /// A new page, written by the caller's fill.
    New(Pfn),
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
            inner: PreemptLock::new(Inner {
                pages: BTreeMap::new(),
                dirty: BTreeSet::new(),
                mapped_writable: BTreeSet::new(),
                backing: None,
            }),
            pages_ops: ops,
            one_mapping: AtomicBool::new(true),
            writemappings: AtomicU64::new(0),
            limit: AtomicU64::new(u64::MAX),
            pager: None,
            owned: AtomicBool::new(false),
            io: AtomicUsize::new(0),
            ra_next: AtomicU64::new(0),
            ra_window: AtomicU64::new(1),
            ahead: PreemptLock::new(VecDeque::new()),
            device: None,
        })
    }

    /// A device's memory, `pages` pages from `base`, mapped as `attr`
    /// (the device pager's object, `dev_pager_ctor`): shared from the
    /// start, its pages never allocated or freed.
    #[must_use]
    pub fn device(
        ops: &'static dyn PageOps,
        base: Pfn,
        pages: PageIndex,
        attr: MemAttr,
    ) -> Arc<Self> {
        let o = Self::anonymous(ops);
        o.set_shared();
        let mut o = Arc::into_inner(o).unwrap_or_else(|| unreachable!("a new object"));
        o.device = Some(Device { base, pages, attr });
        Arc::new(o)
    }

    /// Whether this is a device's memory.
    #[must_use]
    pub fn is_device(&self) -> bool {
        self.device.is_some()
    }

    /// How its pages are mapped.
    #[must_use]
    pub fn memattr(&self) -> MemAttr {
        self.device.map_or(MemAttr::WriteBack, |d| d.attr)
    }

    /// A device object's page `index`.
    fn device_page(d: Device, index: PageIndex) -> Result<Pfn, LookupError> {
        if index < d.pages {
            Ok(Pfn(d.base.0 + index))
        } else {
            Err(LookupError::PastEnd)
        }
    }

    /// An empty object shadowing `backing` from page `offset` on.
    #[must_use]
    pub fn shadow(backing: Arc<Self>, offset: PageIndex) -> Arc<Self> {
        let ops = backing.pages_ops;
        Arc::new(Self {
            inner: PreemptLock::new(Inner {
                pages: BTreeMap::new(),
                dirty: BTreeSet::new(),
                mapped_writable: BTreeSet::new(),
                backing: Some((backing, offset)),
            }),
            pages_ops: ops,
            one_mapping: AtomicBool::new(true),
            writemappings: AtomicU64::new(0),
            limit: AtomicU64::new(u64::MAX),
            pager: None,
            owned: AtomicBool::new(false),
            io: AtomicUsize::new(0),
            ra_next: AtomicU64::new(0),
            ra_window: AtomicU64::new(1),
            ahead: PreemptLock::new(VecDeque::new()),
            device: None,
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
            inner: PreemptLock::new(Inner {
                pages: BTreeMap::new(),
                dirty: BTreeSet::new(),
                mapped_writable: BTreeSet::new(),
                backing: None,
            }),
            pages_ops: ops,
            one_mapping: AtomicBool::new(false),
            writemappings: AtomicU64::new(0),
            limit: AtomicU64::new(u64::MAX),
            pager: Some(pager),
            owned: AtomicBool::new(true),
            io: AtomicUsize::new(0),
            ra_next: AtomicU64::new(0),
            ra_window: AtomicU64::new(1),
            ahead: PreemptLock::new(VecDeque::new()),
            device: None,
        });
        let mut list = PAGED.lock();
        if list.len() >= 64 && list.len().is_power_of_two() {
            list.retain(|w| w.strong_count() > 0);
        }
        list.push(Arc::downgrade(&o));
        drop(list);
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
        self.drop_clean_counted(want).freed
    }

    /// [`Object::drop_clean`], also counting the pages it looked at.
    pub(crate) fn drop_clean_counted(self: &Arc<Self>, want: usize) -> Reclaimed {
        let mut r = Reclaimed::default();
        // Where the next batch looks from: each page is looked at once,
        // however many batches it takes.
        let mut from: PageIndex = 0;
        // In batches, without allocating, each freed with the lock let go.
        while r.freed < want {
            let mut batch = [(0, Pfn(0)); DROP_BATCH];
            let mut k = 0;
            let mut more = false;
            {
                let Some(mut inner) = self.inner.try_lock() else {
                    break;
                };
                // Ours and the file system's: no map entry or shadow
                // reaches it, and a new one finds the pages gone when it
                // looks (under this lock). A transfer takes its guard
                // before it looks.
                let expected = 1 + usize::from(self.owned.load(Ordering::SeqCst));
                if Arc::strong_count(self) > expected || self.io.load(Ordering::SeqCst) != 0 {
                    break;
                }
                let Inner { pages, dirty, .. } = &mut *inner;
                let room = DROP_BATCH.min(want - r.freed);
                for (&i, &Page { pfn, .. }) in pages.range(from..) {
                    r.scanned += 1;
                    if !dirty.contains(&i) {
                        batch[k] = (i, pfn);
                        k += 1;
                    }
                    match i.checked_add(1) {
                        Some(next) if k < room => from = next,
                        Some(next) => {
                            from = next;
                            more = true;
                            break;
                        }
                        None => break,
                    }
                }
                for &(i, _) in &batch[..k] {
                    pages.remove(&i);
                }
            }
            for &(_, pfn) in &batch[..k] {
                self.pages_ops.free(pfn);
            }
            r.freed += k;
            if !more {
                break;
            }
        }
        r
    }

    /// Bytes of shared mappings that are or may become writable.
    #[must_use]
    pub fn writemappings(&self) -> u64 {
        self.writemappings.load(Ordering::Acquire)
    }

    /// Pages a mapping may reach.
    #[must_use]
    pub fn limit(&self) -> PageIndex {
        self.limit.load(Ordering::Acquire)
    }

    /// Limits mappings to the first `pages` pages.
    pub fn set_limit(&self, pages: PageIndex) {
        self.limit.store(pages, Ordering::Release);
    }

    pub(crate) fn add_writemappings(&self, bytes: u64) {
        // The first writable shared mapping may change any page: none is
        // synced for execution any more.
        if self.writemappings.fetch_add(bytes, Ordering::AcqRel) == 0 && bytes != 0 {
            for p in self.inner.lock().pages.values_mut() {
                p.synced = false;
            }
        }
    }

    pub(crate) fn sub_writemappings(&self, bytes: u64) {
        self.writemappings.fetch_sub(bytes, Ordering::AcqRel);
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
        if let Some(d) = self.device {
            return Self::device_page(d, index).map(Found::Top);
        }
        if self.pager.is_some() {
            if let Some(pfn) = self.page_or_fill(index, fill)? {
                return Ok(Found::Top(pfn));
            }
            return Ok(Found::Fill(Arc::clone(self), index));
        } else {
            let inner = self.inner.lock();
            if let Some(&Page { pfn, .. }) = inner.pages.get(&index) {
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
        if let Some(d) = self.device {
            return Self::device_page(d, index).ok();
        }
        if let Some(&Page { pfn, .. }) = self.inner.lock().pages.get(&index) {
            return Some(pfn);
        }
        let mut next = self.backing();
        let mut index = index;
        while let Some((object, off)) = next {
            index += off;
            if let Some(&Page { pfn, .. }) = object.inner.lock().pages.get(&index) {
                return Some(pfn);
            }
            next = object.backing();
        }
        None
    }

    /// This object's page `index`: present, or (with `fill`) filled now if
    /// this is a pager object.
    fn page_or_fill(&self, index: PageIndex, fill: bool) -> Result<Option<Pfn>, LookupError> {
        if let Some(&Page { pfn, .. }) = self.inner.lock().pages.get(&index) {
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
        // A cluster: one transfer into fresh pages where they lie.
        if let Some(pfn) = self.fill_fresh(pager.as_ref(), index, run)? {
            return Ok(Some(pfn));
        }
        // Or into a buffer, copied into pages (a copy costs far less than
        // the transfer saved).
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

    /// Fills the `run` pages from `index` on with one transfer into fresh
    /// pages, then puts them in: the first, or `None` if the pager cannot
    /// (or its data moved meanwhile), for the caller to fill otherwise.
    fn fill_fresh(
        &self,
        pager: &dyn Pager,
        index: PageIndex,
        run: PageIndex,
    ) -> Result<Option<Pfn>, LookupError> {
        let n = usize::try_from(run).map_err(|_| LookupError::OutOfMemory)?;
        let Some(pages) = FreshPages::alloc(self.pages_ops, n) else {
            return Ok(None);
        };
        let pages = Arc::new(pages);
        let Some(fill) = pager.start_run(index, &pages) else {
            return Ok(None);
        };
        self.finish_fill(index, pages, fill)
    }

    /// Waits for fill `fill` of `pages` from `index` on, and puts them in
    /// if the data still stands: the first page.
    fn finish_fill(
        &self,
        index: PageIndex,
        pages: Arc<FreshPages>,
        mut fill: Box<dyn PendingFill>,
    ) -> Result<Option<Pfn>, LookupError> {
        if fill.wait(&pages).is_err() {
            // Read again, the usual way, to report the error.
            return Ok(None);
        }
        // Every transfer is done and has let its part go.
        let Ok(mut pages) = Arc::try_unwrap(pages) else {
            return Ok(None);
        };
        let pfns = pages.take();
        let mut first = None;
        let committed = fill.commit(&mut || {
            for (i, &new) in (index..).zip(&pfns) {
                let pfn = self.insert_filled(i, new);
                first.get_or_insert(pfn);
            }
        });
        if !committed {
            for new in pfns {
                self.pages_ops.free(new);
            }
            return Ok(None);
        }
        Ok(first)
    }

    /// Keeps [`AHEAD`] windows of `max_run` pages read ahead past `index`
    /// (where the reader's window ends), unless they already are; never
    /// past the end or over a page held.
    fn start_ahead(&self, pager: &dyn Pager, index: PageIndex) {
        loop {
            let next = {
                let ahead = self.ahead.lock();
                if ahead.len() >= AHEAD {
                    return;
                }
                ahead.back().map_or(index, |a| {
                    a.index + PageIndex::try_from(a.pages.len() / PAGE_BYTES).unwrap_or(0)
                })
            };
            // The last window can end past the end now (a truncate since
            // it started): nothing more to read ahead.
            let size = pager.size();
            if next >= size {
                return;
            }
            let end = next.saturating_add(pager.max_run()).min(size);
            let end = self
                .inner
                .lock()
                .pages
                .range(next..end)
                .next()
                .map_or(end, |(&i, _)| i);
            if end <= next {
                return;
            }
            let Some(pages) = usize::try_from(end - next)
                .ok()
                .and_then(|n| FreshPages::alloc(self.pages_ops, n))
            else {
                return;
            };
            let pages = Arc::new(pages);
            let Some(fill) = pager.start_run(next, &pages) else {
                return;
            };
            let mut ahead = self.ahead.lock();
            if ahead.back().map_or(index, |a| {
                a.index + PageIndex::try_from(a.pages.len() / PAGE_BYTES).unwrap_or(0)
            }) != next
            {
                // Another reader got there first; this one is dropped.
                return;
            }
            ahead.push_back(Ahead {
                index: next,
                pages,
                fill,
            });
        }
    }

    /// Page `index` from the read-ahead in flight there, if one is: the
    /// windows past it are kept going, then this one waited for and put
    /// in. `Ok(None)` when there is none, or its data no longer stands.
    fn take_ahead(&self, pager: &dyn Pager, index: PageIndex) -> Result<Option<Pfn>, LookupError> {
        let taken = {
            let mut ahead = self.ahead.lock();
            let Some(at) = ahead.iter().position(|a| a.index == index) else {
                return Ok(None);
            };
            // Windows before it were skipped: dropped (unused).
            let skipped: Vec<Ahead> = ahead.drain(..at).collect();
            let taken = ahead.pop_front();
            drop(ahead);
            drop(skipped);
            taken
        };
        let Some(Ahead { pages, fill, .. }) = taken else {
            return Ok(None);
        };
        let n = PageIndex::try_from(pages.len() / PAGE_BYTES).unwrap_or(0);
        self.start_ahead(pager, index + n);
        self.ra_window.store(n, Ordering::Relaxed);
        self.ra_next.store(index + n, Ordering::Relaxed);
        self.finish_fill(index, pages, fill)
    }

    /// Inserts a page just filled at `index`, unless a racing fill got
    /// there first (then that page wins and this one is freed).
    fn insert_filled(&self, index: PageIndex, new: Pfn) -> Pfn {
        let mut inner = self.inner.lock();
        if let Some(&Page { pfn, .. }) = inner.pages.get(&index) {
            drop(inner);
            self.pages_ops.free(new);
            return pfn;
        }
        inner.pages.insert(index, Page::new(new));
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
        // The size is read again here: never below `index + 1`, should a
        // truncate have moved it in since the caller checked.
        let end = index
            .saturating_add(window)
            .min(pager.size())
            .max(index + 1);
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
        if let Some(&Page { pfn, .. }) = self.inner.lock().pages.get(&index) {
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
        if let Some(&Page { pfn, .. }) = inner.pages.get(&index) {
            drop(inner);
            self.pages_ops.free(new);
            return Some(pfn);
        }
        inner.pages.insert(index, Page::new(new));
        Some(new)
    }

    /// Page `index` for a write that covers it whole: one held already
    /// ([`Filled::Held`]; the caller writes into it), or a new one `fill`
    /// writes all of, never zeroed first ([`Filled::New`]). A page put in
    /// meanwhile by someone else gets what `fill` wrote.
    pub fn page_filled(
        &self,
        index: PageIndex,
        fill: &mut dyn FnMut(&mut [u8]),
    ) -> Result<Filled, LookupError> {
        if let Some(pfn) = self.page_here(index) {
            self.forget_synced(index);
            return Ok(Filled::Held(pfn));
        }
        let new = self
            .pages_ops
            .alloc_filled(fill)
            .ok_or(LookupError::OutOfMemory)?;
        let mut inner = self.inner.lock();
        if let Some(&Page { pfn, .. }) = inner.pages.get(&index) {
            drop(inner);
            // Lost a race (a fill or a fault): the bytes go to the winner.
            let mut bytes = Vec::new();
            self.pages_ops
                .access(new, &mut |p| bytes.extend_from_slice(p));
            self.pages_ops
                .access(pfn, &mut |p| p.copy_from_slice(&bytes));
            self.pages_ops.free(new);
            return Ok(Filled::New(pfn));
        }
        inner.pages.insert(index, Page::new(new));
        Ok(Filled::New(new))
    }

    /// Frees the pages in `[start, end)`, which no page table maps any
    /// more (an unmapped range of an object nothing else shares).
    pub fn free_range(&self, start: PageIndex, end: PageIndex) {
        // Pages lent to a transfer stay; they go with the object.
        if self.io.load(Ordering::SeqCst) != 0 {
            return;
        }
        let mut inner = self.inner.lock();
        let tail = inner.pages.split_off(&start);
        let mut keep = tail;
        let mut doomed = keep.split_off(&end);
        core::mem::swap(&mut doomed, &mut keep);
        inner.pages.append(&mut keep);
        drop(inner);
        for page in doomed.into_values() {
            self.pages_ops.free(page.pfn);
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
        self.inner.lock().pages.get(&index).map(|p| p.pfn)
    }

    /// Page `index` of this object to write into: held, read from the
    /// pager, or (for an anonymous object, or past the pager's data) a new
    /// zeroed page.
    pub fn page_for_write(&self, index: PageIndex) -> Result<Pfn, LookupError> {
        self.forget_synced(index);
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
            self.forget_synced(index);
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
        let mut inner = self.inner.lock();
        if let Some(p) = inner.pages.get_mut(&index) {
            p.synced = false;
        }
        if self.tracks_dirty() && inner.dirty.insert(index) {
            DIRTY.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Page `index` is about to be written in place (by the caller, not a
    /// mapping): no longer synced for execution.
    pub fn forget_synced(&self, index: PageIndex) {
        if let Some(p) = self.inner.lock().pages.get_mut(&index) {
            p.synced = false;
        }
    }

    /// The object along this one's chain that holds page `index`, and the
    /// page's index there, if that object can keep pages synced for
    /// execution: one whose written pages go back to a pager (a file).
    #[must_use]
    pub fn code_page(self: &Arc<Self>, index: PageIndex) -> Option<(Arc<Self>, PageIndex)> {
        let mut object = Arc::clone(self);
        let mut index = index;
        loop {
            if object.inner.lock().pages.contains_key(&index) {
                return object.tracks_dirty().then_some((object, index));
            }
            let (next, off) = object.backing()?;
            index = index.checked_add(off)?;
            object = next;
        }
    }

    /// Whether page `index`, still `pfn`, needs no instruction-cache sync
    /// for an executable mapping (synced since it last changed, and no
    /// writable shared mapping can change it).
    #[must_use]
    pub fn icache_synced(&self, index: PageIndex, pfn: Pfn) -> bool {
        let inner = self.inner.lock();
        self.writemappings.load(Ordering::Acquire) == 0
            && inner
                .pages
                .get(&index)
                .is_some_and(|p| p.synced && p.pfn == pfn)
    }

    /// Records that page `index`, still `pfn`, was synced for execution:
    /// kept only while it is clean and no writable shared mapping exists.
    pub fn set_icache_synced(&self, index: PageIndex, pfn: Pfn) {
        let mut inner = self.inner.lock();
        if self.writemappings.load(Ordering::Acquire) != 0 || inner.dirty.contains(&index) {
            return;
        }
        if let Some(p) = inner.pages.get_mut(&index)
            && p.pfn == pfn
        {
            p.synced = true;
        }
    }

    /// Records that a shared mapping maps page `index` writable (it was
    /// just marked dirty): it stays dirty for good.
    pub fn mark_mapped_writable(&self, index: PageIndex) {
        if self.tracks_dirty() {
            let mut inner = self.inner.lock();
            if let Some(p) = inner.pages.get_mut(&index) {
                p.synced = false;
            }
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
                if let Some(&Page { pfn, .. }) = inner.pages.get(&i) {
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

    /// Starts writing back the dirty pages in `[start, end)` without
    /// waiting (FreeBSD's `bawrite` of a filled block): each run of them
    /// is clean from now, lent to the write as it is (or copied where
    /// pages have no fixed place); a run the pager will not start now
    /// stays dirty, for a later [`Self::sync`].
    pub fn start_sync(self: &Arc<Self>, start: PageIndex, end: PageIndex) {
        let Some(pager) = &self.pager else {
            return;
        };
        let todo: Vec<(PageIndex, Pfn)> = {
            let mut inner = self.inner.lock();
            let indexes: Vec<PageIndex> = inner.dirty.range(start..end).copied().collect();
            let mut out = Vec::with_capacity(indexes.len());
            for i in indexes {
                if !inner.mapped_writable.contains(&i) {
                    inner.dirty.remove(&i);
                    DIRTY.fetch_sub(1, Ordering::Relaxed);
                }
                if let Some(&Page { pfn, .. }) = inner.pages.get(&i) {
                    out.push((i, pfn));
                }
            }
            out
        };
        let mut rest = todo.as_slice();
        while let Some(&(first, _)) = rest.first() {
            let len = rest
                .iter()
                .zip(first..)
                .take_while(|&(&(i, _), want)| i == want)
                .count();
            let (run, after) = rest.split_at(len);
            rest = after;
            let buf = self.lend(run).unwrap_or_else(|| {
                let mut buf = Vec::with_capacity(run.len() * PAGE_BYTES);
                for &(_, pfn) in run {
                    self.pages_ops
                        .access(pfn, &mut |page| buf.extend_from_slice(page));
                }
                IoBuf::Owned(buf)
            });
            if pager
                .start_write_back_run(first, buf, Arc::downgrade(self))
                .is_err()
            {
                self.redirty(first, PageIndex::try_from(len).map_or(end, |n| first + n));
            }
        }
    }

    /// Pages `run` of this object lent for a transfer, if pages have fixed
    /// places (see [`PageRun`]).
    fn lend(self: &Arc<Self>, run: &[(PageIndex, Pfn)]) -> Option<IoBuf> {
        let ranges = run
            .iter()
            .map(|&(_, pfn)| self.pages_ops.page_ptr(pfn).map(|p| (p, PAGE_BYTES)))
            .collect::<Option<Vec<_>>>()?;
        self.io.fetch_add(1, Ordering::SeqCst);
        Some(IoBuf::Lent(PageRun {
            ranges,
            object: Arc::clone(self),
        }))
    }

    /// Page `index` lent for a transfer without a copy (`sendfile`'s
    /// wired page, `vm_page_grab` with `VM_ALLOC_WIRED`): read from the
    /// pager if need be (may sleep: call with no map lock held), or a hole
    /// with no pager behind it filled with a new zeroed page, as
    /// `sendfile_swapin` does. `Ok(None)` if pages have no fixed place.
    ///
    /// # Errors
    ///
    /// The pager's failure, or no memory for the page.
    pub fn lend_page(self: &Arc<Self>, index: PageIndex) -> Result<Option<PageRun>, LookupError> {
        let _io = self.io();
        let pfn = match self.page_or_fill(index, true) {
            Ok(Some(pfn)) => pfn,
            Ok(None) | Err(LookupError::PastEnd) => self
                .get_or_insert(index, None)
                .ok_or(LookupError::OutOfMemory)?,
            Err(e) => return Err(e),
        };
        let Some(p) = self.pages_ops.page_ptr(pfn) else {
            return Ok(None);
        };
        self.io.fetch_add(1, Ordering::SeqCst);
        Ok(Some(PageRun {
            ranges: alloc::vec![(p, PAGE_BYTES)],
            object: Arc::clone(self),
        }))
    }

    /// Marks the pages held in `[start, end)` dirty again (a write of them
    /// failed, or was not started).
    pub fn redirty(&self, start: PageIndex, end: PageIndex) {
        let mut inner = self.inner.lock();
        let held: Vec<PageIndex> = inner.pages.range(start..end).map(|(&i, _)| i).collect();
        for i in held {
            if inner.dirty.insert(i) {
                DIRTY.fetch_add(1, Ordering::Relaxed);
            }
        }
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
            // Rewritten in place: no longer synced for execution.
            let kept: Vec<Pfn> = inner
                .pages
                .range_mut(pages..)
                .map(|(_, p)| {
                    p.synced = false;
                    p.pfn
                })
                .collect();
            drop(inner);
            for pfn in kept {
                self.pages_ops.access(pfn, &mut |p| p.fill(0));
            }
            return;
        }
        let doomed = inner.pages.split_off(&pages);
        drop(inner);
        for page in doomed.into_values() {
            self.pages_ops.free(page.pfn);
        }
    }

    /// Gives up pages `[start, end)` (`vm_object_page_remove`): freed if
    /// nothing but its owner holds the object, else zeroed in place, as
    /// [`truncate`](Self::truncate) does, for the mappings that may still
    /// reach them.
    pub fn deallocate(self: &Arc<Self>, start: PageIndex, end: PageIndex) {
        if Arc::strong_count(self) == 1 {
            self.free_range(start, end);
            return;
        }
        let kept: Vec<Pfn> = self
            .inner
            .lock()
            .pages
            .range_mut(start..end)
            .map(|(_, p)| {
                // Rewritten in place: no longer synced for execution.
                p.synced = false;
                p.pfn
            })
            .collect();
        for pfn in kept {
            self.pages_ops.access(pfn, &mut |p| p.fill(0));
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
            for (index, page) in core::mem::take(&mut below.pages) {
                // Only the window this object sees moves up; the rest of
                // the backing object is unreachable.
                let moved = index
                    .checked_sub(offset)
                    .filter(|i| !inner.pages.contains_key(i));
                match moved {
                    Some(i) => {
                        inner.pages.insert(i, page);
                    }
                    None => freed.push(page.pfn),
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
        for page in core::mem::take(&mut inner.pages).into_values() {
            self.pages_ops.free(page.pfn);
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
