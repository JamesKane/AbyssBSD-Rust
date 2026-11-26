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

use core::sync::atomic::{AtomicI32, Ordering};

/// `hw.rge.tso` (a boot-time tunable): -1 unset, else 0 for off and 1 for
/// on, on any chip with the feature (all but the first RTL8125).
static TSO: AtomicI32 = AtomicI32::new(-1);

/// Takes the boot-time tunables (`TUNABLE_INT_FETCH`): `get` gives a
/// name's integer value, if set. The kernel calls it before drivers
/// attach.
pub fn set_tunables(get: &dyn Fn(&str) -> Option<i32>) {
    if let Some(v) = get("hw.rge.tso") {
        // Relaxed: a tunable, read at attach.
        TSO.store(i32::from(v != 0), Ordering::Relaxed);
    }
}

/// Whether a chip of type `mac` gets TSO: as the tunable says, else on the
/// chips it has been shown on (RTL8125D, first revision, on the X870E);
/// the rest follow as each is checked on its hardware.
pub(crate) fn tso_wanted(mac: hw::MacType) -> bool {
    match TSO.load(Ordering::Relaxed) {
        -1 => mac == hw::MacType::R25D1,
        v => v != 0 && mac != hw::MacType::R25,
    }
}

extern crate alloc;

pub mod driver;
pub mod generated;
pub use nicwatch as health;
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
