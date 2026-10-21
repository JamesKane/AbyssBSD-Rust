// SPDX-License-Identifier: BSD-2-Clause
//! A disk as a buffer cache's block device: synchronous transfers that
//! sleep on the request's completion (docs/VFS.md §8.2).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use bufcache::{BlockDev, BufCache, BufHook, IoError, Limits};
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;
use ufs::softdep::Deps;

/// A UFS mount's soft-updates tracker: taken with buffers held, after
/// them.
static SOFTDEP: LockClass = LockClass::new("softdep", 390);

/// The soft-updates tracker of a mount, which its buffer cache asks before
/// and after each write.
pub(crate) struct SoftDeps(Mutex<Deps>);

impl SoftDeps {
    pub(crate) fn new(sb: &ufs::Superblock) -> Arc<Self> {
        Arc::new(Self(Mutex::new(&SOFTDEP, Deps::new(sb))))
    }

    /// Runs `f` with the tracker locked.
    pub(crate) fn with<R>(&self, s: &Sleepable, f: impl FnOnce(&mut Deps) -> R) -> R {
        f(&mut self.0.lock(s))
    }
}

/// A UFS mount's buffer hook: cylinder groups' check-hashes, checked as
/// groups come from the disk and set as they go to it (FreeBSD's
/// `BX_CYLGRP` buffers), and with soft updates, the tracker's rollbacks.
struct UfsHook {
    sb: ufs::Superblock,
    deps: Option<Arc<SoftDeps>>,
}

impl BufHook for UfsHook {
    fn filled(&self, _s: &Sleepable, offset: u64, bytes: &[u8]) -> Result<(), IoError> {
        let Some(cg) = self.sb.group_at(offset) else {
            return Ok(());
        };
        ufs::cg::verify(&self.sb, bytes, cg).map_err(|e| {
            kprintln!("ufs: cylinder group {cg}: {e:?}");
            IoError
        })
    }

    fn start(&self, s: &Sleepable, offset: u64, bytes: &mut [u8]) -> Option<Vec<u8>> {
        if self.sb.group_at(offset).is_some() {
            ufs::cg::update_ckhash(&self.sb, bytes);
        }
        self.deps
            .as_ref()
            .and_then(|d| d.0.lock(s).write_start(offset, bytes))
    }

    fn done(&self, s: &Sleepable, offset: u64, ok: bool) {
        if let Some(d) = &self.deps {
            d.0.lock(s).write_done(offset, ok);
        }
    }
}

/// A disk under a buffer cache.
pub(crate) struct DiskBlocks(pub(crate) Arc<storage::Disk>);

impl BlockDev for DiskBlocks {
    fn sector_size(&self) -> u32 {
        self.0.sector_size()
    }

    fn size(&self) -> u64 {
        self.0.bytes()
    }

    fn read(&self, s: &Sleepable, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let data = self.0.read(s, offset, buf.len()).map_err(|_| IoError)?;
        buf.copy_from_slice(&data);
        Ok(())
    }

    fn write(&self, s: &Sleepable, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        self.0.write(s, offset, buf).map_err(|_| IoError)
    }

    fn flush(&self, s: &Sleepable) -> Result<(), IoError> {
        self.0.flush(s).map_err(|_| IoError)
    }
}

/// A buffer cache over `disk`, its dirty times on the syncer's clock.
pub(crate) fn cache(disk: Arc<storage::Disk>, limits: Limits) -> BufCache {
    BufCache::new(Arc::new(DiskBlocks(disk)), limits, crate::time::now_ns)
}

/// A disk under a file system: metadata through a buffer cache, file data
/// straight to the disk (the page cache holds it).
pub(crate) struct DiskIo {
    pub(crate) cache: BufCache,
    pub(crate) disk: Arc<storage::Disk>,
    /// With soft updates (UFS), the tracker its writes report to.
    deps: SpinLock<Option<Arc<SoftDeps>>>,
    /// UFS's cylinder-group check-hashes are kept by the cache's hook.
    groups_hashed: AtomicBool,
}

impl DiskIo {
    pub(crate) fn new(disk: Arc<storage::Disk>) -> Self {
        Self {
            cache: cache(Arc::clone(&disk), Limits::default()),
            disk,
            deps: SpinLock::new(None),
            groups_hashed: AtomicBool::new(false),
        }
    }

    /// Serves the UFS file system with superblock `sb` from now on: its
    /// groups' check-hashes kept here, and with `deps` (soft updates),
    /// its writes ordered by that tracker.
    pub(crate) fn use_ufs(&self, sb: &ufs::Superblock, deps: Option<&Arc<SoftDeps>>) {
        *self.deps.lock() = deps.cloned();
        self.cache.set_hook(Arc::new(UfsHook {
            sb: sb.clone(),
            deps: deps.cloned(),
        }));
        self.groups_hashed.store(true, Ordering::Release);
    }

    /// The soft-updates tracker, if writes go through one.
    pub(crate) fn deps(&self) -> Option<Arc<SoftDeps>> {
        self.deps.lock().clone()
    }

    /// The sector-aligned span covering `[offset, offset + len)`.
    fn span(&self, offset: u64, len: usize) -> (u64, usize) {
        let ss = u64::from(self.disk.sector_size());
        let start = offset - offset % ss;
        let end = (offset + len as u64).next_multiple_of(ss);
        (start, usize::try_from(end - start).unwrap_or(usize::MAX))
    }

    fn read_meta(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), IoError> {
        self.cache.read(s, offset, len, |b| f(b))
    }

    fn modify_meta(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        zero: bool,
        now: bool,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), IoError> {
        let fill = if zero {
            bufcache::Fill::Overwrite
        } else {
            bufcache::Fill::Read
        };
        let mode = if now {
            bufcache::WriteMode::Now
        } else {
            bufcache::WriteMode::Delayed
        };
        self.cache.write(s, offset, len, fill, mode, |b| f(b))
    }

    fn read_data(&self, s: &Sleepable, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let (start, len) = self.span(offset, buf.len());
        let data = self.disk.read(s, start, len).map_err(|_| IoError)?;
        let from = usize::try_from(offset - start).unwrap_or(0);
        buf.copy_from_slice(&data[from..from + buf.len()]);
        Ok(())
    }

    /// Writes file data straight to the disk; with soft updates, pointers
    /// to new blocks it fills may go out then.
    pub(crate) fn write_data(
        &self,
        s: &Sleepable,
        offset: u64,
        data: &[u8],
    ) -> Result<(), IoError> {
        let (start, len) = self.span(offset, data.len());
        if start == offset && len == data.len() {
            self.disk.write(s, offset, data).map_err(|_| IoError)?;
        } else {
            // Partial sectors: read, change, write.
            let mut buf = self.disk.read(s, start, len).map_err(|_| IoError)?;
            let from = usize::try_from(offset - start).unwrap_or(0);
            buf[from..from + data.len()].copy_from_slice(data);
            self.disk.write(s, start, &buf).map_err(|_| IoError)?;
        }
        if let Some(d) = self.deps() {
            let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
            d.with(s, |d| d.data_written(offset, len));
        }
        Ok(())
    }
}

impl DiskIo {
    /// As [`Self::write_data`] for several parts, whose writes go out
    /// together (a clustered write-back).
    pub(crate) fn write_data_parts(
        &self,
        s: &Sleepable,
        parts: &[(u64, &[u8])],
    ) -> Result<(), IoError> {
        let ss = u64::from(self.disk.sector_size());
        let aligned = |&(at, data): &(u64, &[u8])| {
            at.is_multiple_of(ss) && u64::try_from(data.len()).is_ok_and(|n| n.is_multiple_of(ss))
        };
        if !parts.iter().all(aligned) {
            for &(at, data) in parts {
                self.write_data(s, at, data)?;
            }
            return Ok(());
        }
        self.disk.write_parts(s, parts).map_err(|_| IoError)?;
        if let Some(d) = self.deps() {
            d.with(s, |d| {
                for &(at, data) in parts {
                    d.data_written(at, u64::try_from(data.len()).unwrap_or(u64::MAX));
                }
            });
        }
        Ok(())
    }
}

impl ufs::BlockIo for DiskIo {
    type Cx = Sleepable;

    fn read(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), ufs::Error> {
        self.read_meta(s, offset, len, f)
            .map_err(|_| ufs::Error::Io)
    }

    fn modify(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        fill: ufs::Fill,
        mode: ufs::Mode,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), ufs::Error> {
        self.modify_meta(
            s,
            offset,
            len,
            fill == ufs::Fill::Zero,
            mode == ufs::Mode::Sync,
            f,
        )
        .map_err(|_| ufs::Error::Io)
    }

    fn read_data(&self, s: &Sleepable, offset: u64, buf: &mut [u8]) -> Result<(), ufs::Error> {
        DiskIo::read_data(self, s, offset, buf).map_err(|_| ufs::Error::Io)
    }

    fn write_data(&self, s: &Sleepable, offset: u64, data: &[u8]) -> Result<(), ufs::Error> {
        DiskIo::write_data(self, s, offset, data).map_err(|_| ufs::Error::Io)
    }

    fn forget(&self, s: &Sleepable, offset: u64, len: usize) {
        self.cache.discard(s, offset, len);
    }

    fn flush(&self, s: &Sleepable) -> Result<(), ufs::Error> {
        self.cache.sync(s).map_err(|_| ufs::Error::Io)
    }

    fn deps(&self, s: &Sleepable, f: &mut dyn FnMut(&mut Deps)) -> bool {
        match DiskIo::deps(self) {
            Some(d) => {
                d.with(s, |d| f(d));
                true
            }
            None => false,
        }
    }

    fn hashes_groups(&self) -> bool {
        self.groups_hashed.load(Ordering::Acquire)
    }
}

impl fat::fs::BlockIo for DiskIo {
    type Cx = Sleepable;

    fn read(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), fat::fs::Error> {
        self.read_meta(s, offset, len, f)
            .map_err(|_| fat::fs::Error::Io)
    }

    fn modify(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        fill: fat::fs::Fill,
        mode: fat::fs::Mode,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), fat::fs::Error> {
        self.modify_meta(
            s,
            offset,
            len,
            fill == fat::fs::Fill::Zero,
            mode == fat::fs::Mode::Sync,
            f,
        )
        .map_err(|_| fat::fs::Error::Io)
    }

    fn write_data(&self, s: &Sleepable, offset: u64, data: &[u8]) -> Result<(), fat::fs::Error> {
        DiskIo::write_data(self, s, offset, data).map_err(|_| fat::fs::Error::Io)
    }

    fn forget(&self, s: &Sleepable, offset: u64, len: usize) {
        self.cache.discard(s, offset, len);
    }

    fn flush(&self, s: &Sleepable) -> Result<(), fat::fs::Error> {
        self.cache.sync(s).map_err(|_| fat::fs::Error::Io)
    }
}
