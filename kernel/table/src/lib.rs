// SPDX-License-Identifier: BSD-2-Clause
//! The drivers a kernel build includes (docs/DRIVER-MODEL.md §7.1):
//! reviewable code rather than linker sections.
//!
//! It is its own crate so the host can check it: [`DRIVERS`] must have no
//! unresolved tie, two drivers matching some device at the same rank with
//! neither yielding (`cargo xtask check` runs the test). The identities
//! checked are one built from every entry of every table, plus the devices
//! FreeBSD recorded in `tests/corpus/freebsd`.

#![no_std]
#![forbid(unsafe_code)]

use bus::DriverDecl;

/// The build's drivers, buses first. Order breaks no ties (the check
/// forbids them); it is the order a bus's children are offered drivers
/// of equal rank in, which the check makes irrelevant.
pub static DRIVERS: &[DriverDecl] = &[
    acpi::DRIVER,
    acpi::PCIB,
    acpi::PCI_PCI_BRIDGE,
    acpi::BUTTON,
    acpi::GED,
    atrtc::DRIVER,
    uart::DRIVER,
    timers::ATTIMER,
    timers::HPET,
    timers::ACPI_TIMER,
    isa::ISAB,
    isa::ISA,
    ichwd::DRIVER,
    amdsbwd::DRIVER,
    sbsa_gwdt::DRIVER,
    pci::bus::DRIVER,
    pci::bridge::DRIVER,
    virtio::pci::MODERN,
    virtio::pci::LEGACY,
    virtio_blk::DRIVER,
    nvme::DRIVER,
    ahci::AHCI,
    ahci::AHCICH,
    virtio_random::DRIVER,
];

#[cfg(test)]
mod tests;
