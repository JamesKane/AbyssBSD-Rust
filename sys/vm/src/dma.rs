// SPDX-License-Identifier: BSD-2-Clause
//! DMA: memory devices read and write (docs/DRIVER-MODEL.md §8.4).
//!
//! FreeBSD's busdma model, with ownership in the types:
//!
//! - A [`DmaTag`] holds a device's constraints (address bits, alignment,
//!   boundary, segment size and count, coherency) and chooses a backend:
//!   **direct** when the device reaches every byte of RAM, **bounce** when
//!   it cannot (buffers out of its reach are copied through memory it can
//!   reach), and **IOMMU** when a translation domain ([`IoDomain`]) maps
//!   its buffers at bus addresses of the domain's choosing
//!   (docs/IOMMU.md §4.5).
//! - [`DmaCoherent`] is memory shared for a binding's whole life (queues,
//!   rings), allocated within the device's reach and accessed only through
//!   volatile reads and writes of whole [`DeviceShared`] elements. For a
//!   device that does not snoop the CPU's caches, each access keeps the
//!   caches right itself: a read cleans and invalidates the element's lines
//!   first, so it sees what the device wrote, and a write is cleaned to the
//!   point of coherency after, so the device sees it (FreeBSD's USB stack
//!   does the same around each access, `usb_pc_cpu_invalidate` and
//!   `usb_pc_cpu_flush`). The memory stays cacheable; FreeBSD's busdma maps
//!   it uncached instead, which needs the direct map split. Memory both the
//!   CPU and the device write is therefore not supported on such devices.
//! - [`DmaTag::map`] hands an I/O buffer to the device as a [`Mapped`]:
//!   it does the pre-transfer sync (bounce copy, cache clean) and yields
//!   the bus segments; [`Mapped::unmap`] does the post-transfer sync and
//!   gives the buffer back. While the device owns a buffer the CPU has no
//!   reference to it, because the value moved, so a missed sync cannot
//!   compile. On coherent devices the cache syncs are no-ops. As in
//!   FreeBSD, the mapping refers to the tag's decisions instead of
//!   carrying the tag: the common mapping is the buffer and one segment.
//! - [`DmaPool`] keeps fixed-size buffers allocated and within reach, so
//!   steady-state I/O allocates nothing.
//!
//! Memory comes from the kernel through [`DmaMemory`]; this module holds
//! the policy, tested on the host against fake memory.
//!
//! Behavior reference: sys/kern/subr_bus_dma.c, sys/x86/x86/busdma_bounce.c,
//! sys/dev/iommu/busdma_iommu.c.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ptr::NonNull;

use regs::DeviceShared;

use crate::PAGE_SIZE;

/// Where DMA memory comes from, and how caches are kept in step. The
/// kernel implements it over the physical allocator and the direct map.
///
/// # Safety
///
/// `alloc` returns memory exclusively the caller's until freed, mapped at
/// `virt` (read and write), physically contiguous, and wholly below
/// `below`. `virt` and `phys` translate within allocated memory and stream
/// buffers. `clean` and `invalidate` operate on the given virtual range.
pub unsafe trait DmaMemory: Sync {
    /// `bytes` at `align`, below physical address `below`, or `None`.
    /// Callers pass a size that is a multiple of `align` (itself a
    /// multiple of the page size), so `free` can work from the size.
    fn alloc(&self, bytes: u64, align: u64, below: u64) -> Option<u64>;
    /// Frees memory from [`DmaMemory::alloc`].
    ///
    /// # Safety
    ///
    /// `pa` and `bytes` are from a matching `alloc`, and the memory is no
    /// longer used.
    unsafe fn free(&self, pa: u64, bytes: u64);
    /// The kernel virtual address of physical address `pa`.
    fn virt(&self, pa: u64) -> NonNull<u8>;
    /// The physical address of kernel virtual address `va`.
    fn phys(&self, va: *const u8) -> Option<u64>;
    /// A window `(va, len)` of kernel virtual memory mapped linearly from
    /// physical address 0 (a direct map), which mapping translates inline,
    /// as FreeBSD's `pmap_kextract` does; `phys` serves the rest.
    fn linear(&self) -> Option<(usize, usize)> {
        None
    }
    /// Writes back cache lines over `[va, va + len)` (before the device
    /// reads). Not called for coherent devices.
    fn clean(&self, va: *const u8, len: usize);
    /// Discards cache lines over `[va, va + len)` (before the CPU reads
    /// what the device wrote). Not called for coherent devices.
    fn invalidate(&self, va: *const u8, len: usize);
}

/// A byte count as `usize` (DMA lengths are far below `usize::MAX` on
/// every supported target).
fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Whether the device snoops CPU caches (ACPI `_CCA`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coherency {
    /// Cache-coherent: no cache maintenance.
    Coherent,
    /// Not coherent: streams are cleaned and invalidated around transfers.
    NonCoherent,
}

/// A device's DMA limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaConstraints {
    /// Address bits the device can drive (32, 48, 64, …).
    pub addr_bits: u8,
    /// Required alignment of each segment's start, a power of two.
    pub align: u64,
    /// A segment may not cross a multiple of this (a power of two), or
    /// `None`.
    pub boundary: Option<u64>,
    /// The largest segment.
    pub max_segment: u64,
    /// The most segments one mapping may use.
    pub max_segments: u16,
    /// Cache coherency.
    pub coherency: Coherency,
}

impl DmaConstraints {
    /// No limits a modern 64-bit, coherent device would have.
    pub const UNLIMITED: Self = Self {
        addr_bits: 64,
        align: 1,
        boundary: None,
        max_segment: u64::MAX,
        max_segments: u16::MAX,
        coherency: Coherency::Coherent,
    };

    /// The first physical address the device cannot reach.
    #[must_use]
    pub const fn limit(&self) -> u64 {
        if self.addr_bits >= 64 {
            u64::MAX
        } else {
            1u64 << self.addr_bits
        }
    }

    /// The constraints satisfying both `self` and `other` (a child device
    /// inherits its bus's limits).
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        let boundary = match (self.boundary, other.boundary) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        Self {
            addr_bits: self.addr_bits.min(other.addr_bits),
            align: self.align.max(other.align),
            boundary,
            max_segment: self.max_segment.min(other.max_segment),
            max_segments: self.max_segments.min(other.max_segments),
            coherency: if self.coherency == Coherency::NonCoherent
                || other.coherency == Coherency::NonCoherent
            {
                Coherency::NonCoherent
            } else {
                Coherency::Coherent
            },
        }
    }
}

/// How a tag maps memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// The device reaches all RAM: bus address = physical address.
    Direct,
    /// Some RAM is out of reach: such buffers are copied through bounce
    /// pages within reach.
    Bounce,
    /// An IOMMU translates the device's addresses: buffers are mapped at
    /// bus addresses (IOVAs) of its domain's choosing, and the tag's
    /// constraints apply to those (docs/IOMMU.md §4.5).
    Iommu,
}

/// A translated mapping: the domain's entry `[start, end)` (whole pages)
/// and the bus address of the buffer's first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoMapping {
    /// The entry's first page.
    pub start: u64,
    /// One past its last page.
    pub end: u64,
    /// The bus address of the first byte mapped.
    pub addr: u64,
    /// The bytes mapped from `addr`.
    pub len: u64,
}

/// A translation domain, as `vm::dma` sees it: what an IOMMU backend maps
/// through (docs/IOMMU.md §4.5; `sys/iommu` implements it). Its methods
/// never sleep: they run in submission paths and completion filters.
pub trait IoDomain: Send + Sync {
    /// The first physical address the domain cannot map (the unit's
    /// output size): memory for the device comes from below it.
    fn reach(&self) -> u64;
    /// Maps physical `ranges`, in order, at one range of bus addresses
    /// that `c` allows (below its limit, aligned, within its boundary
    /// when the buffer fits in one), read-only to the device unless
    /// `write`. Every range but the first starts on a page boundary and
    /// every range but the last ends on one, so the bytes are contiguous
    /// at the bus side.
    ///
    /// # Errors
    ///
    /// [`DmaError::NoMemory`] when the domain has no space or no memory
    /// for its tables.
    fn map(
        &self,
        ranges: &[(u64, u64)],
        write: bool,
        c: &DmaConstraints,
    ) -> Result<IoMapping, DmaError>;
    /// Unmaps `m`, invalidates the unit's cached translations, and waits
    /// for that before the range is reused (strict: when this returns,
    /// the device can no longer reach the pages).
    fn unmap(&self, m: &IoMapping);
    /// The bus address at which the MSI doorbell page holding `pa` is
    /// mapped in this domain, mapping it the first time (§4.8).
    ///
    /// # Errors
    ///
    /// [`DmaError::NoMemory`], or [`DmaError::Unsatisfiable`] for a
    /// second doorbell page.
    fn doorbell(&self, pa: u64) -> Result<u64, DmaError>;
}

/// Why DMA memory could not be provided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaError {
    /// No memory within the device's reach.
    NoMemory,
    /// The buffer needs more segments than the device allows.
    TooManySegments,
    /// The buffer is not in memory the kernel can translate.
    BadBuffer,
    /// The constraints cannot be met (for example alignment above a page
    /// for a stream buffer that is not aligned).
    Unsatisfiable,
}

/// One bus-address range of a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The address the device uses.
    pub bus: u64,
    /// Length in bytes.
    pub len: u64,
}

/// A mapping's segments. Most buffers are physically contiguous (heap
/// blocks are buddy blocks), so one segment is kept without allocating,
/// as FreeBSD's tags keep a preallocated segment array.
#[derive(Clone, Debug)]
enum Segments {
    None,
    One(Segment),
    Many(Vec<Segment>),
}

impl Segments {
    const fn new() -> Self {
        Self::None
    }

    fn as_slice(&self) -> &[Segment] {
        match self {
            Self::None => &[],
            Self::One(s) => core::slice::from_ref(s),
            Self::Many(v) => v,
        }
    }

    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn last_mut(&mut self) -> Option<&mut Segment> {
        match self {
            Self::None => None,
            Self::One(s) => Some(s),
            Self::Many(v) => v.last_mut(),
        }
    }

    fn push(&mut self, seg: Segment) {
        match self {
            Self::None => *self = Self::One(seg),
            Self::One(first) => *self = Self::Many(alloc::vec![*first, seg]),
            Self::Many(v) => v.push(seg),
        }
    }

    fn into_vec(self) -> Vec<Segment> {
        match self {
            Self::None => Vec::new(),
            Self::One(seg) => alloc::vec![seg],
            Self::Many(v) => v,
        }
    }
}

/// A device's DMA tag. A translated tag holds its domain (counted), so
/// tags are `Clone`, not `Copy`.
#[derive(Clone)]
pub struct DmaTag {
    constraints: DmaConstraints,
    backend: Backend,
    mem: &'static dyn DmaMemory,
    /// `mem.linear()`, or an empty window.
    linear: (usize, usize),
    /// The domain of an [`Backend::Iommu`] tag.
    domain: Option<Arc<dyn IoDomain>>,
}

impl core::fmt::Debug for DmaTag {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaTag")
            .field("constraints", &self.constraints)
            .field("backend", &self.backend)
            .finish()
    }
}

impl DmaTag {
    /// A tag for `constraints` on a machine whose RAM ends below
    /// `top_of_ram`.
    #[must_use]
    pub fn new(
        mem: &'static dyn DmaMemory,
        mut constraints: DmaConstraints,
        top_of_ram: u64,
    ) -> Self {
        // Powers of two, as documented, so mapping tests them with masks
        // (as FreeBSD's busdma does) rather than divisions: a stray value
        // is made stricter, never looser.
        constraints.align = constraints.align.max(1).next_power_of_two();
        constraints.boundary = constraints.boundary.map(|b| 1 << b.max(1).ilog2());
        let backend = if top_of_ram <= constraints.limit() {
            Backend::Direct
        } else {
            Backend::Bounce
        };
        Self {
            constraints,
            backend,
            mem,
            linear: mem.linear().unwrap_or((0, 0)),
            domain: None,
        }
    }

    /// A tag whose buffers `domain` maps (the IOMMU backend): `constraints`
    /// apply to bus addresses, and memory comes from anywhere the domain
    /// reaches (docs/IOMMU.md §4.5).
    #[must_use]
    pub fn translated(
        mem: &'static dyn DmaMemory,
        constraints: DmaConstraints,
        domain: Arc<dyn IoDomain>,
    ) -> Self {
        let mut t = Self::new(mem, constraints, 0);
        t.backend = Backend::Iommu;
        t.domain = Some(domain);
        t
    }

    /// The memory the tag allocates from.
    #[must_use]
    pub fn memory(&self) -> &'static dyn DmaMemory {
        self.mem
    }

    /// The domain of a translated tag.
    #[must_use]
    pub fn domain(&self) -> Option<&Arc<dyn IoDomain>> {
        self.domain.as_ref()
    }

    /// The first physical address memory for the device may not reach:
    /// the domain's reach when translated, the device's limit otherwise.
    fn alloc_limit(&self) -> u64 {
        match &self.domain {
            Some(d) => d.reach(),
            None => self.constraints.limit(),
        }
    }

    /// The physical address of `va`.
    #[inline]
    fn phys(&self, va: *const u8) -> Option<u64> {
        let (base, len) = self.linear;
        match (va as usize).checked_sub(base) {
            Some(off) if off < len => Some(off as u64),
            _ => self.mem.phys(va),
        }
    }

    /// A tag for a child with further `constraints` (translated if this
    /// one is).
    #[must_use]
    pub fn derive(&self, constraints: &DmaConstraints, top_of_ram: u64) -> Self {
        let c = self.constraints.intersect(constraints);
        match &self.domain {
            Some(d) => Self::translated(self.mem, c, Arc::clone(d)),
            None => Self::new(self.mem, c, top_of_ram),
        }
    }

    /// The effective constraints.
    #[must_use]
    pub fn constraints(&self) -> &DmaConstraints {
        &self.constraints
    }

    /// The backend chosen.
    #[must_use]
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Splits physical range `[pa, pa + len)` into segments the device
    /// accepts, appending them to `out`.
    fn split(&self, mut pa: u64, mut len: u64, out: &mut Segments) -> Result<(), DmaError> {
        let c = &self.constraints;
        while len > 0 {
            let mut n = len.min(c.max_segment.max(1));
            if let Some(b) = c.boundary {
                let to_boundary = b - (pa & (b - 1));
                n = n.min(to_boundary);
            }
            // Merge with the previous segment when contiguous and allowed.
            match out.last_mut() {
                Some(last)
                    if last.bus + last.len == pa
                        && last.len + n <= c.max_segment
                        && c.boundary
                            .is_none_or(|b| last.bus & !(b - 1) == (pa + n - 1) & !(b - 1)) =>
                {
                    last.len += n;
                }
                _ => {
                    if out.len() >= usize::from(c.max_segments) {
                        return Err(DmaError::TooManySegments);
                    }
                    out.push(Segment { bus: pa, len: n });
                }
            }
            pa += n;
            len -= n;
        }
        Ok(())
    }

    /// Allocates `n` elements of `T`, zeroed, for the binding's life.
    pub fn coherent<T: DeviceShared>(&self, n: usize) -> Result<DmaCoherent<T>, DmaError> {
        let align = self.constraints.align.max(PAGE_SIZE);
        // Sizes are multiples of the alignment, so a free can recompute
        // the block from the size alone.
        let bytes = ((size_of::<T>() * n.max(1)) as u64).next_multiple_of(align);
        // One allocation must also respect the boundary, if any.
        if self.constraints.boundary.is_some_and(|b| bytes > b) {
            return Err(DmaError::Unsatisfiable);
        }
        let pa = self
            .mem
            .alloc(bytes, align, self.alloc_limit())
            .ok_or(DmaError::NoMemory)?;
        let va = self.mem.virt(pa);
        // SAFETY: freshly allocated, `bytes` long, ours alone.
        unsafe { va.as_ptr().write_bytes(0, to_usize(bytes)) };
        if self.constraints.coherency == Coherency::NonCoherent {
            self.mem.clean(va.as_ptr(), to_usize(bytes));
        }
        // Translated: mapped once, for the memory's life (§4.5).
        let (bus, mapping) = match &self.domain {
            Some(d) => match d.map(&[(pa, bytes)], true, &self.constraints) {
                Ok(m) => (m.addr, Some((Arc::clone(d), m))),
                Err(e) => {
                    // SAFETY: just allocated, never handed out.
                    unsafe { self.mem.free(pa, bytes) };
                    return Err(e);
                }
            },
            None => (pa, None),
        };
        Ok(DmaCoherent {
            pa,
            bus,
            va: va.cast(),
            n: n.max(1),
            bytes,
            mem: self.mem,
            non_coherent: self.constraints.coherency == Coherency::NonCoherent,
            mapping,
            _t: PhantomData,
        })
    }
}

/// `n` elements of `T` shared with a device for a binding's life.
pub struct DmaCoherent<T: DeviceShared> {
    pa: u64,
    /// The address the device uses: `pa`, or its IOVA when translated.
    bus: u64,
    va: NonNull<T>,
    n: usize,
    bytes: u64,
    mem: &'static dyn DmaMemory,
    /// The device does not snoop: accesses maintain the caches.
    non_coherent: bool,
    /// The translated mapping, removed before the memory is freed.
    mapping: Option<(Arc<dyn IoDomain>, IoMapping)>,
    _t: PhantomData<T>,
}

// SAFETY: the memory is ours alone (the device aside) and only reached
// through volatile accesses of plain old data.
unsafe impl<T: DeviceShared> Send for DmaCoherent<T> {}
// SAFETY: as above; concurrent volatile accesses of POD are sound.
unsafe impl<T: DeviceShared> Sync for DmaCoherent<T> {}

impl<T: DeviceShared> core::fmt::Debug for DmaCoherent<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaCoherent")
            .field("bus", &format_args!("{:#x}", self.bus))
            .field("len", &self.n)
            .finish()
    }
}

impl<T: DeviceShared> DmaCoherent<T> {
    /// The bus address of element 0: the physical address for direct and
    /// bounce tags (which allocate within reach), the IOVA for a
    /// translated one.
    #[must_use]
    pub fn bus_addr(&self) -> u64 {
        self.bus
    }

    /// The number of elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.n
    }

    /// Whether there are no elements (never: at least one is allocated).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Reads element `i`. What a device wrote is untrusted: callers
    /// validate it.
    ///
    /// # Panics
    ///
    /// If `i` is out of range (an index computed from device data must be
    /// checked first).
    #[must_use]
    pub fn read(&self, i: usize) -> T {
        assert!(i < self.n, "DMA index out of range");
        let p = self.va.as_ptr().wrapping_add(i);
        self.for_cpu(p.cast(), size_of::<T>());
        // SAFETY: in bounds, aligned (page-aligned base, `T`'s natural
        // stride), and every bit pattern is a valid `T`.
        unsafe { p.read_volatile() }
    }

    /// Writes element `i`.
    ///
    /// # Panics
    ///
    /// If `i` is out of range.
    pub fn write(&self, i: usize, v: T) {
        assert!(i < self.n, "DMA index out of range");
        let p = self.va.as_ptr().wrapping_add(i);
        // SAFETY: as for `read`.
        unsafe { p.write_volatile(v) };
        self.for_device(p.cast(), size_of::<T>());
    }

    /// Before the CPU reads `[p, p + len)`: drops cached copies, so the
    /// read sees what a non-snooping device wrote.
    fn for_cpu(&self, p: *const u8, len: usize) {
        if self.non_coherent {
            self.mem.invalidate(p, len);
        }
    }

    /// After the CPU wrote `[p, p + len)`: writes it to memory, where a
    /// non-snooping device reads.
    fn for_device(&self, p: *const u8, len: usize) {
        if self.non_coherent {
            self.mem.clean(p, len);
        }
    }
}

impl<T: DeviceShared> DmaCoherent<T> {
    fn at<U: DeviceShared>(&self, off: usize) -> *mut U {
        assert!(
            off.is_multiple_of(align_of::<U>())
                && off
                    .checked_add(size_of::<U>())
                    .is_some_and(|end| end <= self.n * size_of::<T>()),
            "DMA offset out of range"
        );
        self.va.as_ptr().cast::<u8>().wrapping_add(off).cast()
    }

    /// Reads a `U` at byte offset `off`, for memory a device lays out in
    /// mixed types (a virtqueue's rings). Untrusted, as for `read`.
    ///
    /// # Panics
    ///
    /// If `off` is misaligned for `U` or out of range.
    #[must_use]
    pub fn read_at<U: DeviceShared>(&self, off: usize) -> U {
        let p = self.at::<U>(off);
        self.for_cpu(p.cast(), size_of::<U>());
        // SAFETY: in bounds and aligned (checked by `at`); every bit
        // pattern is a valid `U`.
        unsafe { p.read_volatile() }
    }

    /// Writes a `U` at byte offset `off`.
    ///
    /// # Panics
    ///
    /// If `off` is misaligned for `U` or out of range.
    pub fn write_at<U: DeviceShared>(&self, off: usize, v: U) {
        let p = self.at::<U>(off);
        // SAFETY: as for `read_at`.
        unsafe { p.write_volatile(v) };
        self.for_device(p.cast(), size_of::<U>());
    }
}

impl DmaCoherent<u8> {
    /// Copies the bytes at `off` into `dst`: what the device wrote, once
    /// it said it was done (untrusted, as for `read`).
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    pub fn copy_to_slice(&self, off: usize, dst: &mut [u8]) {
        let end = off.checked_add(dst.len()).expect("DMA range overflows");
        assert!(end <= self.n, "DMA range out of range");
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        self.for_cpu(self.va.as_ptr().wrapping_add(off), dst.len());
        // SAFETY: in bounds; the device is done with the range, and `u8`
        // has no invalid values.
        unsafe {
            core::ptr::copy_nonoverlapping(self.va.as_ptr().add(off), dst.as_mut_ptr(), dst.len())
        };
    }

    /// Copies `src` to the bytes at `off`, for the device to read.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    pub fn copy_from_slice(&self, off: usize, src: &[u8]) {
        let end = off.checked_add(src.len()).expect("DMA range overflows");
        assert!(end <= self.n, "DMA range out of range");
        // SAFETY: in bounds; the device does not own the range until it
        // is handed over, after the release fence below.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), self.va.as_ptr().add(off), src.len())
        };
        self.for_device(self.va.as_ptr().wrapping_add(off), src.len());
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    }
}

impl<T: DeviceShared> Drop for DmaCoherent<T> {
    fn drop(&mut self) {
        // Out of the device's reach before the memory goes back.
        if let Some((d, m)) = self.mapping.take() {
            d.unmap(&m);
        }
        // SAFETY: from `alloc` with these values; the binding (and so the
        // device's use of it) is over when the owner drops it.
        unsafe { self.mem.free(self.pa, self.bytes) };
    }
}

/// Transfer directions.
pub mod dir {
    /// The device reads the buffer.
    #[derive(Debug)]
    pub struct ToDevice;
    /// The device writes the buffer.
    #[derive(Debug)]
    pub struct FromDevice;
    /// Both.
    #[derive(Debug)]
    pub struct Bidirectional;
}

/// Directions, as values, for the sync rules.
pub trait Direction {
    /// The device reads the buffer.
    const READS: bool;
    /// The device writes the buffer.
    const WRITES: bool;
}
impl Direction for dir::ToDevice {
    const READS: bool = true;
    const WRITES: bool = false;
}
impl Direction for dir::FromDevice {
    const READS: bool = false;
    const WRITES: bool = true;
}
impl Direction for dir::Bidirectional {
    const READS: bool = true;
    const WRITES: bool = true;
}

/// A byte buffer a stream can carry: it lends its bytes, which lie in
/// kernel memory [`DmaMemory::phys`] can translate, as ranges in order
/// (one for a plain buffer; a gather of pages has one per page run). The
/// ranges are lent by address, not as references: a gather's pages may
/// be written by others while the device reads them (the bytes it sends
/// are then as they were at that moment).
///
/// # Safety
///
/// Each range is `len` bytes of kernel memory at `ptr`, readable while
/// the buffer lives, and writable through it while it is lent to the
/// device for reading into (the mapping holds the buffer meanwhile).
pub unsafe trait DmaBuf {
    /// How many ranges the bytes are in.
    fn ranges(&self) -> usize {
        1
    }
    /// Range `i` of the bytes: where, and how long.
    fn range(&self, i: usize) -> (*mut u8, usize);
    /// How many bytes in all.
    fn total(&self) -> usize {
        (0..self.ranges()).map(|i| self.range(i).1).sum()
    }
    /// Whether the bytes lie in memory that holds only device payload
    /// (buffer-cache pages, mbuf clusters): a translated tag maps such a
    /// buffer in place even where it shares its pages. A buffer from the
    /// general heap (the default) is bounced unless it fills whole pages
    /// (DRIVER-MODEL.md §8.4, rule 2; docs/IOMMU.md §13 decision 5).
    fn payload_only(&self) -> bool {
        false
    }
}

// SAFETY: the boxed bytes, owned.
unsafe impl DmaBuf for alloc::boxed::Box<[u8]> {
    fn range(&self, _: usize) -> (*mut u8, usize) {
        (self.as_ptr().cast_mut(), self.len())
    }
}

// SAFETY: the vector's bytes, owned.
unsafe impl DmaBuf for Vec<u8> {
    fn range(&self, _: usize) -> (*mut u8, usize) {
        (self.as_ptr().cast_mut(), self.len())
    }
}

/// Bounce pages standing in for a buffer the device cannot reach.
struct Bounce {
    pa: u64,
    bytes: u64,
}

/// What a mapping needs beyond one segment: more segments, bounce pages,
/// a translated mapping, or cache maintenance when it ends. Boxed, so the
/// common mapping stays small: moving a value over 64 bytes costs a
/// `rep movs` here.
struct Spill {
    segments: Vec<Segment>,
    bounce: Option<Bounce>,
    mem: &'static dyn DmaMemory,
    non_coherent: bool,
    /// The domain mapping of a translated tag, unmapped first when the
    /// transfer ends.
    iommu: Option<(Arc<dyn IoDomain>, IoMapping)>,
}

/// A mapping's bus side.
enum Map {
    /// One segment of a coherent device, no bounce pages: nothing to do
    /// when the transfer ends.
    One(Segment),
    Spill(Box<Spill>),
}

/// A buffer the device owns, for transfers in direction `D`, from
/// [`DmaTag::map`]. The CPU gets it back from [`Mapped::unmap`].
pub struct Mapped<B: DmaBuf, D: Direction> {
    buf: B,
    map: Map,
    _dir: PhantomData<D>,
}

impl<B: DmaBuf, D: Direction> core::fmt::Debug for Mapped<B, D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mapped")
            .field("len", &self.buf.total())
            .field("segments", &self.segments())
            .field("bounced", &self.bounced())
            .finish()
    }
}

impl DmaTag {
    /// Hands `buf` to the device for direction `D`: bounces it if the
    /// device cannot reach it, does the pre-transfer sync, and computes the
    /// segments. On error the buffer comes back.
    #[inline]
    pub fn map<D: Direction, B: DmaBuf>(&self, buf: B) -> Result<Mapped<B, D>, (B, DmaError)> {
        match self.build::<D, B>(&buf) {
            Ok(map) => Ok(Mapped {
                buf,
                map,
                _dir: PhantomData,
            }),
            Err(e) => Err((buf, e)),
        }
    }

    fn build<D: Direction, B: DmaBuf>(&self, buf: &B) -> Result<Map, DmaError> {
        if let Some(d) = &self.domain {
            return self.build_iommu::<D, B>(buf, d);
        }
        let c = &self.constraints;
        let mut segments = Segments::new();
        // Direct when every byte is within reach and the start is aligned
        // (later ranges start on page boundaries, so alignment beyond a
        // page needs a single range). The buffer's physical ranges are
        // split into segments as they are found, page by page, across its
        // ranges (adjacent ones join).
        let mut direct = true;
        let mut split_err = None;
        let mut range: Option<(u64, u64)> = None;
        'ranges: for i in 0..buf.ranges() {
            let (ptr, len) = buf.range(i);
            let mut off = 0usize;
            while off < len {
                let va = ptr.wrapping_add(off).cast_const();
                let pa = self.phys(va).ok_or(DmaError::BadBuffer)?;
                let in_page = to_usize(PAGE_SIZE - (pa % PAGE_SIZE));
                let n = in_page.min(len - off);
                let n64 = u64::try_from(n).unwrap_or(u64::MAX);
                // A direct tag reaches all RAM (decided once, at creation,
                // as FreeBSD's BUS_DMA_COULD_BOUNCE is).
                let reach = self.backend == Backend::Direct || pa + n64 <= c.limit();
                if !reach || (range.is_none() && pa & (c.align - 1) != 0) {
                    direct = false;
                    break 'ranges;
                }
                range = match range {
                    Some((p, l)) if p + l == pa => Some((p, l + n64)),
                    Some(_) if c.align > PAGE_SIZE => {
                        direct = false;
                        break 'ranges;
                    }
                    Some((p, l)) => {
                        // Keep looking on failure: an unreachable page
                        // later means bouncing, which may need fewer
                        // segments.
                        if split_err.is_none() {
                            split_err = self.split(p, l, &mut segments).err();
                        }
                        Some((pa, n64))
                    }
                    None => Some((pa, n64)),
                };
                off += n;
            }
        }
        if !direct {
            return self.bounce::<D, B>(buf);
        }
        if let Some(e) = split_err {
            return Err(e);
        }
        if let Some((p, l)) = range {
            self.split(p, l, &mut segments)?;
        }
        let non_coherent = c.coherency == Coherency::NonCoherent;
        if non_coherent {
            for i in 0..buf.ranges() {
                let (ptr, len) = buf.range(i);
                if D::READS {
                    self.mem.clean(ptr, len);
                } else {
                    self.mem.invalidate(ptr, len);
                }
            }
        }
        Ok(match segments {
            Segments::One(seg) if !non_coherent => Map::One(seg),
            other => self.spill(other.into_vec(), None),
        })
    }

    fn spill(&self, segments: Vec<Segment>, bounce: Option<Bounce>) -> Map {
        Map::Spill(Box::new(Spill {
            segments,
            bounce,
            mem: self.mem,
            non_coherent: self.constraints.coherency == Coherency::NonCoherent,
            iommu: None,
        }))
    }

    /// The IOMMU backend's map: the buffer's physical pages at one range
    /// of bus addresses, as FreeBSD's `iommu_bus_dmamap_load_something`
    /// gives it (a scattered buffer is one segment when the constraints
    /// allow). Bounced instead, through pages the domain maps, when rule
    /// 2 says so, when a page is beyond the domain's reach, when the
    /// start is misaligned, or when the pieces do not meet at page
    /// boundaries.
    #[inline(never)]
    fn build_iommu<D: Direction, B: DmaBuf>(
        &self,
        buf: &B,
        domain: &Arc<dyn IoDomain>,
    ) -> Result<Map, DmaError> {
        let c = &self.constraints;
        let reach = domain.reach();
        let payload = buf.payload_only();
        let page = to_usize(PAGE_SIZE);
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        let mut in_place = true;
        'ranges: for i in 0..buf.ranges() {
            let (ptr, len) = buf.range(i);
            // Rule 2: heap memory only as whole pages of its own.
            if !payload
                && len > 0
                && (!(ptr as usize).is_multiple_of(page) || !len.is_multiple_of(page))
            {
                in_place = false;
                break;
            }
            let mut off = 0usize;
            while off < len {
                let va = ptr.wrapping_add(off).cast_const();
                let pa = self.phys(va).ok_or(DmaError::BadBuffer)?;
                let in_page = to_usize(PAGE_SIZE - (pa % PAGE_SIZE));
                let n = in_page.min(len - off);
                let n64 = u64::try_from(n).unwrap_or(u64::MAX);
                if pa.saturating_add(n64) > reach {
                    in_place = false;
                    break 'ranges;
                }
                match ranges.last_mut() {
                    Some((p, l)) if *p + *l == pa => *l += n64,
                    // The bus side is contiguous only if the pieces meet
                    // at page boundaries.
                    Some((p, l))
                        if (*p + *l).is_multiple_of(PAGE_SIZE) && pa.is_multiple_of(PAGE_SIZE) =>
                    {
                        ranges.push((pa, n64));
                    }
                    Some(_) => {
                        in_place = false;
                        break 'ranges;
                    }
                    None => {
                        // The start's alignment within its page is the
                        // bus address's; beyond a page the domain aligns
                        // a page-aligned start.
                        let misaligned = if c.align > PAGE_SIZE {
                            !pa.is_multiple_of(PAGE_SIZE)
                        } else {
                            pa & (c.align - 1) != 0
                        };
                        if misaligned {
                            in_place = false;
                            break 'ranges;
                        }
                        ranges.push((pa, n64));
                    }
                }
                off += n;
            }
        }
        if !in_place {
            return self.bounce::<D, B>(buf);
        }
        let mut segments = Segments::new();
        let total: u64 = ranges.iter().map(|r| r.1).sum();
        let mapping = if total == 0 {
            None
        } else {
            let m = domain.map(&ranges, D::WRITES, c)?;
            if let Err(e) = self.split(m.addr, total, &mut segments) {
                domain.unmap(&m);
                return Err(e);
            }
            Some((Arc::clone(domain), m))
        };
        let non_coherent = c.coherency == Coherency::NonCoherent;
        if non_coherent {
            for i in 0..buf.ranges() {
                let (ptr, len) = buf.range(i);
                if D::READS {
                    self.mem.clean(ptr, len);
                } else {
                    self.mem.invalidate(ptr, len);
                }
            }
        }
        Ok(Map::Spill(Box::new(Spill {
            segments: segments.into_vec(),
            bounce: None,
            mem: self.mem,
            non_coherent,
            iommu: mapping,
        })))
    }

    /// Bounce: one contiguous allocation within reach. Out of line, so the
    /// direct path stays small enough to inline into drivers.
    #[cold]
    #[inline(never)]
    fn bounce<D: Direction, B: DmaBuf>(&self, buf: &B) -> Result<Map, DmaError> {
        let mem = self.mem;
        let c = &self.constraints;
        let total = buf.total();
        let len = u64::try_from(total).unwrap_or(u64::MAX);
        let align = c.align.max(PAGE_SIZE);
        let alloc = len.next_multiple_of(align);
        let pa = mem
            .alloc(alloc, align, self.alloc_limit())
            .ok_or(DmaError::NoMemory)?;
        let bva = mem.virt(pa);
        if D::READS {
            let mut at = 0;
            for i in 0..buf.ranges() {
                let (ptr, n) = buf.range(i);
                // SAFETY: the bounce pages are ours and at least `len`
                // long, which the ranges add up to; the buffer lends `n`
                // readable bytes at `ptr`.
                unsafe { bva.as_ptr().add(at).copy_from_nonoverlapping(ptr, n) };
                at += n;
            }
        }
        if c.coherency == Coherency::NonCoherent {
            if D::READS {
                mem.clean(bva.as_ptr(), total);
            } else {
                mem.invalidate(bva.as_ptr(), total);
            }
        }
        let mut segments = Segments::new();
        // Translated, the bounce pages are mapped like any buffer (whole
        // pages of their own).
        let mapping = match &self.domain {
            Some(d) if len > 0 => match d.map(&[(pa, len)], D::WRITES, c) {
                Ok(m) => Some((Arc::clone(d), m)),
                Err(e) => {
                    // SAFETY: just allocated, unused.
                    unsafe { mem.free(pa, alloc) };
                    return Err(e);
                }
            },
            _ => None,
        };
        let bus = mapping.as_ref().map_or(pa, |(_, m)| m.addr);
        if let Err(e) = self.split(bus, len, &mut segments) {
            if let Some((d, m)) = &mapping {
                d.unmap(m);
            }
            // SAFETY: just allocated, unused.
            unsafe { mem.free(pa, alloc) };
            return Err(e);
        }
        let mut map = self.spill(segments.into_vec(), Some(Bounce { pa, bytes: alloc }));
        if let Map::Spill(s) = &mut map {
            s.iommu = mapping;
        }
        Ok(map)
    }
}

impl<B: DmaBuf, D: Direction> Mapped<B, D> {
    /// The bus segments to program into the device.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        match &self.map {
            Map::One(seg) => core::slice::from_ref(seg),
            Map::Spill(s) => &s.segments,
        }
    }

    /// Whether the buffer went through bounce pages.
    #[must_use]
    pub fn bounced(&self) -> bool {
        matches!(&self.map, Map::Spill(s) if s.bounce.is_some())
    }

    /// Takes the buffer back after the transfer: copies bounced data in
    /// (if the device wrote), does the post-transfer sync, and frees the
    /// bounce pages.
    #[inline]
    pub fn unmap(self) -> B {
        let Self { mut buf, map, .. } = self;
        if let Map::Spill(s) = map {
            Self::finish(&mut buf, *s);
        }
        buf
    }

    #[cold]
    fn finish(buf: &mut B, s: Spill) {
        let mem = s.mem;
        // Strict: out of the device's reach before the CPU looks.
        if let Some((d, m)) = &s.iommu {
            d.unmap(m);
        }
        if let Some(b) = s.bounce {
            let bva = mem.virt(b.pa);
            if D::WRITES {
                if s.non_coherent {
                    mem.invalidate(bva.as_ptr(), buf.total());
                }
                let mut at = 0;
                for i in 0..buf.ranges() {
                    let (ptr, n) = buf.range(i);
                    // SAFETY: the bounce pages hold every range's bytes,
                    // in order; the buffer lends `n` writable bytes at
                    // `ptr` while it is mapped for reading into.
                    unsafe { ptr.copy_from_nonoverlapping(bva.as_ptr().add(at), n) };
                    at += n;
                }
            }
            // SAFETY: allocated in `bounce` with these values; the device
            // is done with them.
            unsafe { mem.free(b.pa, b.bytes) };
        } else if D::WRITES && s.non_coherent {
            for i in 0..buf.ranges() {
                let (ptr, n) = buf.range(i);
                mem.invalidate(ptr, n);
            }
        }
    }
}

/// Fixed-size buffers allocated within a device's reach in advance, for
/// steady-state I/O without allocation (receive buffers, PRP lists).
pub struct DmaPool {
    tag: DmaTag,
    size: u64,
    /// Free buffers: physical and bus address.
    free: sync::SpinLock<Vec<(u64, u64)>>,
    /// The chunks: physical address, bytes, and the translated mapping.
    chunks: Vec<(u64, u64, Option<IoMapping>)>,
}

impl core::fmt::Debug for DmaPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaPool")
            .field("size", &self.size)
            .field("free", &self.free.lock().len())
            .finish()
    }
}

/// A buffer from a [`DmaPool`]; returns itself on drop.
pub struct PoolBuf<'p> {
    pool: &'p DmaPool,
    pa: u64,
    bus: u64,
}

impl core::fmt::Debug for PoolBuf<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PoolBuf({:#x})", self.pa)
    }
}

impl DmaPool {
    /// `count` buffers of `size` bytes (rounded up to the tag's alignment),
    /// carved from page-sized or larger chunks within reach. A buffer never
    /// crosses a page unless it is larger than one. Translated, each chunk
    /// is mapped once, for the pool's life (docs/IOMMU.md §4.5).
    pub fn new(tag: &DmaTag, size: u64, count: usize) -> Result<Self, DmaError> {
        let size = size.max(1).next_multiple_of(tag.constraints.align.max(8));
        let per_chunk = if size >= PAGE_SIZE {
            1
        } else {
            PAGE_SIZE / size
        };
        let align = tag.constraints.align.max(PAGE_SIZE);
        let chunk_bytes = if size >= PAGE_SIZE { size } else { PAGE_SIZE }.next_multiple_of(align);
        let mut pool = Self {
            tag: tag.clone(),
            size,
            free: sync::SpinLock::new(Vec::with_capacity(count)),
            chunks: Vec::new(),
        };
        let mut made = 0;
        while made < count {
            let pa = tag
                .mem
                .alloc(chunk_bytes, align, tag.alloc_limit())
                .ok_or(DmaError::NoMemory)?;
            let mapping = match &tag.domain {
                Some(d) => match d.map(&[(pa, chunk_bytes)], true, &tag.constraints) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        // SAFETY: just allocated, unused.
                        unsafe { tag.mem.free(pa, chunk_bytes) };
                        return Err(e);
                    }
                },
                None => None,
            };
            let bus = mapping.map_or(pa, |m| m.addr);
            pool.chunks.push((pa, chunk_bytes, mapping));
            let mut free = pool.free.lock();
            for k in 0..per_chunk {
                if made == count {
                    break;
                }
                free.push((pa + k * size, bus + k * size));
                made += 1;
            }
        }
        Ok(pool)
    }

    /// Takes a buffer, or `None` when all are in use.
    pub fn take(&self) -> Option<PoolBuf<'_>> {
        let (pa, bus) = self.free.lock().pop()?;
        Some(PoolBuf {
            pool: self,
            pa,
            bus,
        })
    }

    /// The buffer size.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

impl Drop for DmaPool {
    fn drop(&mut self) {
        for &(pa, bytes, mapping) in &self.chunks {
            if let (Some(d), Some(m)) = (&self.tag.domain, mapping) {
                d.unmap(&m);
            }
            // SAFETY: allocated in `new`; every PoolBuf borrows the pool,
            // so none outlives it.
            unsafe { self.tag.mem.free(pa, bytes) };
        }
    }
}

impl PoolBuf<'_> {
    /// The bus address.
    #[must_use]
    pub fn bus_addr(&self) -> u64 {
        self.bus
    }

    /// The buffer's bytes (the CPU's view between transfers).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        let va = self.pool.tag.mem.virt(self.pa);
        // SAFETY: a pool buffer of `size` bytes, ours while we hold it.
        unsafe { core::slice::from_raw_parts(va.as_ptr(), to_usize(self.pool.size)) }
    }

    /// The buffer's bytes, writable.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        let va = self.pool.tag.mem.virt(self.pa);
        // SAFETY: as above, and we hold it exclusively.
        unsafe { core::slice::from_raw_parts_mut(va.as_ptr(), to_usize(self.pool.size)) }
    }
}

impl Drop for PoolBuf<'_> {
    fn drop(&mut self) {
        self.pool.free.lock().push((self.pa, self.bus));
    }
}

#[cfg(test)]
mod tests;
