// SPDX-License-Identifier: BSD-2-Clause
//! The device manager's pure logic (docs/DRIVER-MODEL.md §3.1): everything
//! that does not touch hardware, so it is tested on the host.
//!
//! - [`matching`]: choosing a driver from `const` match tables ranked with
//!   FreeBSD's `BUS_PROBE_*` values, and the build-time ambiguity check.
//! - [`ids`]: device identities and match-table entries per bus.
//! - [`graph`]: the supplier → consumer dependency graph that orders
//!   attach, shutdown, suspend, and resume.
//! - [`units`]: unit numbers per devclass.
//! - [`strings`]: pnpinfo and location strings, byte for byte FreeBSD's.
//! - [`udevice`]: `hw.bus.devices` records for `devinfo(8)`.
//! - [`devctl`]: `/dev/devctl` event lines for devd(8).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod devctl;
pub mod graph;
pub mod ids;
pub mod matching;
pub mod strings;
pub mod udevice;
pub mod units;

#[cfg(test)]
mod freebsd;
#[cfg(test)]
mod tests;
