// SPDX-License-Identifier: BSD-2-Clause
//! Block requests.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use sched::{Sleepable, WaitQueue};
use sync::SpinLock;

/// What a request does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BioOp {
    /// Read into the buffer.
    Read,
    /// Write the buffer.
    Write,
    /// Make earlier writes durable.
    Flush,
}

/// Why a request failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BioError {
    /// The device failed it (`EIO`).
    Io,
    /// The disk is gone: detached, or detaching (`ENXIO`).
    Gone,
    /// Misaligned, out of range, or too large (`EINVAL`).
    Invalid,
    /// A write to a read-only disk (`EROFS`).
    ReadOnly,
}

impl BioError {
    /// The errno FreeBSD returns for it.
    #[must_use]
    pub fn errno(self) -> i32 {
        match self {
            Self::Io => 5,
            Self::Gone => 6,
            Self::Invalid => 22,
            Self::ReadOnly => 30,
        }
    }
}

/// What completing a request calls: the buffer back, and the outcome.
pub type Done = Box<dyn FnOnce(Vec<u8>, Result<(), BioError>) + Send>;

/// Requests dropped without being completed (driver bugs).
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Verifier mode: a dropped request is a panic, not a count.
static VERIFIER: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Turns verifier mode on or off (DRIVER-MODEL.md §13.1).
pub fn set_verifier(on: bool) {
    VERIFIER.store(on, Ordering::Relaxed);
}

/// Requests dropped without being completed since boot.
#[must_use]
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// An owned block request. Completing it consumes it.
pub struct Bio {
    /// What it does.
    pub op: BioOp,
    /// Its byte offset on the disk it is sent to (partitions add theirs).
    pub offset: u64,
    /// The data, read into or written from; its length is the transfer's.
    /// A driver may take it for DMA and must put it back before
    /// completing.
    pub buf: Vec<u8>,
    done: Option<Done>,
}

impl core::fmt::Debug for Bio {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bio")
            .field("op", &self.op)
            .field("offset", &self.offset)
            .field("len", &self.buf.len())
            .finish_non_exhaustive()
    }
}

impl Bio {
    /// A request whose completion calls `done`.
    pub fn new(
        op: BioOp,
        offset: u64,
        buf: Vec<u8>,
        done: impl FnOnce(Vec<u8>, Result<(), BioError>) + Send + 'static,
    ) -> Self {
        Self {
            op,
            offset,
            buf,
            done: Some(Box::new(done)),
        }
    }

    /// Bytes transferred.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether it transfers nothing (a flush).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Completes the request.
    pub fn complete(mut self, r: Result<(), BioError>) {
        if let Some(done) = self.done.take() {
            done(core::mem::take(&mut self.buf), r);
        }
    }
}

impl Drop for Bio {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            assert!(
                !VERIFIER.load(Ordering::Relaxed),
                "verifier: a {:?} of {} bytes at {:#x} was dropped uncompleted",
                self.op,
                self.buf.len(),
                self.offset
            );
            done(core::mem::take(&mut self.buf), Err(BioError::Io));
        }
    }
}

/// A finished request: its buffer and outcome.
type Finished = (Vec<u8>, Result<(), BioError>);

/// A completion a thread can sleep on.
pub(crate) struct Waiter {
    slot: SpinLock<Option<Finished>>,
    queue: WaitQueue,
}

impl Waiter {
    /// A request completing into a new waiter.
    pub(crate) fn bio(op: BioOp, offset: u64, buf: Vec<u8>) -> (Bio, Arc<Self>) {
        let w = Arc::new(Self {
            slot: SpinLock::new(None),
            queue: WaitQueue::new(),
        });
        let back = Arc::clone(&w);
        let bio = Bio::new(op, offset, buf, move |buf, r| {
            *back.slot.lock() = Some((buf, r));
            back.queue.wake_all();
        });
        (bio, w)
    }

    /// Sleeps until the request completes.
    pub(crate) fn wait(&self, s: &Sleepable) -> Finished {
        let mut out = None;
        self.queue.wait_until(s, || {
            out = self.slot.lock().take();
            out.is_some()
        });
        out.expect("completion")
    }
}
