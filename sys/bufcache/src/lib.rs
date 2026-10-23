// SPDX-License-Identifier: BSD-2-Clause
//! The metadata buffer cache (docs/VFS.md §8.2): superblocks, cylinder
//! groups, inode blocks, indirect blocks, directory blocks, and FAT
//! sectors, cached by disk offset, written back in the background.
//!
//! A [`BufCache`] serves one block device. Each operation holds a buffer
//! busy (FreeBSD's `BUF_LOCK`) while a closure reads or changes its bytes:
//! [`BufCache::read`] (`bread`, then `brelse`), and [`BufCache::write`],
//! which reads the block first unless told it is overwritten whole
//! (`getblk`), then writes it now (`bwrite`) or marks it dirty for later
//! (`bdwrite`).
//!
//! Dirty buffers remember when they were dirtied, on the clock the cache
//! is given: the syncer writes those older than its delay
//! ([`BufCache::write_aged`]), and `sync` writes all.
//! Past a high watermark of dirty bytes, the writer that crosses it starts
//! writes of the oldest until the total not yet going out is under the
//! low one (FreeBSD's `hidirtybuffers` and `bawrite`), so a burst of
//! writes cannot fill memory. Clean buffers nobody holds are evicted,
//! least recently used first, past the cache's size limit.
//!
//! Background writes do not hold the buffer: they go out from a copy, as
//! FreeBSD's background writes of cylinder groups do, and the buffer stays
//! dirty until the write is done. Whoever next holds the buffer (or the
//! next operation on the cache, for those nobody holds) finishes it,
//! telling the hook; a buffer is not written again until its last write
//! is finished.
//!
//! A file system may give the cache a [`BufHook`]. It checks each block
//! read from the device (a check-hash), and every write of a buffer asks
//! it first: it may finish the bytes (setting a check-hash, as FreeBSD
//! does for cylinder groups as they go out), and a file system ordering
//! its metadata writes (UFS with soft updates) may hand back a copy with
//! changes that must not reach the disk yet rolled back (FreeBSD's
//! `initiate_write` dependency hooks). Such a buffer stays dirty after the
//! write, for the rolled-back changes.
//!
//! A buffer's lock is sleepable and taken under vnode locks, before any
//! of the disk's own (and before a hook's).
//!
//! Behavior reference: sys/kern/vfs_bio.c (the interface and the
//! watermarks, not its implementation).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use core::sync::atomic::{AtomicBool, AtomicU8};

use sched::lock::{LockClass, Mutex};
use sched::{Sleepable, WaitQueue};
use sync::PreemptLock;

/// A device could not move the bytes (`EIO`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoError;

/// Where the buffers' bytes live.
pub trait BlockDev: Send + Sync {
    /// The smallest transfer, and the alignment of every one.
    fn sector_size(&self) -> u32;
    /// The device's size in bytes.
    fn size(&self) -> u64;
    fn read(&self, s: &Sleepable, offset: u64, buf: &mut [u8]) -> Result<(), IoError>;
    fn write(&self, s: &Sleepable, offset: u64, buf: &[u8]) -> Result<(), IoError>;
    /// Starts writing `buf` at `offset` without waiting: `done` runs once
    /// it is over, with whether the device took it, in any context (an
    /// interrupt thread); it does not sleep.
    fn start_write(
        &self,
        s: &Sleepable,
        offset: u64,
        buf: Vec<u8>,
        done: Box<dyn FnOnce(bool) + Send>,
    ) {
        let ok = self.write(s, offset, &buf).is_ok();
        done(ok);
    }
    /// Makes earlier writes durable.
    fn flush(&self, s: &Sleepable) -> Result<(), IoError>;
}

/// What a file system learns of its buffers' transfers, with the buffer's
/// lock held throughout: each read from the device, to check it, and each
/// write, to finish it or hold changes back.
pub trait BufHook: Send + Sync {
    /// The buffer at `offset` was just read from the device: whether its
    /// bytes are sound (a check-hash). An error fails the read, and the
    /// buffer is read again next time.
    fn filled(&self, _s: &Sleepable, _offset: u64, _bytes: &[u8]) -> Result<(), IoError> {
        Ok(())
    }
    /// A write of the buffer at `offset` holding `bytes` begins. The hook
    /// may finish them in place (a check-hash), and returns the bytes to
    /// write instead, if some changes in them may not reach the disk yet.
    fn start(&self, s: &Sleepable, offset: u64, bytes: &mut [u8]) -> Option<Vec<u8>>;
    /// That write ended; `ok` if the device took it.
    fn done(&self, s: &Sleepable, offset: u64, ok: bool);
}

/// The largest buffer (`MAXBSIZE`).
const MAX_BUFFER: u64 = 64 << 10;

/// Buffer locks: under vnode locks, before the disk's.
static BUF: LockClass = LockClass::new("buffer", 380);

/// One buffer's state, behind its lock.
#[derive(Debug)]
struct Data {
    bytes: Vec<u8>,
    /// The bytes hold the disk's (or newer) contents.
    valid: bool,
    /// When it was dirtied (on the cache's clock), if it was.
    dirty: Option<u64>,
    /// Bumped by each change of the bytes.
    generation: u64,
    /// A background write under way or not yet finished.
    writing: Option<Started>,
}

/// A background write of a buffer.
#[derive(Clone, Copy, Debug)]
struct Started {
    /// The buffer's generation when it began.
    generation: u64,
    /// The hook held some changes back.
    rolled: bool,
    len: usize,
}

/// A background write's outcome, as its completion leaves it.
const PENDING: u8 = 0;
const WRITTEN: u8 = 1;
const FAILED: u8 = 2;

#[derive(Debug)]
struct Buffer {
    offset: u64,
    data: Mutex<Data>,
    /// The tick of its last use, for eviction.
    used: AtomicU64,
    /// Its background write's outcome.
    result: AtomicU8,
    /// Out of the table: set with its lock held, before it is let go.
    evicted: AtomicBool,
}

/// What background writes' completions share with the cache.
struct Io {
    /// Buffers whose writes are done, not yet finished.
    done: PreemptLock<Vec<Arc<Buffer>>>,
    any_done: AtomicBool,
    /// Woken by each completion.
    wake: WaitQueue,
    /// Bytes being written.
    running: AtomicUsize,
}

/// The most bytes of background writes under way (FreeBSD's
/// `hirunningspace`): a writer past it waits for half to finish.
const HI_RUNNING: usize = 16 << 20;

/// Limits, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Clean buffers past this total are evicted.
    pub max: usize,
    /// A writer past this many dirty bytes writes some back...
    pub hi_dirty: usize,
    /// ...until the total is under this.
    pub lo_dirty: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max: 32 << 20,
            hi_dirty: 8 << 20,
            lo_dirty: 4 << 20,
        }
    }
}

/// The buffer cache of one device.
pub struct BufCache {
    dev: Arc<dyn BlockDev>,
    buffers: PreemptLock<BTreeMap<u64, Arc<Buffer>>>,
    limits: Limits,
    /// Bytes held, and bytes dirty.
    bytes: AtomicUsize,
    dirty: AtomicUsize,
    ticks: AtomicU64,
    /// Nanoseconds since some start, for dirtying times.
    clock: fn() -> u64,
    hook: PreemptLock<Option<Arc<dyn BufHook>>>,
    io: Arc<Io>,
    /// Dirty bytes whose background writes are not finished.
    started: AtomicUsize,
}

impl core::fmt::Debug for BufCache {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "BufCache({} bytes, {} dirty)",
            self.bytes.load(Ordering::Relaxed),
            self.dirty.load(Ordering::Relaxed)
        )
    }
}

/// How a changed buffer goes to the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// Now, before returning (FreeBSD's `bwrite`).
    Now,
    /// Later, by the syncer or a `sync` (`bdwrite`).
    Delayed,
}

/// Whether a buffer's old contents matter to its writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// Read them first (a partial change).
    Read,
    /// The writer overwrites the whole block (`getblk`): a buffer not
    /// cached comes zeroed.
    Overwrite,
}

impl BufCache {
    #[must_use]
    pub fn new(dev: Arc<dyn BlockDev>, limits: Limits, clock: fn() -> u64) -> Self {
        Self {
            dev,
            buffers: PreemptLock::new(BTreeMap::new()),
            limits,
            bytes: AtomicUsize::new(0),
            dirty: AtomicUsize::new(0),
            ticks: AtomicU64::new(0),
            clock,
            hook: PreemptLock::new(None),
            io: Arc::new(Io {
                done: PreemptLock::new(Vec::new()),
                any_done: AtomicBool::new(false),
                wake: WaitQueue::new(),
                running: AtomicUsize::new(0),
            }),
            started: AtomicUsize::new(0),
        }
    }

    /// Has every write from now on go through `hook`.
    pub fn set_hook(&self, hook: Arc<dyn BufHook>) {
        *self.hook.lock() = Some(hook);
    }

    /// Writes held buffer `d` at `offset` (dirty or not), through the
    /// hook: clean afterwards, unless the hook rolled something back or
    /// the device failed.
    fn write_out(&self, s: &Sleepable, b: &Buffer, d: &mut Data) -> Result<(), IoError> {
        self.settle(s, b, d, true);
        let offset = b.offset;
        let hook = self.hook.lock().clone();
        let rolled = hook.as_ref().and_then(|h| h.start(s, offset, &mut d.bytes));
        let r = self
            .dev
            .write(s, offset, rolled.as_deref().unwrap_or(&d.bytes));
        if let Some(h) = &hook {
            h.done(s, offset, r.is_ok());
        }
        if r.is_ok() && rolled.is_none() {
            if d.dirty.take().is_some() {
                self.dirty.fetch_sub(d.bytes.len(), Ordering::Relaxed);
            }
        } else if d.dirty.is_none() {
            // What did not reach the disk keeps it dirty; a failed write
            // is retried later.
            d.dirty = Some((self.clock)());
            self.dirty.fetch_add(d.bytes.len(), Ordering::Relaxed);
        }
        r
    }

    /// Starts a background write of held buffer `b` (not being written),
    /// through the hook (FreeBSD's `bawrite`).
    fn start_out(&self, s: &Sleepable, b: &Arc<Buffer>, d: &mut Data) {
        let hook = self.hook.lock().clone();
        let rolled = hook
            .as_ref()
            .and_then(|h| h.start(s, b.offset, &mut d.bytes));
        let len = d.bytes.len();
        d.writing = Some(Started {
            generation: d.generation,
            rolled: rolled.is_some(),
            len,
        });
        b.result.store(PENDING, Ordering::Relaxed);
        self.started.fetch_add(len, Ordering::Relaxed);
        self.io.running.fetch_add(len, Ordering::Relaxed);
        let bytes = rolled.unwrap_or_else(|| d.bytes.clone());
        let io = Arc::clone(&self.io);
        let buf = Arc::clone(b);
        self.dev.start_write(
            s,
            b.offset,
            bytes,
            Box::new(move |ok| {
                buf.result
                    .store(if ok { WRITTEN } else { FAILED }, Ordering::Release);
                io.running.fetch_sub(len, Ordering::Relaxed);
                io.done.lock().push(buf);
                io.any_done.store(true, Ordering::Release);
                io.wake.wake_all();
            }),
        );
    }

    /// Finishes held buffer `b`'s background write if it is done (or,
    /// with `wait`, once it is): the hook is told, and the buffer is clean
    /// if all of it went out unchanged since. Returns whether the write
    /// succeeded, if one was finished.
    fn settle(&self, s: &Sleepable, b: &Buffer, d: &mut Data, wait: bool) -> Option<bool> {
        let w = d.writing?;
        if wait {
            self.io
                .wake
                .wait_until(s, || b.result.load(Ordering::Acquire) != PENDING);
        }
        let ok = match b.result.load(Ordering::Acquire) {
            PENDING => return None,
            r => r == WRITTEN,
        };
        d.writing = None;
        self.started.fetch_sub(w.len, Ordering::Relaxed);
        let hook = self.hook.lock().clone();
        if let Some(h) = &hook {
            h.done(s, b.offset, ok);
        }
        if ok && !w.rolled && w.generation == d.generation && d.dirty.take().is_some() {
            self.dirty.fetch_sub(d.bytes.len(), Ordering::Relaxed);
        }
        Some(ok)
    }

    /// Finishes the background writes that are done. The caller holds no
    /// buffer.
    fn reap(&self, s: &Sleepable) {
        if !self.io.any_done.swap(false, Ordering::Acquire) {
            return;
        }
        let done = core::mem::take(&mut *self.io.done.lock());
        for b in done {
            let mut d = b.data.lock(s);
            self.settle(s, &b, &mut d, false);
        }
    }

    #[must_use]
    pub fn dev(&self) -> &Arc<dyn BlockDev> {
        &self.dev
    }

    /// A use count, for eviction order.
    fn tick(&self) -> u64 {
        self.ticks.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Bytes held, and how many are dirty.
    #[must_use]
    pub fn usage(&self) -> (usize, usize) {
        (
            self.bytes.load(Ordering::Relaxed),
            self.dirty.load(Ordering::Relaxed),
        )
    }

    fn check(&self, offset: u64, size: usize) -> Result<(), IoError> {
        let ss = u64::from(self.dev.sector_size());
        let len = u64::try_from(size).map_err(|_| IoError)?;
        if len == 0 || !offset.is_multiple_of(ss) || !len.is_multiple_of(ss) {
            return Err(IoError);
        }
        if offset
            .checked_add(len)
            .is_none_or(|end| end > self.dev.size())
        {
            return Err(IoError);
        }
        Ok(())
    }

    /// The buffer at `offset` of `size` bytes, held while `f` runs: found,
    /// or made (its bytes not read). A cached buffer of another size takes
    /// the new size, keeping its bytes.
    fn with<R>(
        &self,
        s: &Sleepable,
        offset: u64,
        size: usize,
        f: impl FnOnce(&Buffer, &mut Data) -> Result<R, IoError>,
    ) -> Result<R, IoError> {
        self.check(offset, size)?;
        self.reap(s);
        loop {
            let buf = {
                let mut t = self.buffers.lock();
                match t.get(&offset) {
                    Some(b) => Arc::clone(b),
                    None => {
                        let b = Arc::new(Buffer {
                            offset,
                            data: Mutex::new(
                                &BUF,
                                Data {
                                    bytes: alloc::vec![0; size],
                                    valid: false,
                                    dirty: None,
                                    generation: 0,
                                    writing: None,
                                },
                            ),
                            used: AtomicU64::new(0),
                            result: AtomicU8::new(PENDING),
                            evicted: AtomicBool::new(false),
                        });
                        t.insert(offset, Arc::clone(&b));
                        self.bytes.fetch_add(size, Ordering::Relaxed);
                        b
                    }
                }
            };
            let mut data = buf.data.lock(s);
            // It may have been evicted while we waited for it.
            if buf.evicted.load(Ordering::Acquire) {
                continue;
            }
            self.settle(s, &buf, &mut data, false);
            if data.bytes.len() != size {
                data.generation += 1;
                // A fragment grown or shrunk in place: what it holds stays
                // (dirty or not, rolled-back changes included), and a
                // grown tail comes from the device.
                let old = data.bytes.len();
                if data.valid && size > old {
                    let mut tail = alloc::vec![0; size - old];
                    let at = offset + u64::try_from(old).map_err(|_| IoError)?;
                    self.dev.read(s, at, &mut tail)?;
                    data.bytes.extend_from_slice(&tail);
                } else {
                    data.bytes.resize(size, 0);
                }
                self.bytes.fetch_sub(old, Ordering::Relaxed);
                self.bytes.fetch_add(size, Ordering::Relaxed);
                if data.dirty.is_some() {
                    self.dirty.fetch_sub(old, Ordering::Relaxed);
                    self.dirty.fetch_add(size, Ordering::Relaxed);
                }
            }
            buf.used.store(self.tick(), Ordering::Relaxed);
            let r = f(&buf, &mut data);
            drop(data);
            drop(buf);
            self.trim();
            return r;
        }
    }

    /// Reads the block at `offset` of `size` bytes, from the cache or the
    /// device, and calls `f` with it (FreeBSD's `bread` and `brelse`).
    pub fn read<R>(
        &self,
        s: &Sleepable,
        offset: u64,
        size: usize,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, IoError> {
        self.with(s, offset, size, |_, d| {
            self.validate(s, offset, d)?;
            Ok(f(&d.bytes))
        })
    }

    /// Changes the block at `offset` of `size` bytes with `f` and writes
    /// it as `mode` says (`bwrite` or `bdwrite`); with [`Fill::Read`] its
    /// contents are read first.
    pub fn write<R>(
        &self,
        s: &Sleepable,
        offset: u64,
        size: usize,
        fill: Fill,
        mode: WriteMode,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, IoError> {
        let r = self.with(s, offset, size, |b, d| {
            match fill {
                Fill::Read => self.validate(s, offset, d)?,
                Fill::Overwrite => d.valid = true,
            }
            let r = f(&mut d.bytes);
            d.generation += 1;
            match mode {
                WriteMode::Now => self.write_out(s, b, d)?,
                WriteMode::Delayed => {
                    if d.dirty.is_none() {
                        d.dirty = Some((self.clock)());
                        self.dirty.fetch_add(d.bytes.len(), Ordering::Relaxed);
                    }
                }
            }
            Ok(r)
        })?;
        if mode == WriteMode::Delayed {
            self.throttle(s);
        }
        Ok(r)
    }

    /// Reads a held buffer's bytes from the device if they are not there.
    fn validate(&self, s: &Sleepable, offset: u64, d: &mut Data) -> Result<(), IoError> {
        if !d.valid {
            self.dev.read(s, offset, &mut d.bytes)?;
            let hook = self.hook.lock().clone();
            if let Some(h) = hook {
                h.filled(s, offset, &d.bytes)?;
            }
            d.valid = true;
        }
        Ok(())
    }

    /// The dirty buffers, oldest first, each with when it was dirtied.
    /// A buffer someone holds, or one being written, is a candidate (as
    /// the oldest) only if `busy`: whoever waits for it then writes it if
    /// it is still dirty.
    fn dirty_buffers(&self, busy: bool) -> Vec<(u64, Arc<Buffer>)> {
        let all: Vec<Arc<Buffer>> = self.buffers.lock().values().cloned().collect();
        let mut out: Vec<(u64, Arc<Buffer>)> = all
            .into_iter()
            .filter_map(|b| {
                // A peek only to choose; the write looks again under the
                // lock.
                let d = match b.data.try_lock() {
                    Some(g) if g.writing.is_some() && !busy => return None,
                    Some(g) => g.dirty?,
                    None if busy => 0,
                    None => return None,
                };
                Some((d, b))
            })
            .collect();
        out.sort_by_key(|(d, _)| *d);
        out
    }

    /// Starts a background write of buffer `b` if it is still dirty and
    /// not being written (waiting for a write under way, with `wait`).
    /// Returns whether it started one.
    fn start_one(&self, s: &Sleepable, b: &Arc<Buffer>, wait: bool) -> bool {
        let mut data = b.data.lock(s);
        self.settle(s, b, &mut data, wait);
        if data.dirty.is_none() || data.writing.is_some() {
            return false;
        }
        self.start_out(s, b, &mut data);
        true
    }

    /// Writes the buffer at `offset` now if it is cached and dirty
    /// (`bwrite` of a block `fsync` needs). Returns whether it is clean
    /// afterwards.
    pub fn write_buffer(&self, s: &Sleepable, offset: u64) -> Result<bool, IoError> {
        let Some(b) = self.buffers.lock().get(&offset).cloned() else {
            return Ok(true);
        };
        let mut data = b.data.lock(s);
        self.settle(s, &b, &mut data, true);
        if data.dirty.is_some() {
            self.write_out(s, &b, &mut data)?;
        }
        Ok(data.dirty.is_none())
    }

    /// Whether any buffer is dirty.
    #[must_use]
    pub fn any_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed) != 0
    }

    /// Writes the buffers dirtied at or before `before` on the cache's
    /// clock (the syncer: those older than its delay), all under way at
    /// once, and waits for them.
    pub fn write_aged(&self, s: &Sleepable, before: u64) -> Result<(), IoError> {
        self.reap(s);
        let mut started = Vec::new();
        for (d, b) in self.dirty_buffers(before == u64::MAX) {
            if d > before {
                break;
            }
            if self.start_one(s, &b, true) {
                started.push(b);
            }
            self.wait_running(s);
        }
        let mut result = Ok(());
        for b in started {
            let mut data = b.data.lock(s);
            if self.settle(s, &b, &mut data, true) == Some(false) {
                result = Err(IoError);
            }
        }
        result
    }

    /// Past [`HI_RUNNING`] bytes being written, waits for half of them.
    fn wait_running(&self, s: &Sleepable) {
        if self.io.running.load(Ordering::Relaxed) > HI_RUNNING {
            self.io.wake.wait_until(s, || {
                self.io.running.load(Ordering::Relaxed) <= HI_RUNNING / 2
            });
        }
    }

    /// Dirty bytes not yet going out.
    fn unstarted(&self) -> usize {
        self.dirty
            .load(Ordering::Relaxed)
            .saturating_sub(self.started.load(Ordering::Relaxed))
    }

    /// Writes every dirty buffer, then flushes the device (`sync`,
    /// unmount).
    pub fn sync(&self, s: &Sleepable) -> Result<(), IoError> {
        self.write_aged(s, u64::MAX)?;
        self.dev.flush(s)
    }

    /// Past the high watermark: starts writes of the oldest dirty buffers
    /// until the total not going out is under the low one.
    fn throttle(&self, s: &Sleepable) {
        if self.unstarted() > self.limits.hi_dirty {
            for (_, b) in self.dirty_buffers(false) {
                if self.unstarted() <= self.limits.lo_dirty {
                    break;
                }
                // A failed write stays dirty, for the syncer to retry.
                self.start_one(s, &b, false);
                self.wait_running(s);
            }
        }
        self.reap(s);
    }

    /// Evicts clean buffers nobody holds, least recently used first, until
    /// the cache is within its limit.
    fn trim(&self) {
        if self.bytes.load(Ordering::Relaxed) <= self.limits.max {
            return;
        }
        // Down to seven eighths, so the sort is paid once per eighth.
        let target = self.limits.max - self.limits.max / 8;
        let mut idle: Vec<Arc<Buffer>> = self
            .buffers
            .lock()
            .values()
            .filter(|b| Arc::strong_count(b) == 1)
            .cloned()
            .collect();
        // Each read once: users move `used` while this sorts.
        idle.sort_by_cached_key(|b| b.used.load(Ordering::Relaxed));
        for b in idle {
            if self.bytes.load(Ordering::Relaxed) <= target {
                break;
            }
            // Not held, and clean; then (in `with`'s order: buffer, then
            // table) nobody else has found it: only the table and we
            // refer to it.
            let Some(g) = b.data.try_lock() else {
                continue;
            };
            if g.dirty.is_some() {
                continue;
            }
            let mut t = self.buffers.lock();
            if Arc::strong_count(&b) != 2 || !t.get(&b.offset).is_some_and(|x| Arc::ptr_eq(x, &b)) {
                continue;
            }
            t.remove(&b.offset);
            b.evicted.store(true, Ordering::Release);
            self.bytes.fetch_sub(g.bytes.len(), Ordering::Relaxed);
        }
    }

    /// Drops the buffers overlapping `[offset, offset + len)`, dirty or
    /// not: the blocks were freed, and may hold file data written past
    /// the cache next (a stale buffer written back would corrupt it).
    pub fn discard(&self, s: &Sleepable, offset: u64, len: usize) {
        let end = offset.saturating_add(len as u64);
        // A buffer starting before `offset` may reach into the range.
        let candidates: Vec<Arc<Buffer>> = self
            .buffers
            .lock()
            .range(..end)
            .rev()
            .take_while(|(o, _)| o.saturating_add(MAX_BUFFER) > offset)
            .map(|(_, b)| Arc::clone(b))
            .collect();
        for b in candidates {
            let mut data = b.data.lock(s);
            // A write of it under way must land before the blocks are
            // reused.
            self.settle(s, &b, &mut data, true);
            let blen = data.bytes.len();
            if b.offset.saturating_add(blen as u64) <= offset {
                continue;
            }
            let mut t = self.buffers.lock();
            if !t.get(&b.offset).is_some_and(|x| Arc::ptr_eq(x, &b)) {
                continue;
            }
            t.remove(&b.offset);
            b.evicted.store(true, Ordering::Release);
            drop(t);
            self.bytes.fetch_sub(blen, Ordering::Relaxed);
            if data.dirty.is_some() {
                self.dirty.fetch_sub(blen, Ordering::Relaxed);
            }
        }
    }

    /// Forgets every buffer (unmount, after `sync`); dirty ones are lost.
    pub fn invalidate(&self) {
        let gone = core::mem::take(&mut *self.buffers.lock());
        for b in gone.values() {
            b.evicted.store(true, Ordering::Release);
        }
        self.bytes.store(0, Ordering::Relaxed);
        self.dirty.store(0, Ordering::Relaxed);
        drop(gone);
    }
}

#[cfg(test)]
mod tests;
