// SPDX-License-Identifier: BSD-2-Clause
//! The CIX Sky1 DisplayPort transmitter (a Trilinear DPTX core with the
//! vendor's glue): host initialisation, the auxiliary channel, sink
//! capabilities, link training and the video stream, over a physical
//! layer that is another driver's.
//!
//! Written from the clean-room specification `sky1-dp-tx`; modules cite
//! its sections. Single-stream output at one pixel per clock, RGB. Not
//! built: multi-stream, panel self refresh, audio, secondary data packets,
//! variable refresh, HDCP, fast retraining, and the firmware handover.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod aux;
pub mod dpcd;
pub mod dptx;
pub mod link;
pub mod regs;
pub mod stream;

/// Waits the hardware's protocols need.
pub trait Delay: Send + Sync {
    /// Busy-waits `us` microseconds.
    fn delay_us(&self, us: u64);
}

#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;
