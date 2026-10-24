// SPDX-License-Identifier: BSD-2-Clause
//! Transfers: owned requests, completed exactly once (DRIVER-MODEL.md
//! §9.1, USB.md §5.4).
//!
//! A [`Transfer`] carries its buffer and its completion. The controller
//! completes it with the bytes moved or an error, which hands the buffer
//! back to the completion. Dropping one uncompleted completes it with
//! [`UsbError::Cancelled`], so a bug cannot strand a waiter.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vm::object::IoBuf;

use crate::UsbError;

/// What a completion gets: the buffer, and the bytes moved or the error.
pub type Done = Box<dyn FnOnce(IoBuf, Result<usize, UsbError>) + Send>;

/// One transfer on one endpoint.
pub struct Transfer {
    /// The data: read into (IN) or written from (OUT). Its length is the
    /// transfer's.
    pub buf: IoBuf,
    /// Milliseconds before the controller cancels it with
    /// [`UsbError::TimedOut`]; 0 for none (an interrupt endpoint polled
    /// until data comes).
    pub timeout_ms: u32,
    /// The bulk stream it goes on, on an endpoint with streams
    /// ([`HostController::open_streams`](crate::HostController::open_streams));
    /// 0 otherwise.
    pub stream: u16,
    done: Option<Done>,
}

impl core::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Transfer")
            .field("len", &self.buf.len())
            .field("timeout_ms", &self.timeout_ms)
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

impl Transfer {
    /// A transfer of `buf` whose completion calls `done`.
    pub fn new(
        buf: IoBuf,
        timeout_ms: u32,
        done: impl FnOnce(IoBuf, Result<usize, UsbError>) + Send + 'static,
    ) -> Self {
        Self {
            buf,
            timeout_ms,
            stream: 0,
            done: Some(Box::new(done)),
        }
    }

    /// The same, on bulk stream `stream`.
    #[must_use]
    pub fn on_stream(mut self, stream: u16) -> Self {
        self.stream = stream;
        self
    }

    /// Its length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether it moves no data.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.len() == 0
    }

    /// Completes it: `Ok` with the bytes moved, or the error.
    pub fn complete(mut self, r: Result<usize, UsbError>) {
        if let Some(done) = self.done.take() {
            done(core::mem::take(&mut self.buf), r);
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done(core::mem::take(&mut self.buf), Err(UsbError::Cancelled));
        }
    }
}

/// A submission the controller refused, with the transfer handed back
/// uncompleted.
#[derive(Debug)]
pub struct Rejected {
    /// The transfer.
    pub transfer: Transfer,
    /// Why.
    pub error: UsbError,
}

/// A completion a thread waits for: [`Waiter::transfer`] makes the
/// transfer, [`Waiter::wait`] sleeps until it completes.
#[derive(Debug)]
pub struct Waiter {
    done: AtomicBool,
    result: SpinLock<Option<(IoBuf, Result<usize, UsbError>)>>,
    queue: WaitQueue,
}

impl Waiter {
    /// A new waiter.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            done: AtomicBool::new(false),
            result: SpinLock::new(None),
            queue: WaitQueue::new(),
        })
    }

    /// A transfer of `buf` that completes into this waiter.
    #[must_use]
    pub fn transfer(self: &Arc<Self>, buf: IoBuf, timeout_ms: u32) -> Transfer {
        let me = Arc::clone(self);
        Transfer::new(buf, timeout_ms, move |buf, r| {
            *me.result.lock() = Some((buf, r));
            me.done.store(true, Ordering::Release);
            me.queue.wake_all();
        })
    }

    /// Whether the transfer has completed.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Sleeps until the transfer completes; returns its buffer and
    /// result.
    pub fn wait(&self, s: &Sleepable) -> (IoBuf, Result<usize, UsbError>) {
        self.queue
            .wait_until(s, || self.done.load(Ordering::Acquire));
        self.result
            .lock()
            .take()
            .unwrap_or((IoBuf::Owned(Vec::new()), Err(UsbError::Cancelled)))
    }
}
