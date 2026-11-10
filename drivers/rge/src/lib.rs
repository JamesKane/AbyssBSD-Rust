// SPDX-License-Identifier: BSD-2-Clause
//! `rge`: Realtek RTL8125/RTL8126/RTL8127 Ethernet. The Orange Pi 6 Plus's
//! two NICs are RTL8126 5 GbE controllers (`RTL8126_2`).
//!
//! - [`hw`]: bringing the chip up, over [`hw::Regs`]: the OCP, CSI, and
//!   EPHY register windows, reset, MAC and PHY microcode, PHY
//!   configuration, and the MAC setup `rge_init_locked` does.
//! - [`ring`]: the descriptor rings and frame reassembly.
//! - [`driver`]: `rgeN`, the PCI driver and its interface.
//! - [`health`]: the once-a-second tick's decisions: the transmit
//!   watchdog, receive stalls, a device gone from the bus.
//! - [`generated`]: Realtek's microcode and PHY configuration programs,
//!   read from FreeBSD's driver by `cargo xtask rgegen`; [`program`]
//!   replays them.
//!
//! No emulator models these chips, so everything is tested on the host
//! against a model of the register protocol before it meets a board.
//!
//! Behavior reference: sys/dev/rge/.

#![no_std]
#![forbid(unsafe_code)]

pub use driver::DRIVER;

extern crate alloc;

pub mod driver;
pub mod generated;
pub mod health;
pub mod hw;
pub mod program;
pub mod regs;
pub mod ring;

#[cfg(test)]
mod health_tests;
#[cfg(test)]
mod ring_tests;
#[cfg(test)]
mod tests;

/// The multicast filter's hash words for `groups` (`rge_hash_maddr`):
/// the top six bits of each address's big-endian CRC pick one of 64.
#[must_use]
pub fn multicast_hash(groups: &[[u8; 6]]) -> [u32; 2] {
    let mut h = [0u32; 2];
    for g in groups {
        let bit = net::ether::crc32_be(g) >> 26;
        h[(bit / 32) as usize] |= 1 << (bit % 32);
    }
    h
}
