// SPDX-License-Identifier: BSD-2-Clause
//! The kernel heap: size-class slabs with per-CPU caches, and whole page
//! blocks for large requests. [`Heap`] implements `GlobalAlloc`.
//!
//! Each size class carves slabs (one or four pages, naturally aligned, from
//! a [`PageSource`]) into equal objects. A slab's header sits at its start,
//! so freeing an object finds its slab by rounding the address down; the
//! `Layout` passed to `dealloc` says which class it came from. Free objects
//! are linked through their own first word, and never-used objects are
//! handed out by a bump index, so a new slab is not touched up front. Each
//! class keeps one empty slab cached and returns others to the source.
//!
//! In front of the classes sit magazines, Bonwick-style (UMA's buckets):
//! every CPU holds two per class, a loaded one and the previous one, used
//! in a critical section (preemption held off, interrupts left on, as
//! UMA does), so the common allocation and free take no lock. Interrupt
//! handlers, which could interrupt that use, go to the classes directly.
//! When both are exhausted the CPU trades a whole magazine with the class's
//! depot of full and empty magazines, so a refill or flush moves one
//! pointer under a lock rather than many objects. Only when the depot has
//! no full magazine are objects taken from slabs. A magazine holds about
//! 32 KiB of objects (16 to 256 of them) and lives in one page from the
//! page source.
//!
//! Requests larger than the largest class, or more aligned than a page, go
//! straight to the page source as one buddy block.
//!
//! Behavior reference: sys/vm/uma_core.c (zones, per-CPU buckets),
//! sys/kern/kern_malloc.c (size classes, large allocations).

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use sync::SpinLock;

use crate::Order;

/// [`crate::PAGE_SIZE`] as a `usize`.
const PAGE: usize = 4096;
const _: () = assert!(PAGE as u64 == crate::PAGE_SIZE);

/// Whole naturally aligned page blocks for the heap.
///
/// # Safety
///
/// [`PageSource::alloc_pages`] must return memory that is exclusively the
/// caller's until passed back to [`PageSource::free_pages`], readable and
/// writable, and aligned to its size (`PAGE_SIZE << order`).
pub unsafe trait PageSource {
    /// A block of `2^order` pages, or `None` when out of memory.
    fn alloc_pages(&self, order: Order) -> Option<NonNull<u8>>;

    /// Returns a block.
    ///
    /// # Safety
    ///
    /// `p` came from `alloc_pages(order)` and is not used afterwards.
    unsafe fn free_pages(&self, p: NonNull<u8>, order: Order);
}

#[derive(Clone, Copy, Debug)]
struct ClassSpec {
    size: usize,
    align: usize,
    order: Order,
}

impl ClassSpec {
    const fn new(size: usize) -> Self {
        // Objects are aligned to the largest power of two dividing their
        // size, which is what consecutive objects can guarantee.
        let align = 1 << size.trailing_zeros();
        let order = if size <= 512 { Order::ZERO } else { Order(2) };
        Self { size, align, order }
    }

    const fn slab_bytes(&self) -> usize {
        PAGE << self.order.0
    }

    const fn offset(&self) -> usize {
        size_of::<SlabHeader>().next_multiple_of(self.align)
    }

    const fn objects(&self) -> usize {
        (self.slab_bytes() - self.offset()) / self.size
    }
}

const CLASSES: [ClassSpec; 14] = [
    ClassSpec::new(16),
    ClassSpec::new(32),
    ClassSpec::new(48),
    ClassSpec::new(64),
    ClassSpec::new(96),
    ClassSpec::new(128),
    ClassSpec::new(192),
    ClassSpec::new(256),
    ClassSpec::new(384),
    ClassSpec::new(512),
    ClassSpec::new(768),
    ClassSpec::new(1024),
    ClassSpec::new(1536),
    ClassSpec::new(2048),
];
const NCLASSES: usize = CLASSES.len();

/// The largest request served from a size class.
pub const MAX_SMALL: usize = 2048;

/// The smallest class holding each 16-byte granule count, `0..=MAX_SMALL / 16`.
const CLASS_BY_GRANULE: [u8; MAX_SMALL / 16 + 1] = {
    let mut t = [0u8; MAX_SMALL / 16 + 1];
    let mut g = 0;
    while g < t.len() {
        let mut c = 0;
        while CLASSES[c].size < g * 16 {
            c += 1;
        }
        // Fourteen classes: the index fits.
        #[allow(clippy::cast_possible_truncation)]
        {
            t[g] = c as u8;
        }
        g += 1;
    }
    t
};

/// The class for `layout`, or `None` for a large allocation.
#[inline]
fn class_for(layout: Layout) -> Option<usize> {
    let size = layout.size();
    if size > MAX_SMALL {
        return None;
    }
    let c = usize::from(CLASS_BY_GRANULE[size.div_ceil(16)]);
    // Every class is at least 16-aligned; stricter alignment is rare.
    if layout.align() <= 16 {
        return Some(c);
    }
    CLASSES[c..]
        .iter()
        .position(|k| k.align >= layout.align())
        .map(|i| c + i)
}

#[repr(C)]
struct SlabHeader {
    next: *mut SlabHeader,
    prev: *mut SlabHeader,
    free: *mut u8,
    fresh: u32,
    inuse: u32,
}

const _: () = assert!(size_of::<SlabHeader>() == 32);

struct ClassState {
    /// Slabs with at least one free object.
    available: *mut SlabHeader,
    empty: usize,
    slabs: usize,
    /// Objects out of slabs (held by callers or in magazines).
    used: u64,
}

// SAFETY: the slab pointers are owned by the class and only followed under
// its lock.
unsafe impl Send for ClassState {}

/// The most objects a magazine page holds.
const MAG_SLOTS: usize = (PAGE - 2 * size_of::<usize>()) / size_of::<*mut u8>();

/// A magazine: a stack of free objects of one class, in its own page.
#[repr(C)]
struct Mag {
    /// The next magazine on a depot list.
    next: *mut Mag,
    count: usize,
    objects: [*mut u8; MAG_SLOTS],
}

const _: () = assert!(size_of::<Mag>() <= PAGE);

/// Objects a magazine of class `c` holds: about 32 KiB worth.
const fn capacity(c: usize) -> usize {
    let n = 32 * 1024 / CLASSES[c].size;
    if n < 16 {
        16
    } else if n > 256 {
        256
    } else {
        n
    }
}

/// One CPU's magazines for one class (null until first needed).
struct CpuMags {
    loaded: *mut Mag,
    prev: *mut Mag,
    /// Objects in both, for [`Heap::stats`] from other CPUs.
    cached: AtomicUsize,
}

impl CpuMags {
    const fn new() -> Self {
        Self {
            loaded: ptr::null_mut(),
            prev: ptr::null_mut(),
            cached: AtomicUsize::new(0),
        }
    }
}

/// A class's spare magazines.
struct Depot {
    full: *mut Mag,
    nfull: usize,
    empty: *mut Mag,
}

// SAFETY: the magazines are owned by the depot and only touched under its
// lock.
unsafe impl Send for Depot {}

impl Depot {
    const fn new() -> Self {
        Self {
            full: ptr::null_mut(),
            nfull: 0,
            empty: ptr::null_mut(),
        }
    }

    /// # Safety
    ///
    /// `m` is a magazine no one else holds.
    unsafe fn push(list: &mut *mut Mag, m: *mut Mag) {
        // SAFETY: per the contract.
        unsafe { (*m).next = *list };
        *list = m;
    }

    fn pop(list: &mut *mut Mag) -> *mut Mag {
        let m = *list;
        if !m.is_null() {
            // SAFETY: magazines on a depot list are live and the depot's.
            *list = unsafe { (*m).next };
        }
        m
    }
}

fn mag_count(m: *mut Mag) -> usize {
    if m.is_null() {
        0
    } else {
        // SAFETY: a CPU's magazines are live and that CPU's (see callers).
        unsafe { (*m).count }
    }
}

/// Heap occupancy, for leak checks and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Small objects held by callers.
    pub live_objects: u64,
    /// Small objects cached in magazines (per-CPU and depot).
    pub cached_objects: u64,
    /// Pages holding magazines.
    pub magazine_pages: u64,
    /// Slabs, across all classes.
    pub slabs: u64,
    /// Pages in slabs.
    pub slab_pages: u64,
    /// Pages in large allocations.
    pub large_pages: u64,
}

/// The kernel heap over page source `S`, with magazines for `CPUS` CPUs
/// (zero disables them).
pub struct Heap<S, const CPUS: usize> {
    source: S,
    classes: [SpinLock<ClassState>; NCLASSES],
    depots: [SpinLock<Depot>; NCLASSES],
    magazines: [[UnsafeCell<CpuMags>; NCLASSES]; CPUS],
    magazine_pages: AtomicU64,
    large_pages: AtomicU64,
}

// SAFETY: class state is behind locks; a CPU's magazines are only touched
// by that CPU with interrupts masked (see `with_magazine`), and their
// counts, which others read for statistics, are atomics.
unsafe impl<S: Sync, const CPUS: usize> Sync for Heap<S, CPUS> {}
// SAFETY: the raw pointers inside are owned by the heap.
unsafe impl<S: Send, const CPUS: usize> Send for Heap<S, CPUS> {}

impl<S, const CPUS: usize> core::fmt::Debug for Heap<S, CPUS> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Heap")
            .field("cpus", &CPUS)
            .finish_non_exhaustive()
    }
}

impl<S: PageSource, const CPUS: usize> Heap<S, CPUS> {
    /// An empty heap.
    pub const fn new(source: S) -> Self {
        Self {
            source,
            classes: [const {
                SpinLock::new(ClassState {
                    available: ptr::null_mut(),
                    empty: 0,
                    slabs: 0,
                    used: 0,
                })
            }; NCLASSES],
            depots: [const { SpinLock::new(Depot::new()) }; NCLASSES],
            magazines: [const { [const { UnsafeCell::new(CpuMags::new()) }; NCLASSES] }; CPUS],
            magazine_pages: AtomicU64::new(0),
            large_pages: AtomicU64::new(0),
        }
    }

    /// The page source.
    pub const fn source(&self) -> &S {
        &self.source
    }

    /// Allocates memory for `layout`, or returns null.
    pub fn allocate(&self, layout: Layout) -> *mut u8 {
        match class_for(layout) {
            Some(c) => self.alloc_small(c),
            None => self.alloc_large(layout),
        }
    }

    /// Frees memory from [`Heap::allocate`].
    ///
    /// # Safety
    ///
    /// `p` came from `allocate(layout)` on this heap and is not used
    /// afterwards.
    pub unsafe fn deallocate(&self, p: *mut u8, layout: Layout) {
        match class_for(layout) {
            // SAFETY: per the caller's contract.
            Some(c) => unsafe { self.free_small(c, p) },
            // SAFETY: as above.
            None => unsafe { self.free_large(p, layout) },
        }
    }

    #[inline]
    fn alloc_small(&self, c: usize) -> *mut u8 {
        if CPUS == 0 || hal::interrupt::active() {
            return self.classes[c].lock().alloc(c, &self.source);
        }
        self.with_mags(c, |m| {
            let l = m.loaded;
            // SAFETY: this CPU's magazines are live and only this CPU's
            // (`with_mags`).
            unsafe {
                if !l.is_null() && (*l).count > 0 {
                    (*l).count -= 1;
                    return (*l).objects[(*l).count];
                }
            }
            self.alloc_refill(c, m)
        })
    }

    /// The loaded magazine is empty: swap in the previous one if it has
    /// objects, else trade the previous (empty) one for a full one from
    /// the depot, else fill from slabs.
    #[cold]
    fn alloc_refill(&self, c: usize, m: &mut CpuMags) -> *mut u8 {
        if mag_count(m.prev) > 0 {
            core::mem::swap(&mut m.loaded, &mut m.prev);
        } else {
            let mut d = self.depots[c].lock();
            let full = Depot::pop(&mut d.full);
            if !full.is_null() {
                d.nfull -= 1;
                if !m.prev.is_null() {
                    // SAFETY: an empty magazine of ours, handed over.
                    unsafe { Depot::push(&mut d.empty, m.prev) };
                }
                m.prev = m.loaded;
                m.loaded = full;
            } else {
                // No full magazine anywhere: take this object from a slab,
                // then fill a magazine half full for the next ones.
                if m.loaded.is_null() {
                    m.loaded = Depot::pop(&mut d.empty);
                }
                drop(d);
                let mut class = self.classes[c].lock();
                let first = class.alloc(c, &self.source);
                if first.is_null() {
                    return first;
                }
                if m.loaded.is_null() {
                    drop(class);
                    m.loaded = self.new_mag();
                    if m.loaded.is_null() {
                        return first;
                    }
                    class = self.classes[c].lock();
                }
                let l = m.loaded;
                // SAFETY: our (empty) magazine.
                unsafe {
                    while (*l).count < capacity(c) / 2 {
                        let p = class.alloc(c, &self.source);
                        if p.is_null() {
                            break;
                        }
                        (*l).objects[(*l).count] = p;
                        (*l).count += 1;
                    }
                }
                return first;
            }
        }
        let l = m.loaded;
        // SAFETY: our magazine.
        unsafe {
            if (*l).count == 0 {
                return ptr::null_mut();
            }
            (*l).count -= 1;
            (*l).objects[(*l).count]
        }
    }

    /// # Safety
    ///
    /// `p` is an object of class `c` from this heap.
    #[inline]
    unsafe fn free_small(&self, c: usize, p: *mut u8) {
        if CPUS == 0 || hal::interrupt::active() {
            // SAFETY: per the caller's contract.
            unsafe { self.classes[c].lock().free(c, p, &self.source) };
            return;
        }
        self.with_mags(c, |m| {
            let l = m.loaded;
            // SAFETY: our magazines (`with_mags`).
            unsafe {
                if !l.is_null() && (*l).count < capacity(c) {
                    (*l).objects[(*l).count] = p;
                    (*l).count += 1;
                    return;
                }
                self.free_flush(c, m, p);
            }
        });
    }

    /// The loaded magazine is full (or missing): swap in the previous one
    /// if it has room, else trade the previous (full) one to the depot for
    /// an empty one.
    ///
    /// # Safety
    ///
    /// As for [`Self::free_small`].
    #[cold]
    unsafe fn free_flush(&self, c: usize, m: &mut CpuMags, p: *mut u8) {
        if !m.prev.is_null() && mag_count(m.prev) < capacity(c) {
            core::mem::swap(&mut m.loaded, &mut m.prev);
        } else {
            let mut d = self.depots[c].lock();
            if !m.prev.is_null() {
                // SAFETY: a full magazine of ours, handed over.
                unsafe { Depot::push(&mut d.full, m.prev) };
                d.nfull += 1;
            }
            m.prev = m.loaded;
            m.loaded = Depot::pop(&mut d.empty);
            drop(d);
            if m.loaded.is_null() {
                m.loaded = self.new_mag();
            }
            if m.loaded.is_null() {
                // No page for a magazine: free straight to the slab.
                // SAFETY: per the caller's contract.
                unsafe { self.classes[c].lock().free(c, p, &self.source) };
                return;
            }
        }
        let l = m.loaded;
        // SAFETY: our magazine, with room.
        unsafe {
            (*l).objects[(*l).count] = p;
            (*l).count += 1;
        }
    }

    /// A fresh empty magazine page, or null.
    fn new_mag(&self) -> *mut Mag {
        let Some(page) = self.source.alloc_pages(Order::ZERO) else {
            return ptr::null_mut();
        };
        self.magazine_pages.fetch_add(1, Ordering::Relaxed);
        let m = page.as_ptr().cast::<Mag>();
        // SAFETY: the page is ours and big enough; only the header needs
        // initializing (slots beyond `count` are never read).
        unsafe {
            (&raw mut (*m).next).write(ptr::null_mut());
            (&raw mut (*m).count).write(0);
        }
        m
    }

    /// Runs `f` on this CPU's magazines for class `c` in a critical
    /// section, then publishes how many objects they hold. Not from
    /// interrupt context.
    #[inline]
    fn with_mags<R>(&self, c: usize, f: impl FnOnce(&mut CpuMags) -> R) -> R {
        hal::critical::enter();
        let cpu = hal::cpu_id().index();
        assert!(cpu < CPUS, "cpu{cpu} has no heap magazines");
        // SAFETY: only this CPU uses its magazines, from thread context; in
        // the critical section no other thread runs on this CPU until `f`
        // returns, and interrupt handlers bypass the magazines.
        let m = unsafe { &mut *self.magazines[cpu][c].get() };
        let r = f(m);
        m.cached
            .store(mag_count(m.loaded) + mag_count(m.prev), Ordering::Relaxed);
        hal::critical::exit();
        r
    }

    /// Returns this CPU's cached objects, and every full magazine in the
    /// depots, to their classes, and frees the magazines involved.
    pub fn drain_this_cpu(&self) {
        for c in 0..if CPUS == 0 { 0 } else { NCLASSES } {
            let mut d = self.depots[c].lock();
            let mut mags = core::mem::replace(&mut d.full, ptr::null_mut());
            d.nfull = 0;
            let mut empties = core::mem::replace(&mut d.empty, ptr::null_mut());
            drop(d);
            self.with_mags(c, |m| {
                for own in [&mut m.loaded, &mut m.prev] {
                    let x = core::mem::replace(own, ptr::null_mut());
                    if !x.is_null() {
                        // SAFETY: ours, now unreachable from the CPU.
                        unsafe { Depot::push(&mut mags, x) };
                    }
                }
            });
            let mut class = self.classes[c].lock();
            loop {
                let x = Depot::pop(&mut mags);
                if x.is_null() {
                    break;
                }
                // SAFETY: magazines hold only free objects of class `c`,
                // and these are no longer reachable by anyone else.
                unsafe {
                    let mag = &mut *x;
                    for &q in &mag.objects[..mag.count] {
                        class.free(c, q, &self.source);
                    }
                    mag.count = 0;
                    Depot::push(&mut empties, x);
                }
            }
            drop(class);
            loop {
                let x = Depot::pop(&mut empties);
                let Some(page) = NonNull::new(x.cast::<u8>()) else {
                    break;
                };
                self.magazine_pages.fetch_sub(1, Ordering::Relaxed);
                // SAFETY: the page came from `new_mag` and is unused now.
                unsafe { self.source.free_pages(page, Order::ZERO) };
            }
        }
    }

    fn alloc_large(&self, layout: Layout) -> *mut u8 {
        let Some(order) = Order::for_bytes(layout.size().max(layout.align()) as u64) else {
            return ptr::null_mut();
        };
        match self.source.alloc_pages(order) {
            Some(p) => {
                // Relaxed: a statistic.
                self.large_pages.fetch_add(order.pages(), Ordering::Relaxed);
                p.as_ptr()
            }
            None => ptr::null_mut(),
        }
    }

    /// # Safety
    ///
    /// `p` came from `alloc_large(layout)`.
    unsafe fn free_large(&self, p: *mut u8, layout: Layout) {
        let (Some(order), Some(p)) = (
            Order::for_bytes(layout.size().max(layout.align()) as u64),
            NonNull::new(p),
        ) else {
            return;
        };
        self.large_pages.fetch_sub(order.pages(), Ordering::Relaxed);
        // SAFETY: per the caller's contract, `p` is a block of `order`.
        unsafe { self.source.free_pages(p, order) };
    }

    /// Current occupancy. Magazine counts of other CPUs are read without
    /// synchronization, so the split between live and cached objects is
    /// exact only when the heap is quiet.
    pub fn stats(&self) -> HeapStats {
        let mut s = HeapStats {
            large_pages: self.large_pages.load(Ordering::Relaxed),
            ..HeapStats::default()
        };
        for (c, spec) in CLASSES.iter().enumerate() {
            let class = self.classes[c].lock();
            s.live_objects += class.used;
            s.slabs += class.slabs as u64;
            s.slab_pages += (class.slabs as u64) << spec.order.0;
        }
        for cpu in &self.magazines {
            for m in cpu {
                // SAFETY: only the atomic count is read.
                let n = unsafe { &(*m.get()).cached }.load(Ordering::Relaxed);
                s.cached_objects += n as u64;
            }
        }
        for (c, d) in self.depots.iter().enumerate() {
            s.cached_objects += (d.lock().nfull * capacity(c)) as u64;
        }
        s.magazine_pages = self.magazine_pages.load(Ordering::Relaxed);
        s.live_objects -= s.cached_objects;
        s
    }
}

impl ClassState {
    fn alloc(&mut self, c: usize, source: &impl PageSource) -> *mut u8 {
        let spec = &CLASSES[c];
        if self.available.is_null() {
            let Some(page) = source.alloc_pages(spec.order) else {
                return ptr::null_mut();
            };
            let slab = page.as_ptr().cast::<SlabHeader>();
            // SAFETY: the source gave us the block exclusively; the header
            // fits at its (page-aligned) start.
            unsafe {
                slab.write(SlabHeader {
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                    free: ptr::null_mut(),
                    fresh: 0,
                    inuse: 0,
                });
            }
            self.available = slab;
            self.slabs += 1;
            self.empty += 1;
        }
        let slab = self.available;
        // SAFETY: slabs on the available list are live and ours (lock
        // held), and have at least one free object.
        unsafe {
            let h = &mut *slab;
            let obj = if h.free.is_null() {
                let i = h.fresh as usize;
                h.fresh += 1;
                slab.cast::<u8>().add(spec.offset() + i * spec.size)
            } else {
                let obj = h.free;
                h.free = obj.cast::<*mut u8>().read();
                obj
            };
            if h.inuse == 0 {
                self.empty -= 1;
            }
            h.inuse += 1;
            if h.inuse as usize == spec.objects() {
                self.unlink(slab);
            }
            self.used += 1;
            obj
        }
    }

    /// # Safety
    ///
    /// `p` is an allocated object of class `c` from this class's slabs.
    unsafe fn free(&mut self, c: usize, p: *mut u8, source: &impl PageSource) {
        let spec = &CLASSES[c];
        let slab = p
            .map_addr(|a| a & !(spec.slab_bytes() - 1))
            .cast::<SlabHeader>();
        // SAFETY: per the contract, `slab` is the live slab holding `p`,
        // and the class lock is held.
        unsafe {
            let h = &mut *slab;
            if h.inuse as usize == spec.objects() {
                self.push(slab);
            }
            p.cast::<*mut u8>().write(h.free);
            h.free = p;
            h.inuse -= 1;
            self.used -= 1;
            if h.inuse == 0 {
                if self.empty >= 1 {
                    self.unlink(slab);
                    self.slabs -= 1;
                    source.free_pages(NonNull::new_unchecked(slab.cast()), spec.order);
                } else {
                    self.empty += 1;
                }
            }
        }
    }

    /// # Safety
    ///
    /// `slab` is a live slab of this class, not on the list.
    unsafe fn push(&mut self, slab: *mut SlabHeader) {
        // SAFETY: per the contract; the lock is held.
        unsafe {
            (*slab).prev = ptr::null_mut();
            (*slab).next = self.available;
            if !self.available.is_null() {
                (*self.available).prev = slab;
            }
        }
        self.available = slab;
    }

    /// # Safety
    ///
    /// `slab` is on this class's list.
    unsafe fn unlink(&mut self, slab: *mut SlabHeader) {
        // SAFETY: per the contract; the lock is held.
        unsafe {
            let (next, prev) = ((*slab).next, (*slab).prev);
            if prev.is_null() {
                self.available = next;
            } else {
                (*prev).next = next;
            }
            if !next.is_null() {
                (*next).prev = prev;
            }
            (*slab).next = ptr::null_mut();
            (*slab).prev = ptr::null_mut();
        }
    }
}

// SAFETY: `allocate` returns memory valid for `layout` (slab objects are at
// least `layout.size()` and aligned to `layout.align()` by class choice;
// large blocks are aligned to their size, which is at least the alignment),
// and `deallocate` takes it back.
unsafe impl<S: PageSource + Sync, const CPUS: usize> GlobalAlloc for Heap<S, CPUS> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.allocate(layout)
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: GlobalAlloc's contract is ours.
        unsafe { self.deallocate(p, layout) }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::sync::Arc;
    use std::sync::atomic::AtomicI64;
    use std::vec::Vec;

    use super::*;

    /// Pages from the host allocator, counted.
    #[derive(Default)]
    struct HostPages {
        out: AtomicI64,
        limit: Option<i64>,
    }

    // SAFETY: std's allocator returns exclusive memory with the requested
    // alignment.
    unsafe impl PageSource for HostPages {
        fn alloc_pages(&self, order: Order) -> Option<NonNull<u8>> {
            let n = i64::try_from(order.pages()).unwrap();
            if self
                .limit
                .is_some_and(|l| self.out.load(Ordering::Relaxed) + n > l)
            {
                return None;
            }
            let bytes = usize::try_from(order.bytes()).unwrap();
            let layout = Layout::from_size_align(bytes, bytes).unwrap();
            // SAFETY: nonzero size.
            let p = NonNull::new(unsafe { std::alloc::alloc(layout) })?;
            self.out.fetch_add(n, Ordering::Relaxed);
            Some(p)
        }

        unsafe fn free_pages(&self, p: NonNull<u8>, order: Order) {
            let bytes = usize::try_from(order.bytes()).unwrap();
            let layout = Layout::from_size_align(bytes, bytes).unwrap();
            self.out
                .fetch_sub(i64::try_from(order.pages()).unwrap(), Ordering::Relaxed);
            // SAFETY: allocated above with this layout.
            unsafe { std::alloc::dealloc(p.as_ptr(), layout) };
        }
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn class_table_matches_search() {
        for size in 0..=MAX_SMALL + 1 {
            for align in [1, 2, 4, 8, 16, 32, 64, 256, 2048, 4096] {
                let l = Layout::from_size_align(size, align).unwrap();
                let want = CLASSES
                    .iter()
                    .position(|c| c.size >= size && c.align >= align);
                assert_eq!(class_for(l), want, "size {size} align {align}");
            }
        }
    }

    #[test]
    fn classes_are_sound() {
        for c in &CLASSES {
            assert!(c.objects() >= 2, "{c:?}");
            assert_eq!(c.size % c.align, 0);
            assert!(c.offset() + c.objects() * c.size <= c.slab_bytes());
        }
        assert_eq!(class_for(layout(1, 1)), Some(0));
        assert_eq!(class_for(layout(17, 8)), Some(1));
        assert_eq!(class_for(layout(40, 16)), Some(2));
        // 48 is only 16-aligned; a 32-aligned request skips it.
        assert_eq!(CLASSES[class_for(layout(40, 32)).unwrap()].size, 64);
        assert_eq!(class_for(layout(2049, 8)), None);
        assert_eq!(class_for(layout(64, 4096)), None);
    }

    fn exercise<const CPUS: usize>() {
        let heap: Heap<HostPages, CPUS> = Heap::new(HostPages::default());
        let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
        let mut rng = 0xdead_beef_u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for round in 0..30_000u32 {
            if next() % 5 < 3 || live.is_empty() {
                let size = match next() % 10 {
                    0 => 1 + usize::try_from(next() % 20_000).unwrap(),
                    _ => 1 + usize::try_from(next() % 600).unwrap(),
                };
                let align = 1 << (next() % 7);
                let l = layout(size, align);
                let p = heap.allocate(l);
                assert!(!p.is_null());
                assert_eq!(p.addr() % align, 0);
                let tag = u8::try_from(round % 251).unwrap();
                // SAFETY: freshly allocated for `size` bytes.
                unsafe { p.write_bytes(tag, size) };
                live.push((p, l, tag));
            } else {
                let k = usize::try_from(next()).unwrap() % live.len();
                let (p, l, tag) = live.swap_remove(k);
                // SAFETY: still allocated; nothing else wrote it.
                let bytes = unsafe { core::slice::from_raw_parts(p, l.size()) };
                assert!(bytes.iter().all(|&b| b == tag), "object was overwritten");
                // SAFETY: from `allocate(l)`.
                unsafe { heap.deallocate(p, l) };
            }
        }
        let small = live
            .iter()
            .filter(|(_, l, _)| class_for(*l).is_some())
            .count();
        assert_eq!(heap.stats().live_objects, small as u64);
        for (p, l, _) in live {
            // SAFETY: from `allocate(l)`.
            unsafe { heap.deallocate(p, l) };
        }
        heap.drain_this_cpu();
        let s = heap.stats();
        assert_eq!((s.live_objects, s.cached_objects, s.large_pages), (0, 0, 0));
        // At most one empty slab stays cached per class.
        assert!(s.slabs <= NCLASSES as u64, "{s:?}");
        let out = heap.source().out.load(Ordering::Relaxed);
        assert_eq!(s.magazine_pages, 0);
        assert_eq!(out, i64::try_from(s.slab_pages).unwrap());
    }

    #[test]
    fn churn_without_magazines() {
        exercise::<0>();
    }

    #[test]
    fn churn_with_magazines() {
        exercise::<1>();
    }

    #[test]
    fn out_of_memory_is_null() {
        let heap: Heap<HostPages, 1> = Heap::new(HostPages {
            limit: Some(2),
            ..HostPages::default()
        });
        assert!(heap.allocate(layout(3 * 4096, 8)).is_null());
        let mut got = Vec::new();
        loop {
            let p = heap.allocate(layout(1024, 8));
            if p.is_null() {
                break;
            }
            got.push(p);
        }
        // One four-page slab would exceed the two-page limit.
        assert!(got.is_empty());
        let p = heap.allocate(layout(64, 8));
        assert!(!p.is_null());
        // SAFETY: from `allocate`.
        unsafe { heap.deallocate(p, layout(64, 8)) };
    }

    #[test]
    fn threads_share_classes() {
        let heap: Arc<Heap<HostPages, 0>> = Arc::new(Heap::new(HostPages::default()));
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let heap = Arc::clone(&heap);
                std::thread::spawn(move || {
                    let mut v = Vec::new();
                    for i in 0..20_000usize {
                        let l = layout(8 + (i * 7 + t) % 300, 8);
                        let p = heap.allocate(l);
                        // SAFETY: allocated for at least 8 bytes.
                        unsafe { p.cast::<usize>().write(i) };
                        v.push((p, l, i));
                        if i % 3 == 0 {
                            let (p, l, i) = v.swap_remove(i % v.len());
                            // SAFETY: still allocated.
                            assert_eq!(unsafe { p.cast::<usize>().read() }, i);
                            // SAFETY: from `allocate(l)`.
                            unsafe { heap.deallocate(p, l) };
                        }
                    }
                    for (p, l, _) in v {
                        // SAFETY: from `allocate(l)`.
                        unsafe { heap.deallocate(p, l) };
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(heap.stats().live_objects, 0);
    }
}
