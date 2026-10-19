// SPDX-License-Identifier: BSD-2-Clause
//! DMA: memory devices read and write (docs/DRIVER-MODEL.md §8.4).
//!
//! FreeBSD's busdma model, with ownership in the types:
//!
//! - A [`DmaTag`] holds a device's constraints (address bits, alignment,
//!   boundary, segment size and count, coherency) and chooses a backend:
//!   **direct** when the device reaches every byte of RAM, **bounce** when
//!   it cannot (buffers out of its reach are copied through memory it can
//!   reach). An IOMMU backend comes in P8.
//! - [`DmaCoherent`] is memory shared for a binding's whole life (queues,
//!   rings), allocated within the device's reach and accessed only through
//!   volatile reads and writes of whole [`DeviceShared`] elements.
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
//! Behavior reference: sys/kern/subr_bus_dma.c, sys/x86/x86/busdma_bounce.c.

use alloc::boxed::Box;
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

/// A device's DMA tag.
#[derive(Clone, Copy)]
pub struct DmaTag {
    constraints: DmaConstraints,
    backend: Backend,
    mem: &'static dyn DmaMemory,
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
        }
    }

    /// A tag for a child with further `constraints`.
    #[must_use]
    pub fn derive(&self, constraints: &DmaConstraints, top_of_ram: u64) -> Self {
        Self::new(
            self.mem,
            self.constraints.intersect(constraints),
            top_of_ram,
        )
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
            .alloc(bytes, align, self.constraints.limit())
            .ok_or(DmaError::NoMemory)?;
        let va = self.mem.virt(pa);
        // SAFETY: freshly allocated, `bytes` long, ours alone.
        unsafe { va.as_ptr().write_bytes(0, to_usize(bytes)) };
        if self.constraints.coherency == Coherency::NonCoherent {
            self.mem.clean(va.as_ptr(), to_usize(bytes));
        }
        Ok(DmaCoherent {
            pa,
            va: va.cast(),
            n: n.max(1),
            bytes,
            mem: self.mem,
            _t: PhantomData,
        })
    }
}

/// `n` elements of `T` shared with a device for a binding's life.
pub struct DmaCoherent<T: DeviceShared> {
    pa: u64,
    va: NonNull<T>,
    n: usize,
    bytes: u64,
    mem: &'static dyn DmaMemory,
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
            .field("bus", &format_args!("{:#x}", self.pa))
            .field("len", &self.n)
            .finish()
    }
}

impl<T: DeviceShared> DmaCoherent<T> {
    /// The bus address of element 0 (direct and bounce tags allocate
    /// within reach, so it is the physical address).
    #[must_use]
    pub fn bus_addr(&self) -> u64 {
        self.pa
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
        // SAFETY: in bounds, aligned (page-aligned base, `T`'s natural
        // stride), and every bit pattern is a valid `T`.
        unsafe { self.va.as_ptr().add(i).read_volatile() }
    }

    /// Writes element `i`.
    ///
    /// # Panics
    ///
    /// If `i` is out of range.
    pub fn write(&self, i: usize, v: T) {
        assert!(i < self.n, "DMA index out of range");
        // SAFETY: as for `read`.
        unsafe { self.va.as_ptr().add(i).write_volatile(v) }
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
        // SAFETY: in bounds and aligned (checked by `at`); every bit
        // pattern is a valid `U`.
        unsafe { self.at::<U>(off).read_volatile() }
    }

    /// Writes a `U` at byte offset `off`.
    ///
    /// # Panics
    ///
    /// If `off` is misaligned for `U` or out of range.
    pub fn write_at<U: DeviceShared>(&self, off: usize, v: U) {
        // SAFETY: as for `read_at`.
        unsafe { self.at::<U>(off).write_volatile(v) }
    }
}

impl<T: DeviceShared> Drop for DmaCoherent<T> {
    fn drop(&mut self) {
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
/// kernel memory [`DmaMemory::phys`] can translate.
pub trait DmaBuf {
    /// The bytes.
    fn bytes(&self) -> &[u8];
    /// The bytes, writable.
    fn bytes_mut(&mut self) -> &mut [u8];
}

impl DmaBuf for alloc::boxed::Box<[u8]> {
    fn bytes(&self) -> &[u8] {
        self
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        self
    }
}

impl DmaBuf for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        self
    }
}

/// Bounce pages standing in for a buffer the device cannot reach.
struct Bounce {
    pa: u64,
    bytes: u64,
}

/// What a mapping needs beyond one segment: more segments, bounce pages,
/// or cache maintenance when it ends. Boxed, so the common mapping stays
/// small: moving a value over 64 bytes costs a `rep movs` here.
struct Spill {
    segments: Vec<Segment>,
    bounce: Option<Bounce>,
    mem: &'static dyn DmaMemory,
    non_coherent: bool,
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
            .field("len", &self.buf.bytes().len())
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
        match self.build::<D>(buf.bytes()) {
            Ok(map) => Ok(Mapped {
                buf,
                map,
                _dir: PhantomData,
            }),
            Err(e) => Err((buf, e)),
        }
    }

    fn build<D: Direction>(&self, bytes: &[u8]) -> Result<Map, DmaError> {
        let c = &self.constraints;
        let mut segments = Segments::new();
        // Direct when every byte is within reach and the start is aligned
        // (later ranges start on page boundaries, so alignment beyond a
        // page needs a single range). The buffer's physical ranges are
        // split into segments as they are found, page by page.
        let mut direct = true;
        let mut split_err = None;
        let mut range: Option<(u64, u64)> = None;
        let mut off = 0usize;
        while off < bytes.len() {
            let va = bytes[off..].as_ptr();
            let pa = self.mem.phys(va).ok_or(DmaError::BadBuffer)?;
            let in_page = to_usize(PAGE_SIZE - (pa % PAGE_SIZE));
            let n = in_page.min(bytes.len() - off);
            // A direct tag reaches all RAM (decided once, at creation, as
            // FreeBSD's BUS_DMA_COULD_BOUNCE is).
            let reach = self.backend == Backend::Direct || pa + n as u64 <= c.limit();
            if !reach || (off == 0 && pa & (c.align - 1) != 0) {
                direct = false;
                break;
            }
            range = match range {
                Some((p, l)) if p + l == pa => Some((p, l + n as u64)),
                Some(_) if c.align > PAGE_SIZE => {
                    direct = false;
                    break;
                }
                Some((p, l)) => {
                    // Keep looking on failure: an unreachable page later
                    // means bouncing, which may need fewer segments.
                    if split_err.is_none() {
                        split_err = self.split(p, l, &mut segments).err();
                    }
                    Some((pa, n as u64))
                }
                None => Some((pa, n as u64)),
            };
            off += n;
        }
        if !direct {
            return self.bounce::<D>(bytes);
        }
        if let Some(e) = split_err {
            return Err(e);
        }
        if let Some((p, l)) = range {
            self.split(p, l, &mut segments)?;
        }
        let non_coherent = c.coherency == Coherency::NonCoherent;
        if non_coherent {
            if D::READS {
                self.mem.clean(bytes.as_ptr(), bytes.len());
            } else {
                self.mem.invalidate(bytes.as_ptr(), bytes.len());
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
        }))
    }

    /// Bounce: one contiguous allocation within reach. Out of line, so the
    /// direct path stays small enough to inline into drivers.
    #[cold]
    #[inline(never)]
    fn bounce<D: Direction>(&self, bytes: &[u8]) -> Result<Map, DmaError> {
        let mem = self.mem;
        let c = &self.constraints;
        let len = bytes.len() as u64;
        let align = c.align.max(PAGE_SIZE);
        let alloc = len.next_multiple_of(align);
        let pa = mem
            .alloc(alloc, align, c.limit())
            .ok_or(DmaError::NoMemory)?;
        let bva = mem.virt(pa);
        if D::READS {
            // SAFETY: the bounce pages are ours and at least `len` long.
            unsafe {
                bva.as_ptr()
                    .copy_from_nonoverlapping(bytes.as_ptr(), bytes.len())
            };
        }
        if c.coherency == Coherency::NonCoherent {
            if D::READS {
                mem.clean(bva.as_ptr(), bytes.len());
            } else {
                mem.invalidate(bva.as_ptr(), bytes.len());
            }
        }
        let mut segments = Segments::new();
        if let Err(e) = self.split(pa, len, &mut segments) {
            // SAFETY: just allocated, unused.
            unsafe { mem.free(pa, alloc) };
            return Err(e);
        }
        Ok(self.spill(segments.into_vec(), Some(Bounce { pa, bytes: alloc })))
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
        let out = buf.bytes_mut();
        if let Some(b) = s.bounce {
            let bva = mem.virt(b.pa);
            if D::WRITES {
                if s.non_coherent {
                    mem.invalidate(bva.as_ptr(), out.len());
                }
                // SAFETY: the bounce pages hold at least `out.len()` bytes.
                unsafe {
                    out.as_mut_ptr()
                        .copy_from_nonoverlapping(bva.as_ptr(), out.len())
                };
            }
            // SAFETY: allocated in `bounce` with these values; the device
            // is done with them.
            unsafe { mem.free(b.pa, b.bytes) };
        } else if D::WRITES && s.non_coherent {
            mem.invalidate(out.as_ptr(), out.len());
        }
    }
}

/// Fixed-size buffers allocated within a device's reach in advance, for
/// steady-state I/O without allocation (receive buffers, PRP lists).
pub struct DmaPool {
    tag: DmaTag,
    size: u64,
    free: sync::SpinLock<Vec<u64>>,
    chunks: Vec<(u64, u64)>,
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
}

impl core::fmt::Debug for PoolBuf<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PoolBuf({:#x})", self.pa)
    }
}

impl DmaPool {
    /// `count` buffers of `size` bytes (rounded up to the tag's alignment),
    /// carved from page-sized or larger chunks within reach. A buffer never
    /// crosses a page unless it is larger than one.
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
            tag: *tag,
            size,
            free: sync::SpinLock::new(Vec::with_capacity(count)),
            chunks: Vec::new(),
        };
        let mut made = 0;
        while made < count {
            let pa = tag
                .mem
                .alloc(chunk_bytes, align, tag.constraints.limit())
                .ok_or(DmaError::NoMemory)?;
            pool.chunks.push((pa, chunk_bytes));
            let mut free = pool.free.lock();
            for k in 0..per_chunk {
                if made == count {
                    break;
                }
                free.push(pa + k * size);
                made += 1;
            }
        }
        Ok(pool)
    }

    /// Takes a buffer, or `None` when all are in use.
    pub fn take(&self) -> Option<PoolBuf<'_>> {
        let pa = self.free.lock().pop()?;
        Some(PoolBuf { pool: self, pa })
    }

    /// The buffer size.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

impl Drop for DmaPool {
    fn drop(&mut self) {
        for &(pa, bytes) in &self.chunks {
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
        self.pa
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
        self.pool.free.lock().push(self.pa);
    }
}

#[cfg(test)]
mod tests;
