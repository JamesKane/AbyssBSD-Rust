// SPDX-License-Identifier: BSD-2-Clause
//! Memory the kernel must keep, allocated as plain `EfiLoaderData`.
//!
//! The handoff data (`BootData`) and the log ring (`LogRing`) used to be
//! allocated with OS-defined memory types. Some firmware (the Gigabyte X870E
//! AORUS ELITE's AMI Aptio, UEFI 2.9) never returns from `ExitBootServices`
//! when the memory map holds such a type, so every allocation here is
//! `EfiLoaderData` and its address range is recorded; `handoff::finish`
//! relabels the recorded ranges in the map it hands the kernel.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use efi::{Allocate, MemoryType, Status, SystemTable};

/// Most ranges kept per kind, after coalescing neighbours.
const MAX: usize = 256;

/// A set of page ranges.
struct Set {
    starts: [AtomicU64; MAX],
    ends: [AtomicU64; MAX],
    len: AtomicUsize,
}

impl Set {
    const fn new() -> Self {
        Self {
            starts: [const { AtomicU64::new(0) }; MAX],
            ends: [const { AtomicU64::new(0) }; MAX],
            len: AtomicUsize::new(0),
        }
    }

    /// Adds `start..end`, merging it with a range it touches. False if the
    /// set is full.
    fn add(&self, start: u64, end: u64) -> bool {
        let len = self.len.load(Ordering::Relaxed);
        for i in 0..len {
            let (s, e) = (
                self.starts[i].load(Ordering::Relaxed),
                self.ends[i].load(Ordering::Relaxed),
            );
            if end == s {
                self.starts[i].store(start, Ordering::Relaxed);
                return true;
            }
            if start == e {
                self.ends[i].store(end, Ordering::Relaxed);
                return true;
            }
        }
        if len == MAX {
            return false;
        }
        self.starts[len].store(start, Ordering::Relaxed);
        self.ends[len].store(end, Ordering::Relaxed);
        self.len.store(len + 1, Ordering::Relaxed);
        true
    }

    fn get(&self, i: usize) -> Option<(u64, u64)> {
        (i < self.len.load(Ordering::Relaxed)).then(|| {
            (
                self.starts[i].load(Ordering::Relaxed),
                self.ends[i].load(Ordering::Relaxed),
            )
        })
    }
}

static BOOT_DATA: Set = Set::new();
static LOG_RING: Set = Set::new();

fn record(set: &Set, st: &SystemTable, addr: u64, pages: usize) -> Result<u64, Status> {
    if !set.add(addr, addr + pages as u64 * 4096) {
        // SAFETY: just allocated by the caller, unused.
        let _ = unsafe { st.free_pages(addr, pages) };
        return Err(Status::OUT_OF_RESOURCES);
    }
    Ok(addr)
}

/// Allocates `pages` of handoff data anywhere.
pub(crate) fn boot_data(st: &SystemTable, pages: usize) -> Result<u64, Status> {
    let addr = st.allocate_pages(Allocate::Anywhere, MemoryType::LOADER_DATA, pages)?;
    record(&BOOT_DATA, st, addr, pages)
}

/// Allocates the log ring at `at`, or anywhere.
pub(crate) fn log_ring(st: &SystemTable, at: Option<u64>, pages: usize) -> Result<u64, Status> {
    let how = at.map_or(Allocate::Anywhere, Allocate::At);
    let addr = st.allocate_pages(how, MemoryType::LOADER_DATA, pages)?;
    record(&LOG_RING, st, addr, pages)
}

/// The recorded handoff-data ranges.
pub(crate) fn boot_data_ranges() -> impl Iterator<Item = (u64, u64)> {
    (0..).map_while(|i| BOOT_DATA.get(i))
}

/// The recorded log-ring ranges.
pub(crate) fn log_ring_ranges() -> impl Iterator<Item = (u64, u64)> {
    (0..).map_while(|i| LOG_RING.get(i))
}
