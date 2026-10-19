// SPDX-License-Identifier: BSD-2-Clause
//! Gates: revocable references from long-lived objects to a binding
//! (docs/DRIVER-MODEL.md §9.2).
//!
//! A class object (a disk, a cdev, a provider handle) outlives the driver
//! binding it fronts. Every call into the driver goes through the object's
//! [`Gate`]: [`Gate::enter`] counts the call on this CPU and checks that
//! the gate is open; the [`GateGuard`] uncounts it when dropped. Detach
//! [`revokes`](Gate::revoke) the gate: later calls fail with [`Revoked`]
//! (which classes report as `ENXIO`), and revocation waits until every
//! call already inside has left. That is FreeBSD's `destroy_dev` drain,
//! done once for every kind of object.
//!
//! Entering costs one atomic add on this CPU's cache line and one load of
//! a flag that only changes at revocation, so the I/O path shares no
//! cache line between CPUs.

use core::sync::atomic::{AtomicBool, Ordering, fence};

use sched::Sleepable;
use sync::PerCpuCounter;

/// The gate is closed: the binding behind it is detaching or gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revoked;

/// A revocable way into a binding.
#[derive(Debug)]
pub struct Gate {
    inside: PerCpuCounter,
    revoked: AtomicBool,
}

impl Gate {
    /// An open gate.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inside: PerCpuCounter::new(),
            revoked: AtomicBool::new(false),
        }
    }

    /// Enters for one call, or fails once the gate is revoked.
    #[inline]
    pub fn enter(&self) -> Result<GateGuard<'_>, Revoked> {
        // Count first, then check: with the full fences here and in
        // `revoke`, either the revoker sees our count or we see its flag.
        self.inside.add(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        if self.revoked.load(Ordering::Relaxed) {
            self.inside.add(-1, Ordering::Release);
            return Err(Revoked);
        }
        Ok(GateGuard { gate: self })
    }

    /// Whether the gate has been revoked.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Closes the gate and waits until no call is inside. Calls that
    /// entered before keep running (they may sleep); later ones fail. A
    /// call that never leaves stalls its own detach, which the verifier's
    /// detach timeout reports.
    pub fn revoke(&self, s: &Sleepable) {
        self.revoked.store(true, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        // Calls inside may be sleeping: give way rather than spin, and say
        // so if the drain stalls (the verifier's detach timeout).
        let mut waited_ms = 0u64;
        while self.inside.sum(Ordering::Acquire) != 0 {
            if waited_ms < 5000 {
                sched::yield_now();
                waited_ms += crate::manager::try_sleep_ms(s, 1);
            } else {
                if let Some(m) = crate::manager::try_manager() {
                    m.log(format_args!(
                        "gate: {} call(s) still inside after 5 s of draining",
                        self.inside.sum(Ordering::Acquire)
                    ));
                }
                waited_ms = 0;
            }
        }
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

/// One call inside a [`Gate`]; leaves on drop (on whatever CPU).
#[derive(Debug)]
#[must_use = "dropping the guard leaves the gate at once"]
pub struct GateGuard<'a> {
    gate: &'a Gate,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        // Release: the call's effects happen before the revoker sees zero.
        self.gate.inside.add(-1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_and_revoke() {
        let g = Gate::new();
        {
            let _a = g.enter().unwrap();
            let _b = g.enter().unwrap();
            assert_eq!(g.inside.sum(Ordering::Relaxed), 2);
        }
        assert_eq!(g.inside.sum(Ordering::Relaxed), 0);
        // SAFETY: host tests have no scheduler; nothing sleeps here.
        let s = unsafe { Sleepable::assume() };
        g.revoke(&s);
        assert!(g.is_revoked());
        assert_eq!(g.enter().unwrap_err(), Revoked);
        assert_eq!(
            g.inside.sum(Ordering::Relaxed),
            0,
            "a failed entry leaves no count"
        );
    }
}
