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
//! Past a high watermark of dirty bytes, the writer that crosses it writes
//! the oldest until the total is under the low one (FreeBSD's
//! `hidirtybuffers`), so a burst of writes cannot fill memory. Clean
//! buffers nobody holds are evicted, least recently used first, past the
//! cache's size limit.
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

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;

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
}

#[derive(Debug)]
struct Buffer {
    offset: u64,
    data: Mutex<Data>,
    /// The tick of its last use, for eviction.
    used: AtomicU64,
}

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
    buffers: SpinLock<BTreeMap<u64, Arc<Buffer>>>,
    limits: Limits,
    /// Bytes held, and bytes dirty.
    bytes: AtomicUsize,
    dirty: AtomicUsize,
    ticks: AtomicU64,
    /// Nanoseconds since some start, for dirtying times.
    clock: fn() -> u64,
    hook: SpinLock<Option<Arc<dyn BufHook>>>,
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
            buffers: SpinLock::new(BTreeMap::new()),
            limits,
            bytes: AtomicUsize::new(0),
            dirty: AtomicUsize::new(0),
            ticks: AtomicU64::new(0),
            clock,
            hook: SpinLock::new(None),
        }
    }

    /// Has every write from now on go through `hook`.
    pub fn set_hook(&self, hook: Arc<dyn BufHook>) {
        *self.hook.lock() = Some(hook);
    }

    /// Writes held buffer `d` at `offset` (dirty or not), through the
    /// hook: clean afterwards, unless the hook rolled something back or
    /// the device failed.
    fn write_out(&self, s: &Sleepable, offset: u64, d: &mut Data) -> Result<(), IoError> {
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
        f: impl FnOnce(&mut Data) -> Result<R, IoError>,
    ) -> Result<R, IoError> {
        self.check(offset, size)?;
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
                                },
                            ),
                            used: AtomicU64::new(0),
                        });
                        t.insert(offset, Arc::clone(&b));
                        self.bytes.fetch_add(size, Ordering::Relaxed);
                        b
                    }
                }
            };
            let mut data = buf.data.lock(s);
            // It may have been evicted while we waited for it.
            let current = self
                .buffers
                .lock()
                .get(&offset)
                .is_some_and(|b| Arc::ptr_eq(b, &buf));
            if !current {
                continue;
            }
            if data.bytes.len() != size {
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
            let r = f(&mut data);
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
        self.with(s, offset, size, |d| {
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
        let r = self.with(s, offset, size, |d| {
            match fill {
                Fill::Read => self.validate(s, offset, d)?,
                Fill::Overwrite => d.valid = true,
            }
            let r = f(&mut d.bytes);
            match mode {
                WriteMode::Now => self.write_out(s, offset, d)?,
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
    /// A buffer someone holds is a candidate (as the oldest) only if
    /// `busy`: whoever waits for it then writes it if it is still dirty.
    fn dirty_buffers(&self, busy: bool) -> Vec<(u64, Arc<Buffer>)> {
        let all: Vec<Arc<Buffer>> = self.buffers.lock().values().cloned().collect();
        let mut out: Vec<(u64, Arc<Buffer>)> = all
            .into_iter()
            .filter_map(|b| {
                // A peek only to choose; the write looks again under the
                // lock.
                let d = match b.data.try_lock() {
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

    /// Writes buffer `b` if it is still dirty.
    fn write_one(&self, s: &Sleepable, b: &Buffer) -> Result<(), IoError> {
        let mut data = b.data.lock(s);
        if data.dirty.is_some() {
            self.write_out(s, b.offset, &mut data)?;
        }
        Ok(())
    }

    /// Writes the buffer at `offset` now if it is cached and dirty
    /// (`bwrite` of a block `fsync` needs). Returns whether it is clean
    /// afterwards.
    pub fn write_buffer(&self, s: &Sleepable, offset: u64) -> Result<bool, IoError> {
        let Some(b) = self.buffers.lock().get(&offset).cloned() else {
            return Ok(true);
        };
        let mut data = b.data.lock(s);
        if data.dirty.is_some() {
            self.write_out(s, offset, &mut data)?;
        }
        Ok(data.dirty.is_none())
    }

    /// Whether any buffer is dirty.
    #[must_use]
    pub fn any_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed) != 0
    }

    /// Writes the buffers dirtied at or before `before` on the cache's
    /// clock (the syncer: those older than its delay).
    pub fn write_aged(&self, s: &Sleepable, before: u64) -> Result<(), IoError> {
        let mut result = Ok(());
        for (d, b) in self.dirty_buffers(before == u64::MAX) {
            if d > before {
                break;
            }
            if let Err(e) = self.write_one(s, &b) {
                result = Err(e);
            }
        }
        result
    }

    /// Writes every dirty buffer, then flushes the device (`sync`,
    /// unmount).
    pub fn sync(&self, s: &Sleepable) -> Result<(), IoError> {
        self.write_aged(s, u64::MAX)?;
        self.dev.flush(s)
    }

    /// Past the high watermark: writes the oldest dirty buffers until the
    /// total is under the low one.
    fn throttle(&self, s: &Sleepable) {
        if self.dirty.load(Ordering::Relaxed) <= self.limits.hi_dirty {
            return;
        }
        for (_, b) in self.dirty_buffers(false) {
            if self.dirty.load(Ordering::Relaxed) <= self.limits.lo_dirty {
                break;
            }
            // A failed write stays dirty, for the syncer to retry.
            let _ = self.write_one(s, &b);
        }
    }

    /// Evicts clean buffers nobody holds, least recently used first, until
    /// the cache is within its limit.
    fn trim(&self) {
        if self.bytes.load(Ordering::Relaxed) <= self.limits.max {
            return;
        }
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
            if self.bytes.load(Ordering::Relaxed) <= self.limits.max {
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
            let data = b.data.lock(s);
            let blen = data.bytes.len();
            if b.offset.saturating_add(blen as u64) <= offset {
                continue;
            }
            let mut t = self.buffers.lock();
            if !t.get(&b.offset).is_some_and(|x| Arc::ptr_eq(x, &b)) {
                continue;
            }
            t.remove(&b.offset);
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
        self.bytes.store(0, Ordering::Relaxed);
        self.dirty.store(0, Ordering::Relaxed);
        drop(gone);
    }
}

#[cfg(test)]
mod tests;
