// SPDX-License-Identifier: BSD-2-Clause
//! The ACPI bus: the AML namespace loaded at boot, and `acpi0` with a
//! child for every namespace device (docs/DRIVER-MODEL.md §5, §6.1).
//!
//! - [`Ops`]: what the kernel provides (physical memory, ports, PCI
//!   configuration space, time, interrupt routing).
//! - [`service`]: the thread that owns the interpreter. AML recurses
//!   deeply on some firmware, so it runs on its own large stack, and every
//!   evaluation is a job sent to it.
//! - [`bus`]: the `acpi` bus driver.
//! - [`pcib`]: the ACPI host-PCI bridge.
//! - [`iort`]: the SMMUs the IORT lists, and the devices they translate.
//!
//! Behavior reference: sys/dev/acpica/acpi.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod aei;
pub mod battery;
pub mod bus;
pub mod button;
pub mod cpc;
pub mod cppc;
pub mod cpu;
pub mod deps;
#[cfg(test)]
mod deps_corpus;
pub mod events;
pub mod ged;
mod host;
pub mod iicbus;
#[cfg(test)]
mod iicbus_corpus;
pub mod iort;
pub mod pcib;
pub mod power;
pub mod properties;
pub mod ranges;
#[cfg(test)]
mod ranges_corpus;
pub mod service;
pub mod thermal;

pub use aei::GPIO_AEI;
pub use bus::{AcpiBus, AcpiDevice, DRIVER, companion};
pub use button::BUTTON;
pub use cppc::CPPC;
pub use cpu::CPU;
pub use events::{Block, FixedHardware, Space};
pub use ged::GED;
pub use pcib::{
    AcpiPciBridge, AcpiPcib, GENERIC_PCIB, GenericPcieAcpi, HostBridge, PCI_PCI_BRIDGE, PCIB,
};
pub use service::{LoadSummary, init, service};
pub use thermal::THERMAL;

use alloc::vec::Vec;

/// Kernel services the ACPI bus needs.
pub trait Ops: Sync {
    /// Reads `bytes` (1, 2, 4, or 8) of physical memory at `pa`.
    fn mem_read(&self, pa: u64, bytes: u8) -> Option<u64>;
    /// Writes `bytes` of physical memory at `pa`.
    fn mem_write(&self, pa: u64, bytes: u8, v: u64) -> Option<()>;
    /// Reads `bytes` (1, 2, or 4) from an I/O port (x86 only).
    fn io_read(&self, port: u16, bytes: u8) -> Option<u32>;
    /// Writes `bytes` to an I/O port (x86 only).
    fn io_write(&self, port: u16, bytes: u8, v: u32) -> Option<()>;
    /// The configuration space serving `a`.
    fn pci_config(&self, a: pci::PciAddress) -> Option<&'static (dyn pci::ConfigSpace + Sync)>;
    /// Sleeps for `ms` milliseconds.
    fn sleep_ms(&self, s: &sched::Sleepable, ms: u64);
    /// Busy-waits for `us` microseconds.
    fn stall_us(&self, us: u64);
    /// A monotonic count in 100 ns units.
    fn timer_100ns(&self) -> u64;
    /// The domain and input of global system interrupt `gsi`.
    fn gsi(&self, gsi: u32) -> Option<(&'static dyn irq::IrqDomain, u32)>;
    /// An ISA IRQ's GSI with its trigger and polarity, after interrupt
    /// source overrides.
    fn isa_irq(&self, irq: u8) -> (u32, irq::Trigger, irq::Polarity);
    /// The DSDT and then each SSDT.
    fn definition_blocks(&self) -> Vec<&'static [u8]>;
    /// `acpi0`'s description: the root table's OEM ID and OEM table ID.
    fn oem_description(&self) -> alloc::string::String;
    /// The FADT's fixed hardware, or `None` on a hardware-reduced
    /// platform (arm64), whose events come through GED devices.
    fn fixed_hardware(&self) -> Option<FixedHardware>;
    /// The power button was pressed: power off (FreeBSD's
    /// `shutdown_nice(RB_POWEROFF)`). Called on the AML or SCI thread, so
    /// it must not wait on ACPI.
    fn power_button(&self);
    /// A console line.
    fn log(&self, args: core::fmt::Arguments<'_>);
    /// The platforms built in (`kernel/table`'s `PLATFORMS`, ADR-0020):
    /// the graph builder runs each that applies. None by default.
    fn platforms(&self) -> &'static [&'static dyn acpi_platform::PlatformSource] {
        &[]
    }
    /// The firmware's identity, from the root table's header.
    fn firmware_identity(&self) -> acpi_platform::FirmwareIdentity {
        acpi_platform::FirmwareIdentity::default()
    }
    /// The IORT, if firmware has one: its SMMUs become children of
    /// `acpi0`, and the devices they translate depend on them
    /// ([`iort`]). None by default.
    fn iort(&self) -> Option<acpi_tables::Sdt<'static>> {
        None
    }
    /// The DMA controllers the CSRT describes, which serve `_CRS`
    /// `FixedDMA` request lines. None by default.
    fn dma_controllers(&self) -> Vec<DmaController> {
        Vec::new()
    }
    /// The kernel's ID of the CPU whose MADT entry has processor UID
    /// `uid` (FreeBSD's `acpi_pcpu_get_id`). None by default.
    fn cpu_of_uid(&self, uid: u32) -> Option<u32> {
        let _ = uid;
        None
    }
    /// Reads functional fixed hardware register `address` on the current
    /// CPU, as the architecture defines them for `_CPC`: on arm64, 0 is
    /// the activity monitors' core cycle counter and 1 their constant
    /// cycle counter. `None` where there is no such register.
    fn ffh_read(&self, address: u64) -> Option<u64> {
        let _ = address;
        None
    }
    /// The rate of the constant cycle counter `ffh_read(1)` reads, in Hz
    /// (arm64's system counter frequency). `None` where there is none.
    fn ffh_constant_hz(&self) -> Option<u64> {
        None
    }
    /// A thermal zone reached its critical temperature: shut down now
    /// (FreeBSD's `acpi_tz_monitor` calls `shutdown_nice(RB_POWEROFF)`).
    /// Called on a thermal zone's thread; by default, as the power button.
    fn critical_temperature(&self) {
        self.power_button();
    }
}

/// A DMA controller from the CSRT: the MMIO base that identifies its
/// namespace device, and the request lines it serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmaController {
    /// Its MMIO base, as its device's `_CRS` gives it.
    pub mmio_base: u64,
    /// The request lines it serves.
    pub requests: core::ops::Range<u16>,
}

#[cfg(test)]
mod tests;
