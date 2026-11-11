// SPDX-License-Identifier: BSD-2-Clause
//! The drivers and platforms a kernel build includes (docs/DRIVER-MODEL.md
//! §7.1, ADR-0020, ADR-0021): reviewable code rather than linker sections.
//!
//! There is one table per ISA, and a kernel uses its own ([`DRIVERS`],
//! [`PLATFORMS`]). A driver is in an ISA's table when FreeBSD builds it for
//! that architecture (`sys/conf/files`, `files.x86`/`files.amd64`,
//! `files.arm64`), and a driver for a plug-in device (PCIe, USB) is in every
//! table (docs/MODULES.md §3.1). Its crate is a dependency only of the kernels
//! that list it, and of every host build, so the host checks see each table.
//!
//! It is its own crate so the host can check it: no table may have an
//! unresolved tie, two drivers matching some device at the same rank with
//! neither yielding (`cargo xtask check` runs the test). The identities
//! checked are one built from every entry of every table, plus the devices
//! FreeBSD recorded in `tests/corpus/freebsd`.

#![no_std]
#![forbid(unsafe_code)]

use acpi_platform::PlatformSource;
use bus::DriverDecl;

/// The drivers in every ISA's table, followed by `$isa`'s own. Buses come
/// first. Order breaks no ties (the check forbids them); it is the order a
/// bus's children are offered drivers of equal rank in, which the check
/// makes irrelevant.
macro_rules! drivers {
    ($($isa:expr),* $(,)?) => {
        &[
            acpi::DRIVER,
            acpi::BUTTON,
            acpi::CPU,
            acpi::CPPC,
            acpi::THERMAL,
            cpufreq::DRIVER,
            acpi::GED,
            acpi::GPIO_AEI,
            uart::DRIVER,
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
            usbus::USBUS,
            usbus::UHUB,
            hidbus::USBHID,
            hidbus::HIDBUS,
            hkbd::DRIVER,
            hms::DRIVER,
            umass::DRIVER,
            iicbus::IICBUS,
            iicbus::IIC,
            $($isa),*
        ]
    };
}

/// The x86_64 kernel's drivers: the ACPI host and PCI-PCI bridges
/// (`acpi_pcib_acpi`, `acpi_pcib_pci`: `files.x86`), and the chipset's
/// (`files.x86`, `files.amd64`).
#[cfg(any(target_arch = "x86_64", not(target_os = "none")))]
pub static X86_64: &[DriverDecl] = drivers![
    acpi::PCIB,
    acpi::PCI_PCI_BRIDGE,
    isa::ISAB,
    isa::ISA,
    atrtc::DRIVER,
    timers::ATTIMER,
    timers::HPET,
    timers::ACPI_TIMER,
    ichwd::DRIVER,
    amdsbwd::DRIVER,
];

/// The aarch64 kernel's drivers: the ACPI host bridge
/// (`pci_host_generic_acpi`), the SBSA watchdog, the SMMUv3 the IORT
/// lists, and the xHCI that firmware describes in ACPI (`files.arm64`;
/// `smmu` is `optional iommu`, which the board's FreeBSD and its
/// AbyssBSD have, IOMMU.md §2.2), `gpiobus` and `gpioc`
/// (arm64's `std.dev` has `device gpio`; amd64's GENERIC does not), and
/// the Sky1's SoC blocks,
/// which FreeBSD lacks or builds for arm64 only (`cdnc_i2c`).
#[cfg(any(target_arch = "aarch64", not(target_os = "none")))]
pub static AARCH64: &[DriverDecl] = drivers![
    acpi::GENERIC_PCIB,
    sbsa_gwdt::DRIVER,
    smmu::DRIVER,
    xhci_driver::ACPI,
    sky1_clk::DRIVER,
    sky1_clk::SCMI_DRIVER,
    sky1_clk::SHMEM_DRIVER,
    sky1_pinctrl::DRIVER,
    sky1_reset::DRIVER,
    sky1_syscon::DRIVER,
    cix_mbox::DRIVER,
    cdns_gpio::DRIVER,
    gpiobus::GPIOBUS,
    gpiobus::GPIOC,
    cdnc_i2c::DRIVER,
];

/// The x86_64 kernel's platforms (ADR-0020): none yet.
#[cfg(any(target_arch = "x86_64", not(target_os = "none")))]
pub static PLATFORMS_X86_64: &[&'static dyn PlatformSource] = &[];

/// The aarch64 kernel's platforms. Every one is built in and the ACPI bus
/// runs each whose firmware identity matches, so no board needs a setting.
/// Adding a SoC family is a crate under `platform/` and a line here.
#[cfg(any(target_arch = "aarch64", not(target_os = "none")))]
pub static PLATFORMS_AARCH64: &[&'static dyn PlatformSource] = &[&cix_sky1::PLATFORM];

/// This kernel's drivers.
#[cfg(target_arch = "aarch64")]
pub use AARCH64 as DRIVERS;
/// This kernel's platforms.
#[cfg(target_arch = "aarch64")]
pub use PLATFORMS_AARCH64 as PLATFORMS;
/// This kernel's platforms.
#[cfg(target_arch = "x86_64")]
pub use PLATFORMS_X86_64 as PLATFORMS;
/// This kernel's drivers.
#[cfg(target_arch = "x86_64")]
pub use X86_64 as DRIVERS;

#[cfg(test)]
mod tests;
