// SPDX-License-Identifier: BSD-2-Clause
//! User address spaces: a `vm` map plus the page tables that mirror it
//! (FreeBSD's `vmspace` and `pmap`).
//!
//! The map is the truth; the tables are a cache of it, filled by faults.
//! Every operation takes the map lock (a sleepable mutex), changes the
//! map, and brings the tables in line before releasing it, so a fault and
//! an `munmap` never interleave. The tables themselves sit behind a spin
//! lock, held only while entries are edited; TLB shootdowns happen after
//! it is released.
//!
//! On x86_64 the tables' upper half is the kernel's (its root entries are
//! copied in, and all of them exist from boot), and TLB entries of other
//! CPUs are shot down with an IPI to the CPUs the space is loaded on. On
//! aarch64 the kernel lives in TTBR1, user tables in TTBR0 tagged with an
//! ASID, and invalidation broadcasts by ASID.

// Processes (P4.3) create address spaces; until then only the kernel tests
// do.
#![cfg_attr(not(feature = "ktest"), allow(dead_code))]

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use addr::{PhysAddr, VirtAddr};
use bootproto::layout::dmap;
use pt::{Attrs, Cache, PageTable};
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;
use vm::fault::{self, Access, FaultError};
use vm::map::{Entry, MapError, Prot, VmMap};
use vm::object::PageOps;
use vm::{Order, PAGE_SIZE, Pfn};

use super::{Format, Frames, alloc_pages, free_pages};

/// [`PAGE_SIZE`] as a length.
const PAGE_BYTES: usize = 4096;
const _: () = assert!(PAGE_BYTES as u64 == PAGE_SIZE);
use crate::arch;

/// Taken in the page-fault path, which `copyout` can enter with other
/// sleepable locks held (sysctl's, for one): it ranks above all of them.
static MAP_LOCK: LockClass = LockClass::new("vm map", 900);

/// The lowest user address: page 0 stays unmapped (FreeBSD's
/// `security.bsd.map_at_zero=0`).
pub(crate) const USER_START: u64 = PAGE_SIZE;
/// The end of the user half.
pub(crate) const USER_END: u64 = arch::user::USER_END;

/// The kernel's physical pages, as `vm` objects use them.
struct KernelPages;

impl KernelPages {
    fn page(pfn: Pfn) -> *mut u8 {
        dmap(pfn.addr()).expect("RAM outside the DMAP").get() as *mut u8
    }
}

impl PageOps for KernelPages {
    fn alloc_zeroed(&self) -> Option<Pfn> {
        let pa = alloc_pages(Order::ZERO)?;
        let pfn = Pfn::containing(pa);
        // SAFETY: a fresh page, reached through the DMAP, used by no one.
        unsafe { core::ptr::write_bytes(Self::page(pfn), 0, PAGE_BYTES) };
        Some(pfn)
    }

    fn alloc_copy(&self, from: Pfn) -> Option<Pfn> {
        let pa = alloc_pages(Order::ZERO)?;
        let pfn = Pfn::containing(pa);
        // SAFETY: `from` is a live page of some object (only read while
        // its object reaches it, and no one writes it through the kernel);
        // `pfn` is fresh. Both are in the DMAP.
        unsafe {
            core::ptr::copy_nonoverlapping(Self::page(from), Self::page(pfn), PAGE_BYTES);
        }
        Some(pfn)
    }

    fn alloc_filled(&self, fill: &mut dyn FnMut(&mut [u8])) -> Option<Pfn> {
        let pa = alloc_pages(Order::ZERO)?;
        let pfn = Pfn::containing(pa);
        // SAFETY: a fresh page, reached through the DMAP, used by no one.
        fill(unsafe { core::slice::from_raw_parts_mut(Self::page(pfn), PAGE_BYTES) });
        Some(pfn)
    }

    fn free(&self, pfn: Pfn) {
        free_pages(pfn.addr(), Order::ZERO);
    }
}

static PAGES: KernelPages = KernelPages;

/// The physical page operations `vm` objects use.
pub(crate) fn page_ops() -> &'static dyn PageOps {
    &PAGES
}

/// The page-table attributes for a mapping with rights `prot`.
const fn attrs(prot: Prot) -> Attrs {
    Attrs {
        write: prot.contains(Prot::WRITE),
        exec: prot.contains(Prot::EXEC),
        user: true,
        global: false,
        cache: Cache::WriteBack,
    }
}

/// ASIDs in use (aarch64). ASID 0 is the fallback when all are taken:
/// spaces that share it flush it each time one is loaded.
#[cfg(target_arch = "aarch64")]
static ASIDS: SpinLock<[u64; arch::user::ASIDS as usize / 64]> =
    SpinLock::new([1; arch::user::ASIDS as usize / 64]);

#[cfg(target_arch = "aarch64")]
fn alloc_asid() -> u16 {
    let mut words = ASIDS.lock();
    for (i, w) in words.iter_mut().enumerate() {
        if *w != u64::MAX {
            let bit = w.trailing_ones();
            *w |= 1 << bit;
            return u16::try_from(i * 64).unwrap_or(0) + u16::try_from(bit).unwrap_or(0);
        }
    }
    0
}

#[cfg(target_arch = "aarch64")]
fn free_asid(asid: u16) {
    if asid != 0 {
        // No CPU may keep translations tagged with it for the next owner.
        arch::user::flush_asid(asid);
        ASIDS.lock()[usize::from(asid / 64)] &= !(1 << (asid % 64));
    }
}

/// Pages whose translations changed, to flush: listed while they are few.
struct Batch {
    pages: [VirtAddr; 64],
    /// How many there were; more than `pages` holds means too many.
    n: usize,
}

impl Default for Batch {
    fn default() -> Self {
        Self {
            pages: [VirtAddr::ZERO; 64],
            n: 0,
        }
    }
}

impl Batch {
    fn push(&mut self, va: VirtAddr) {
        if let Some(slot) = self.pages.get_mut(self.n) {
            *slot = va;
        }
        self.n = self.n.saturating_add(1);
    }

    /// The pages, or `None` if there were too many to list.
    fn pages(&self) -> Option<&[VirtAddr]> {
        self.pages.get(..self.n)
    }
}

/// Where and how to map: anonymous memory, or an object.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Anon {
    /// Exactly here, replacing what is there (`MAP_FIXED`).
    pub(crate) fixed: Option<VirtAddr>,
    /// With `fixed`, fail rather than replace (`MAP_EXCL`).
    pub(crate) exclusive: bool,
    /// Otherwise, the lowest free range at or above this.
    pub(crate) hint: VirtAddr,
    pub(crate) len: u64,
    /// A power of two of at least a page.
    pub(crate) align: u64,
    pub(crate) prot: Prot,
    pub(crate) max_prot: Prot,
    pub(crate) inherit: vm::map::Inherit,
}

impl Default for Anon {
    fn default() -> Self {
        Self {
            fixed: None,
            exclusive: false,
            hint: VirtAddr::new(USER_START),
            len: 0,
            align: PAGE_SIZE,
            prot: Prot::RW,
            max_prot: Prot::ALL,
            inherit: vm::map::Inherit::Copy,
        }
    }
}

/// Why an address-space operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpaceError {
    Map(MapError),
    /// No memory for the page tables.
    OutOfMemory,
}

impl From<MapError> for SpaceError {
    fn from(e: MapError) -> Self {
        Self::Map(e)
    }
}

/// A user address space.
pub(crate) struct AddressSpace {
    map: Mutex<VmMap>,
    tables: SpinLock<PageTable<Format>>,
    /// CPUs this space is loaded on, for shootdowns (x86_64).
    loaded_on: AtomicU64,
    #[cfg(target_arch = "aarch64")]
    asid: u16,
}

impl AddressSpace {
    /// Runs `f` on the current thread's address space, borrowed (no
    /// reference taken): `None` if it runs in none.
    pub(crate) fn with_current<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        sched::with_address_space(|a| a?.as_any().downcast_ref::<Self>().map(f))
    }

    /// An empty address space.
    pub(crate) fn new() -> Result<Arc<Self>, SpaceError> {
        Self::with_map(VmMap::new(
            VirtAddr::new(USER_START)..VirtAddr::new(USER_END),
            &PAGES,
        ))
    }

    fn with_map(map: VmMap) -> Result<Arc<Self>, SpaceError> {
        let mut frames = Frames;
        let table = PageTable::<Format>::new(&mut frames, pt::PageSize::Size4K)
            .map_err(|_| SpaceError::OutOfMemory)?;
        #[cfg(target_arch = "x86_64")]
        {
            // The kernel's half: the same root entries, so the same tables.
            let kernel = super::kernel_root();
            let upper = *pt::Memory::frame(&mut frames, kernel);
            pt::Memory::frame(&mut frames, table.root())[256..].copy_from_slice(&upper[256..]);
        }
        Ok(Arc::new(Self {
            map: Mutex::new(&MAP_LOCK, map),
            tables: SpinLock::new(table),
            loaded_on: AtomicU64::new(0),
            #[cfg(target_arch = "aarch64")]
            asid: alloc_asid(),
        }))
    }

    /// Removes the stale translations for `[va, va + len)` everywhere.
    /// Call with the tables' lock released and interrupts enabled.
    fn flush(&self, va: VirtAddr, len: u64) {
        #[cfg(target_arch = "x86_64")]
        {
            let me = 1u64 << hal::cpu_id().0;
            // The table stores just made must be visible before the CPUs
            // that load this space are read: a CPU that sets its bit after
            // this read then walks the new tables. x86 lets a load pass an
            // earlier store, so without the fence that CPU could cache an
            // old entry and be skipped (`activate` sets its bit with a
            // locked instruction, a full barrier on its side).
            core::sync::atomic::fence(Ordering::SeqCst);
            let loaded = self.loaded_on.load(Ordering::SeqCst);
            if loaded & me != 0 {
                if len > 64 * PAGE_SIZE {
                    arch::user::flush_user();
                } else {
                    let mut off = 0;
                    while off < len {
                        arch::user::flush_page(va.get() + off);
                        off += PAGE_SIZE;
                    }
                }
            }
            if loaded & !me != 0 {
                crate::smp::shootdown_cpus(va, len, loaded & !me);
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            if len > 64 * PAGE_SIZE {
                arch::user::flush_asid(self.asid);
            } else {
                let mut off = 0;
                while off < len {
                    arch::user::flush_page(va.get() + off, self.asid);
                    off += PAGE_SIZE;
                }
            }
        }
    }

    /// [`Self::flush`] for a few scattered pages: one barrier and one
    /// look at the CPUs for all of them.
    fn flush_pages(&self, pages: &[VirtAddr]) {
        let (Some(first), Some(last)) = (pages.iter().min(), pages.iter().max()) else {
            return;
        };
        #[cfg(target_arch = "x86_64")]
        {
            let me = 1u64 << hal::cpu_id().0;
            // As in `flush`.
            core::sync::atomic::fence(Ordering::SeqCst);
            let loaded = self.loaded_on.load(Ordering::SeqCst);
            if loaded & me != 0 {
                for va in pages {
                    arch::user::flush_page(va.get());
                }
            }
            if loaded & !me != 0 {
                let len = last.get() - first.get() + PAGE_SIZE;
                crate::smp::shootdown_cpus(*first, len, loaded & !me);
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            let _ = (first, last);
            for va in pages {
                arch::user::flush_page(va.get(), self.asid);
            }
        }
    }

    /// Maps anonymous memory: at `fixed` (replacing what was there), or
    /// wherever `len` bytes fit at or above `hint`.
    pub(crate) fn map_anonymous(
        &self,
        s: &Sleepable,
        fixed: Option<VirtAddr>,
        hint: VirtAddr,
        len: u64,
        prot: Prot,
    ) -> Result<VirtAddr, SpaceError> {
        self.map_anon(
            s,
            &Anon {
                fixed,
                hint,
                len,
                prot,
                ..Anon::default()
            },
        )
    }

    /// The lowest free, page-aligned range of `len` bytes at or above
    /// `hint` (wrapping around), as a mapping without a fixed address
    /// would take.
    pub(crate) fn find_space(&self, s: &Sleepable, len: u64, hint: VirtAddr) -> Option<VirtAddr> {
        self.map.lock(s).find_space(len, PAGE_SIZE, hint)
    }

    /// Maps anonymous memory as `a` says (`mmap` with `MAP_ANON`).
    pub(crate) fn map_anon(&self, s: &Sleepable, a: &Anon) -> Result<VirtAddr, SpaceError> {
        self.map_entry(s, a, |range| {
            Entry::anonymous(range, a.prot, a.max_prot).with_inherit(a.inherit)
        })
    }

    /// Maps `object` from page `offset` on, placed as `a` says, privately
    /// (`mmap` of a file: shared until written, then copied).
    pub(crate) fn map_object(
        &self,
        s: &Sleepable,
        a: &Anon,
        object: Arc<vm::object::Object>,
        offset: u64,
    ) -> Result<VirtAddr, SpaceError> {
        self.map_entry(s, a, |range| {
            Entry::private(range, a.prot, a.max_prot, object, offset).with_inherit(a.inherit)
        })
    }

    /// Places a mapping as `a` says and inserts the entry `make` builds
    /// for its range.
    fn map_entry(
        &self,
        s: &Sleepable,
        a: &Anon,
        make: impl FnOnce(core::ops::Range<VirtAddr>) -> Entry,
    ) -> Result<VirtAddr, SpaceError> {
        let mut map = self.map.lock(s);
        let start = match a.fixed {
            Some(at) => {
                let end = at.checked_add(a.len).ok_or(MapError::OutOfRange)?;
                if a.exclusive && !map.is_free(&(at..end)) {
                    return Err(MapError::Overlap.into());
                }
                self.unmap_locked(&mut map, at..end)?;
                at
            }
            None => map
                .find_space(a.len, a.align.max(PAGE_SIZE), a.hint)
                .ok_or(MapError::NoSpace)?,
        };
        let end = start.checked_add(a.len).ok_or(MapError::OutOfRange)?;
        map.insert(make(start..end))?;
        Ok(start)
    }

    /// Sets what a child inherits of `range` (`minherit`).
    pub(crate) fn set_inherit(
        &self,
        s: &Sleepable,
        range: core::ops::Range<VirtAddr>,
        inherit: vm::map::Inherit,
    ) -> Result<(), SpaceError> {
        self.map.lock(s).set_inherit(range, inherit)?;
        Ok(())
    }

    /// Maps `object` from page `offset` on over `range`, privately (a
    /// file's text or data: shared until written, then copied), with
    /// rights `prot` that `mprotect` may raise to `max_prot`.
    pub(crate) fn map_private(
        &self,
        s: &Sleepable,
        range: core::ops::Range<VirtAddr>,
        object: Arc<vm::object::Object>,
        offset: u64,
        prot: Prot,
        max_prot: Prot,
    ) -> Result<(), SpaceError> {
        let mut map = self.map.lock(s);
        map.insert(Entry::private(range, prot, max_prot, object, offset))?;
        Ok(())
    }

    /// Writes `bytes` at `va` through the kernel, faulting the pages in for
    /// writing as user mode would (so copy-on-write applies), whether or
    /// not this space is loaded anywhere. For building a new image.
    pub(crate) fn poke(&self, s: &Sleepable, va: VirtAddr, bytes: &[u8]) -> Result<(), FaultError> {
        let mut done = 0;
        while done < bytes.len() {
            let at = va.get() + done as u64;
            let in_page = (at % PAGE_SIZE) as usize;
            let n = (PAGE_BYTES - in_page).min(bytes.len() - done);
            let mut map = self.map.lock(s);
            let r = fault::resolve(&mut map, VirtAddr::new(at), Access::Write)?;
            let replaced = self
                .tables
                .lock()
                .set(&mut Frames, r.page, r.pfn.addr(), attrs(r.prot))
                .map_err(|_| FaultError::OutOfMemory)?;
            if replaced.is_some() {
                self.flush(r.page, PAGE_SIZE);
            }
            // SAFETY: the page is this space's (just resolved for writing,
            // under the map lock), reached through the DMAP.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes[done..].as_ptr(),
                    KernelPages::page(r.pfn).add(in_page),
                    n,
                );
            }
            drop(map);
            done += n;
        }
        Ok(())
    }

    fn unmap_locked(
        &self,
        map: &mut VmMap,
        range: core::ops::Range<VirtAddr>,
    ) -> Result<(), SpaceError> {
        if range.start >= range.end {
            return Err(MapError::Misaligned.into());
        }
        let len = range.end.get() - range.start.get();
        let mut cleared = false;
        self.tables
            .lock()
            .clear(&mut Frames, range.start, len, |_, _| cleared = true);
        if cleared {
            self.flush(range.start, len);
        }
        map.remove(range)?;
        Ok(())
    }

    /// Unmaps `range` (`munmap`).
    pub(crate) fn unmap(
        &self,
        s: &Sleepable,
        range: core::ops::Range<VirtAddr>,
    ) -> Result<(), SpaceError> {
        let mut map = self.map.lock(s);
        self.unmap_locked(&mut map, range)
    }

    /// Sets the rights of `range` (`mprotect`).
    pub(crate) fn protect(
        &self,
        s: &Sleepable,
        range: core::ops::Range<VirtAddr>,
        prot: Prot,
    ) -> Result<(), SpaceError> {
        let mut map = self.map.lock(s);
        map.protect(range.clone(), prot)?;
        let len = range.end.get() - range.start.get();
        let allowed = attrs(prot);
        let mut changed = false;
        // Only rights go away here; added ones arrive by fault. A page
        // with no rights left is unmapped outright.
        if prot == Prot::NONE {
            self.tables
                .lock()
                .clear(&mut Frames, range.start, len, |_, _| changed = true);
        } else {
            self.tables.lock().protect(
                &mut Frames,
                range.start,
                len,
                |a| Attrs {
                    write: a.write && allowed.write,
                    exec: a.exec && allowed.exec,
                    ..a
                },
                |_| changed = true,
            );
        }
        if changed {
            self.flush(range.start, len);
        }
        Ok(())
    }

    /// The child's address space for `fork`.
    pub(crate) fn fork(&self, s: &Sleepable) -> Result<Arc<Self>, SpaceError> {
        let mut map = self.map.lock(s);
        let forked = map.fork();
        // The pages that lose write: usually the few the parent wrote since
        // the last fork, which are flushed one by one; past a batch, the
        // whole space.
        let mut changed = Batch::default();
        {
            let mut tables = self.tables.lock();
            for r in forked.write_protect {
                let len = r.end.get() - r.start.get();
                tables.protect(
                    &mut Frames,
                    r.start,
                    len,
                    |a| Attrs { write: false, ..a },
                    |va| changed.push(va),
                );
            }
        }
        match changed.pages() {
            Some(pages) => self.flush_pages(pages),
            None => self.flush(VirtAddr::new(USER_START), USER_END - USER_START),
        }
        drop(map);
        Self::with_map(forked.child)
    }

    /// Resolves a fault at `va` and maps the page.
    pub(crate) fn fault(
        &self,
        s: &Sleepable,
        va: VirtAddr,
        access: Access,
    ) -> Result<(), FaultError> {
        let mut map = self.map.lock(s);
        let r = fault::resolve(&mut map, va, access)?;
        // Code reaches instruction fetch only once synced (aarch64; x86
        // keeps the caches coherent).
        #[cfg(target_arch = "aarch64")]
        if r.prot.contains(Prot::EXEC) {
            let kva = KernelPages::page(r.pfn);
            arch::cache::sync_icache(kva as usize, PAGE_BYTES);
        }
        let replaced = self
            .tables
            .lock()
            .set(&mut Frames, r.page, r.pfn.addr(), attrs(r.prot))
            .map_err(|_| FaultError::OutOfMemory)?;
        if replaced.is_some() {
            self.flush(r.page, PAGE_SIZE);
        }
        // Not after a write: the neighbors would be mapped read-only, and
        // a process writing a run of copy-on-write pages would then take a
        // protection fault and a flush for each instead of a cheap fault.
        if access != Access::Write {
            self.prefault(&map, va);
        }
        Ok(())
    }

    /// Maps the resident pages around a read or execute fault, read-only,
    /// where nothing is mapped yet (FreeBSD's `vm_fault_prefault`, four
    /// pages each side): a program touching its text or data in order
    /// faults once per few pages instead of once per page. New entries
    /// need no flush.
    fn prefault(&self, map: &VmMap, va: VirtAddr) {
        const AROUND: u64 = 4;
        let near = fault::prefault(map, va, AROUND);
        if near.is_empty() {
            return;
        }
        let mut tables = self.tables.lock();
        for r in near {
            if tables.translate(&mut Frames, r.page).is_some() {
                continue;
            }
            #[cfg(target_arch = "aarch64")]
            if r.prot.contains(Prot::EXEC) {
                let kva = KernelPages::page(r.pfn);
                arch::cache::sync_icache(kva as usize, PAGE_BYTES);
            }
            // Out of table memory: the page faults in later instead.
            let _ = tables.set(&mut Frames, r.page, r.pfn.addr(), attrs(r.prot));
        }
    }

    /// Unmaps everything (at exit); the empty space stays loaded until its
    /// CPUs switch to another.
    pub(crate) fn clear(&self, s: &Sleepable) {
        let mut map = self.map.lock(s);
        let _ = self.unmap_locked(&mut map, VirtAddr::new(USER_START)..VirtAddr::new(USER_END));
    }

    /// The physical page `va` is mapped to, if it is (for tests).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub(crate) fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.tables.lock().translate(&mut Frames, va).map(|t| t.pa)
    }
}

impl sched::AddressSpace for AddressSpace {
    fn activate(&self) {
        let root = self.tables.lock().root();
        self.loaded_on
            .fetch_or(1 << hal::cpu_id().0, Ordering::AcqRel);
        #[cfg(target_arch = "x86_64")]
        // SAFETY: the root's upper half is the kernel's (copied at
        // creation; the kernel's root entries never change).
        unsafe {
            arch::user::activate(root);
        }
        #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: a lower-half hierarchy; an ASID of our own carries no
            // other space's entries, and ASID 0 is flushed when shared.
            unsafe { arch::user::activate(root, self.asid) };
            if self.asid == 0 {
                arch::user::flush_asid(0);
            }
        }
    }

    fn deactivate(&self) {
        self.loaded_on
            .fetch_and(!(1 << hal::cpu_id().0), Ordering::AcqRel);
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        let mut frames = Frames;
        let table = self.tables.get_mut();
        // No CPU has it loaded (the scheduler holds a reference while one
        // does), so no TLB holds its entries but under its ASID. The leaf
        // entries need no clearing: the tables holding them go, and the
        // pages they map belong to the map's objects.
        table.free_tables(&mut frames, VirtAddr::ZERO, USER_END);
        pt::Memory::free_frame(&mut frames, table.root());
        #[cfg(target_arch = "aarch64")]
        free_asid(self.asid);
        // The map drops after this, freeing the pages no table maps now.
    }
}

/// Stops this CPU using the user space it has loaded, if the current
/// thread runs in none, so the space can be freed.
pub(crate) fn unload() {
    sched::unload_address_space(load_kernel_tables);
}

/// Points the MMU at tables with no user half. Interrupts are masked.
pub(crate) fn load_kernel_tables() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the kernel's own tables map the kernel as every user root
    // does.
    unsafe {
        arch::user::activate(super::kernel_root());
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: the kernel keeps nothing in the lower half.
    unsafe {
        arch::mmu::disable_lower_half();
    }
}

/// The page-fault hook: resolves faults on user addresses from user mode,
/// or from the kernel's copy routines, in the current thread's space.
pub(crate) fn page_fault(f: &arch::PageFault) -> bool {
    if f.address >= USER_END || !(f.user || f.in_copy) {
        return false;
    }
    // Resolving may sleep (the map lock): only in a thread that could.
    if !f.interrupts_enabled || hal::interrupt::active() || hal::critical::depth() != 0 {
        return false;
    }
    let access = match f.access {
        arch::FaultAccess::Read => Access::Read,
        arch::FaultAccess::Write => Access::Write,
        arch::FaultAccess::Execute => Access::Execute,
    };
    AddressSpace::with_current(|space| {
        hal::irq::enable();
        // SAFETY: the fault came from a thread (interrupts were enabled,
        // outside interrupt handlers and critical sections), which may
        // sleep as the code it interrupted could have.
        let s = unsafe { Sleepable::assume() };
        let ok = space.fault(&s, VirtAddr::new(f.address), access).is_ok();
        // Masked again for the trap's return path; it restores the
        // interrupted state from the frame.
        let _ = hal::irq::disable();
        ok
    })
    .unwrap_or(false)
}
