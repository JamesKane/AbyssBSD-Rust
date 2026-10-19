// SPDX-License-Identifier: BSD-2-Clause
#![allow(clippy::cast_possible_truncation)] // test arena sizes and indices are small

extern crate std;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::NonNull;
use std::sync::Mutex;

use super::*;

/// Fake physical memory: an arena whose byte `pa` is physical address `pa`,
/// with a first-fit page allocator and a log of cache operations.
struct Fake {
    base: usize,
    size: u64,
    used: Mutex<Vec<bool>>,
    log: Mutex<Vec<(&'static str, usize, usize)>>,
}

impl Fake {
    fn new(size: u64) -> &'static Self {
        let mem = vec![0u8; size as usize + PAGE_SIZE as usize].leak();
        let base = (mem.as_mut_ptr() as usize).next_multiple_of(PAGE_SIZE as usize);
        Box::leak(Box::new(Self {
            base,
            size,
            used: Mutex::new(vec![false; (size / PAGE_SIZE) as usize]),
            log: Mutex::new(Vec::new()),
        }))
    }

    fn pages_used(&self) -> usize {
        self.used.lock().unwrap().iter().filter(|&&u| u).count()
    }

    /// A buffer at physical address `pa` (pages already taken).
    fn buf_at(&'static self, pa: u64, len: usize) -> FakeBuf {
        FakeBuf { mem: self, pa, len }
    }

    fn take(&self, pa: u64, bytes: u64) {
        let mut used = self.used.lock().unwrap();
        for p in pa / PAGE_SIZE..(pa + bytes).div_ceil(PAGE_SIZE) {
            used[p as usize] = true;
        }
    }
}

// SAFETY: allocations are disjoint pages of the leaked arena.
unsafe impl DmaMemory for Fake {
    fn alloc(&self, bytes: u64, align: u64, below: u64) -> Option<u64> {
        let pages = (bytes / PAGE_SIZE) as usize;
        let step = (align / PAGE_SIZE).max(1) as usize;
        let mut used = self.used.lock().unwrap();
        let top = (below.min(self.size) / PAGE_SIZE) as usize;
        let first = (0..top.saturating_sub(pages - 1))
            .step_by(step)
            .find(|&p| p + pages <= top && used[p..p + pages].iter().all(|&u| !u))?;
        used[first..first + pages]
            .iter_mut()
            .for_each(|u| *u = true);
        Some(first as u64 * PAGE_SIZE)
    }

    unsafe fn free(&self, pa: u64, bytes: u64) {
        let mut used = self.used.lock().unwrap();
        for p in pa / PAGE_SIZE..(pa + bytes) / PAGE_SIZE {
            assert!(used[p as usize], "double free");
            used[p as usize] = false;
        }
    }

    fn virt(&self, pa: u64) -> NonNull<u8> {
        NonNull::new((self.base + pa as usize) as *mut u8).unwrap()
    }

    fn phys(&self, va: *const u8) -> Option<u64> {
        let va = va as usize;
        (self.base..self.base + self.size as usize)
            .contains(&va)
            .then(|| (va - self.base) as u64)
    }

    fn clean(&self, va: *const u8, len: usize) {
        self.log
            .lock()
            .unwrap()
            .push(("clean", va as usize - self.base, len));
    }

    fn invalidate(&self, va: *const u8, len: usize) {
        self.log
            .lock()
            .unwrap()
            .push(("invalidate", va as usize - self.base, len));
    }
}

/// A buffer living in fake physical memory.
struct FakeBuf {
    mem: &'static Fake,
    pa: u64,
    len: usize,
}

impl DmaBuf for FakeBuf {
    fn bytes(&self) -> &[u8] {
        // SAFETY: the test reserved these bytes of the arena.
        unsafe { core::slice::from_raw_parts(self.mem.virt(self.pa).as_ptr(), self.len) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, exclusively.
        unsafe { core::slice::from_raw_parts_mut(self.mem.virt(self.pa).as_ptr(), self.len) }
    }
}

const MIB: u64 = 1 << 20;

fn bits(addr_bits: u8) -> DmaConstraints {
    DmaConstraints {
        addr_bits,
        ..DmaConstraints::UNLIMITED
    }
}

regs::device_shared! {
    struct Entry { a: u32, b: u32 }
}

#[test]
fn constraints_combine() {
    let parent = DmaConstraints {
        addr_bits: 32,
        boundary: Some(1 << 32),
        ..DmaConstraints::UNLIMITED
    };
    let child = DmaConstraints {
        addr_bits: 64,
        align: 4096,
        boundary: Some(1 << 16),
        max_segments: 32,
        coherency: Coherency::NonCoherent,
        ..DmaConstraints::UNLIMITED
    };
    let c = parent.intersect(&child);
    assert_eq!(
        (c.addr_bits, c.align, c.boundary, c.max_segments),
        (32, 4096, Some(1 << 16), 32)
    );
    assert_eq!(c.coherency, Coherency::NonCoherent);
    assert_eq!(c.limit(), 1 << 32);
    assert_eq!(DmaConstraints::UNLIMITED.limit(), u64::MAX);
}

#[test]
fn backend_follows_reach() {
    let mem = Fake::new(8 * MIB);
    assert_eq!(
        DmaTag::new(mem, bits(32), 8 * MIB).backend(),
        Backend::Direct
    );
    assert_eq!(
        DmaTag::new(mem, bits(22), 8 * MIB).backend(),
        Backend::Bounce
    );
    let child = DmaTag::new(mem, bits(64), 8 * MIB).derive(&bits(20), 8 * MIB);
    assert_eq!(child.backend(), Backend::Bounce);
}

#[test]
fn coherent_memory() {
    let mem = Fake::new(8 * MIB);
    let tag = DmaTag::new(mem, bits(22), 8 * MIB);
    let before = mem.pages_used();
    {
        let ring: DmaCoherent<Entry> = tag.coherent(1024).unwrap();
        assert_eq!(ring.len(), 1024);
        assert!(ring.bus_addr() + 8 * 1024 <= 4 * MIB, "within reach");
        assert_eq!(ring.read(7).a, 0, "zeroed");
        ring.write(7, Entry { a: 1, b: 2 });
        assert_eq!(ring.read(7).b, 2);
        assert_eq!(mem.pages_used(), before + 2);
    }
    assert_eq!(mem.pages_used(), before, "freed on drop");
}

#[test]
#[should_panic(expected = "DMA index out of range")]
fn coherent_index_is_checked() {
    let mem = Fake::new(MIB);
    let ring: DmaCoherent<Entry> = DmaTag::new(mem, bits(64), MIB).coherent(4).unwrap();
    let _ = ring.read(4);
}

#[test]
fn coherent_byte_offsets() {
    let mem = Fake::new(MIB);
    let block: DmaCoherent<u8> = DmaTag::new(mem, bits(64), MIB).coherent(64).unwrap();
    block.write_at::<u64>(8, 0x1122_3344_5566_7788);
    block.write_at::<u16>(18, 0xabcd);
    assert_eq!(block.read_at::<u32>(8), 0x5566_7788);
    assert_eq!(block.read(18), 0xcd);
    assert_eq!(block.read_at::<u16>(18), 0xabcd);
    assert_eq!(block.read_at::<u64>(56), 0, "the last word is in range");
}

#[test]
#[should_panic(expected = "DMA offset out of range")]
fn coherent_offsets_are_checked() {
    let mem = Fake::new(MIB);
    let block: DmaCoherent<u8> = DmaTag::new(mem, bits(64), MIB).coherent(64).unwrap();
    let _ = block.read_at::<u32>(62);
}

#[test]
#[should_panic(expected = "DMA offset out of range")]
fn coherent_offsets_must_be_aligned() {
    let mem = Fake::new(MIB);
    let block: DmaCoherent<u8> = DmaTag::new(mem, bits(64), MIB).coherent(64).unwrap();
    block.write_at::<u16>(3, 1);
}

#[test]
fn direct_stream_segments() {
    let mem = Fake::new(8 * MIB);
    let c = DmaConstraints {
        max_segment: 4096,
        boundary: Some(8192),
        max_segments: 8,
        ..DmaConstraints::UNLIMITED
    };
    let tag = DmaTag::new(mem, c, 8 * MIB);
    // Three pages' worth starting mid-page: pieces of at most 4 KiB that
    // never cross an 8 KiB line (crossing a page line is fine).
    mem.take(MIB, 4 * PAGE_SIZE);
    let s = DmaStream::new(&tag, mem.buf_at(MIB + 512, 3 * 4096));
    let s = s.map::<dir::ToDevice>().unwrap();
    assert!(!s.bounced());
    let total: u64 = s.segments().iter().map(|g| g.len).sum();
    assert_eq!(total, 3 * 4096);
    assert_eq!(
        s.segments()[0],
        Segment {
            bus: MIB + 512,
            len: 4096
        }
    );
    assert_eq!(
        s.segments()[1],
        Segment {
            bus: MIB + 4608,
            len: 8192 - 4608
        },
        "stops at 8 KiB"
    );
    assert!(
        s.segments()
            .iter()
            .all(|g| g.len <= 4096 && g.bus / 8192 == (g.bus + g.len - 1) / 8192)
    );
    let s = s.unmap();
    // Too many segments: the stream comes back unmapped.
    let few = DmaTag::new(
        mem,
        DmaConstraints {
            max_segments: 2,
            ..c
        },
        8 * MIB,
    );
    let s = DmaStream::new(&few, s.into_inner());
    let (s, e) = s.map::<dir::FromDevice>().unwrap_err();
    assert_eq!(e, DmaError::TooManySegments);
    assert_eq!(s.buf().len, 3 * 4096);
}

#[test]
fn many_segments_spill_from_inline() {
    let mem = Fake::new(8 * MIB);
    let c = DmaConstraints {
        max_segment: 512,
        ..DmaConstraints::UNLIMITED
    };
    let tag = DmaTag::new(mem, c, 8 * MIB);
    mem.take(MIB, 2 * PAGE_SIZE);
    let s = DmaStream::new(&tag, mem.buf_at(MIB, 2 * 4096));
    let s = s.map::<dir::FromDevice>().unwrap();
    let want: Vec<Segment> = (0..16)
        .map(|i| Segment {
            bus: MIB + i * 512,
            len: 512,
        })
        .collect();
    assert_eq!(s.segments(), &want[..]);
    // Mapped again, from the inline storage up.
    let s = DmaStream::new(&tag, s.unmap().into_inner())
        .map::<dir::ToDevice>()
        .unwrap();
    assert_eq!(s.segments(), &want[..]);
}

#[test]
fn bounced_stream_round_trip() {
    let mem = Fake::new(8 * MIB);
    let tag = DmaTag::new(mem, bits(22), 8 * MIB);
    let before = mem.pages_used();
    // A buffer at 6 MiB is out of a 22-bit device's reach.
    mem.take(6 * MIB, PAGE_SIZE);
    let mut s = DmaStream::new(&tag, mem.buf_at(6 * MIB, 100));
    s.buf_mut().bytes_mut().copy_from_slice(&[7u8; 100]);
    let out = s.map::<dir::ToDevice>().unwrap();
    assert!(out.bounced());
    let seg = out.segments()[0];
    assert!(seg.bus + seg.len <= 4 * MIB);
    // SAFETY: the bounce page the device would read.
    let seen = unsafe { core::slice::from_raw_parts(mem.virt(seg.bus).as_ptr(), 100) };
    assert!(seen.iter().all(|&b| b == 7), "data copied out");
    let s = out.unmap();
    assert_eq!(mem.pages_used(), before + 1, "bounce page freed");

    // The device writes: data is copied back on unmap.
    let inn = s.map::<dir::FromDevice>().unwrap();
    let seg = inn.segments()[0];
    // SAFETY: the device's write into the bounce page.
    unsafe { mem.virt(seg.bus).as_ptr().write_bytes(9, 100) };
    let s = inn.unmap();
    assert!(s.buf().bytes().iter().all(|&b| b == 9));
}

#[test]
fn non_coherent_syncs() {
    let mem = Fake::new(4 * MIB);
    let tag = DmaTag::new(
        mem,
        DmaConstraints {
            coherency: Coherency::NonCoherent,
            ..DmaConstraints::UNLIMITED
        },
        4 * MIB,
    );
    mem.take(MIB, PAGE_SIZE);
    mem.log.lock().unwrap().clear();
    let s = DmaStream::new(&tag, mem.buf_at(MIB, 64));
    let s = s.map::<dir::ToDevice>().unwrap().unmap();
    let s = s.map::<dir::FromDevice>().unwrap().unmap();
    drop(s);
    let log = mem.log.lock().unwrap().clone();
    assert_eq!(
        log,
        [
            ("clean", MIB as usize, 64),
            ("invalidate", MIB as usize, 64),
            ("invalidate", MIB as usize, 64)
        ]
    );
}

#[test]
fn pools_recycle() {
    let mem = Fake::new(4 * MIB);
    let tag = DmaTag::new(mem, bits(22), 4 * MIB);
    let pool = DmaPool::new(&tag, 512, 10).unwrap();
    assert_eq!(mem.pages_used(), 2, "8 per page");
    let mut held: Vec<_> = (0..10).map(|_| pool.take().unwrap()).collect();
    assert!(pool.take().is_none());
    for b in &held {
        assert_eq!(
            b.bus_addr() / PAGE_SIZE,
            (b.bus_addr() + 511) / PAGE_SIZE,
            "no page crossing"
        );
    }
    held[0].bytes_mut()[0] = 1;
    assert_eq!(held[0].bytes()[0], 1);
    held.pop();
    assert!(pool.take().is_some());
    drop(held);
    drop(pool);
    assert_eq!(mem.pages_used(), 0);
}
