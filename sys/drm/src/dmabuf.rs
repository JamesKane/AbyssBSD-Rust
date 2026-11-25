// SPDX-License-Identifier: BSD-2-Clause
//! Sharing buffers: a dma-buf is a buffer that a file descriptor names.
//!
//! The descriptor can be sent over a socket, mapped, and imported by
//! another process (or another card) as a handle of its own, which is
//! how a compositor and a client pass frames. This is the same-device
//! core of it: export, import, map. Fences, per-device attachments and
//! cache maintenance between devices come with the fence work.
//!
//! Behavior reference: `docs/GRAPHICS.md` §4.4.

use alloc::sync::Arc;

use abi::errno::Errno;
use vm::object::{Object, PageIndex};

use crate::gem::Gem;

/// What a dma-buf descriptor holds: a reference to the buffer. The
/// buffer lives as long as any handle, mapping or descriptor does, and
/// outlives the card that made it.
#[derive(Debug)]
pub struct DmaBuf {
    gem: Arc<Gem>,
}

impl DmaBuf {
    /// A dma-buf of `gem`.
    #[must_use]
    pub fn new(gem: Arc<Gem>) -> Arc<Self> {
        Arc::new(Self { gem })
    }

    /// The buffer it names.
    #[must_use]
    pub fn gem(&self) -> &Arc<Gem> {
        &self.gem
    }

    /// Its size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.gem.size()
    }

    /// Its memory for a mapping of `pages` pages from page `page` of the
    /// descriptor: the object, and the object page the mapping starts at.
    ///
    /// # Errors
    ///
    /// `EINVAL` when the range leaves the buffer.
    pub fn mapping(&self, page: u64, pages: u64) -> Result<(Arc<Object>, PageIndex), Errno> {
        self.gem.mapping(page, pages)
    }
}

/// The sync file requests on a dma-buf: `EXPORT_SYNC_FILE` makes a sync file
/// of the fences someone who will read (`DMA_BUF_SYNC_READ`) or write
/// (`DMA_BUF_SYNC_WRITE`) the buffer waits for; `IMPORT_SYNC_FILE` adds a
/// sync file's fences to the buffer as those of a reader or a writer.
///
/// # Errors
///
/// `ENOTTY` for any other request; `EINVAL` for flags that are not read,
/// write or both; descriptor errors as `export_syncfile` and
/// `import_syncfile` give them.
pub fn ioctl(buf: &DmaBuf, s: &sched::Sleepable, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    use abi::drm::{
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, DMA_BUF_SYNC_READ,
        DMA_BUF_SYNC_RW, DMA_BUF_SYNC_WRITE,
    };
    let env = crate::env().ok_or(Errno::ENXIO)?;
    if arg.len() < 8 {
        return Err(Errno::EINVAL);
    }
    let flags = u32::from_le_bytes([arg[0], arg[1], arg[2], arg[3]]);
    // At least one of read and write, and nothing else.
    if flags == 0 || u64::from(flags) & !DMA_BUF_SYNC_RW != 0 {
        return Err(Errno::EINVAL);
    }
    let writes = u64::from(flags) & DMA_BUF_SYNC_WRITE != 0;
    let _ = DMA_BUF_SYNC_READ;
    match req {
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE => {
            // A writer waits for every fence, a reader for the writers'.
            let fences = buf.gem().reservation().lock().blocking(writes, false);
            let file = crate::SyncFile::new(fences);
            let fd = (env.export_syncfile)(s, file, true)?;
            arg[4..8].copy_from_slice(&fd.to_le_bytes());
            Ok(0)
        }
        DMA_BUF_IOCTL_IMPORT_SYNC_FILE => {
            let fd = i32::from_le_bytes([arg[4], arg[5], arg[6], arg[7]]);
            let file = (env.import_syncfile)(s, fd)?;
            // Done already: nothing to add.
            if !file.signaled() {
                buf.gem()
                    .reservation()
                    .lock()
                    .add(file.fence(), writes, true);
            }
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    }
}
