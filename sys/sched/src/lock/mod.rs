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

/// The longest pause between looks at a contended lock, in spin-loop hints:
/// FreeBSD's `lock_delay` default, 256 per CPU rounded up to a power of
/// two. Long pauses keep the lock's cache line from bouncing.
fn max_backoff() -> u32 {
    let cpus = u32::try_from(crate::online_cpus()).unwrap_or(1).max(1);
    cpus.next_power_of_two().saturating_mul(256).min(32_767)
}

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

/// Prints a report the same way (with the kernel's backtrace): for other
/// subsystems' stall diagnostics.
pub fn report(args: core::fmt::Arguments<'_>) {
    if let Some(r) = REPORTER.get() {
        r(args);
    }
}
