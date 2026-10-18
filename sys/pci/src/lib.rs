// SPDX-License-Identifier: BSD-2-Clause
//! PCI: configuration space, BARs, capabilities, and enumeration
//! (docs/DRIVER-MODEL.md §5).
//!
//! - [`config`]: the [`ConfigSpace`] trait and the ECAM accessor.
//! - [`header`]: identity and header fields of a function.
//! - [`bar`]: sizing and decoding base address registers.
//! - [`caps`]: the standard and extended capability lists.
//! - [`enumerate`]: scanning a bus, and the depth-first walk of a host
//!   bridge's buses.
//! - [`bus`]: the `pci` bus driver.
//! - [`bridge`]: the PCI-PCI bridge driver.
//! - [`msi`]: MSI and MSI-X as an interrupt domain.
//!
//! Everything read from a device is untrusted: lists are walked with
//! bounds, BARs are validated, and a function reading all-ones is absent.
//! Bus numbers are kept as firmware assigned them.
//!
//! Behavior reference: sys/dev/pci/pci.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "configuration registers pack narrower fields; extracting them truncates by design"
)]

extern crate alloc;

pub mod bar;
pub mod bridge;
pub mod bus;
pub mod caps;
pub mod config;
pub mod enumerate;
pub mod header;
pub mod msi;

pub use bar::{Bar, BarError, BarKind};
pub use caps::{Capability, ExtCapability};
pub use config::{ConfigSpace, Ecam};
pub use devtree::strings::PciAddress;
pub use enumerate::{Function, enumerate, scan};

#[cfg(test)]
mod tests;
