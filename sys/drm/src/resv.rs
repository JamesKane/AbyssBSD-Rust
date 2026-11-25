// SPDX-License-Identifier: BSD-2-Clause
//! A buffer's reservation: the fences of the jobs that use it, by how they
//! use it. A job that writes the buffer adds a write fence; one that only
//! reads it adds a read fence. Someone who wants to read the buffer must
//! wait for the write fences; someone who wants to write it, for all of
//! them. That is implicit synchronisation, and `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`
//! and `IMPORT_SYNC_FILE` are how another program takes part in it.
//!
//! Behavior reference: docs/GRAPHICS.md §5.1 (`Reservation`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::syncobj::Fence;

/// Most fences a reservation keeps before it drops the finished ones.
const PRUNE_AT: usize = 16;

/// One fence on a reservation.
struct Entry {
    fence: Arc<dyn Fence>,
    /// A writer's fence (a reader's if not).
    write: bool,
    /// It came from outside this driver (an imported sync file), so the
    /// driver's own ordering does not cover it and a job must wait for it.
    external: bool,
}

/// The fences on a buffer.
#[derive(Default)]
pub struct Reservation {
    entries: Vec<Entry>,
}

impl core::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Reservation({})", self.entries.len())
    }
}

impl Reservation {
    /// Adds a fence: `write` if the job writes the buffer, `external` if it
    /// is not one of the driver's own.
    pub fn add(&mut self, fence: Arc<dyn Fence>, write: bool, external: bool) {
        if self.entries.len() >= PRUNE_AT {
            self.entries.retain(|e| !e.fence.signaled());
        }
        // A new write fence from the driver follows its earlier ones on the
        // same rings, but they may be on others: keep all, it is bounded.
        self.entries.push(Entry {
            fence,
            write,
            external,
        });
    }

    /// The unfinished fences someone who will read (`write` false) or write
    /// (`write` true) the buffer has to wait for; the external ones only
    /// when `external_only`.
    #[must_use]
    pub fn blocking(&self, write: bool, external_only: bool) -> Vec<Arc<dyn Fence>> {
        self.entries
            .iter()
            .filter(|e| (write || e.write) && (!external_only || e.external))
            .filter(|e| !e.fence.signaled())
            .map(|e| Arc::clone(&e.fence))
            .collect()
    }
}
