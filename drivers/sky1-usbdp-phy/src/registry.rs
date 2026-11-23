// SPDX-License-Identifier: BSD-2-Clause
//! Handing a PHY to the DisplayPort controller it serves.
//!
//! The device graph already makes the controller wait for its PHY (the
//! `dp_phy` reference is a required dependency), but it has no provider
//! kind for a PHY. Until the M-G design of one (docs/GRAPHICS.md §15, lane
//! leasing), the PHY's driver publishes its shared parts here, by the
//! controller's unit number, and the controller's driver takes them.

use alloc::sync::Arc;
use alloc::vec::Vec;

use sync::SpinLock;

use crate::phy::{Core, Platform, Regs16};

/// What the controller needs of the PHY device.
#[derive(Clone)]
pub struct Shared {
    pub core: Arc<Core>,
    pub regs: Arc<dyn Regs16>,
    pub plat: Arc<dyn Platform>,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

static PHYS: SpinLock<Vec<(u32, Shared)>> = SpinLock::new(Vec::new());

/// Publishes the PHY that serves DisplayPort controller `unit`.
pub fn publish(unit: u32, s: Shared) {
    let mut p = PHYS.lock();
    p.retain(|e| e.0 != unit);
    p.push((unit, s));
}

/// Withdraws it.
pub fn withdraw(unit: u32) {
    PHYS.lock().retain(|e| e.0 != unit);
}

/// The PHY of controller `unit`, if its driver has published it.
#[must_use]
pub fn lookup(unit: u32) -> Option<Shared> {
    PHYS.lock()
        .iter()
        .find(|e| e.0 == unit)
        .map(|e| e.1.clone())
}
