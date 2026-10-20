// SPDX-License-Identifier: BSD-2-Clause
//! A disk as a buffer cache's block device: synchronous transfers that
//! sleep on the request's completion (docs/VFS.md §8.2).

use alloc::sync::Arc;

use bufcache::{BlockDev, BufCache, IoError, Limits};
use sched::Sleepable;

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
}

impl DiskIo {
    pub(crate) fn new(disk: Arc<storage::Disk>) -> Self {
        Self {
            cache: cache(Arc::clone(&disk), Limits::default()),
            disk,
        }
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

    fn write_data(&self, s: &Sleepable, offset: u64, data: &[u8]) -> Result<(), IoError> {
        let (start, len) = self.span(offset, data.len());
        if start == offset && len == data.len() {
            return self.disk.write(s, offset, data).map_err(|_| IoError);
        }
        // Partial sectors: read, change, write.
        let mut buf = self.disk.read(s, start, len).map_err(|_| IoError)?;
        let from = usize::try_from(offset - start).unwrap_or(0);
        buf[from..from + data.len()].copy_from_slice(data);
        self.disk.write(s, start, &buf).map_err(|_| IoError)
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
