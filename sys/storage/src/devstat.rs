// SPDX-License-Identifier: BSD-2-Clause
//! Device statistics (`devstat(9)`): each disk's entry in the device list
//! (`iostat`, `systat -vmstat`: `kern.devstat.all`), and an entry for each
//! GEOM provider (`gstat`, fastfetch: `/dev/devstat`'s pages, matched to
//! `kern.geom.confxml` by `id`), counted as requests start and finish.
//!
//! Entries live in pages the kernel gives once ([`Region`]), as
//! `struct devstat`s userland maps read-only. Each has a lock (FreeBSD's
//! GEOM updates its providers' under a pool mutex, and `geom_disk` its
//! disk's under the disk's) and a copy of its state; an update changes
//! the copy and stores the words it changed into the page, bracketed by
//! the entry's sequence numbers as `devstat_start_transaction` and
//! `devstat_end_transaction` bracket theirs. A request costs each entry on
//! its path one lock, a few additions, and a few stores, at the start and
//! at the end, with one clock read for all of them.
//!
//! Unlike FreeBSD's, the pages do not grow: the region holds a fixed
//! number of entries, and a disk made when it is full has none (its I/O
//! is not counted; docs/compat-deviations.md).
//!
//! Behavior reference: sys/kern/subr_devstat.c, sys/sys/devicestat.h,
//! sys/geom/geom_io.c (`g_io_request`, `g_io_deliver`), geom_disk.c.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::offset_of;
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use abi::devstat::{Bintime, Devstat, NAME_LEN, SIZE, priority, trans, ty};
use sync::{Once, SpinLock};

use crate::bio::BioOp;

/// The kernel's pages for the entries: `words` is all of them, as
/// 64-bit words, starting at physical page `pfn`.
#[derive(Debug)]
pub struct Region {
    pub words: &'static [AtomicU64],
    pub pfn: u64,
}

/// Bytes in a page.
pub const PAGE: usize = 4096;
/// Entries in a page (`statsperpage`).
pub const PER_PAGE: usize = PAGE / SIZE;
const WORDS_PER_PAGE: usize = PAGE / 8;
const WORDS: usize = SIZE / 8;

static REGION: Once<Region> = Once::new();
/// Nanoseconds since boot.
static CLOCK: Once<fn() -> u64> = Once::new();
/// A disk's or partition's provider id in `kern.geom.confxml`, by its
/// serial number.
static PROVIDER_ID: Once<fn(u64) -> u64> = Once::new();

/// Gives the entries their pages, a clock (the uptime, in nanoseconds),
/// and the GEOM provider ids of disks by serial number. Once, before the
/// first disk is published.
pub fn init(region: Region, clock: fn() -> u64, provider_id: fn(u64) -> u64) {
    let _ = REGION.set(region);
    let _ = CLOCK.set(clock);
    let _ = PROVIDER_ID.set(provider_id);
}

/// The provider entry of the disk or partition with serial `serial`.
pub(crate) fn for_provider(serial: u64) -> Option<Arc<Stat>> {
    Stat::provider(PROVIDER_ID.get()?(serial))
}

/// The uptime, for a request's start.
#[must_use]
pub fn now() -> u64 {
    CLOCK.get().map_or(0, |c| c())
}

/// Allocation of slots, and the list's bookkeeping.
struct Book {
    /// Slots in use, by index.
    used: Vec<bool>,
    /// Pages holding an allocated entry, as `pagelist` counts them: the
    /// highest page in use, plus one.
    pages: usize,
    /// The next `device_number`.
    next_number: u32,
}

static BOOK: SpinLock<Book> = SpinLock::new(Book {
    used: Vec::new(),
    pages: 0,
    next_number: 0,
});
/// `devstat_generation`: changes whenever an entry is made or removed.
static GENERATION: AtomicI64 = AtomicI64::new(1);
/// `devstat_num_devs`: entries in the device list.
static NUM_DEVS: AtomicU32 = AtomicU32::new(0);
/// The device list's entries.
static LIST: SpinLock<Vec<Arc<Stat>>> = SpinLock::new(Vec::new());

/// `kern.devstat.generation`.
#[must_use]
pub fn generation() -> i64 {
    GENERATION.load(Ordering::Relaxed)
}

/// `kern.devstat.numdevs`.
#[must_use]
pub fn num_devs() -> u32 {
    NUM_DEVS.load(Ordering::Relaxed)
}

/// The bytes of `/dev/devstat` (`DIOCGMEDIASIZE`): its pages that hold
/// entries.
#[must_use]
pub fn mapped_bytes() -> u64 {
    u64::try_from(BOOK.lock().pages * PAGE).unwrap_or(0)
}

/// The first physical page of the entries' pages, and how many there are
/// (`devstat_mmap`).
#[must_use]
pub fn pages() -> Option<(u64, usize)> {
    let r = REGION.get()?;
    Some((r.pfn, BOOK.lock().pages))
}

/// `kern.devstat.all`: the generation (a `long`), then each entry of the
/// device list, highest priority first and in the order made within one.
#[must_use]
pub fn all() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&generation().to_ne_bytes());
    let mut list: Vec<Arc<Stat>> = LIST.lock().clone();
    list.sort_by_key(|s| {
        let d = s.state.lock();
        (core::cmp::Reverse(d.priority), d.device_number)
    });
    for s in list {
        let d = *s.state.lock();
        out.extend(bytes_of(&d));
    }
    out
}

fn bytes_of(d: &Devstat) -> Vec<u8> {
    let mut v = Vec::with_capacity(SIZE);
    for i in 0..WORDS {
        v.extend_from_slice(&word(d, i).to_ne_bytes());
    }
    v
}

/// Word `i` of `d` as it lies in memory (both architectures are
/// little-endian: a word's first `u32` is its low half).
fn word(d: &Devstat, i: usize) -> u64 {
    let at = i * 8;
    let pair = |lo: u32, hi: u32| u64::from(lo) | u64::from(hi) << 32;
    let bt = |b: &Bintime, base: usize| {
        if at == base {
            b.sec.cast_unsigned()
        } else {
            b.frac
        }
    };
    let name = |from: usize| {
        let mut b = [0u8; 8];
        for (k, x) in b.iter_mut().enumerate() {
            let o = from + k;
            *x = match o {
                o if (40..44).contains(&o) => d.device_number.to_le_bytes()[o - 40],
                o if (44..44 + NAME_LEN).contains(&o) => d.device_name[o - 44],
                o if (60..64).contains(&o) => d.unit_number.to_le_bytes()[o - 60],
                _ => 0,
            };
        }
        u64::from_le_bytes(b)
    };
    match at {
        0 => pair(d.sequence0, d.allocated.cast_unsigned()),
        8 => pair(d.start_count, d.end_count),
        16 | 24 => bt(&d.busy_from, 16),
        32 => d.dev_links,
        40..=56 => name(at),
        64..=88 => d.bytes[(at - 64) / 8],
        96..=120 => d.operations[(at - 96) / 8],
        128..=184 => {
            let k = (at - 128) / 16;
            bt(&d.duration[k], 128 + k * 16)
        }
        192 | 200 => bt(&d.busy_time, 192),
        208 | 216 => bt(&d.creation_time, 208),
        224 => pair(d.block_size, 0),
        232..=248 => d.tag_types[(at - 232) / 8],
        256 => pair(d.flags, d.device_type),
        264 => pair(d.priority, 0),
        272 => d.id,
        280 => pair(d.sequence1, 0),
        _ => 0,
    }
}

const _: () = {
    assert!(offset_of!(Devstat, busy_from) == 16);
    assert!(offset_of!(Devstat, dev_links) == 32);
    assert!(offset_of!(Devstat, device_number) == 40);
    assert!(offset_of!(Devstat, device_name) == 44);
    assert!(offset_of!(Devstat, unit_number) == 60);
    assert!(offset_of!(Devstat, bytes) == 64);
    assert!(offset_of!(Devstat, operations) == 96);
    assert!(offset_of!(Devstat, duration) == 128);
    assert!(offset_of!(Devstat, busy_time) == 192);
    assert!(offset_of!(Devstat, creation_time) == 208);
    assert!(offset_of!(Devstat, block_size) == 224);
    assert!(offset_of!(Devstat, tag_types) == 232);
    assert!(offset_of!(Devstat, flags) == 256);
    assert!(offset_of!(Devstat, priority) == 264);
    assert!(offset_of!(Devstat, id) == 272);
    assert!(offset_of!(Devstat, sequence1) == 280);
};

/// What a device-list entry says of its device (`devstat_new_entry`'s
/// arguments): `geom_disk`'s defaults unless the driver says otherwise,
/// as CAM's drivers do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kind {
    /// `devstat_type_flags`.
    pub device_type: u32,
    /// `devstat_priority`.
    pub priority: u32,
    /// `devstat_support_flags`.
    pub flags: u32,
    /// The block size (0 for none, with `DEVSTAT_BS_UNAVAILABLE`); `None`
    /// is the disk's sector size.
    pub block_size: Option<u32>,
}

impl Kind {
    /// `geom_disk`'s entry for a disk whose driver makes none (`vtbd`,
    /// `md`).
    pub const GEOM_DISK: Self = Self {
        device_type: ty::DIRECT,
        priority: priority::MAX,
        flags: abi::devstat::ALL_SUPPORTED,
        block_size: None,
    };
    /// `nda`'s: an NVMe namespace.
    pub const NVME: Self = Self {
        device_type: ty::DIRECT | ty::IF_NVME,
        priority: priority::DISK,
        ..Self::GEOM_DISK
    };
    /// `ada`'s: an ATA disk.
    pub const ATA: Self = Self {
        device_type: ty::DIRECT | ty::IF_IDE,
        priority: priority::DISK,
        ..Self::GEOM_DISK
    };
    /// `da`'s: a SCSI direct-access device (a USB disk), which reports no
    /// block size (`DEVSTAT_BS_UNAVAILABLE`).
    pub const SCSI: Self = Self {
        device_type: ty::DIRECT | ty::IF_SCSI,
        priority: priority::DISK,
        flags: 0x04,
        block_size: Some(0),
    };
}

/// One entry.
pub struct Stat {
    slot: usize,
    listed: bool,
    state: SpinLock<Devstat>,
}

impl core::fmt::Debug for Stat {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Stat")
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

/// The region's word `i` of slot `slot`.
fn cell(slot: usize, i: usize) -> Option<&'static AtomicU64> {
    let page = slot / PER_PAGE;
    let index = slot % PER_PAGE;
    REGION
        .get()?
        .words
        .get(page * WORDS_PER_PAGE + index * WORDS + i)
}

fn alloc_slot() -> Option<usize> {
    let r = REGION.get()?;
    let slots = r.words.len() / WORDS_PER_PAGE * PER_PAGE;
    let mut b = BOOK.lock();
    if b.used.len() < slots {
        b.used.resize(slots, false);
    }
    let slot = b.used.iter().position(|u| !u)?;
    b.used[slot] = true;
    b.pages = b.pages.max(slot / PER_PAGE + 1);
    Some(slot)
}

impl Stat {
    fn new(d: Devstat, listed: bool) -> Option<Arc<Self>> {
        let slot = alloc_slot()?;
        let s = Arc::new(Self {
            slot,
            listed,
            state: SpinLock::new(d),
        });
        s.store_all(&d);
        GENERATION.fetch_add(1, Ordering::Relaxed);
        Some(s)
    }

    /// A GEOM provider's entry (`g_new_providerf`: unit -1, its `id`).
    #[must_use]
    pub fn provider(id: u64) -> Option<Arc<Self>> {
        Self::new(
            Devstat {
                allocated: 1,
                unit_number: -1,
                id,
                creation_time: Bintime::from_ns(now()),
                ..Devstat::default()
            },
            false,
        )
    }

    /// A device's entry in the list (`devstat_new_entry` with a unit),
    /// `name` and `unit`, blocks of `block_size` bytes, as `kind` says.
    #[must_use]
    pub fn device(name: &str, unit: u32, block_size: u32, kind: Kind) -> Option<Arc<Self>> {
        let mut device_name = [0u8; NAME_LEN];
        let n = name.len().min(NAME_LEN - 1);
        device_name[..n].copy_from_slice(&name.as_bytes()[..n]);
        let number = {
            let mut b = BOOK.lock();
            let n = b.next_number;
            b.next_number += 1;
            n
        };
        let s = Self::new(
            Devstat {
                allocated: 1,
                device_number: number,
                device_name,
                unit_number: i32::try_from(unit).unwrap_or(i32::MAX),
                block_size: kind.block_size.unwrap_or(block_size),
                flags: kind.flags,
                device_type: kind.device_type,
                priority: kind.priority,
                creation_time: Bintime::from_ns(now()),
                ..Devstat::default()
            },
            true,
        )?;
        LIST.lock().push(Arc::clone(&s));
        NUM_DEVS.fetch_add(1, Ordering::Relaxed);
        Some(s)
    }

    /// Changes what the entry says of its device (a driver that knows
    /// more once it has published the disk).
    pub fn set_kind(&self, kind: Kind) {
        let mut d = self.state.lock();
        d.device_type = kind.device_type;
        d.priority = kind.priority;
        d.flags = kind.flags;
        if let Some(b) = kind.block_size {
            d.block_size = b;
        }
        self.store_all(&d);
    }

    fn store(&self, d: &Devstat, i: usize) {
        if let Some(c) = cell(self.slot, i) {
            // Readers copy the page and tolerate a torn entry, as
            // FreeBSD's do; the sequence words say an update ran.
            c.store(word(d, i), Ordering::Relaxed);
        }
    }

    fn store_all(&self, d: &Devstat) {
        for i in 0..WORDS {
            self.store(d, i);
        }
    }

    /// `sequence1`'s word, then the update's words, then `sequence0`'s.
    fn publish(&self, d: &Devstat, words: &[usize]) {
        self.store(d, offset_of!(Devstat, sequence1) / 8);
        for &i in words {
            self.store(d, i);
        }
        self.store(d, 0);
    }

    /// `devstat_start_transaction`: a request starts at `now`.
    pub fn start(&self, now: u64) {
        let mut d = self.state.lock();
        d.sequence1 = d.sequence1.wrapping_add(1);
        let idle = d.start_count == d.end_count;
        d.start_count = d.start_count.wrapping_add(1);
        if idle {
            d.busy_from = Bintime::from_ns(now);
        }
        d.sequence0 = d.sequence0.wrapping_add(1);
        self.publish(&d, &[1, 2, 3]);
    }

    /// `devstat_end_transaction_bio`: a request of `op` that started at
    /// `t0` ends at `now`, having moved `bytes`.
    pub fn end(&self, op: BioOp, bytes: u64, t0: u64, now: u64) {
        let flag = match op {
            BioOp::Read => trans::READ,
            BioOp::Write => trans::WRITE,
            BioOp::Flush => trans::NO_DATA,
        };
        let mut d = self.state.lock();
        d.sequence1 = d.sequence1.wrapping_add(1);
        d.bytes[flag] = d.bytes[flag].wrapping_add(bytes);
        d.operations[flag] = d.operations[flag].wrapping_add(1);
        d.tag_types[abi::devstat::tag::SIMPLE] =
            d.tag_types[abi::devstat::tag::SIMPLE].wrapping_add(1);
        add(&mut d.duration[flag], now.saturating_sub(t0));
        let busy_from = to_ns(&d.busy_from);
        add(&mut d.busy_time, now.saturating_sub(busy_from));
        d.busy_from = Bintime::from_ns(now);
        d.end_count = d.end_count.wrapping_add(1);
        d.sequence0 = d.sequence0.wrapping_add(1);
        let b = offset_of!(Devstat, bytes) / 8 + flag;
        let o = offset_of!(Devstat, operations) / 8 + flag;
        let du = offset_of!(Devstat, duration) / 8 + flag * 2;
        let bt = offset_of!(Devstat, busy_time) / 8;
        let tg = offset_of!(Devstat, tag_types) / 8;
        self.publish(&d, &[1, 2, 3, b, o, du, du + 1, bt, bt + 1, tg]);
    }
}

/// Adds `ns` nanoseconds to `b` (`bintime_add`).
fn add(b: &mut Bintime, ns: u64) {
    let d = Bintime::from_ns(ns);
    let (frac, carry) = b.frac.overflowing_add(d.frac);
    b.frac = frac;
    b.sec = b.sec.wrapping_add(d.sec).wrapping_add(i64::from(carry));
}

/// `b` in nanoseconds.
fn to_ns(b: &Bintime) -> u64 {
    let sec = u64::try_from(b.sec).unwrap_or(0);
    let frac = u64::try_from((u128::from(b.frac) * 1_000_000_000) >> 64).unwrap_or(0);
    sec.saturating_mul(1_000_000_000).saturating_add(frac)
}

impl Drop for Stat {
    /// `devstat_remove_entry`: the entry is zeroed and its slot freed.
    fn drop(&mut self) {
        let zero = Devstat::default();
        self.store_all(&zero);
        BOOK.lock().used[self.slot] = false;
        if self.listed {
            NUM_DEVS.fetch_sub(1, Ordering::Relaxed);
        }
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Removes a device's entry from the list; it goes when the last request
/// holding it ends.
pub fn unlist(s: &Arc<Stat>) {
    LIST.lock().retain(|e| !Arc::ptr_eq(e, s));
}

/// The entries on a request's path, ended together when it completes.
#[derive(Debug, Default)]
pub(crate) struct Trail {
    t0: u64,
    stats: [Option<Arc<Stat>>; 3],
}

impl Trail {
    /// Starts `s` for a request (now `now`, the first entry's time kept as
    /// its start).
    pub(crate) fn start(&mut self, s: &Arc<Stat>, now: u64) {
        if self.stats[0].is_none() {
            self.t0 = now;
        }
        if let Some(slot) = self.stats.iter_mut().find(|x| x.is_none()) {
            s.start(now);
            *slot = Some(Arc::clone(s));
        }
    }

    /// When the first entry started, if one has.
    pub(crate) fn started(&self) -> Option<u64> {
        self.stats[0].is_some().then_some(self.t0)
    }

    /// Ends every entry started: `op`, `bytes` moved.
    pub(crate) fn end(&mut self, op: BioOp, bytes: u64) {
        if self.stats[0].is_none() {
            return;
        }
        let now = now();
        for s in self.stats.iter_mut().filter_map(Option::take) {
            s.end(op, bytes, self.t0, now);
        }
    }
}

#[cfg(test)]
mod tests;
