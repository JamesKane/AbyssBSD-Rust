// SPDX-License-Identifier: BSD-2-Clause
//! `bge`: Broadcom BCM57765-family Gigabit Ethernet (NetXtreme 57xx), as
//! FreeBSD's `bge` drives that family. The Mac Pro (Late 2013)'s two
//! ports are BCM57762s (docs/MACPRO.md).
//!
//! - [`hw`]: identification, the core reset and the bootcode handshake,
//!   `bge_chipinit`, `bge_blockinit`, the station address, MII access, the
//!   receive filter, the link's MAC settings, `bge_stop`.
//! - [`phy`]: the internal PHY, as `brgphy` drives the BCM57765's.
//! - [`ring`]: the producer, return and send rings, and the status block.
//! - [`driver`]: `bgeN`, the PCI driver and its interface.
//!
//! Only the 57765 family (`BGE_FLAG_57765_PLUS`) is driven: the other
//! chips `bge` knows take other paths through most of the bring-up, and
//! none is in a machine we have. No emulator models these chips, so the
//! programming is tested on the host against a model of the chip.
//!
//! Behavior reference: sys/dev/bge/if_bge.c, sys/dev/mii/brgphy.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod driver;
pub mod hw;
pub mod phy;
pub mod regs;
pub mod ring;

pub use driver::DRIVER;

#[cfg(test)]
mod tests;
