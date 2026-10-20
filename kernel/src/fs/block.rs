// SPDX-License-Identifier: BSD-2-Clause
//! A disk as a buffer cache's block device: synchronous transfers that
//! sleep on the request's completion (docs/VFS.md §8.2).

#![cfg_attr(not(feature = "ktest"), allow(dead_code))] // UFS (P5.4) mounts on it

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
