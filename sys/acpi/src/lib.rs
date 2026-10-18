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
//!
//! Behavior reference: sys/dev/acpica/acpi.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod bus;
pub mod button;
pub mod events;
pub mod ged;
mod host;
pub mod pcib;
pub mod service;

pub use bus::{AcpiBus, AcpiDevice, DRIVER};
pub use button::BUTTON;
pub use events::{Block, FixedHardware, Space};
pub use ged::GED;
pub use pcib::{AcpiPciBridge, AcpiPcib, PCI_PCI_BRIDGE, PCIB};
pub use service::{LoadSummary, init, service};

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
}

#[cfg(test)]
mod tests;
