// SPDX-License-Identifier: BSD-2-Clause
//! The CIX Sky1 USB-C/DisplayPort combo PHY (a Cadence multi-protocol
//! SerDes) as DisplayPort's physical layer: the tables for each mode and
//! plug orientation, the link-rate change, the drive levels, and the
//! controller-side interface. It implements `sky1_dp_tx::link::Phy`.
//!
//! Written from the clean-room specification `sky1-dp-tx` section 03.
//! Not built: the USB 3 side's use of the same PHY (the shared-user
//! accounting), the Type-C controller that supplies mode and orientation,
//! and the eDP PHY of unit 2 (section 04).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod phy;
pub mod tables;

#[cfg(test)]
mod tests;
