// SPDX-License-Identifier: BSD-2-Clause
//! Early memory management: the physical memory map, a boot-time frame
//! allocator, and the kernel's own page tables.
//!
//! The kernel tables replace the loader's: the DMAP covers every RAM
//! range, the image is mapped W^X with an unmapped guard page under the
//! boot stack, the console devices keep their device-window addresses, and
//! the identity map is gone. A second table maps the UEFI runtime regions
//! 1:1 for firmware calls.
//!
//! The frame allocator here hands out pages in address order and never
//! frees; the buddy allocator replaces it in Phase 2 (docs/ROADMAP.md).

use addr::{PhysAddr, VirtAddr};
use bootproto::layout::dmap;
use bootproto::{BootInfo, MemoryKind, MemoryRegion};
use pt::{Attrs, Frame, PageTable};
use sync::SpinLock;

use crate::arch;

type Format = arch::mmu::Format;

/// Frames below this are left alone (x86 real-mode trampolines later).
const LOW_RESERVE: u64 = 1 << 20;

/// Hands out free frames in address order.
#[derive(Debug)]
pub(crate) struct BootFrames {
    region: usize,
    next: u64,
    /// Frames handed out so far.
    pub(crate) used: u64,
}

impl BootFrames {
    const fn new() -> Self {
        Self {
            region: 0,
            next: 0,
            used: 0,
        }
    }

    fn take(&mut self) -> Option<PhysAddr> {
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
}

static FRAMES: SpinLock<BootFrames> = SpinLock::new(BootFrames::new());

struct Frames;

impl pt::Memory for Frames {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        let pa = FRAMES.lock().take()?;
        self.frame(pa).fill(0);
        Some(pa)
    }

    fn frame(&mut self, pa: PhysAddr) -> &mut Frame {
        let va = dmap(pa).expect("page-table frame outside the DMAP");
        // SAFETY: `pa` is a page-table frame (from `alloc_frame`, or the
        // root being copied), RAM in the DMAP, used only by the table
        // builder while it runs.
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

static KERNEL_ROOT: SpinLock<Option<PhysAddr>> = SpinLock::new(None);

/// Summarizes the memory map, builds the kernel tables, and switches to
/// them.
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

    let firmware = firmware_table(&mut mem, &kernel, map);

    // SAFETY: the new table maps the image (code, data, the boot stack),
    // the DMAP (boot info, page tables), and the console devices at the
    // addresses in use now.
    unsafe { arch::mmu::install_kernel_tables(kernel.root()) };
    *KERNEL_ROOT.lock() = Some(kernel.root());
    let used = FRAMES.lock().used;
    kprintln!("page tables: {} KiB in {used} frames", used * 4);
    Tables {
        firmware_root: firmware,
    }
}

/// Looks up `va` in the live kernel tables.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn translate(va: VirtAddr) -> Option<pt::Translation> {
    let root = (*KERNEL_ROOT.lock())?;
    // Rebuild a handle on the live root; `translate` only reads.
    PageTable::<Format>::from_root(root).translate(&mut Frames, va)
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
