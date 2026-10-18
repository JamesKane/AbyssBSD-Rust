// SPDX-License-Identifier: BSD-2-Clause
//! Memory management glue: the boot-time frame allocator, the kernel's own
//! page tables, and the hand-over to `sys/vm`'s buddy allocator and heap.
//!
//! The kernel tables replace the loader's: the DMAP covers every RAM
//! range, the image is mapped W^X with an unmapped guard page under the
//! boot stack, the console devices keep their device-window addresses, and
//! the identity map is gone. A second table maps the UEFI runtime regions
//! 1:1 for firmware calls.
//!
//! Until the buddy allocator exists, page-table frames come from
//! [`BootFrames`], which hands out free frames in address order. [`init`]
//! then carves the `VmPage` array out of free memory, gives every frame
//! nobody has used yet to the buddy allocator, and retires the boot
//! allocator.

pub(crate) mod dma;
mod heap;
pub(crate) mod kstack;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use addr::{PhysAddr, VirtAddr};
use bootproto::layout::{KVA_BASE, KVA_SIZE, dmap};
use bootproto::{BootInfo, MemoryKind, MemoryRegion};
use hal::MAX_CPUS;
use pt::{Attrs, Frame, PageTable};
use sync::SpinLock;
use vm::phys::{self, DomainId, PageMagazine, PhysAllocator, Segment, VmPage};
use vm::vmem::{Vmem, VmemError};
use vm::{Order, Pfn};

use crate::arch;

pub(crate) use heap::heap_stats;

type Format = arch::mmu::Format;

/// Frames below this are left alone (x86 real-mode trampolines later).
pub(crate) const LOW_RESERVE: u64 = 1 << 20;

/// Hands out free frames in address order until the buddy allocator takes
/// over.
#[derive(Debug)]
struct BootFrames {
    region: usize,
    next: u64,
    /// Frames handed out so far.
    used: u64,
    retired: bool,
}

impl BootFrames {
    const fn new() -> Self {
        Self {
            region: 0,
            next: 0,
            used: 0,
            retired: false,
        }
    }

    fn take(&mut self) -> Option<PhysAddr> {
        if self.retired {
            return None;
        }
        let map = crate::boot::memory_map();
        while let Some(r) = map.get(self.region) {
            let range = r.range();
            let start = self.next.max(range.start.get()).max(LOW_RESERVE);
            if r.kind == MemoryKind::Free && start + 4096 <= range.end.get() {
                self.next = start + 4096;
                self.used += 1;
                return Some(PhysAddr::new(start));
            }
            self.region += 1;
        }
        None
    }

    /// The part of free region `i` this allocator has not handed out.
    fn unused_part(&self, i: usize, r: &MemoryRegion) -> Option<(u64, u64)> {
        if r.kind != MemoryKind::Free || i < self.region {
            return None;
        }
        let range = r.range();
        let mut start = range.start.get().max(LOW_RESERVE);
        if i == self.region {
            start = start.max(self.next);
        }
        (start < range.end.get()).then_some((start, range.end.get()))
    }

    /// `pages` contiguous frames from the top of the highest free region
    /// that has room, outside what [`BootFrames::take`] handed out. Only
    /// valid as the last allocation before retiring.
    fn carve(&mut self, pages: u64) -> Option<PhysAddr> {
        let map = crate::boot::memory_map();
        let bytes = pages * 4096;
        map.iter().enumerate().rev().find_map(|(i, r)| {
            let (start, end) = self.unused_part(i, r)?;
            (end - start >= bytes).then(|| PhysAddr::new(end - bytes))
        })
    }
}

static FRAMES: SpinLock<BootFrames> = SpinLock::new(BootFrames::new());

/// The buddy allocator, once [`init`] has built it.
static PHYS: SpinLock<Option<PhysAllocator<'static>>> = SpinLock::new(None);

/// Each CPU's cache of single pages, touched only by that CPU, from thread
/// context, in a critical section.
struct PageCaches([UnsafeCell<PageMagazine>; MAX_CPUS]);

// SAFETY: see the type's comment: a CPU's slot is only used by that CPU,
// in a critical section (no other thread runs there meanwhile), and never
// from interrupt context (which could interrupt that use).
unsafe impl Sync for PageCaches {}

static PAGE_CACHES: PageCaches =
    PageCaches([const { UnsafeCell::new(PageMagazine::new()) }; MAX_CPUS]);
/// How many pages each CPU's cache holds (they count as free in
/// [`phys_stats`]). Written only by the owning CPU, on its own cache line.
#[repr(align(64))]
struct CachedCount(AtomicU64);
static CACHED_PAGES: [CachedCount; MAX_CPUS] = [const { CachedCount(AtomicU64::new(0)) }; MAX_CPUS];

/// Runs `f` on this CPU's page cache in a critical section, then
/// publishes the cache's size. Not from interrupt context.
fn with_page_cache<R>(f: impl FnOnce(&mut PageMagazine) -> R) -> R {
    hal::critical::enter();
    let cpu = hal::cpu_id().index();
    // SAFETY: this CPU's slot, in a critical section, from thread context
    // (see `PageCaches`).
    let m = unsafe { &mut *PAGE_CACHES.0[cpu].get() };
    let r = f(m);
    CACHED_PAGES[cpu].0.store(m.len() as u64, Ordering::Relaxed);
    hal::critical::exit();
    r
}

/// Allocates a block of `order` from the buddy allocator (single pages
/// through this CPU's cache).
pub(crate) fn alloc_pages(order: Order) -> Option<PhysAddr> {
    if order != Order::ZERO || hal::interrupt::active() {
        return PHYS
            .lock()
            .as_mut()?
            .alloc(order, DomainId(0))
            .map(Pfn::addr);
    }
    with_page_cache(|m| {
        if m.is_empty() {
            m.refill(PHYS.lock().as_mut()?, DomainId(0));
        }
        m.pop().map(Pfn::addr)
    })
}

/// Returns a block from [`alloc_pages`]. A bad free is a kernel bug.
pub(crate) fn free_pages(pa: PhysAddr, order: Order) {
    let pfn = Pfn::containing(pa);
    if order == Order::ZERO && !hal::interrupt::active() {
        let cached = with_page_cache(|m| {
            if m.push(pfn).is_ok() {
                return true;
            }
            let r = m.drain(
                PHYS.lock()
                    .as_mut()
                    .expect("free_pages before the buddy allocator"),
                false,
            );
            if let Err(e) = r {
                panic!("free_pages: {e}");
            }
            m.push(pfn).is_ok()
        });
        if cached {
            return;
        }
    }
    let mut phys = PHYS.lock();
    let phys = phys
        .as_mut()
        .expect("free_pages before the buddy allocator");
    if let Err(e) = phys.free(pfn, order) {
        panic!("free_pages: {e}");
    }
}

/// Frame counts from the buddy allocator; cached pages count as free.
pub(crate) fn phys_stats() -> phys::Stats {
    let mut s = PHYS
        .lock()
        .as_ref()
        .map(PhysAllocator::stats)
        .unwrap_or_default();
    s.free += CACHED_PAGES
        .iter()
        .map(|c| c.0.load(Ordering::Relaxed))
        .sum::<u64>();
    s
}

/// Page-table frames: from the boot allocator, then the buddy allocator.
struct Frames;

impl pt::Memory for Frames {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        let pa = match alloc_pages(Order::ZERO) {
            Some(pa) => pa,
            None => FRAMES.lock().take()?,
        };
        self.frame(pa).fill(0);
        Some(pa)
    }

    fn frame(&mut self, pa: PhysAddr) -> &mut Frame {
        let va = dmap(pa).expect("page-table frame outside the DMAP");
        // SAFETY: `pa` is a page-table frame (from `alloc_frame`, or a
        // root being read), RAM in the DMAP, used only by the table code
        // while it holds the kernel map lock (or during single-threaded
        // init).
        unsafe { &mut *(va.get() as *mut Frame) }
    }
}

unsafe extern "C" {
    static __kernel_start: u8;
    static __text_end: u8;
    static __rodata_end: u8;
    static __boot_stack_guard: u8;
    static __kernel_end: u8;
}

/// Kernel image section boundaries, at run time (slid) addresses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    pub(crate) start: u64,
    pub(crate) text_end: u64,
    pub(crate) rodata_end: u64,
    pub(crate) guard: u64,
    pub(crate) end: u64,
}

pub(crate) fn layout() -> Layout {
    Layout {
        start: (&raw const __kernel_start) as u64,
        text_end: (&raw const __text_end) as u64,
        rodata_end: (&raw const __rodata_end) as u64,
        guard: (&raw const __boot_stack_guard) as u64,
        end: (&raw const __kernel_end) as u64,
    }
}

/// The tables built by [`init`].
#[derive(Debug)]
pub(crate) struct Tables {
    /// The table firmware runtime calls run under.
    pub(crate) firmware_root: PhysAddr,
}

/// The live kernel page table. Changes to it (kernel stacks, `kmem`) go
/// through this lock.
static KERNEL_MAP: SpinLock<Option<PageTable<Format>>> = SpinLock::new(None);

/// Summarizes the memory map, builds the kernel tables, switches to them,
/// and starts the buddy allocator.
pub(crate) fn init(info: &BootInfo) -> Tables {
    let map = crate::boot::memory_map();
    let pages =
        |k: MemoryKind| -> u64 { map.iter().filter(|r| r.kind == k).map(|r| r.pages).sum() };
    let ram: u64 = map
        .iter()
        .filter(|r| r.kind.is_ram())
        .map(|r| r.pages)
        .sum();
    kprintln!(
        "memory: {} MiB RAM, {} MiB free, {} regions",
        (ram * 4096) >> 20,
        (pages(MemoryKind::Free) * 4096) >> 20,
        map.len()
    );

    let mut mem = Frames;
    let mut kernel =
        PageTable::<Format>::new(&mut mem, arch::mmu::max_leaf()).expect("kernel root");
    let mut map_or_die = |t: &mut PageTable<Format>, va: VirtAddr, pa: PhysAddr, len, a| {
        t.map(&mut mem, va, pa, len, a)
            .unwrap_or_else(|e| panic!("mapping {va:?} -> {pa:?} ({len:#x}): {e:?}"));
    };

    // The DMAP, coalescing adjacent RAM of any kind so large pages fit.
    for (start, end) in ram_ranges(map) {
        let va = dmap(start).expect("RAM beyond the DMAP");
        map_or_die(
            &mut kernel,
            va,
            start,
            end.get() - start.get(),
            Attrs::KERNEL_RW,
        );
    }

    // The image, W^X, with the guard page left out.
    let l = layout();
    let phys = |va: u64| info.kernel.phys + (va - info.kernel.virt.get());
    for (from, to, attrs) in [
        (l.start, l.text_end, Attrs::KERNEL_RX),
        (l.text_end, l.rodata_end, Attrs::KERNEL_RO),
        (l.rodata_end, l.guard, Attrs::KERNEL_RW),
        (l.guard + 4096, l.end, Attrs::KERNEL_RW),
    ] {
        if to > from {
            map_or_die(
                &mut kernel,
                VirtAddr::new(from),
                phys(from),
                to - from,
                attrs,
            );
        }
    }

    // The console devices, where the loader put them.
    if let Some(m) = info.serial.get().and_then(|s| match s {
        bootproto::Serial::Ns16550Mmio(m) | bootproto::Serial::Pl011(m) => Some(m),
        bootproto::Serial::Ns16550Port { .. } => None,
    }) {
        let (va, pa, len) = page_span(m.virt, m.phys, m.size);
        map_or_die(&mut kernel, va, pa, len, Attrs::DEVICE);
    }
    if let Some(f) = info.framebuffer.get() {
        let (va, pa, len) = page_span(f.virt, f.phys, f.size);
        map_or_die(&mut kernel, va, pa, len, Attrs::FRAMEBUFFER);
    }

    // Kernel virtual memory gets its top-level tables now, before the
    // firmware table copies the upper half.
    kernel
        .populate_root(&mut mem, VirtAddr::new(KVA_BASE), KVA_SIZE)
        .expect("KVA tables");

    let firmware = firmware_table(&mut mem, &kernel, map);

    // SAFETY: the new table maps the image (code, data, the boot stack),
    // the DMAP (boot info, page tables), and the console devices at the
    // addresses in use now.
    unsafe { arch::mmu::install_kernel_tables(kernel.root()) };
    *KERNEL_MAP.lock() = Some(kernel);
    let used = FRAMES.lock().used;
    kprintln!("page tables: {} KiB in {used} frames", used * 4);

    init_phys(map);
    Tables {
        firmware_root: firmware,
    }
}

/// Builds the buddy allocator over all RAM above [`LOW_RESERVE`] and hands
/// it every free frame the boot allocator has not used.
fn init_phys(map: &[MemoryRegion]) {
    let mut segs = [Segment {
        start: Pfn(0),
        end: Pfn(0),
        domain: DomainId(0),
    }; phys::MAX_SEGMENTS];
    let mut n = 0;
    for (start, end) in ram_ranges(map) {
        let start = Pfn::containing(PhysAddr::new(start.get().max(LOW_RESERVE)));
        let end = Pfn::containing(end);
        if start >= end {
            continue;
        }
        if n == segs.len() {
            kprintln!("phys: more than {n} RAM ranges; ignoring the rest");
            break;
        }
        segs[n] = Segment {
            start,
            end,
            domain: DomainId(0),
        };
        n += 1;
    }
    let segs = &segs[..n];
    let entries = phys::pages_needed(segs);
    let array_pages = (entries * size_of::<VmPage>() as u64).div_ceil(4096);

    let mut frames = FRAMES.lock();
    let array = frames
        .carve(array_pages)
        .expect("no room for the page array");
    let va = dmap(array).expect("page array outside the DMAP");
    // SAFETY: `carve` reserved these frames (free RAM in the DMAP) for the
    // array, and the boot allocator is retired below, so nothing else will
    // ever use them.
    let pages = unsafe {
        core::slice::from_raw_parts_mut(
            va.get() as *mut VmPage,
            usize::try_from(entries).expect("page array size"),
        )
    };
    let mut alloc = PhysAllocator::new(pages, segs).expect("buddy allocator setup");
    let carved = (array.get(), array.get() + array_pages * 4096);
    for (i, r) in map.iter().enumerate() {
        let Some((start, end)) = frames.unused_part(i, r) else {
            continue;
        };
        for (s, e) in [(start, end.min(carved.0)), (start.max(carved.1), end)] {
            if s < e {
                alloc.add_free(
                    Pfn::containing(PhysAddr::new(s)),
                    Pfn::containing(PhysAddr::new(e)),
                );
            }
        }
    }
    frames.retired = true;
    drop(frames);
    let stats = alloc.stats();
    kprintln!(
        "phys: {} MiB in {n} segments, {} MiB free, page array {} KiB",
        (stats.managed * 4096) >> 20,
        (stats.free * 4096) >> 20,
        array_pages * 4
    );
    *PHYS.lock() = Some(alloc);
}

/// Maps `[va, va + len)` to fresh frames in the kernel table, zeroed. On
/// failure nothing stays mapped.
pub(crate) fn map_fresh(va: VirtAddr, len: u64, attrs: Attrs) -> Result<(), pt::MapError> {
    let mut map = KERNEL_MAP.lock();
    let table = map.as_mut().expect("kernel map");
    let mut done = 0;
    while done < len {
        let r = Frames
            .alloc_frame_checked()
            .ok_or(pt::MapError::OutOfMemory)
            .and_then(|pa| {
                table
                    .map(&mut Frames, va + done, pa, 4096, attrs)
                    .inspect_err(|_| {
                        free_pages(pa, Order::ZERO);
                    })
            });
        if let Err(e) = r {
            unmap_locked(table, va, done);
            return Err(e);
        }
        done += 4096;
    }
    arch::mmu::sync_new_mappings();
    Ok(())
}

/// The kernel virtual memory arena: stacks and device mappings.
pub(crate) static KVA: SpinLock<Vmem<1024>> = SpinLock::new(Vmem::new(KVA_BASE, KVA_SIZE, 4096));

/// Why a device could not be mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceMapError {
    /// No kernel virtual memory.
    Address(VmemError),
    /// The page tables could not be extended.
    Map(pt::MapError),
}

/// Maps the device registers at `[pa, pa + len)` (uncached) into kernel
/// virtual memory for the life of the kernel, returning the address of
/// `pa`.
pub(crate) fn map_device(pa: PhysAddr, len: u64) -> Result<VirtAddr, DeviceMapError> {
    let off = pa.get() % 4096;
    let span =
        addr::align_up(len + off, 4096).ok_or(DeviceMapError::Map(pt::MapError::Misaligned))?;
    let va = VirtAddr::new(
        KVA.lock()
            .alloc(span, 4096)
            .map_err(DeviceMapError::Address)?,
    );
    let mut map = KERNEL_MAP.lock();
    let table = map.as_mut().expect("kernel map");
    if let Err(e) = table.map(&mut Frames, va, pa - off, span, Attrs::DEVICE) {
        // Mappings made before an out-of-memory failure stay, and the
        // address range is leaked with them rather than reused.
        return Err(DeviceMapError::Map(e));
    }
    arch::mmu::sync_new_mappings();
    Ok(va + off)
}

/// Unmaps `[va, va + len)` from the kernel table, removes the stale
/// translations from every CPU, and frees the frames. Call from thread
/// context with interrupts enabled (the shootdown waits for other CPUs).
pub(crate) fn unmap_and_free(va: VirtAddr, len: u64) {
    let mut freed = alloc::vec::Vec::new();
    {
        let mut map = KERNEL_MAP.lock();
        map.as_mut()
            .expect("kernel map")
            .unmap(&mut Frames, va, len, |pa| freed.push(pa))
            .unwrap_or_else(|e| panic!("unmapping {va:?}+{len:#x}: {e:?}"));
    }
    flush_range(va, len);
    crate::smp::shootdown(va, len);
    for pa in freed {
        free_pages(pa, Order::ZERO);
    }
}

/// Unmaps a range nobody has used yet (a failed `map_fresh`): no other
/// CPU can have cached it, so no shootdown.
fn unmap_locked(table: &mut PageTable<Format>, va: VirtAddr, len: u64) {
    table
        .unmap(&mut Frames, va, len, |pa| free_pages(pa, Order::ZERO))
        .unwrap_or_else(|e| panic!("unmapping {va:?}+{len:#x}: {e:?}"));
    flush_range(va, len);
}

/// Drops this CPU's translations for `[va, va + len)` (every CPU's on
/// aarch64, where TLBI broadcasts).
pub(crate) fn flush_range(va: VirtAddr, len: u64) {
    let mut off = 0;
    while off < len {
        arch::mmu::flush_page(va + off);
        off += 4096;
    }
}

/// The kernel page table's root, for loading on other CPUs.
pub(crate) fn kernel_root() -> PhysAddr {
    KERNEL_MAP.lock().as_ref().expect("kernel map").root()
}

/// A page table identity-mapping `[pa, pa + len)` (executable), for CPUs
/// turning their MMU on. Built from buddy frames; never freed.
#[cfg(target_arch = "aarch64")]
pub(crate) fn identity_table(pa: PhysAddr, len: u64) -> PhysAddr {
    let mut t =
        PageTable::<Format>::new(&mut Frames, arch::mmu::max_leaf()).expect("identity root");
    let start = pa.align_down(2 << 20);
    let end = addr::align_up(pa.get() + len, 2 << 20).expect("identity range");
    t.map(
        &mut Frames,
        VirtAddr::new(start.get()),
        start,
        end - start.get(),
        Attrs::IDENTITY,
    )
    .expect("identity map");
    t.root()
}

/// `n` contiguous free pages below 1 MiB (which the buddy allocator never
/// manages), for the x86 start-up trampoline. Reserved for good.
#[cfg(target_arch = "x86_64")]
pub(crate) fn low_pages(n: u64) -> Option<PhysAddr> {
    static TAKEN: SpinLock<u64> = SpinLock::new(0);
    let mut taken = TAKEN.lock();
    if *taken != 0 {
        return None;
    }
    let bytes = n * 4096;
    let found = crate::boot::memory_map().iter().rev().find_map(|r| {
        let range = r.range();
        let end = range.end.get().min(LOW_RESERVE);
        // Page 0 holds the real-mode IVT; keep away from it.
        let start = range.start.get().max(0x1000);
        (r.kind == MemoryKind::Free && end >= start + bytes).then(|| end - bytes)
    })?;
    *taken = found;
    Some(PhysAddr::new(found))
}

impl Frames {
    /// A zeroed frame from the buddy allocator only (no boot fallback).
    fn alloc_frame_checked(&mut self) -> Option<PhysAddr> {
        let pa = alloc_pages(Order::ZERO)?;
        pt::Memory::frame(self, pa).fill(0);
        Some(pa)
    }
}

/// Looks up `va` in the live kernel tables.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn translate(va: VirtAddr) -> Option<pt::Translation> {
    KERNEL_MAP.lock().as_ref()?.translate(&mut Frames, va)
}

/// The page-aligned span covering `[virt, virt + size)`.
fn page_span(virt: VirtAddr, phys: PhysAddr, size: u64) -> (VirtAddr, PhysAddr, u64) {
    let off = virt.get() % 4096;
    let len = addr::align_up(size + off, 4096).unwrap_or(4096);
    (virt - off, phys - off, len)
}

/// RAM ranges, adjacent regions merged.
fn ram_ranges(map: &[MemoryRegion]) -> impl Iterator<Item = (PhysAddr, PhysAddr)> + '_ {
    let mut it = map
        .iter()
        .filter(|r| r.kind.is_ram())
        .map(MemoryRegion::range)
        .peekable();
    core::iter::from_fn(move || {
        let first = it.next()?;
        let mut end = first.end;
        while let Some(next) = it.next_if(|n| n.start == end) {
            end = next.end;
        }
        Some((first.start, end))
    })
}

/// The table UEFI runtime calls run under: the runtime regions (and the
/// MMIO firmware needs at runtime) mapped 1:1, sharing the kernel's upper
/// half. On x86_64 that means copying the kernel's upper PML4 entries into
/// a second root; on aarch64 the upper half is TTBR1 and needs nothing.
fn firmware_table(mem: &mut Frames, kernel: &PageTable<Format>, map: &[MemoryRegion]) -> PhysAddr {
    use pt::Memory;
    let mut t = PageTable::<Format>::new(mem, arch::mmu::max_leaf()).expect("firmware root");
    if cfg!(target_arch = "x86_64") {
        let upper: Frame = *mem.frame(kernel.root());
        mem.frame(t.root())[256..].copy_from_slice(&upper[256..]);
    }
    const EFI_MEMORY_RUNTIME: u64 = 1 << 63;
    for r in map {
        let attrs = match r.kind {
            // Runtime code regions often mix code and data.
            MemoryKind::RuntimeCode => Attrs::IDENTITY,
            MemoryKind::RuntimeData => Attrs {
                exec: false,
                ..Attrs::IDENTITY
            },
            MemoryKind::Mmio | MemoryKind::Reserved
                if r.firmware_attributes & EFI_MEMORY_RUNTIME != 0 =>
            {
                Attrs {
                    global: false,
                    ..Attrs::DEVICE
                }
            }
            _ => continue,
        };
        let range = r.range();
        t.map(
            mem,
            VirtAddr::new(range.start.get()),
            range.start,
            range.len(),
            attrs,
        )
        .unwrap_or_else(|e| panic!("firmware mapping {range:?}: {e:?}"));
    }
    t.root()
}
