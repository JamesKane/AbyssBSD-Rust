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
use vm::object::{MemAttr, PageOps};
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

/// A page for an object: from the free lists, or, when they are empty,
/// after reclaiming clean file pages nothing maps.
fn user_page() -> Option<vm::PhysAddr> {
    alloc_pages(Order::ZERO).or_else(|| {
        // A batch, so the next allocations find pages too.
        (vm::object::reclaim_clean(256) > 0)
            .then(|| alloc_pages(Order::ZERO))
            .flatten()
    })
}

/// Pages `vm` objects hold (anonymous memory and file pages): what
/// `vm.stats.vm.v_active_count` reports.
pub(crate) static OBJECT_PAGES: sync::PerCpuCounter = sync::PerCpuCounter::new();

impl PageOps for KernelPages {
    fn alloc_zeroed(&self) -> Option<Pfn> {
        let pa = user_page()?;
        OBJECT_PAGES.add(1, core::sync::atomic::Ordering::Relaxed);
        let pfn = Pfn::containing(pa);
        // SAFETY: a fresh page, reached through the DMAP, used by no one.
        unsafe { core::ptr::write_bytes(Self::page(pfn), 0, PAGE_BYTES) };
        Some(pfn)
    }

    fn alloc_copy(&self, from: Pfn) -> Option<Pfn> {
        let pa = user_page()?;
        OBJECT_PAGES.add(1, core::sync::atomic::Ordering::Relaxed);
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
        let pa = user_page()?;
        OBJECT_PAGES.add(1, core::sync::atomic::Ordering::Relaxed);
        let pfn = Pfn::containing(pa);
        // SAFETY: a fresh page, reached through the DMAP, used by no one.
        fill(unsafe { core::slice::from_raw_parts_mut(Self::page(pfn), PAGE_BYTES) });
        Some(pfn)
    }

    fn free(&self, pfn: Pfn) {
        OBJECT_PAGES.add(-1, core::sync::atomic::Ordering::Relaxed);
        free_pages(pfn.addr(), Order::ZERO);
    }

    fn access(&self, pfn: Pfn, f: &mut dyn FnMut(&mut [u8])) {
        // SAFETY: a page an object holds, reached through the DMAP; the
        // object's owner serializes writers (the vnode lock, the map).
        f(unsafe { core::slice::from_raw_parts_mut(Self::page(pfn), PAGE_BYTES) });
    }

    fn page_ptr(&self, pfn: Pfn) -> Option<core::ptr::NonNull<u8>> {
        // Its place in the DMAP, fixed while it is allocated.
        core::ptr::NonNull::new(Self::page(pfn))
    }
}

static PAGES: KernelPages = KernelPages;

/// The physical page operations `vm` objects use.
pub(crate) fn page_ops() -> &'static dyn PageOps {
    &PAGES
}

/// The page-table attributes for a mapping with rights `prot`.
const fn attrs(prot: Prot, attr: MemAttr) -> Attrs {
    Attrs {
        write: prot.contains(Prot::WRITE),
        exec: prot.contains(Prot::EXEC),
        user: true,
        global: false,
        cache: match attr {
            MemAttr::WriteBack => Cache::WriteBack,
            MemAttr::WriteCombining => Cache::WriteCombining,
        },
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
    /// And ending at or below this (`MAP_32BIT`).
    pub(crate) below: Option<VirtAddr>,
    pub(crate) len: u64,
    /// A power of two of at least a page.
    pub(crate) align: u64,
    pub(crate) prot: Prot,
    pub(crate) max_prot: Prot,
    pub(crate) inherit: vm::map::Inherit,
    pub(crate) kind: AnonKind,
}

/// What anonymous memory [`AddressSpace::map_anon`] makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnonKind {
    /// Memory.
    Memory,
    /// A guard (`MAP_GUARD`).
    Guard,
    /// A stack that grows down, never into its bottom `guard` bytes
    /// (`MAP_STACK`).
    Stack { guard: u64 },
}

impl Default for Anon {
    fn default() -> Self {
        Self {
            fixed: None,
            exclusive: false,
            hint: VirtAddr::new(USER_START),
            below: None,
            len: 0,
            align: PAGE_SIZE,
            prot: Prot::RW,
            max_prot: Prot::ALL,
            inherit: vm::map::Inherit::Copy,
            kind: AnonKind::Memory,
        }
    }
}

/// Why an address-space operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpaceError {
    Map(MapError),
    /// No memory for the page tables.
    OutOfMemory,
    /// A pager could not write pages back.
    Io,
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
    /// The System V shared memory segments attached (FreeBSD's
    /// `vm_shm`).
    pub(crate) sysv_shm: sync::PreemptLock<crate::sysv::Attached>,
    /// Writable and executable mappings are refused (`MAP_WXORX`).
    wxorx: core::sync::atomic::AtomicBool,
    /// The pages `mlock` wired (`MAP_ENTRY_USER_WIRED`).
    wired: SpinLock<Wired>,
}

/// User-wired pages, as ranges of page numbers (`[start, end)`), merged.
/// Every page here is resident already, so wiring is accounting: it holds
/// `RLIMIT_MEMLOCK` and `vm.max_user_wired`.
#[derive(Debug, Default)]
pub(crate) struct Wired {
    ranges: alloc::collections::BTreeMap<u64, u64>,
    /// How many pages (`pmap_wired_count`).
    pub(crate) count: u64,
    /// `mlockall(MCL_FUTURE)`: new mappings come wired
    /// (`MAP_WIREFUTURE`).
    pub(crate) future: bool,
}

/// Pages user processes have wired, system-wide
/// (`vm.stats.vm.v_user_wire_count`).
pub(crate) static USER_WIRED: AtomicU64 = AtomicU64::new(0);

impl Wired {
    /// Whether all of pages `[start, end)` are wired.
    pub(crate) fn covers(&self, start: u64, end: u64) -> bool {
        self.ranges
            .range(..=start)
            .next_back()
            .is_some_and(|(_, &e)| e >= end)
    }

    /// Wires `[start, end)`: the pages newly wired.
    fn wire(&mut self, start: u64, end: u64) -> u64 {
        let before = self.count;
        self.unwire(start, end);
        // Merge with neighbors that touch.
        let (mut s, mut e) = (start, end);
        if let Some((&ps, &pe)) = self.ranges.range(..=s).next_back()
            && pe >= s
        {
            self.ranges.remove(&ps);
            self.count -= pe - ps;
            s = ps;
            e = e.max(pe);
        }
        if let Some((&ns, &ne)) = self.ranges.range(s..).next()
            && ns <= e
        {
            self.ranges.remove(&ns);
            self.count -= ne - ns;
            e = e.max(ne);
        }
        self.ranges.insert(s, e);
        self.count += e - s;
        let added = self.count - before;
        USER_WIRED.fetch_add(added, Ordering::Relaxed);
        added
    }

    /// Unwires whatever of `[start, end)` is wired.
    fn unwire(&mut self, start: u64, end: u64) {
        let hit: alloc::vec::Vec<(u64, u64)> = self
            .ranges
            .range(..end)
            .filter(|&(_, &e)| e > start)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in hit {
            self.ranges.remove(&s);
            self.count -= e - s;
            USER_WIRED.fetch_sub(e - s, Ordering::Relaxed);
            if s < start {
                self.ranges.insert(s, start);
                self.count += start - s;
                USER_WIRED.fetch_add(start - s, Ordering::Relaxed);
            }
            if e > end {
                self.ranges.insert(end, e);
                self.count += e - end;
                USER_WIRED.fetch_add(e - end, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for Wired {
    fn drop(&mut self) {
        USER_WIRED.fetch_sub(self.count, Ordering::Relaxed);
    }
}

impl AddressSpace {
    /// Runs `f` on the current thread's address space, borrowed (no
    /// reference taken): `None` if it runs in none.
    pub(crate) fn with_current<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        sched::with_address_space(|a| a?.as_any().downcast_ref::<Self>().map(f))
    }

    /// Whether mappings both writable and executable are refused
    /// (`MAP_WXORX`).
    pub(crate) fn wxorx(&self) -> bool {
        self.wxorx.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn set_wxorx(&self, on: bool) {
        self.wxorx.store(on, core::sync::atomic::Ordering::Relaxed);
    }

    /// Its user-wired pages.
    pub(crate) fn wired(&self) -> sync::SpinGuard<'_, Wired> {
        self.wired.lock()
    }

    /// Wires `range` (`vm_map_wire` with `VM_MAP_WIRE_USER |
    /// VM_MAP_WIRE_NOHOLES`): `None` if part of it is not mapped, else the
    /// pages newly wired.
    pub(crate) fn wire(&self, s: &Sleepable, range: core::ops::Range<VirtAddr>) -> Option<u64> {
        let map = self.map.lock(s);
        let mut at = range.start;
        while at < range.end {
            let e = map.entry(at)?;
            at = e.range().end;
        }
        drop(map);
        let (a, b) = (range.start.get() / PAGE_SIZE, range.end.get() / PAGE_SIZE);
        Some(self.wired.lock().wire(a, b))
    }

    /// Each entry's range, in order.
    pub(crate) fn mapped_ranges(
        &self,
        s: &Sleepable,
    ) -> alloc::vec::Vec<core::ops::Range<VirtAddr>> {
        self.map.lock(s).entries().map(Entry::range).collect()
    }

    /// Unwires `range` (`vm_map_unwire`).
    pub(crate) fn unwire(&self, range: core::ops::Range<VirtAddr>) {
        let (a, b) = (range.start.get() / PAGE_SIZE, range.end.get() / PAGE_SIZE);
        self.wired.lock().unwire(a, b);
    }

    /// Unwires everything (`munlockall`, and `MCL_FUTURE` goes).
    pub(crate) fn unwire_all(&self) {
        let mut w = self.wired.lock();
        w.unwire(0, u64::MAX);
        w.future = false;
    }

    /// How much it maps, in bytes (`vm_map`'s `size`).
    pub(crate) fn size(&self, s: &Sleepable) -> u64 {
        self.map
            .lock(s)
            .entries()
            .map(|e| e.range().end.get() - e.range().start.get())
            .sum()
    }

    /// A copy of every entry, in order (for process information:
    /// `kern.proc.vmmap`, `ki_size`).
    pub(crate) fn entries(&self, s: &Sleepable) -> alloc::vec::Vec<Entry> {
        self.map.lock(s).entries().cloned().collect()
    }

    /// The process's main stack (`vm_maxsaddr` and `vm_ssize`).
    pub(crate) fn proc_stack(&self, s: &Sleepable) -> Option<vm::map::ProcStack> {
        self.map.lock(s).proc_stack().cloned()
    }

    /// Pages of `range` the page tables map (`pmap_resident_count` over
    /// it): what is resident for this space.
    pub(crate) fn resident(&self, range: core::ops::Range<VirtAddr>) -> u64 {
        let len = range.end.get().saturating_sub(range.start.get());
        self.tables.lock().resident(&mut Frames, range.start, len)
    }

    /// Reads `buf.len()` bytes at `va` through the kernel, faulting the
    /// pages in for reading as user mode would, whether or not this space
    /// is loaded anywhere (FreeBSD's `proc_readmem`, for another process's
    /// environment).
    pub(crate) fn peek(
        &self,
        s: &Sleepable,
        va: VirtAddr,
        buf: &mut [u8],
    ) -> Result<(), FaultError> {
        let mut done = 0;
        while done < buf.len() {
            let at = va
                .get()
                .checked_add(done as u64)
                .ok_or(FaultError::Protection)?;
            let in_page = (at % PAGE_SIZE) as usize;
            let n = (PAGE_BYTES - in_page).min(buf.len() - done);
            let (map, r) = self.resolve(s, VirtAddr::new(at), Access::Read)?;
            // SAFETY: the page is this space's (just resolved for reading,
            // under the map lock), reached through the DMAP.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    KernelPages::page(r.pfn).add(in_page),
                    buf[done..].as_mut_ptr(),
                    n,
                );
            }
            drop(map);
            done += n;
        }
        Ok(())
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
            // An entry at a time, not 4 KiB of table on the stack.
            for i in 256..512 {
                let e = pt::Memory::frame(&mut frames, kernel)[i];
                pt::Memory::frame(&mut frames, table.root())[i] = e;
            }
        }
        Ok(Arc::new(Self {
            map: Mutex::new(&MAP_LOCK, map),
            tables: SpinLock::new(table),
            loaded_on: AtomicU64::new(0),
            #[cfg(target_arch = "aarch64")]
            asid: alloc_asid(),
            sysv_shm: sync::PreemptLock::new(alloc::vec::Vec::new()),
            wxorx: core::sync::atomic::AtomicBool::new(false),
            wired: SpinLock::new(Wired::default()),
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
        match a.kind {
            AnonKind::Memory => self.map_entry(s, a, |map, range| {
                map.insert(Entry::anonymous(range, a.prot, a.max_prot).with_inherit(a.inherit))
            }),
            AnonKind::Guard => self.map_entry(s, a, |map, range| map.insert(Entry::guard(range))),
            AnonKind::Stack { guard } => self.map_entry(s, a, |map, range| {
                map.insert_stack(range, guard, a.prot, a.max_prot)
            }),
        }
    }

    /// Makes `stack` the process's main stack, which `RLIMIT_STACK`'s
    /// current value, `limit`, bounds.
    pub(crate) fn set_proc_stack(&self, s: &Sleepable, stack: vm::map::ProcStack, limit: u64) {
        let mut map = self.map.lock(s);
        map.set_proc_stack(stack);
        map.set_stack_limit(limit);
    }

    /// Sets `RLIMIT_STACK`'s current value (`setrlimit`).
    pub(crate) fn set_stack_limit(&self, s: &Sleepable, limit: u64) {
        self.map.lock(s).set_stack_limit(limit);
    }

    /// Maps `object` from page `offset` on, placed as `a` says: shared
    /// (counted among its write mappings if `writecnt`), or privately
    /// (shared until written, then copied).
    pub(crate) fn map_object(
        &self,
        s: &Sleepable,
        a: &Anon,
        object: Arc<vm::object::Object>,
        offset: u64,
        writecnt: bool,
    ) -> Result<VirtAddr, SpaceError> {
        self.map_entry(s, a, |map, range| {
            map.insert(if a.inherit == vm::map::Inherit::Share {
                // `MAP_SHARED`: writes reach the file's pages.
                let e = Entry::shared(range, a.prot, a.max_prot, object, offset);
                if writecnt { e.counting_writes() } else { e }
            } else {
                Entry::private(range, a.prot, a.max_prot, object, offset).with_inherit(a.inherit)
            })
        })
    }

    /// Places a mapping as `a` says and has `insert` put its entry in
    /// the map over the range chosen.
    fn map_entry(
        &self,
        s: &Sleepable,
        a: &Anon,
        insert: impl FnOnce(&mut VmMap, core::ops::Range<VirtAddr>) -> Result<(), MapError>,
    ) -> Result<VirtAddr, SpaceError> {
        let mut map = self.map.lock(s);
        let start = match a.fixed {
            Some(at) => {
                let end = at.checked_add(a.len).ok_or(MapError::OutOfRange)?;
                // Taken: no space there (FreeBSD's `KERN_NO_SPACE`, so
                // `mmap` fails with `ENOMEM`).
                if a.exclusive && !map.is_free(&(at..end)) {
                    return Err(MapError::NoSpace.into());
                }
                self.unmap_locked(&mut map, at..end)?;
                at
            }
            None => match a.below {
                Some(limit) => map.find_space_below(a.len, a.align, a.hint, limit),
                None => map.find_space(a.len, a.align, a.hint),
            }
            .ok_or(MapError::NoSpace)?,
        };
        let end = start.checked_add(a.len).ok_or(MapError::OutOfRange)?;
        insert(&mut map, start..end)?;
        Ok(start)
    }

    /// Writes back the dirty pages of the files mapped over `range`
    /// (`msync`); `NotMapped` if some of it is not mapped.
    pub(crate) fn msync(
        &self,
        s: &Sleepable,
        range: core::ops::Range<VirtAddr>,
    ) -> Result<(), SpaceError> {
        let objects = self.map.lock(s).objects_in(&range)?;
        // Written with the map unlocked: a pager may sleep.
        for (object, start, end) in objects {
            if object.sync(start, end).is_err() {
                return Err(SpaceError::Io);
            }
        }
        Ok(())
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
            let (map, r) = self.resolve(s, VirtAddr::new(at), Access::Write)?;
            let replaced = self
                .tables
                .lock()
                .set(&mut Frames, r.page, r.pfn.addr(), attrs(r.prot, r.attr))
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
        // Unmapped pages are no longer wired.
        if self.wired.lock().count > 0 {
            self.unwire(range.clone());
        }
        let len = range.end.get() - range.start.get();
        if map.populated(&range) {
            let mut cleared = false;
            self.tables
                .lock()
                .clear(&mut Frames, range.start, len, |_, _| cleared = true);
            if cleared {
                self.flush(range.start, len);
            }
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
        max: Option<Prot>,
    ) -> Result<(), SpaceError> {
        let mut map = self.map.lock(s);
        map.protect(range.clone(), prot, max)?;
        let len = range.end.get() - range.start.get();
        let allowed = attrs(prot, MemAttr::WriteBack);
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
        let child = Self::with_map(forked.child)?;
        child.set_wxorx(self.wxorx());
        // The child shares the segments attached (`shmfork`).
        let attached = self.sysv_shm.lock().clone();
        crate::sysv::shm_forked(&attached);
        *child.sysv_shm.lock() = attached;
        Ok(child)
    }

    /// Resolves a fault at `va`: the map, locked, and what to map. A page
    /// a pager must read first is read with the map unlocked (it may sleep
    /// on the file system, whose locks come before the map's), then the
    /// fault is resolved again.
    fn resolve(
        &self,
        s: &Sleepable,
        va: VirtAddr,
        access: Access,
    ) -> Result<(sched::lock::MutexGuard<'_, VmMap>, fault::Resolved), FaultError> {
        loop {
            let mut map = self.map.lock(s);
            match fault::resolve(&mut map, va, access)? {
                fault::Outcome::Map(r) => return Ok((map, r)),
                fault::Outcome::Fill(object, index) => {
                    drop(map);
                    object.fill(index).map_err(|e| match e {
                        vm::object::LookupError::OutOfMemory => FaultError::OutOfMemory,
                        vm::object::LookupError::PastEnd | vm::object::LookupError::Io => {
                            FaultError::PastEnd
                        }
                    })?;
                }
            }
        }
    }

    /// The memory object behind `va` and the byte offset of `va` in it,
    /// as a umtx key for a word other processes may map names it
    /// (FreeBSD's `vm_map_lookup` for `VM_PROT_WRITE` in `umtx_key_get`):
    /// for any mapping if `any`, else only for one children inherit
    /// shared (`None` otherwise). The write lookup gives a copy-on-write
    /// mapping its own copy first, and fails as a write would.
    pub(crate) fn object_at(
        &self,
        s: &Sleepable,
        va: VirtAddr,
        any: bool,
    ) -> Result<Option<(Arc<vm::object::Object>, u64)>, FaultError> {
        self.object_for(s, va, Access::Write, any)
    }

    /// [`object_at`](Self::object_at) for an `access` lookup.
    pub(crate) fn object_for(
        &self,
        s: &Sleepable,
        va: VirtAddr,
        access: Access,
        any: bool,
    ) -> Result<Option<(Arc<vm::object::Object>, u64)>, FaultError> {
        let (map, _) = self.resolve(s, va, access)?;
        let e = map.entry(va).ok_or(FaultError::NotMapped)?;
        if !any && e.inherit() != vm::map::Inherit::Share {
            return Ok(None);
        }
        let object = e.object().cloned().ok_or(FaultError::NotMapped)?;
        Ok(Some((
            object,
            e.index_of(va) * PAGE_SIZE + va.get() % PAGE_SIZE,
        )))
    }

    /// Resolves a fault at `va` and maps the page.
    pub(crate) fn fault(
        &self,
        s: &Sleepable,
        va: VirtAddr,
        access: Access,
    ) -> Result<(), FaultError> {
        let (map, r) = self.resolve(s, va, access)?;
        // Code reaches instruction fetch only once synced (aarch64; x86
        // keeps the caches coherent).
        #[cfg(target_arch = "aarch64")]
        if r.prot.contains(Prot::EXEC) && r.attr == MemAttr::WriteBack {
            let kva = KernelPages::page(r.pfn);
            arch::cache::sync_icache(kva as usize, PAGE_BYTES);
        }
        let replaced = self
            .tables
            .lock()
            .set(&mut Frames, r.page, r.pfn.addr(), attrs(r.prot, r.attr))
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
            if r.prot.contains(Prot::EXEC) && r.attr == MemAttr::WriteBack {
                let kva = KernelPages::page(r.pfn);
                arch::cache::sync_icache(kva as usize, PAGE_BYTES);
            }
            // Out of table memory: the page faults in later instead.
            let _ = tables.set(&mut Frames, r.page, r.pfn.addr(), attrs(r.prot, r.attr));
        }
    }

    /// Unmaps everything (at exit); the empty space stays loaded until its
    /// CPUs switch to another.
    pub(crate) fn clear(&self, s: &Sleepable) {
        let mut map = self.map.lock(s);
        let _ = self.unmap_locked(&mut map, VirtAddr::new(USER_START)..VirtAddr::new(USER_END));
    }

    /// How many times the map has changed (`PT_VM_TIMESTAMP`).
    pub(crate) fn timestamp(&self) -> u32 {
        // SAFETY: a sysctl or ptrace request reads it with no lock held,
        // which may sleep.
        let s = unsafe { Sleepable::assume() };
        self.map.lock(&s).timestamp()
    }

    /// The `index`th mapping (in address order), as `PT_VM_ENTRY` and
    /// core dumps describe it.
    pub(crate) fn entry_info(&self, s: &Sleepable, index: usize) -> Option<EntryInfo> {
        let map = self.map.lock(s);
        let e = map.entries().nth(index)?;
        let prot = e.prot();
        let bits = u32::from(prot.contains(Prot::READ))
            | u32::from(prot.contains(Prot::WRITE)) << 1
            | u32::from(prot.contains(Prot::EXEC)) << 2;
        Some(EntryInfo {
            start: e.start().get(),
            end: e.end().get(),
            prot: bits,
            dumpable: e.object().is_some_and(|o| !o.is_device()),
        })
    }

    /// `proc_rwmem` for a debugger: reads `buf.len()` bytes at `va` into
    /// `buf`, or writes them there, page by page, through the kernel,
    /// whether or not the space is loaded anywhere. A write gives a
    /// private mapping its own copy first (`VM_PROT_COPY`), and keeps the
    /// instruction cache coherent with what it wrote. Returns how many
    /// bytes moved before a page that could not be had.
    pub(crate) fn debug_rw(&self, s: &Sleepable, va: u64, buf: &mut [u8], write: bool) -> usize {
        let access = if write { Access::Copy } else { Access::Read };
        let mut done = 0;
        while done < buf.len() {
            let Some(at) = va.checked_add(done as u64).filter(|&a| a < USER_END) else {
                break;
            };
            let in_page = (at % PAGE_SIZE) as usize;
            let n = (PAGE_BYTES - in_page).min(buf.len() - done);
            let Ok((map, r)) = self.resolve(s, VirtAddr::new(at), access) else {
                break;
            };
            // SAFETY: the page belongs to this space's map (resolved under
            // its lock, for writing a private copy if `write`), reached
            // through the DMAP while the lock is held.
            unsafe {
                let page = KernelPages::page(r.pfn).add(in_page);
                if write {
                    core::ptr::copy_nonoverlapping(buf[done..].as_ptr(), page, n);
                } else {
                    core::ptr::copy_nonoverlapping(page, buf[done..].as_mut_ptr(), n);
                }
            }
            if write {
                // The page may have been replaced by its copy: what is
                // mapped there now must be it.
                let replaced = self.tables.lock().set(
                    &mut Frames,
                    r.page,
                    r.pfn.addr(),
                    attrs(r.prot, r.attr),
                );
                if let Ok(Some(_)) = replaced {
                    self.flush(r.page, PAGE_SIZE);
                }
                #[cfg(target_arch = "aarch64")]
                {
                    // SAFETY: as above.
                    let kva = unsafe { KernelPages::page(r.pfn).add(in_page) };
                    arch::cache::sync_icache(kva as usize, n);
                }
            }
            drop(map);
            done += n;
        }
        done
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
        // Its System V segments are detached (`shmexit`).
        crate::sysv::shm_exited(core::mem::take(self.sysv_shm.get_mut()));
        // The map drops after this, freeing the pages no table maps now.
    }
}

/// A mapping as a debugger sees it ([`AddressSpace::entry_info`]).
pub(crate) struct EntryInfo {
    pub(crate) start: u64,
    pub(crate) end: u64,
    /// `VM_PROT_*`.
    pub(crate) prot: u32,
    /// It has memory a core dump holds: an object (touched), not a
    /// device's (`each_dumpable_segment`).
    pub(crate) dumpable: bool,
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
        // A traced process's faults are recorded (`trap_pfault`): the
        // access as `VM_PROT_*`, the result as `KERN_*`.
        let traced = crate::ktrace::point(abi::ktrace::KTR_FAULT)
            || crate::ktrace::point(abi::ktrace::KTR_FAULTEND);
        if traced {
            let prot = match access {
                Access::Read | Access::Copy => 1,
                Access::Write => 2,
                Access::Execute => 4,
            };
            crate::ktrace::fault(f.address, prot);
        }
        let r = space.fault(&s, VirtAddr::new(f.address), access);
        if traced {
            crate::ktrace::fault_end(match r {
                Ok(()) => 0,
                Err(vm::fault::FaultError::NotMapped) => 1,
                Err(vm::fault::FaultError::Protection) => 2,
                Err(vm::fault::FaultError::OutOfMemory) => 6,
                Err(vm::fault::FaultError::PastEnd) => 9,
            });
        }
        let ok = r.is_ok();
        // Masked again for the trap's return path; it restores the
        // interrupted state from the frame.
        let _ = hal::irq::disable();
        ok
    })
    .unwrap_or(false)
}
