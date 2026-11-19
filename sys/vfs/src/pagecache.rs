// SPDX-License-Identifier: BSD-2-Clause
//! File data through a VM object's pages (docs/VFS.md §8.1): the page
//! cache. `read`, `write`, and `mmap` share the object, so a mapping sees
//! a write at once and nothing is cached twice.
//!
//! A file system keeps the size; these move bytes between the caller and
//! the pages. Pages a pager supplies are read on first touch; in an object
//! with no pager (tmpfs), a page never written reads as zeros without
//! being made. Written pages are marked dirty when the object's pages go
//! back to a pager.
//!
//! Each transfer holds the object's I/O guard, so reclaiming never frees a
//! frame being copied. A writer that finds more than [`dirty_limit`] pages
//! dirty in all files writes its own file's back before returning
//! (FreeBSD's dirty-buffer throttling, by file rather than by buffer), so
//! a burst of writes cannot fill memory faster than the disk drains it.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use abi::errno::Errno;
use vm::object::{LookupError, Object, PageIndex};

use crate::Uio;

const PAGE: u64 = vm::PAGE_SIZE;
/// [`PAGE`], as an index into a page.
const PAGE_BYTES: usize = 4096;

/// Where in its page byte `at` falls.
fn offset_in_page(at: u64) -> usize {
    usize::try_from(at % PAGE).unwrap_or(0)
}

fn errno(e: LookupError) -> Errno {
    match e {
        LookupError::OutOfMemory => Errno::ENOMEM,
        LookupError::PastEnd | LookupError::Io => Errno::EIO,
    }
}

/// Dirty pages in all files past which writers write back their own.
static DIRTY_LIMIT: AtomicUsize = AtomicUsize::new(8192);

/// The dirty-page limit for writers.
#[must_use]
pub fn dirty_limit() -> usize {
    DIRTY_LIMIT.load(Ordering::Relaxed)
}

/// Sets the dirty-page limit (the kernel sizes it from memory).
pub fn set_dirty_limit(pages: usize) {
    DIRTY_LIMIT.store(pages.max(1), Ordering::Relaxed);
}

/// Zeros for holes.
static ZEROS: [u8; PAGE_BYTES] = [0; PAGE_BYTES];

/// Reads from `offset` into `uio`, up to the file's `size`; returns the
/// bytes read (0 at or past the end).
pub fn read(object: &Object, size: u64, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
    let _io = object.io();
    let mut done = 0;
    let mut at = offset;
    while at < size && uio.resid() > 0 {
        let index = at / PAGE;
        let within = offset_in_page(at);
        let n = (PAGE_BYTES - within)
            .min(uio.resid())
            .min(usize::try_from(size - at).unwrap_or(usize::MAX));
        let page = match object.page_here(index) {
            Some(p) => Some(p),
            // A hole in an object with no pager: zeros, made by nobody.
            None if !object.has_pager() => None,
            None => Some(object.fill(index).map_err(errno)?),
        };
        let moved = match page {
            Some(pfn) => {
                let mut r = Ok(0);
                object.access(pfn, &mut |p| r = uio.copy_out(&p[within..within + n]));
                r?
            }
            None => uio.copy_out(&ZEROS[within..within + n])?,
        };
        done += moved;
        at += moved as u64;
        if moved < n {
            break;
        }
    }
    Ok(done)
}

/// Whole pages a write looks up at once.
const RUN: usize = 16;

/// The pages a write has put bytes in and not yet marked dirty: a run of
/// consecutive ones, marked when another page does not continue it and
/// when this is dropped.
struct Written<'a> {
    object: &'a Object,
    first: PageIndex,
    n: u64,
    /// New pages the write filled, not yet in the object: put there
    /// together, before any is marked.
    new: [(PageIndex, vm::Pfn); RUN],
    new_n: usize,
}

impl Written<'_> {
    /// A new page, filled, for `index`.
    fn made(&mut self, index: PageIndex, pfn: vm::Pfn) {
        if self.new_n == RUN {
            self.settle();
        }
        self.new[self.new_n] = (index, pfn);
        self.new_n += 1;
    }

    /// The new pages into the object.
    fn settle(&mut self) {
        self.object.insert_filled_run(&self.new[..self.new_n]);
        self.new_n = 0;
    }

    fn add(&mut self, index: PageIndex) {
        if self.n != 0 && self.first.checked_add(self.n) != Some(index) {
            self.settle();
            self.object.mark_dirty_run(self.first, self.n);
            self.n = 0;
        }
        if self.n == 0 {
            self.first = index;
        }
        self.n += 1;
    }
}

impl Drop for Written<'_> {
    fn drop(&mut self) {
        self.settle();
        self.object.mark_dirty_run(self.first, self.n);
    }
}

/// Writes `uio` at `offset` into the pages; returns the bytes written. The
/// file system has made room (and will set the size from the result).
pub fn write(object: &Object, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
    write_fresh(object, offset, uio, &|_| false)
}

/// As [`write`], where `fresh` says which pages hold nothing on the disk
/// (past the old end, or in space just allocated): those start from zeros
/// instead of being read, as do pages the write covers whole. A copy that
/// faults partway leaves the rest of such a page zero, as FreeBSD's
/// `ffs_write` clears a buffer it did not read.
pub fn write_fresh(
    object: &Object,
    offset: u64,
    uio: &mut dyn Uio,
    fresh: &dyn Fn(PageIndex) -> bool,
) -> Result<usize, Errno> {
    let io = object.io();
    let mut done = 0;
    let mut at = offset;
    // The pages written, marked dirty a run at a time: once a call that
    // writes in order, and on every way out.
    let mut dirty = Written {
        object,
        first: 0,
        n: 0,
        new: [(0, vm::Pfn(0)); RUN],
        new_n: 0,
    };
    'pages: while uio.resid() > 0 {
        let index = at / PAGE;
        let within = offset_in_page(at);
        let n = (PAGE_BYTES - within).min(uio.resid());
        if n == PAGE_BYTES {
            // Pages covered whole, a run of them at a time: the ones held
            // are found under one hold of the object's lock.
            let mut held = [None; RUN];
            let run = (uio.resid() / PAGE_BYTES).min(RUN);
            object.pages_for_overwrite(index, &mut held[..run]);
            for (k, held) in (0u64..).zip(&held[..run]) {
                let mut r = Ok(0);
                let moved = if let Some(pfn) = *held {
                    object.access(pfn, &mut |p| r = uio.copy_in(p));
                    r?
                } else {
                    // A new page takes the bytes straight, with no zeroing
                    // first (what a fault leaves uncopied reads as zeros).
                    let new = object
                        .alloc_filled(&mut |p| {
                            r = uio.copy_in(p);
                            let got = *r.as_ref().unwrap_or(&0);
                            p[got..].fill(0);
                        })
                        .ok_or(Errno::ENOMEM)?;
                    // In the object with the run's others, whatever the
                    // copy came to.
                    dirty.made(index + k, new);
                    r?
                };
                dirty.add(index + k);
                done += moved;
                at += moved as u64;
                if moved < PAGE_BYTES {
                    break 'pages;
                }
            }
            continue;
        }
        let mut r = Ok(0);
        let pfn = if fresh(index) {
            object.page_for_overwrite(index)
        } else {
            object.page_for_write(index)
        }
        .map_err(errno)?;
        object.access(pfn, &mut |p| r = uio.copy_in(&mut p[within..within + n]));
        let moved = r?;
        dirty.add(index);
        done += moved;
        at += moved as u64;
        if moved < n {
            break;
        }
    }
    drop(dirty);
    drop(io);
    if vm::object::dirty_total() > dirty_limit() {
        // A failure leaves the pages dirty, for fsync or the syncer to
        // report.
        let _ = object.sync(0, u64::MAX);
    }
    Ok(done)
}

/// The file shrinks or grows from `old` to `new` bytes: pages wholly past
/// the end go (if no mapping still holds the object), and the part of the
/// last page past the end is zeroed, so growing again reads zeros.
pub fn resize(object: &Arc<Object>, old: u64, new: u64) {
    let _io = object.io();
    if new < old {
        object.truncate(new.div_ceil(PAGE));
    }
    let within = offset_in_page(new.min(old));
    if within != 0
        && let Some(pfn) = object.page_here(new.min(old) / PAGE)
    {
        object.forget_synced(new.min(old) / PAGE);
        object.access(pfn, &mut |p| p[within..].fill(0));
    }
}
