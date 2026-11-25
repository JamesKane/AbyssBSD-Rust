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
    /// Which of the driver's rings the job ran on, or [`EXTERNAL`] for a
    /// fence that came from outside (an imported sync file). A ring orders
    /// its own jobs, so only the others' need waiting for.
    owner: u32,
}

/// The owner of a fence that did not come from the driver's own rings.
pub const EXTERNAL: u32 = u32::MAX;

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
    /// Adds a fence: `write` if the job writes the buffer, `owner` the ring it
    /// ran on (or [`EXTERNAL`]).
    pub fn add(&mut self, fence: Arc<dyn Fence>, write: bool, owner: u32) {
        if self.entries.len() >= PRUNE_AT {
            self.entries.retain(|e| !e.fence.signaled());
        }
        // A new write fence from the driver follows its earlier ones on the
        // same rings, but they may be on others: keep all, it is bounded.
        self.entries.push(Entry {
            fence,
            write,
            owner,
        });
    }

    /// The unfinished fences someone who will read (`write` false) or write
    /// (`write` true) the buffer has to wait for; those of ring `except`
    /// are left out when it is a job on that ring that waits (the ring
    /// orders its own).
    #[must_use]
    pub fn blocking(&self, write: bool, except: Option<u32>) -> Vec<Arc<dyn Fence>> {
        self.entries
            .iter()
            .filter(|e| (write || e.write) && except != Some(e.owner))
            .filter(|e| !e.fence.signaled())
            .map(|e| Arc::clone(&e.fence))
            .collect()
    }
}
