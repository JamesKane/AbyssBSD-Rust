// SPDX-License-Identifier: BSD-2-Clause
//! Sleepable locks: [`Mutex`] (priority propagation), [`RwLock`] and
//! [`SxLock`] (shared/exclusive), and [`Epoch`] (read-mostly data), all
//! checked by the [`witness`].
//!
//! Spin locks live in `sys/sync`; these build on the scheduler because
//! they block.

mod epoch;
mod mutex;
mod rw;
pub mod witness;

pub use epoch::{Epoch, EpochGuard};
pub use mutex::{Mutex, MutexCore, MutexGuard};
pub use rw::{ReadGuard, RwLock, SxLock, WriteGuard};
pub use witness::LockClass;

/// Prints a witness report (set by the kernel).
pub type Reporter = fn(core::fmt::Arguments<'_>);

static REPORTER: sync::Once<Reporter> = sync::Once::new();

/// Sets where witness reports go. Call once.
pub fn set_reporter(r: Reporter) {
    let _ = REPORTER.set(r);
}
