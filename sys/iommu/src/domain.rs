// SPDX-License-Identifier: BSD-2-Clause
//! Translation domains (docs/IOMMU.md §4.4–§4.7): one per context, as
//! FreeBSD's arm64 `iommu_get_ctx` has it. A [`Domain`] owns an IOVA space
//! (`iommu::gas`) and the I/O page tables its unit walks (`pt::smmu`
//! descriptors in frames from [`DmaMemory`]), and is what `vm::dma`'s IOMMU
//! backend maps through ([`IoDomain`]).
//!
//! - **Map**: the lowest IOVA range the tag's constraints allow, guard
//!   pages around it; the pages entered under the domain's table lock; then
//!   the unit's TLB entries for the range invalidated and a sync awaited,
//!   as FreeBSD's `smmu_map` does (whether a map needs that is stage 6's
//!   question, §8).
//! - **Unmap is strict** (DRIVER-MODEL.md §8.4 rule 3): the entries are
//!   cleared, the unit invalidates them and syncs (one sync for all of a
//!   mapping's pages), and only then is the IOVA range free for reuse. If
//!   the unit cannot confirm the invalidation, the range is never reused.
//! - **Attributes**: Normal write-back inner-shareable for a coherent
//!   device, Normal non-cacheable outer-shareable for one that is not (the
//!   board port's `2427574f670`), Device-nGnRE for the MSI doorbell; the
//!   device reads only what it reads (`ToDevice` is read-only).
//! - **Tables** are frames from the physical allocator, reached through the
//!   direct map. On a unit whose table walks do not snoop the CPU's caches,
//!   each frame a change touched is cleaned before the unit is told.
//!   Intermediate tables stay until the domain goes, so no walk-cache
//!   invalidation is ever needed.
//!
//! Locks: the space and the tables each have a spin lock of their own, and
//! the unit's command queue its own; none is held while another is taken,
//! so maps and unmaps on other CPUs only meet at the unit's queue.
//!
//! Behavior reference: sys/arm64/iommu/iommu.c, iommu_pmap.c,
//! sys/arm64/iommu/smmu.c (`smmu_map`, `smmu_unmap`), sys/dev/iommu/iommu_gas.c.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use iommu::gas::{Request, SPACE_48, Space};
use iommu::{Iova, PAGE};
use pt::smmu::Smmu;
use pt::{Cache, Frame, PageTable, PhysAddr, VirtAddr};
use sync::SpinLock;
use vm::dma::{DmaConstraints, DmaError, DmaMemory, IoDomain, IoMapping};

/// The unit could not confirm an invalidation (a sync timed out, or the
/// command queue stayed full).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unconfirmed;

/// What a domain asks of its unit (the SMMU driver implements it).
pub trait Invalidate: Send + Sync {
    /// Invalidates the unit's cached leaf translations of `asid` for
    /// `[iova, iova + len)` and waits until the unit has done so.
    ///
    /// # Errors
    ///
    /// The unit could not confirm it (a sync timed out).
    fn invalidate(&self, asid: u16, iova: u64, len: u64) -> Result<(), Unconfirmed>;
}

/// How a domain is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Its ASID on its unit.
    pub asid: u16,
    /// The first physical address its unit cannot output (`1 << OAS`).
    pub reach: u64,
    /// The unit's table walks snoop the CPU's caches.
    pub walk_coherent: bool,
    /// The device's accesses are coherent (`_CCA`, the IORT).
    pub device_coherent: bool,
}

/// Why a domain could not be made or changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DomainError {
    /// No memory for a table or a tree node.
    NoMemory,
    /// The range is malformed or overlaps what it may not.
    Invalid,
}

/// The input address boundary `pt`'s `map` will not cross in one call
/// (the CPU formats' half; the SMMU's TTB0 covers both halves).
const HALF: u64 = 1 << 47;

/// The page-table frames of one domain: allocated from `mem` below
/// `below`, reached through its direct map. Cleans what it touched when
/// the unit's walks are not coherent.
struct Frames<'a> {
    mem: &'a dyn DmaMemory,
    below: u64,
    /// Frames to clean: `Some` for a unit whose walks do not snoop.
    touched: Option<Vec<u64>>,
}

impl Frames<'_> {
    fn touch(&mut self, pa: u64) {
        if let Some(t) = &mut self.touched
            && t.last() != Some(&pa)
        {
            t.push(pa);
        }
    }

    /// Cleans every frame touched to where the unit reads it.
    fn flush(&mut self) {
        if let Some(t) = &mut self.touched {
            t.sort_unstable();
            t.dedup();
            for &pa in t.iter() {
                self.mem.clean(
                    self.mem.virt(pa).as_ptr(),
                    usize::try_from(PAGE).unwrap_or(4096),
                );
            }
            t.clear();
        }
    }
}

impl pt::Memory for Frames<'_> {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        let pa = self.mem.alloc(PAGE, PAGE, self.below)?;
        // SAFETY: a page just allocated, ours alone, mapped read-write at
        // `virt` (the `DmaMemory` contract).
        unsafe {
            self.mem
                .virt(pa)
                .as_ptr()
                .write_bytes(0, usize::try_from(PAGE).unwrap_or(4096));
        }
        self.touch(pa);
        Some(PhysAddr::new(pa))
    }

    fn frame(&mut self, pa: PhysAddr) -> &mut Frame {
        self.touch(pa.get());
        // SAFETY: `pa` is a frame of this domain's tables: the root or one
        // `alloc_frame` gave and a table entry this domain wrote points
        // to, so a page of RAM the domain owns, mapped read-write and
        // page aligned at `virt`. Its callers hold the domain's table lock
        // (or own the domain, in `Drop`), and the reference borrows `self`
        // mutably, so no other reference to it exists in the kernel. The
        // unit reads it concurrently, as the CPU's MMU reads the kernel's
        // tables: the entries it may see are each written whole.
        unsafe { &mut *self.mem.virt(pa.get()).as_ptr().cast::<Frame>() }
    }

    fn free_frame(&mut self, pa: PhysAddr) {
        // SAFETY: a frame from `alloc_frame`, which `free_tables` took out
        // of the tables; the unit no longer walks it (the domain is gone).
        unsafe { self.mem.free(pa.get(), PAGE) };
    }
}

/// A translation domain.
pub struct Domain {
    unit: Weak<dyn Invalidate>,
    mem: &'static dyn DmaMemory,
    p: Params,
    /// The level-0 table: the CD's TTB0.
    root: u64,
    /// Held while the tables change.
    tables: SpinLock<()>,
    space: SpinLock<Space>,
    /// The MSI doorbell page and its IOVA, once mapped.
    doorbell: SpinLock<Option<(u64, u64)>>,
    /// Pages never reused because their invalidation was not confirmed.
    leaked: AtomicU64,
    maps: AtomicU64,
}

impl core::fmt::Debug for Domain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Domain")
            .field("asid", &self.p.asid)
            .field("root", &format_args!("{:#x}", self.root))
            .finish_non_exhaustive()
    }
}

impl Domain {
    /// A domain with an empty 48-bit space and an empty level-0 table.
    ///
    /// # Errors
    ///
    /// [`DomainError::NoMemory`].
    pub fn new(
        mem: &'static dyn DmaMemory,
        unit: Weak<dyn Invalidate>,
        p: Params,
    ) -> Result<Arc<Self>, DomainError> {
        let space = Space::new(SPACE_48).map_err(|_| DomainError::NoMemory)?;
        let mut f = Frames {
            mem,
            below: p.reach,
            touched: (!p.walk_coherent).then(Vec::new),
        };
        let root = pt::Memory::alloc_frame(&mut f).ok_or(DomainError::NoMemory)?;
        f.flush();
        Ok(Arc::new(Self {
            unit,
            mem,
            p,
            root: root.get(),
            tables: SpinLock::new(()),
            space: SpinLock::new(space),
            doorbell: SpinLock::new(None),
            leaked: AtomicU64::new(0),
            maps: AtomicU64::new(0),
        }))
    }

    /// The level-0 table's address (the CD's TTB0).
    #[must_use]
    pub fn root(&self) -> u64 {
        self.root
    }

    /// The domain's ASID.
    #[must_use]
    pub fn asid(&self) -> u16 {
        self.p.asid
    }

    /// How it was made.
    #[must_use]
    pub fn params(&self) -> Params {
        self.p
    }

    /// Maps made so far.
    #[must_use]
    pub fn maps(&self) -> u64 {
        self.maps.load(Ordering::Relaxed)
    }

    /// IOVA pages withheld after an unconfirmed invalidation.
    #[must_use]
    pub fn leaked_pages(&self) -> u64 {
        self.leaked.load(Ordering::Relaxed)
    }

    /// Keeps `[start, end)` (rounded out to pages) from ever being
    /// allocated: a host bridge's windows at their bus addresses, where a
    /// request would reach a device rather than memory (the board port's
    /// `01b65ffd609`). Parts already reserved are skipped, as FreeBSD's
    /// `iommu_gas_reserve_region_extend`.
    ///
    /// # Errors
    ///
    /// [`DomainError::NoMemory`], or [`DomainError::Invalid`] for a range
    /// that overlaps a mapping.
    pub fn reserve(&self, start: u64, end: u64) -> Result<(), DomainError> {
        let s = start & !(PAGE - 1);
        let e = end.min(SPACE_48).next_multiple_of(PAGE);
        if s >= e {
            return Ok(());
        }
        self.space
            .lock()
            .reserve_extend(Iova::new(s), Iova::new(e))
            .map_err(|g| match g {
                iommu::gas::GasError::NoMemory => DomainError::NoMemory,
                _ => DomainError::Invalid,
            })
    }

    /// The entries of the space, in address order (diagnostics, tests).
    #[must_use]
    pub fn entries(&self) -> Vec<iommu::gas::Entry> {
        self.space.lock().iter().collect()
    }

    /// What `iova` translates to in the tables (diagnostics, tests).
    #[must_use]
    pub fn translate(&self, iova: u64) -> Option<pt::Translation> {
        let _g = self.tables.lock();
        let mut f = self.frames();
        PageTable::<Smmu>::from_root(PhysAddr::new(self.root))
            .translate(&mut f, VirtAddr::new(iova))
    }

    fn frames(&self) -> Frames<'static> {
        Frames {
            mem: self.mem,
            below: self.p.reach,
            touched: (!self.p.walk_coherent).then(Vec::new),
        }
    }

    fn cache(&self) -> Cache {
        if self.p.device_coherent {
            Cache::WriteBack
        } else {
            Cache::WriteCombining
        }
    }

    /// The unit's invalidation of `[iova, iova + len)`; done if the unit
    /// is gone (it no longer translates).
    fn invalidate(&self, iova: u64, len: u64) -> Result<(), Unconfirmed> {
        match self.unit.upgrade() {
            Some(u) => u.invalidate(self.p.asid, iova, len),
            None => Ok(()),
        }
    }

    /// Enters `ranges` at `iova`, page by page, under the table lock.
    /// On failure nothing stays entered.
    fn enter(&self, iova: u64, ranges: &[(u64, u64)], attrs: pt::Attrs) -> Result<(), DmaError> {
        let _g = self.tables.lock();
        let mut f = self.frames();
        let mut t = PageTable::<Smmu>::from_root(PhysAddr::new(self.root));
        let mut at = iova;
        let mut failed = false;
        for &(pa, len) in ranges {
            let p0 = pa & !(PAGE - 1);
            let bytes = (pa + len).next_multiple_of(PAGE) - p0;
            // One call per side of bit 47, which `pt` will not cross.
            let first = if at < HALF && at + bytes > HALF {
                HALF - at
            } else {
                bytes
            };
            let r = t
                .map(&mut f, VirtAddr::new(at), PhysAddr::new(p0), first, attrs)
                .and_then(|()| {
                    if first < bytes {
                        t.map(
                            &mut f,
                            VirtAddr::new(at + first),
                            PhysAddr::new(p0 + first),
                            bytes - first,
                            attrs,
                        )
                    } else {
                        Ok(())
                    }
                });
            if r.is_err() {
                failed = true;
                at += bytes;
                break;
            }
            at += bytes;
        }
        if failed {
            // A failed map leaves what it entered: take it all out again
            // (the device was never given these addresses).
            let mut v = iova;
            while v < at {
                if t.translate(&mut f, VirtAddr::new(v)).is_some() {
                    let _ = t.unmap(&mut f, VirtAddr::new(v), PAGE, |_| {});
                }
                v += PAGE;
            }
        }
        f.flush();
        if failed {
            Err(DmaError::NoMemory)
        } else {
            Ok(())
        }
    }

    /// Clears the entries of `[start, end)`, under the table lock.
    fn remove(&self, start: u64, end: u64) {
        let _g = self.tables.lock();
        let mut f = self.frames();
        let mut t = PageTable::<Smmu>::from_root(PhysAddr::new(self.root));
        let r = t.unmap(&mut f, VirtAddr::new(start), end - start, |_| {});
        debug_assert!(r.is_ok(), "unmapping {start:#x}-{end:#x}: {r:?}");
        f.flush();
    }

    /// Allocates IOVA space for `r`.
    fn alloc(&self, r: &Request) -> Result<iommu::gas::Allocation, DmaError> {
        self.space.lock().alloc(r).map_err(|_| DmaError::NoMemory)
    }

    /// Gives back the range at `start` once its invalidation is confirmed;
    /// otherwise keeps it out of use for good.
    fn release(&self, start: u64, end: u64, invalidated: Result<(), Unconfirmed>) {
        if invalidated.is_ok() {
            let _ = self.space.lock().free(Iova::new(start));
        } else {
            self.leaked
                .fetch_add((end - start) / PAGE, Ordering::Relaxed);
        }
    }
}

impl IoDomain for Domain {
    fn reach(&self) -> u64 {
        self.p.reach
    }

    fn map(
        &self,
        ranges: &[(u64, u64)],
        write: bool,
        c: &DmaConstraints,
    ) -> Result<IoMapping, DmaError> {
        let &(first, _) = ranges.first().ok_or(DmaError::Unsatisfiable)?;
        let total: u64 = ranges.iter().map(|r| r.1).sum();
        if total == 0
            || ranges
                .iter()
                .any(|&(pa, len)| pa.saturating_add(len) > self.p.reach)
        {
            return Err(DmaError::Unsatisfiable);
        }
        let offset = first % PAGE;
        // The tag's limits apply to the bus side (§4.5).
        let limit = c.limit().saturating_sub(1).min(SPACE_48 - 1);
        let mut r = Request::new(total)
            .offset(offset)
            .align(c.align.max(1))
            .limit(limit);
        // In one boundary unit when it fits in one; otherwise the tag
        // splits its segments at the boundaries.
        if let Some(b) = c.boundary
            && b >= PAGE
            && offset + total <= b
        {
            r = r.boundary(b);
        }
        let a = self.alloc(&r)?;
        let (start, end) = (a.start.get(), a.end.get());
        if let Err(e) = self.enter(start, ranges, pt::smmu::dma(write, self.cache())) {
            let _ = self.space.lock().free(a.start);
            return Err(e);
        }
        // FreeBSD's `smmu_map` invalidates what it entered; the entries
        // are visible either way, so a failure here changes nothing.
        let _ = self.invalidate(start, end - start);
        self.maps.fetch_add(1, Ordering::Relaxed);
        Ok(IoMapping {
            start,
            end,
            addr: a.addr.get(),
            len: total,
        })
    }

    fn unmap(&self, m: &IoMapping) {
        self.remove(m.start, m.end);
        let done = self.invalidate(m.start, m.end - m.start);
        self.release(m.start, m.end, done);
    }

    fn doorbell(&self, pa: u64) -> Result<u64, DmaError> {
        let page = pa & !(PAGE - 1);
        let mut g = self.doorbell.lock();
        if let Some((p, iova)) = *g {
            return if p == page {
                Ok(iova + pa % PAGE)
            } else {
                Err(DmaError::Unsatisfiable)
            };
        }
        let a = self.alloc(&Request::new(PAGE))?;
        let attrs = pt::smmu::dma(true, Cache::Device);
        if let Err(e) = self.enter(a.start.get(), &[(page, PAGE)], attrs) {
            let _ = self.space.lock().free(a.start);
            return Err(e);
        }
        let _ = self.invalidate(a.start.get(), PAGE);
        *g = Some((page, a.start.get()));
        Ok(a.start.get() + pa % PAGE)
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        // Nothing walks the tables any more (the unit cleared the STE
        // before its last reference went).
        let mut f = self.frames();
        let mut t = PageTable::<Smmu>::from_root(PhysAddr::new(self.root));
        t.free_tables(&mut f, VirtAddr::new(0), SPACE_48);
        pt::Memory::free_frame(&mut f, PhysAddr::new(self.root));
    }
}
