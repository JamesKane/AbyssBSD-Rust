// SPDX-License-Identifier: BSD-2-Clause
//! USB as data (USB 2.0 ch. 9 and 11, USB 3.2 ch. 9 and 10): descriptors
//! parsed from the bytes a device returns, standard and hub requests,
//! hub port status, and route strings. Everything a device sends is
//! untrusted: parsing returns errors, never panics.
//!
//! The kernel's USB bus (`sys/usb`) and the hub driver are built on it
//! ([USB.md §5]).
//!
//! [USB.md §5]: ../../../docs/USB.md

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod descriptor;
pub mod hub;
pub mod request;

#[cfg(test)]
mod tests;

pub use descriptor::*;
pub use request::Setup;

/// The low byte of `v`.
pub(crate) fn lo(v: u16) -> u8 {
    v.to_le_bytes()[0]
}

/// A little-endian `u16` at `at`, if `b` holds it.
pub(crate) fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

/// The route string for a device on hub port `port` of a hub at `depth`
/// tiers below the root hub (0 for a hub on a root port) whose own route
/// is `parent` (USB 3.2 §8.9). Ports above 15 are recorded as 15.
#[must_use]
pub fn route(parent: u32, depth: u8, port: u8) -> u32 {
    if depth >= 5 {
        return parent;
    }
    parent | (u32::from(port.min(15)) << (4 * u32::from(depth)))
}
