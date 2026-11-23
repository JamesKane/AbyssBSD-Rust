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
