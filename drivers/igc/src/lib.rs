// SPDX-License-Identifier: BSD-2-Clause
//! `igc`: Intel I225 and I226 2.5 GbE Ethernet. The MSI PRO Z690-A's NIC is
//! an I225-V.
//!
//! - [`hw`]: the controller over its registers: reset, the station address,
//!   the PHY's management interface and auto-negotiation, the link, the
//!   receive filter, the queues, and the interrupt mask.
//! - [`ring`]: the advanced descriptor rings and the frame reassembly.
//! - [`driver`]: `igcN`, the PCI driver and its interface.
//! - [`regs`]: the registers and their bits.
//!
//! No emulator models these chips, so the hardware layer and the rings are
//! tested on the host against a model of the register protocol before they
//! meet a board. The once-a-second watchdog's decisions are `rge`'s, in
//! [`nicwatch`].
//!
//! Behavior reference: sys/dev/igc/.

#![no_std]
#![forbid(unsafe_code)]

pub use driver::DRIVER;

extern crate alloc;

pub mod driver;
pub mod hw;
pub mod regs;
pub mod ring;

#[cfg(test)]
mod tests;
