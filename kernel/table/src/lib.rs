// SPDX-License-Identifier: BSD-2-Clause
//! The drivers and platforms a kernel build includes (docs/DRIVER-MODEL.md
//! §7.1, ADR-0020): reviewable code rather than linker sections.
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
    acpi::GPIO_AEI,
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
    sky1_clk::DRIVER,
    sky1_pinctrl::DRIVER,
    sky1_reset::DRIVER,
    sky1_syscon::DRIVER,
    cdns_gpio::DRIVER,
    cdnc_i2c::DRIVER,
    pci::bus::DRIVER,
    pci::bridge::DRIVER,
    virtio::pci::MODERN,
    virtio::pci::LEGACY,
    virtio_blk::DRIVER,
    virtio_net::DRIVER,
    nvme::DRIVER,
    rge::DRIVER,
    ahci::AHCI,
    ahci::AHCICH,
    virtio_random::DRIVER,
    virtio_input::DRIVER,
    xhci_driver::PCI,
    xhci_driver::ACPI,
    usbus::USBUS,
    usbus::UHUB,
    hidbus::USBHID,
    hidbus::HIDBUS,
    hkbd::DRIVER,
    hms::DRIVER,
    umass::DRIVER,
];

/// The platforms built in (ADR-0020): every one is always present, and
/// the ACPI bus runs each whose firmware identity matches. Adding a SoC
/// family is a crate under `platform/` and a line here.
pub static PLATFORMS: &[&'static dyn acpi_platform::PlatformSource] = &[&cix_sky1::PLATFORM];

#[cfg(test)]
mod tests;
