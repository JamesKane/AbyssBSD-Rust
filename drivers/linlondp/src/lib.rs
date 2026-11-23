// SPDX-License-Identifier: BSD-2-Clause
//! The Arm China Linlon-D6 display processor, as the CIX Sky1 has it: the
//! display half of the Sky1 display card. Its encoders (the DisplayPort
//! transmitters, their PHYs, the panel and the backlight) are other
//! drivers that join it in an aggregate.
//!
//! Written from the clean-room specification `linlon-dpu`; the sections
//! each module follows are named in its header. Kernel mode setting
//! objects are built over [`hw::Dpu`], which is host tested against a
//! simulated device.

#![no_std]
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod card;
pub mod discover;
pub mod format;
pub mod hw;
pub mod komeda;
pub mod regs;
pub mod timing;

#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;
