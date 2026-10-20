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

use alloc::sync::Arc;

use abi::errno::Errno;
use vm::object::{LookupError, Object};

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

/// Zeros for holes.
static ZEROS: [u8; PAGE_BYTES] = [0; PAGE_BYTES];

/// Reads from `offset` into `uio`, up to the file's `size`; returns the
/// bytes read (0 at or past the end).
pub fn read(object: &Object, size: u64, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
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

/// Writes `uio` at `offset` into the pages; returns the bytes written. The
/// file system has made room (and will set the size from the result).
pub fn write(object: &Object, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
    let mut done = 0;
    let mut at = offset;
    while uio.resid() > 0 {
        let index = at / PAGE;
        let within = offset_in_page(at);
        let n = (PAGE_BYTES - within).min(uio.resid());
        let pfn = object.page_for_write(index).map_err(errno)?;
        let mut r = Ok(0);
        object.access(pfn, &mut |p| r = uio.copy_in(&mut p[within..within + n]));
        let moved = r?;
        object.mark_dirty(index);
        done += moved;
        at += moved as u64;
        if moved < n {
            break;
        }
    }
    Ok(done)
}

/// The file shrinks or grows from `old` to `new` bytes: pages wholly past
/// the end go (if no mapping still holds the object), and the part of the
/// last page past the end is zeroed, so growing again reads zeros.
pub fn resize(object: &Arc<Object>, old: u64, new: u64) {
    if new < old {
        object.truncate(new.div_ceil(PAGE));
    }
    let within = offset_in_page(new.min(old));
    if within != 0
        && let Some(pfn) = object.page_here(new.min(old) / PAGE)
    {
        object.access(pfn, &mut |p| p[within..].fill(0));
    }
}
