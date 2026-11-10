// SPDX-License-Identifier: BSD-2-Clause
//! Translated DMA end to end on the host (docs/IOMMU.md §7, §8 stage 3):
//! `vm::dma`'s IOMMU backend over `sys/iommu`'s domains over this driver's
//! unit over the SMMU model, all in one memory. The CPU side reaches it
//! as `DmaMemory` (so the domains' page-table writes are the kernel's
//! code), the model as `PhysMemory` (so its walks read those very
//! tables). Devices act through the model's `dma_read` and `dma_write`.
//!
//! The randomized test follows stage 1's strict-unmap test: maps and
//! unmaps through tags, checked against a reference after every step, so
//! a page the CPU has back is never reachable by the device.

extern crate std;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use bus::iommu::{Context, ContextRequest, Iommu, Want};
use iommu::event::{Access, Event, Record};
use iommu::model::{Config, Model, PhysMemory, Termination, Translation};
use iommu::regs::Features;
use iommu::{Iova, StreamId};
use pt::Cache;
use vm::dma::{
    Backend, DmaBuf, DmaConstraints, DmaError, DmaMemory, DmaPool, DmaTag, IoDomain, dir,
};

use crate::hw::Hw;
use crate::unit::Unit;
use crate::{Provider, Shared, event_line};

const PAGE: u64 = 4096;
/// Where the arena sits in the physical address space.
const BASE: u64 = 0x4000_0000;
/// Its size.
const SIZE: u64 = 64 << 20;

/// Memory both sides reach: the CPU by pointer, the SMMU model by
/// physical address. A first-fit page allocator hands it out.
#[derive(Debug)]
struct Arena {
    base: usize,
    used: Mutex<Vec<bool>>,
}

impl Arena {
    fn new() -> &'static Self {
        let mem = vec![0u8; usize::try_from(SIZE + PAGE).unwrap()].leak();
        let base = (mem.as_mut_ptr() as usize).next_multiple_of(4096);
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Self {
            base,
            used: Mutex::new(vec![false; usize::try_from(SIZE / PAGE).unwrap()]),
        }))
    }

    fn ptr(&self, pa: u64) -> *mut u8 {
        assert!(
            (BASE..BASE + SIZE).contains(&pa),
            "{pa:#x} outside the arena"
        );
        (self.base + usize::try_from(pa - BASE).unwrap()) as *mut u8
    }

    fn pages_used(&self) -> usize {
        self.used.lock().unwrap().iter().filter(|&&u| u).count()
    }

    fn bytes(&self, pa: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0; len];
        // SAFETY: arena bytes; the test is single-threaded.
        unsafe { core::ptr::copy_nonoverlapping(self.ptr(pa), v.as_mut_ptr(), len) };
        v
    }

    fn fill(&self, pa: u64, data: &[u8]) {
        // SAFETY: as above.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr(pa), data.len()) };
    }
}

// SAFETY: allocations are disjoint pages of the leaked arena, below
// `below`, mapped at `virt`.
unsafe impl DmaMemory for Arena {
    fn alloc(&self, bytes: u64, align: u64, below: u64) -> Option<u64> {
        let pages = usize::try_from(bytes.div_ceil(PAGE)).ok()?.max(1);
        let step = usize::try_from((align / PAGE).max(1)).ok()?;
        let mut used = self.used.lock().unwrap();
        let top = usize::try_from(below.saturating_sub(BASE).min(SIZE) / PAGE).ok()?;
        let first = (0..top.saturating_sub(pages - 1))
            .step_by(step)
            .find(|&p| p + pages <= top && used[p..p + pages].iter().all(|&u| !u))?;
        used[first..first + pages]
            .iter_mut()
            .for_each(|u| *u = true);
        Some(BASE + first as u64 * PAGE)
    }

    unsafe fn free(&self, pa: u64, bytes: u64) {
        let mut used = self.used.lock().unwrap();
        let first = usize::try_from((pa - BASE) / PAGE).unwrap();
        for u in &mut used[first..first + usize::try_from(bytes.div_ceil(PAGE)).unwrap()] {
            assert!(*u, "double free at {pa:#x}");
            *u = false;
        }
    }

    fn virt(&self, pa: u64) -> NonNull<u8> {
        NonNull::new(self.ptr(pa)).unwrap()
    }

    fn phys(&self, va: *const u8) -> Option<u64> {
        let va = va as usize;
        (self.base..self.base + usize::try_from(SIZE).unwrap())
            .contains(&va)
            .then(|| BASE + (va - self.base) as u64)
    }

    fn clean(&self, _: *const u8, _: usize) {}
    fn invalidate(&self, _: *const u8, _: usize) {}
}

/// The arena as the model's memory.
struct ArenaMem(&'static Arena);

impl PhysMemory for ArenaMem {
    fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        if pa < BASE || pa + buf.len() as u64 > BASE + SIZE {
            return false;
        }
        buf.copy_from_slice(&self.0.bytes(pa, buf.len()));
        true
    }

    fn write(&mut self, pa: u64, data: &[u8]) -> bool {
        if pa < BASE || pa + data.len() as u64 > BASE + SIZE {
            return false;
        }
        self.0.fill(pa, data);
        true
    }
}

/// The unit's hardware: the model's registers, the arena's memory.
struct ArenaHw {
    m: Mutex<Model<ArenaMem>>,
    mem: &'static Arena,
    clock: AtomicU64,
}

impl Hw for ArenaHw {
    type Mem = u64;

    fn read32(&self, off: u32) -> u32 {
        self.m.lock().unwrap().read32(off)
    }

    fn write32(&self, off: u32, v: u32) {
        self.m.lock().unwrap().write32(off, v);
    }

    fn alloc(&self, bytes: u64, align: u64) -> Option<u64> {
        let pa = self
            .mem
            .alloc(bytes.next_multiple_of(PAGE), align.max(PAGE), u64::MAX)?;
        self.mem.fill(pa, &vec![0; usize::try_from(bytes).unwrap()]);
        Some(pa)
    }

    fn pa(&self, m: &u64) -> u64 {
        *m
    }

    fn load(&self, m: &u64, word: usize) -> u64 {
        let b = self.mem.bytes(m + 8 * word as u64, 8);
        u64::from_le_bytes(b.try_into().unwrap())
    }

    fn store(&self, m: &u64, word: usize, v: u64) {
        self.mem.fill(m + 8 * word as u64, &v.to_le_bytes());
    }

    fn load32(&self, m: &u64, off: usize) -> u32 {
        let b = self.mem.bytes(m + off as u64, 4);
        u32::from_le_bytes(b.try_into().unwrap())
    }

    fn write_barrier(&self) {}
    fn doorbell_barrier(&self) {}
    fn read_barrier(&self) {}

    fn now_ns(&self) -> u64 {
        self.clock.fetch_add(100, Ordering::Relaxed)
    }

    fn relax(&self) {
        self.clock.fetch_add(1_000, Ordering::Relaxed);
    }
}

/// A unit running over the model, with its provider.
struct Rig {
    mem: &'static Arena,
    shared: Arc<Shared<ArenaHw>>,
    provider: Provider<ArenaHw>,
}

impl Rig {
    fn new(cfg: Config) -> Self {
        let mem = Arena::new();
        let f = Features::decode(
            cfg.idr0.encode(),
            cfg.idr1.encode(),
            cfg.idr3,
            cfg.idr5.encode(),
            cfg.iidr.encode(),
            false,
        );
        let hw = ArenaHw {
            m: Mutex::new(Model::new(cfg, ArenaMem(mem))),
            mem,
            clock: AtomicU64::new(0),
        };
        let mut u = Unit::new(hw, f).expect("memory");
        u.enable().expect("enable");
        let shared = Shared::new(0, Some(u), Some(mem));
        Self {
            mem,
            provider: Provider(Arc::clone(&shared)),
            shared,
        }
    }

    fn model(&self) -> ModelGuard<'_> {
        ModelGuard {
            unit: self.shared.unit.lock(),
        }
    }

    fn context(&self, sid: u32, want: Want, reserved: &[(u64, u64)]) -> Context {
        self.provider
            .context(
                sid,
                &ContextRequest {
                    want,
                    name: "nvme0",
                    rid: u16::try_from(sid).unwrap(),
                    segment: 0,
                    coherent: true,
                    reserved,
                },
            )
            .expect("context")
    }

    fn domain(&self, sid: u32) -> Arc<dyn IoDomain> {
        match self.context(sid, Want::Translate, &[]) {
            Context::Translated(d) => d,
            Context::PassThrough => panic!("pass-through"),
        }
    }

    fn tag(&self, sid: u32, c: DmaConstraints) -> DmaTag {
        DmaTag::translated(self.mem, c, self.domain(sid))
    }

    /// A buffer of `len` bytes at `pa` (pages already taken), payload
    /// only or from the heap.
    fn buf(&self, pa: u64, len: usize, payload: bool) -> Buf {
        Buf {
            mem: self.mem,
            ranges: vec![(pa, len)],
            payload,
        }
    }

    fn take(&self, pages: u64) -> u64 {
        self.mem.alloc(pages * PAGE, PAGE, u64::MAX).expect("arena")
    }

    fn write(&self, sid: u32, iova: u64, data: &[u8]) -> Result<(), Termination> {
        self.model()
            .m()
            .dma_write(StreamId(sid), Iova::new(iova), data)
    }

    fn read(&self, sid: u32, iova: u64, len: usize) -> Result<Vec<u8>, Termination> {
        let mut b = vec![0; len];
        self.model()
            .m()
            .dma_read(StreamId(sid), Iova::new(iova), &mut b)?;
        Ok(b)
    }

    fn probe(&self, sid: u32, iova: u64, access: Access) -> Result<Translation, Termination> {
        self.model()
            .m()
            .translate(StreamId(sid), Iova::new(iova), access)
    }

    fn assert_clean(&self) {
        assert_eq!(
            self.model().m().violations(),
            &[],
            "architecture violations"
        );
    }
}

/// The unit locked, for the model behind it.
struct ModelGuard<'a> {
    unit: sync::SpinGuard<'a, Option<Unit<ArenaHw>>>,
}

impl ModelGuard<'_> {
    fn m(&self) -> MutexGuard<'_, Model<ArenaMem>> {
        self.unit.as_ref().unwrap().hw().m.lock().unwrap()
    }
}

/// A buffer in the arena: one or more ranges.
#[derive(Debug)]
struct Buf {
    mem: &'static Arena,
    ranges: Vec<(u64, usize)>,
    payload: bool,
}

// SAFETY: arena bytes the test reserved for this buffer.
unsafe impl DmaBuf for Buf {
    fn ranges(&self) -> usize {
        self.ranges.len()
    }
    fn range(&self, i: usize) -> (*mut u8, usize) {
        let (pa, len) = self.ranges[i];
        (self.mem.ptr(pa), len)
    }
    fn payload_only(&self) -> bool {
        self.payload
    }
}

fn coherent() -> Config {
    let mut c = Config::qemu();
    c.idr0.cohacc = true;
    c
}

fn fault_at(r: Result<Translation, Termination>, iova: u64) {
    match r {
        Err(Termination::Fault(Record::Translation(f))) => assert_eq!(f.addr.get(), iova),
        other => panic!("{iova:#x}: expected a translation fault, got {other:?}"),
    }
}

#[test]
fn a_translated_tag_maps_and_unmaps_strictly() {
    for cfg in [Config::board(), coherent()] {
        let r = Rig::new(cfg);
        let tag = r.tag(0x10, DmaConstraints::UNLIMITED);
        assert_eq!(tag.backend(), Backend::Iommu);
        // A payload buffer, mid-page: mapped in place, at the same offset.
        let pa = r.take(2);
        let buf = r.buf(pa + 0x100, 0x1200, true);
        let m = tag.map::<dir::FromDevice, _>(buf).unwrap();
        assert!(!m.bounced());
        let seg = m.segments()[0];
        assert_eq!(
            (m.segments().len(), seg.len, seg.bus % PAGE),
            (1, 0x1200, 0x100)
        );
        // Lowest first, past the guard page at 0.
        assert_eq!(seg.bus & !(PAGE - 1), PAGE);
        r.write(0x10, seg.bus, &[0xab; 0x1200]).unwrap();
        let buf = m.unmap();
        assert_eq!(r.mem.bytes(pa + 0x100, 0x1200), vec![0xab; 0x1200]);
        // Strict: gone from the device the moment the CPU has it back.
        fault_at(r.probe(0x10, seg.bus, Access::WRITE), seg.bus);
        fault_at(
            r.probe(0x10, seg.bus + 0x1000, Access::READ),
            seg.bus + 0x1000,
        );
        // The range is reused.
        let m = tag.map::<dir::ToDevice, _>(buf).unwrap();
        assert_eq!(m.segments()[0].bus, seg.bus);
        // Read-only to the device.
        assert_eq!(r.read(0x10, seg.bus, 4).unwrap(), vec![0xab; 4]);
        assert!(matches!(
            r.write(0x10, seg.bus, &[1]),
            Err(Termination::Fault(Record::Permission(_)))
        ));
        drop(m.unmap());
        r.assert_clean();
    }
}

#[test]
fn scattered_pages_are_one_segment() {
    let r = Rig::new(coherent());
    let tag = r.tag(0x10, DmaConstraints::UNLIMITED);
    let (a, b, c) = (r.take(1), r.take(3), r.take(1));
    // Three page-aligned runs that meet at page boundaries.
    let buf = Buf {
        mem: r.mem,
        ranges: vec![(a + 0x800, 0x800), (b + PAGE, 2 * 4096), (c, 0x10)],
        payload: true,
    };
    for (pa, len) in &buf.ranges {
        r.mem.fill(*pa, &vec![0x5a; *len]);
    }
    let m = tag.map::<dir::ToDevice, _>(buf).unwrap();
    assert_eq!(m.segments().len(), 1);
    let s = m.segments()[0];
    assert_eq!(s.len, 0x800 + 0x2000 + 0x10);
    assert_eq!(
        r.read(0x10, s.bus, usize::try_from(s.len).unwrap())
            .unwrap(),
        vec![0x5a; usize::try_from(s.len).unwrap()]
    );
    drop(m.unmap());
    // Pieces that do not meet at page boundaries are bounced.
    let buf = Buf {
        mem: r.mem,
        ranges: vec![(a, 0x10), (c + 8, 0x10)],
        payload: true,
    };
    let m = tag.map::<dir::ToDevice, _>(buf).unwrap();
    assert!(m.bounced());
    drop(m.unmap());
    r.assert_clean();
}

#[test]
fn heap_buffers_bounce_unless_whole_pages() {
    let r = Rig::new(coherent());
    let tag = r.tag(0x10, DmaConstraints::UNLIMITED);
    // Rule 2: a heap buffer sharing its page is bounced, through pages
    // the domain maps.
    let pa = r.take(1);
    r.mem.fill(pa + 0x40, &[7; 100]);
    let m = tag
        .map::<dir::ToDevice, _>(r.buf(pa + 0x40, 100, false))
        .unwrap();
    assert!(m.bounced());
    let s = m.segments()[0];
    assert_eq!(r.read(0x10, s.bus, 100).unwrap(), vec![7; 100]);
    match r.probe(0x10, s.bus, Access::READ) {
        Ok(Translation::Mapped { pa: out, .. }) => assert_ne!(out & !(PAGE - 1), pa),
        other => panic!("{other:?}"),
    }
    drop(m.unmap());
    // The bounce pages are given back and their IOVA unmapped.
    fault_at(r.probe(0x10, s.bus, Access::READ), s.bus);
    // Whole pages of its own are mapped in place.
    let pa = r.take(2);
    let m = tag
        .map::<dir::FromDevice, _>(r.buf(pa, 8192, false))
        .unwrap();
    assert!(!m.bounced());
    drop(m.unmap());
    // Reading into a bounced heap buffer copies back after the unmap.
    let pa = r.take(1);
    let m = tag
        .map::<dir::FromDevice, _>(r.buf(pa + 8, 64, false))
        .unwrap();
    assert!(m.bounced());
    r.write(0x10, m.segments()[0].bus, &[9; 64]).unwrap();
    drop(m.unmap());
    assert_eq!(r.mem.bytes(pa + 8, 64), vec![9; 64]);
    r.assert_clean();
}

#[test]
fn constraints_apply_to_bus_addresses() {
    let r = Rig::new(coherent());
    // The high region: a page above the 32 bits the tag allows, mapped
    // below them (the reason for all this, docs/IOMMU.md §1.1).
    let tag = r.tag(
        0x10,
        DmaConstraints {
            addr_bits: 32,
            align: 0x1_0000,
            max_segment: 0x1000,
            boundary: Some(0x2_0000),
            ..DmaConstraints::UNLIMITED
        },
    );
    let pa = r.take(4);
    let m = tag
        .map::<dir::ToDevice, _>(r.buf(pa, 0x3000, true))
        .unwrap();
    assert!(!m.bounced());
    let segs = m.segments();
    assert_eq!(segs.len(), 3, "{segs:?}");
    assert_eq!(segs[0].bus % 0x1_0000, 0);
    assert!(
        segs.iter()
            .all(|s| s.bus + s.len <= 1 << 32 && s.len <= 0x1000)
    );
    drop(m.unmap());
    // Too many segments: the mapping is undone.
    let few = r.tag(
        0x10,
        DmaConstraints {
            max_segment: 0x1000,
            max_segments: 1,
            ..DmaConstraints::UNLIMITED
        },
    );
    let before = r
        .shared
        .contexts
        .lock()
        .get(&0x10)
        .unwrap()
        .domain
        .clone()
        .unwrap()
        .entries();
    let (_, e) = few
        .map::<dir::ToDevice, _>(r.buf(pa, 0x2000, true))
        .unwrap_err();
    assert_eq!(e, DmaError::TooManySegments);
    let after = r
        .shared
        .contexts
        .lock()
        .get(&0x10)
        .unwrap()
        .domain
        .clone()
        .unwrap()
        .entries();
    assert_eq!(before, after);
    r.assert_clean();
}

#[test]
fn coherent_memory_and_pools_are_mapped_for_life() {
    let r = Rig::new(coherent());
    let tag = r.tag(0x20, DmaConstraints::UNLIMITED);
    let used = r.mem.pages_used();
    let ring = tag.coherent::<u32>(16).unwrap();
    let bus = ring.bus_addr();
    r.write(0x20, bus + 8, &5u32.to_le_bytes()).unwrap();
    assert_eq!(ring.read(2), 5);
    drop(ring);
    fault_at(r.probe(0x20, bus, Access::READ), bus);
    let pool = DmaPool::new(&tag, 512, 12).unwrap();
    let mut bufs: Vec<_> = (0..12).map(|_| pool.take().unwrap()).collect();
    for (i, b) in bufs.iter_mut().enumerate() {
        b.bytes_mut()[0] = u8::try_from(i).unwrap();
    }
    for (i, b) in bufs.iter().enumerate() {
        assert_eq!(
            r.read(0x20, b.bus_addr(), 1).unwrap()[0],
            u8::try_from(i).unwrap()
        );
    }
    let first = bufs[0].bus_addr();
    drop(bufs);
    drop(pool);
    fault_at(r.probe(0x20, first, Access::READ), first);
    drop(tag);
    // Everything but the domain's tables (kept for the context) is back.
    let tables = r.mem.pages_used() - used;
    assert!(tables <= 4, "{tables} pages kept");
    r.assert_clean();
}

#[test]
fn windows_are_never_handed_out_and_contexts_are_per_stream() {
    let r = Rig::new(coherent());
    // QEMU's 32-bit window, scaled down: everything from 64 KiB to 1 MiB.
    let reserved = [(0x1_0000u64, 0xf_0000u64)];
    let Context::Translated(d) = r.context(0x30, Want::Translate, &reserved) else {
        panic!("pass-through");
    };
    let tag = DmaTag::translated(r.mem, DmaConstraints::UNLIMITED, d);
    let pa = r.take(4);
    let mut maps = Vec::new();
    for _ in 0..12 {
        let m = tag
            .map::<dir::ToDevice, _>(r.buf(pa, 0x4000, true))
            .unwrap();
        let s = m.segments()[0];
        assert!(
            s.bus + s.len <= 0x1_0000 || s.bus >= 0x10_0000,
            "{s:?} in the window"
        );
        maps.push(m);
    }
    drop(maps);
    // The same stream again: the same domain; another stream: its own.
    let Context::Translated(again) = r.context(0x30, Want::Translate, &[]) else {
        panic!();
    };
    assert!(Arc::ptr_eq(&again, tag.domain().unwrap()));
    let other = r.domain(0x31);
    assert!(!Arc::ptr_eq(&other, &again));
    // A request for physical addresses gets them, the stream unchanged.
    assert!(matches!(
        r.context(0x30, Want::PassThrough, &[]),
        Context::PassThrough
    ));
    // A stream that first wants physical addresses bypasses.
    assert!(matches!(
        r.context(0x40, Want::PassThrough, &[]),
        Context::PassThrough
    ));
    assert!(matches!(
        r.context(0x40, Want::Translate, &[]),
        Context::PassThrough
    ));
    assert_eq!(
        r.probe(0x40, 0x1234_5000, Access::WRITE),
        Ok(Translation::Bypass(0x1234_5000))
    );
    // Released, a stream aborts again.
    r.shared.release(0x31).unwrap();
    assert!(matches!(
        r.probe(0x31, 0x1000, Access::READ),
        Err(Termination::Fault(Record::BadSte))
    ));
    r.assert_clean();
}

#[test]
fn the_doorbell_is_mapped_once() {
    let r = Rig::new(coherent());
    let d = r.domain(0x10);
    let a = d.doorbell(0x0e06_0040).unwrap();
    assert_eq!(a % PAGE, 0x40);
    assert_eq!(d.doorbell(0x0e06_0000).unwrap(), a - 0x40);
    assert_eq!(d.doorbell(0x0e07_0000), Err(DmaError::Unsatisfiable));
    match r.probe(0x10, a, Access::WRITE) {
        Ok(Translation::Mapped { pa, attrs, .. }) => {
            assert_eq!(pa, 0x0e06_0040);
            assert_eq!(attrs.cache, Cache::Device);
        }
        other => panic!("{other:?}"),
    }
    r.assert_clean();
}

#[test]
fn event_lines_name_the_device() {
    use iommu::event::Fault;
    let e = Event {
        sid: StreamId(0xc100),
        ssid: None,
        record: Record::Translation(Fault::walk(Iova::new(0x123_4000), Access::WRITE)),
    };
    assert_eq!(
        event_line("nvme0 (rid 0xc100)", &e),
        "nvme0 (rid 0xc100): translation fault at 0x1234000 (write)"
    );
    let e = Event {
        sid: StreamId(1),
        ssid: None,
        record: Record::BadSte,
    };
    assert_eq!(
        event_line("vtnet0 (rid 0x8)", &e),
        "vtnet0 (rid 0x8): invalid stream table entry"
    );
}

/// Random maps and unmaps through translated tags on three streams, as
/// drivers do them: after each step, every page a live mapping covers
/// translates to its buffer and every page of a range just unmapped
/// faults (no stale TLB entry), against a reference.
#[test]
fn random_maps_and_strict_unmaps_against_reference() {
    let mut seed = 0x5eed_1234_u64;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    for cfg in [Config::board(), coherent()] {
        let r = Rig::new(cfg);
        let sids = [0x100u32, 0x200, 0x8];
        let tags: Vec<DmaTag> = sids
            .iter()
            .map(|&s| r.tag(s, DmaConstraints::UNLIMITED))
            .collect();
        let pool: Vec<u64> = (0..24).map(|_| r.take(4)).collect();
        type Live = (usize, vm::dma::Mapped<Buf, dir::Bidirectional>, u64, u64);
        let mut live: Vec<Live> = Vec::new();
        let mut unmapped: Vec<(u32, u64, u64)> = Vec::new();
        for _ in 0..1500 {
            if !live.is_empty() && rnd(3) == 0 {
                let (t, m, _, _) =
                    live.swap_remove(usize::try_from(rnd(live.len() as u64)).unwrap());
                let s = m.segments()[0];
                drop(m.unmap());
                unmapped.push((
                    sids[t],
                    s.bus & !(PAGE - 1),
                    (s.bus + s.len).next_multiple_of(PAGE),
                ));
            } else if live.len() < 16 {
                let t = usize::try_from(rnd(3)).unwrap();
                let pa = pool[usize::try_from(rnd(pool.len() as u64)).unwrap()];
                let off = rnd(PAGE) & !7;
                let len = 1 + rnd(3 * PAGE - off);
                let m = tags[t]
                    .map::<dir::Bidirectional, _>(r.buf(
                        pa + off,
                        usize::try_from(len).unwrap(),
                        true,
                    ))
                    .unwrap();
                assert!(!m.bounced());
                live.push((t, m, pa + off, len));
            }
            // Every live mapping reaches its own bytes.
            for (t, m, pa, len) in &live {
                let s = m.segments()[0];
                assert_eq!(s.len, *len);
                for at in [0, *len - 1, rnd(*len)] {
                    match r.probe(sids[*t], s.bus + at, Access::WRITE) {
                        Ok(Translation::Mapped { pa: out, .. }) => assert_eq!(out, pa + at),
                        other => panic!("live {:#x}: {other:?}", s.bus + at),
                    }
                }
            }
            // Nothing unmapped is reachable, unless a live mapping of the
            // same stream took the range again.
            for &(sid, start, end) in &unmapped {
                let mut p = start;
                while p < end {
                    let reused = live.iter().any(|(t, m, _, _)| {
                        let s = m.segments()[0];
                        sids[*t] == sid && (s.bus & !(PAGE - 1)) <= p && p < s.bus + s.len
                    });
                    if !reused {
                        fault_at(r.probe(sid, p, Access::READ), p);
                    }
                    p += PAGE;
                }
            }
            unmapped.retain(|_| rnd(4) != 0);
        }
        drop(live);
        assert!(r.model().m().stats().tlb_hits > 100);
        r.assert_clean();
    }
}
