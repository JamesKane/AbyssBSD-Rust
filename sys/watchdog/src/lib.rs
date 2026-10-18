// SPDX-License-Identifier: BSD-2-Clause
//! The watchdog class: hardware timers that reset the machine unless
//! patted, which turn hangs into resets (FreeBSD's watchdog(9); the
//! `/dev/fido` interface comes with devfs).
//!
//! Drivers register their watchdogs; [`first`] gives the one to use.
//!
//! Behavior reference: sys/dev/watchdog/watchdog.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use sync::SpinLock;

/// Why a watchdog could not be armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchdogError {
    /// The timeout is outside what the hardware can count (`EINVAL`).
    OutOfRange,
    /// Firmware or a strap keeps it from resetting the machine.
    Disabled,
}

/// A hardware watchdog.
pub trait Watchdog: Send + Sync {
    /// Its name (`ichwd0`).
    fn name(&self) -> &str;
    /// Arms it (or re-arms it) to expire `timeout_ms` from now, returning
    /// the timeout it actually counts (the hardware's granularity).
    fn arm(&self, timeout_ms: u64) -> Result<u64, WatchdogError>;
    /// Restarts the countdown.
    fn pat(&self);
    /// Stops it.
    fn disarm(&self);
    /// Time left before it expires, if the hardware says.
    fn remaining_ms(&self) -> Option<u64> {
        None
    }
}

static WATCHDOGS: SpinLock<Vec<Arc<dyn Watchdog>>> = SpinLock::new(Vec::new());

/// Registers `w`.
pub fn register(w: Arc<dyn Watchdog>) {
    WATCHDOGS.lock().push(w);
}

/// Unregisters `w` (its driver is detaching).
pub fn unregister(w: &Arc<dyn Watchdog>) {
    WATCHDOGS.lock().retain(|x| !Arc::ptr_eq(x, w));
}

/// The first registered watchdog.
#[must_use]
pub fn first() -> Option<Arc<dyn Watchdog>> {
    WATCHDOGS.lock().first().cloned()
}
