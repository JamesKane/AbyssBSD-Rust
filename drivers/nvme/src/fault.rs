// SPDX-License-Identifier: BSD-2-Clause
//! Faults the kernel's tests inject into one controller, to drive the
//! timeout recovery: interrupts that do nothing (a missed interrupt the
//! watchdog's poll must make up for), completions the driver drops as if
//! they never came (a command the controller never answers, which only a
//! reset recovers), and a short I/O timeout. Nothing is injected until a
//! test selects a controller.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// The unit faults are injected into; `u32::MAX` for none.
static UNIT: AtomicU32 = AtomicU32::new(u32::MAX);
static IGNORE_INTERRUPTS: AtomicBool = AtomicBool::new(false);
static LOSE: AtomicU32 = AtomicU32::new(0);
/// The I/O timeout in milliseconds, 0 for the controller's own.
static TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);

// Relaxed throughout: the hooks are set before the I/O they affect is
// started, by the thread that starts it.

/// Injects into `nvme<unit>`, or clears every fault with `None`.
pub fn target(unit: Option<u32>) {
    if unit.is_none() {
        IGNORE_INTERRUPTS.store(false, Ordering::Relaxed);
        LOSE.store(0, Ordering::Relaxed);
        TIMEOUT_MS.store(0, Ordering::Relaxed);
    }
    UNIT.store(unit.unwrap_or(u32::MAX), Ordering::Relaxed);
}

/// Makes the controller's interrupt handlers do nothing.
pub fn ignore_interrupts(on: bool) {
    IGNORE_INTERRUPTS.store(on, Ordering::Relaxed);
}

/// Drops the controller's next `n` I/O completions.
pub fn lose_completions(n: u32) {
    LOSE.store(n, Ordering::Relaxed);
}

/// Times the controller's I/O out after `ms` milliseconds (below the
/// sysctl's five-second floor), or after its own period with `None`.
pub fn timeout_ms(ms: Option<u64>) {
    TIMEOUT_MS.store(ms.unwrap_or(0), Ordering::Relaxed);
}

fn targets(unit: u32) -> bool {
    UNIT.load(Ordering::Relaxed) == unit
}

pub(crate) fn ignoring(unit: u32) -> bool {
    IGNORE_INTERRUPTS.load(Ordering::Relaxed) && targets(unit)
}

/// Whether to drop this completion of `unit`'s.
pub(crate) fn lose(unit: u32) -> bool {
    LOSE.load(Ordering::Relaxed) != 0
        && targets(unit)
        && LOSE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
}

pub(crate) fn timeout_override_ns(unit: u32) -> Option<u64> {
    let ms = TIMEOUT_MS.load(Ordering::Relaxed);
    (ms != 0 && targets(unit)).then(|| ms.saturating_mul(1_000_000))
}
