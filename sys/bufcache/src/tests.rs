// SPDX-License-Identifier: BSD-2-Clause
//! The buffer cache over a simulated disk that counts its I/O.

extern crate std;

use alloc::sync::Arc;
use alloc::vec::Vec;
use std::sync::Mutex as StdMutex;

use super::*;

#[allow(unsafe_code)]
fn s() -> Sleepable {
    // SAFETY: a host test thread may sleep.
    unsafe { Sleepable::assume() }
}

/// A disk in memory: its bytes, and counts of reads, writes, and flushes.
struct Disk {
    bytes: StdMutex<Vec<u8>>,
    reads: AtomicUsize,
    writes: AtomicUsize,
    flushes: AtomicUsize,
    fail_writes: core::sync::atomic::AtomicBool,
}

impl Disk {
    fn new(size: usize) -> Arc<Self> {
        Arc::new(Self {
            bytes: StdMutex::new(alloc::vec![0; size]),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            flushes: AtomicUsize::new(0),
            fail_writes: core::sync::atomic::AtomicBool::new(false),
        })
    }

    fn at(&self, offset: usize) -> u8 {
        self.bytes.lock().unwrap()[offset]
    }
}

impl BlockDev for Disk {
    fn sector_size(&self) -> u32 {
        512
    }

    fn size(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }

    fn read(&self, _s: &Sleepable, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let o = usize::try_from(offset).unwrap();
        buf.copy_from_slice(&self.bytes.lock().unwrap()[o..o + buf.len()]);
        Ok(())
    }

    fn write(&self, _s: &Sleepable, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        if self.fail_writes.load(Ordering::Relaxed) {
            return Err(IoError);
        }
        self.writes.fetch_add(1, Ordering::Relaxed);
        let o = usize::try_from(offset).unwrap();
        self.bytes.lock().unwrap()[o..o + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self, _s: &Sleepable) -> Result<(), IoError> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// A clock the tests move: one tick per reading.
static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn clock() -> u64 {
    CLOCK.fetch_add(1, Ordering::Relaxed) + 1
}

fn cache(disk: &Arc<Disk>, limits: Limits) -> BufCache {
    BufCache::new(Arc::clone(disk) as Arc<dyn BlockDev>, limits, clock)
}

#[test]
fn reads_are_cached_and_misaligned_ones_refused() {
    let disk = Disk::new(1 << 20);
    disk.bytes.lock().unwrap()[4096] = 7;
    let c = cache(&disk, Limits::default());
    let s = s();
    assert_eq!(c.read(&s, 4096, 4096, |d| d[0]), Ok(7));
    assert_eq!(c.read(&s, 4096, 4096, |d| d[0]), Ok(7));
    assert_eq!(disk.reads.load(Ordering::Relaxed), 1);
    assert_eq!(c.read(&s, 100, 512, |_| ()), Err(IoError));
    assert_eq!(c.read(&s, 0, 100, |_| ()), Err(IoError));
    assert_eq!(c.read(&s, 1 << 20, 512, |_| ()), Err(IoError));
}

#[test]
fn delayed_writes_wait_for_the_syncer_or_sync() {
    let disk = Disk::new(1 << 20);
    let c = cache(&disk, Limits::default());
    let s = s();
    c.write(&s, 8192, 4096, Fill::Overwrite, WriteMode::Delayed, |d| {
        d[0] = 1
    })
    .unwrap();
    let early = CLOCK.load(Ordering::Relaxed);
    c.write(&s, 16384, 4096, Fill::Read, WriteMode::Delayed, |d| {
        d[0] = 2
    })
    .unwrap();
    assert_eq!(disk.writes.load(Ordering::Relaxed), 0);
    assert_eq!(c.usage().1, 8192);
    // The syncer writes only what is old enough.
    c.write_aged(&s, early).unwrap();
    assert_eq!((disk.at(8192), disk.at(16384)), (1, 0));
    // A synchronous write goes now; sync writes the rest and flushes.
    c.write(&s, 0, 512, Fill::Read, WriteMode::Now, |d| d[0] = 3)
        .unwrap();
    assert_eq!(disk.at(0), 3);
    c.sync(&s).unwrap();
    assert_eq!(disk.at(16384), 2);
    assert_eq!(c.usage().1, 0);
    assert_eq!(disk.flushes.load(Ordering::Relaxed), 1);
}

#[test]
fn a_burst_of_writes_is_throttled() {
    let disk = Disk::new(4 << 20);
    let limits = Limits {
        max: 4 << 20,
        hi_dirty: 64 << 10,
        lo_dirty: 32 << 10,
    };
    let c = cache(&disk, limits);
    let s = s();
    for i in 0..64u64 {
        c.write(
            &s,
            i * 4096,
            4096,
            Fill::Overwrite,
            WriteMode::Delayed,
            |d| d.fill(9),
        )
        .unwrap();
        assert!(
            c.usage().1 <= limits.hi_dirty + 4096,
            "dirty {}",
            c.usage().1
        );
    }
    // The oldest went first.
    assert_eq!(disk.at(0), 9);
}

#[test]
fn clean_idle_buffers_are_evicted_past_the_limit() {
    let disk = Disk::new(4 << 20);
    let limits = Limits {
        max: 16 << 10,
        ..Limits::default()
    };
    let c = cache(&disk, limits);
    let s = s();
    for i in 0..10u64 {
        c.read(&s, i * 4096, 4096, |_| ()).unwrap();
    }
    assert!(c.usage().0 <= 16 << 10, "held {}", c.usage().0);
    // The most recent stay.
    let reads = disk.reads.load(Ordering::Relaxed);
    c.read(&s, 9 * 4096, 4096, |_| ()).unwrap();
    assert_eq!(disk.reads.load(Ordering::Relaxed), reads);
    // A dirty buffer is never evicted.
    c.write(&s, 0, 4096, Fill::Overwrite, WriteMode::Delayed, |d| {
        d[0] = 4
    })
    .unwrap();
    for i in 1..10u64 {
        c.read(&s, i * 4096, 4096, |_| ()).unwrap();
    }
    c.sync(&s).unwrap();
    assert_eq!(disk.at(0), 4);
}

#[test]
fn a_failed_write_stays_dirty() {
    let disk = Disk::new(1 << 20);
    let c = cache(&disk, Limits::default());
    let s = s();
    c.write(&s, 0, 4096, Fill::Overwrite, WriteMode::Delayed, |d| {
        d[0] = 5
    })
    .unwrap();
    disk.fail_writes.store(true, Ordering::Relaxed);
    assert_eq!(c.sync(&s), Err(IoError));
    assert_eq!(c.usage().1, 4096);
    disk.fail_writes.store(false, Ordering::Relaxed);
    c.sync(&s).unwrap();
    assert_eq!(disk.at(0), 5);
}

#[test]
fn a_new_size_rewrites_the_old_buffer() {
    let disk = Disk::new(1 << 20);
    let c = cache(&disk, Limits::default());
    let s = s();
    c.write(&s, 0, 4096, Fill::Overwrite, WriteMode::Delayed, |d| {
        d[4000] = 8
    })
    .unwrap();
    // A block-sized read of the same spot sees the data written back.
    assert_eq!(c.read(&s, 0, 32768, |d| d[4000]), Ok(8));
    assert_eq!(disk.at(4000), 8);
}

#[test]
fn discarded_buffers_are_never_written() {
    let disk = Disk::new(1 << 20);
    let c = cache(&disk, Limits::default());
    let s = s();
    c.write(&s, 8192, 8192, Fill::Overwrite, WriteMode::Delayed, |d| {
        d.fill(3)
    })
    .unwrap();
    c.write(&s, 32768, 4096, Fill::Overwrite, WriteMode::Delayed, |d| {
        d.fill(4)
    })
    .unwrap();
    // A range that overlaps the first buffer's tail only.
    c.discard(&s, 12288, 4096);
    assert_eq!(c.usage(), (4096, 4096));
    c.sync(&s).unwrap();
    assert_eq!((disk.at(8192), disk.at(32768)), (0, 4));
}
