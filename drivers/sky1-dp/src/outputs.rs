// SPDX-License-Identifier: BSD-2-Clause
//! Handing a transmitter's encoder and connector to the display card.
//!
//! The device graph orders the display processor after the transmitter it
//! names (the display driver waits for it by path); the objects pass
//! through here, by unit number, until the M-G aggregate node takes over
//! the job (docs/GRAPHICS.md §7.2).

use alloc::sync::Arc;
use alloc::vec::Vec;

use drm::kms::{ConnectorHw, EncoderHw};
use sync::SpinLock;

/// What the card needs of an output.
#[derive(Clone)]
pub struct Output {
    pub encoder: Arc<dyn EncoderHw>,
    pub connector: Arc<dyn ConnectorHw>,
}

impl core::fmt::Debug for Output {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Output").finish_non_exhaustive()
    }
}

static OUTPUTS: SpinLock<Vec<(u32, Output)>> = SpinLock::new(Vec::new());

/// Publishes the output of transmitter `unit`.
pub fn publish(unit: u32, o: Output) {
    let mut v = OUTPUTS.lock();
    v.retain(|e| e.0 != unit);
    v.push((unit, o));
}

/// Withdraws it.
pub fn withdraw(unit: u32) {
    OUTPUTS.lock().retain(|e| e.0 != unit);
}

/// The output of transmitter `unit`, if its driver has published it.
#[must_use]
pub fn lookup(unit: u32) -> Option<Output> {
    OUTPUTS
        .lock()
        .iter()
        .find(|e| e.0 == unit)
        .map(|e| e.1.clone())
}
