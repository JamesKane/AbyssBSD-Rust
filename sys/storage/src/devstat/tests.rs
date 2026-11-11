// SPDX-License-Identifier: BSD-2-Clause
extern crate std;

use std::sync::Mutex;
use std::vec::Vec;

use super::*;

/// The tests share the one region (as the kernel's entries do).
static SERIAL: Mutex<()> = Mutex::new(());
static FAKE_NOW: AtomicU64 = AtomicU64::new(0);

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let words: Vec<AtomicU64> = (0..4 * WORDS_PER_PAGE).map(|_| AtomicU64::new(0)).collect();
    init(
        Region {
            words: Vec::leak(words),
            pfn: 0x1234,
        },
        || FAKE_NOW.load(Ordering::Relaxed),
        |serial| 0xffff_f800_0000_0000 | serial << 8 | 1 << 3,
    );
    g
}

/// Entry `slot` as userland's copy of the pages has it.
fn read(slot: usize) -> Devstat {
    let mut b = [0u8; SIZE];
    for i in 0..WORDS {
        let w = cell(slot, i).unwrap().load(Ordering::Relaxed);
        b[i * 8..i * 8 + 8].copy_from_slice(&w.to_ne_bytes());
    }
    let u32_at = |o: usize| u32::from_ne_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_ne_bytes(b[o..o + 8].try_into().unwrap());
    let bt = |o: usize| Bintime {
        sec: u64_at(o).cast_signed(),
        frac: u64_at(o + 8),
    };
    Devstat {
        sequence0: u32_at(0),
        allocated: u32_at(4).cast_signed(),
        start_count: u32_at(8),
        end_count: u32_at(12),
        busy_from: bt(16),
        dev_links: u64_at(32),
        device_number: u32_at(40),
        device_name: b[44..60].try_into().unwrap(),
        unit_number: u32_at(60).cast_signed(),
        bytes: core::array::from_fn(|k| u64_at(64 + 8 * k)),
        operations: core::array::from_fn(|k| u64_at(96 + 8 * k)),
        duration: core::array::from_fn(|k| bt(128 + 16 * k)),
        busy_time: bt(192),
        creation_time: bt(208),
        block_size: u32_at(224),
        pad0: 0,
        tag_types: core::array::from_fn(|k| u64_at(232 + 8 * k)),
        flags: u32_at(256),
        device_type: u32_at(260),
        priority: u32_at(264),
        pad1: 0,
        id: u64_at(272),
        sequence1: u32_at(280),
        pad2: 0,
    }
}

#[test]
fn an_entry_reads_in_the_page_as_its_struct() {
    let _g = setup();
    FAKE_NOW.store(5_500_000_000, Ordering::Relaxed);
    let s = Stat::device("nda", 1, 512, Kind::NVME).unwrap();
    let d = read(s.slot);
    assert_eq!(d.allocated, 1);
    assert_eq!(&d.device_name[..4], b"nda\0");
    assert_eq!(d.unit_number, 1);
    assert_eq!(d.block_size, 512);
    assert_eq!(d.device_type, ty::DIRECT | ty::IF_NVME);
    assert_eq!(d.priority, priority::DISK);
    assert_eq!(d.creation_time.sec, 5);
    assert_eq!(d.creation_time.frac, 1 << 63);
    // A read of 4 KiB that took 2 ms, then a flush.
    s.start(6_000_000_000);
    let d = read(s.slot);
    assert_eq!((d.start_count, d.end_count), (1, 0));
    assert_eq!(d.busy_from.sec, 6);
    s.end(BioOp::Read, 4096, 6_000_000_000, 6_002_000_000);
    s.start(6_010_000_000);
    s.end(BioOp::Flush, 0, 6_010_000_000, 6_011_000_000);
    let d = read(s.slot);
    assert_eq!((d.start_count, d.end_count), (2, 2));
    assert_eq!(d.bytes[trans::READ], 4096);
    assert_eq!(d.operations[trans::READ], 1);
    assert_eq!(d.operations[trans::NO_DATA], 1);
    assert_eq!(to_ns(&d.duration[trans::READ]), 1_999_999);
    // Busy only while a request was out: 2 ms and 1 ms.
    let busy = to_ns(&d.busy_time);
    assert!((2_999_990..=3_000_000).contains(&busy), "{busy}");
    assert_eq!(d.sequence0, d.sequence1);
    assert_eq!(d.sequence0, 4);
    unlist(&s);
}

#[test]
fn the_list_is_by_priority_and_counts_and_frees() {
    let _g = setup();
    let before = (num_devs(), generation());
    let vt = Stat::device("vtbd", 0, 512, Kind::GEOM_DISK).unwrap();
    let nda = Stat::device("nda", 0, 512, Kind::GEOM_DISK).unwrap();
    nda.set_kind(Kind::NVME);
    let p = Stat::provider(0xabc).unwrap();
    assert_eq!(read(p.slot).unit_number, -1);
    assert_eq!(read(p.slot).id, 0xabc);
    assert_eq!(num_devs(), before.0 + 2);
    assert!(generation() > before.1);
    // `kern.devstat.all`: the generation, then the entries, highest
    // priority number first (`DEVSTAT_PRIORITY_MAX`, vtbd, before
    // `DEVSTAT_PRIORITY_DISK`, nda).
    let all = all();
    assert_eq!(
        all.len(),
        8 + 2 * SIZE + (num_devs() as usize - before.0 as usize - 2) * SIZE
    );
    let first_name = &all[8 + 44..8 + 48];
    assert_eq!(first_name, b"vtbd", "PRIORITY_MAX (0xfff) sorts first");
    assert!(mapped_bytes() >= PAGE as u64);
    let slot = p.slot;
    drop(p);
    assert_eq!(read(slot).allocated, 0);
    unlist(&vt);
    unlist(&nda);
    let at = nda.slot;
    drop(nda);
    assert_eq!(read(at), Devstat::default());
    assert_eq!(num_devs(), before.0 + 1);
    drop(vt);
    assert_eq!(num_devs(), before.0);
}

#[test]
fn a_trail_ends_each_entry_it_started() {
    let _g = setup();
    let a = Stat::provider(1).unwrap();
    let b = Stat::provider(2).unwrap();
    let mut t = Trail::default();
    assert_eq!(t.started(), None);
    FAKE_NOW.store(100, Ordering::Relaxed);
    t.start(&a, 100);
    t.start(&b, t.started().unwrap());
    FAKE_NOW.store(1100, Ordering::Relaxed);
    t.end(BioOp::Write, 512);
    for s in [&a, &b] {
        let d = read(s.slot);
        assert_eq!((d.start_count, d.end_count), (1, 1));
        assert_eq!(d.bytes[trans::WRITE], 512);
        assert_eq!(to_ns(&d.duration[trans::WRITE]), 999);
    }
    // Ending again does nothing.
    t.end(BioOp::Write, 512);
    assert_eq!(read(a.slot).end_count, 1);
}
