// SPDX-License-Identifier: BSD-2-Clause
//! Virtio: the PCI transport as a bus, and split virtqueues
//! (docs/DRIVER-MODEL.md §5; virtio 1.2).
//!
//! - [`pci`]: the `virtio_pci` drivers. Each attaches to a PCI function,
//!   brings up its transport, and adds one virtio child (`vtblk0` on
//!   `virtio_pci0`), as FreeBSD does.
//! - [`VirtioDevice`]: what the child's driver gets as bus data: status,
//!   feature negotiation, device configuration, queues, and interrupts,
//!   whichever transport is underneath.
//! - [`ring`]: the split ring, shared with the device; [`Virtqueue`] ties
//!   one to its device.
//!
//! Like FreeBSD 16, a transitional device is driven through its modern
//! interface, and the legacy interface (I/O ports, x86 only) is the
//! fallback for legacy-only devices such as bhyve's.
//!
//! Everything a device writes is untrusted: used-ring entries are checked
//! against the descriptors outstanding, and capability structures against
//! their BARs.
//!
//! Behavior reference: sys/dev/virtio/virtio.c, sys/dev/virtio/pci/.

#![no_std]
// The tests' fake DMA memory implements an unsafe trait; the crate
// itself has no unsafe code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]
#![allow(
    clippy::cast_possible_truncation,
    reason = "64-bit features and addresses are written as 32-bit halves"
)]

extern crate alloc;

pub mod pci;
pub mod ring;
mod transport;

pub use ring::{Full, InterruptMask, RingError, Used, Virtqueue};
pub use transport::{NegotiateError, QueueError, VirtioDevice};

/// The PCI vendor ID of virtio devices.
pub const VENDOR: u16 = 0x1af4;

/// Device types (virtio 1.2 §5).
pub mod device_type {
    /// Network card.
    pub const NETWORK: u32 = 1;
    /// Block device.
    pub const BLOCK: u32 = 2;
    /// Console.
    pub const CONSOLE: u32 = 3;
    /// Entropy source.
    pub const ENTROPY: u32 = 4;
}

/// Device status bits (virtio 1.2 §2.1).
pub mod status {
    /// The guest noticed the device.
    pub const ACKNOWLEDGE: u8 = 1;
    /// The guest has a driver for it.
    pub const DRIVER: u8 = 2;
    /// The driver is ready.
    pub const DRIVER_OK: u8 = 4;
    /// Feature negotiation is complete.
    pub const FEATURES_OK: u8 = 8;
    /// The device hit an error it cannot recover from without a reset.
    pub const NEEDS_RESET: u8 = 64;
    /// The guest gave up on the device.
    pub const FAILED: u8 = 128;
}

/// Transport and ring feature bits (virtio 1.2 §6).
pub mod feature {
    /// Indirect descriptors.
    pub const RING_INDIRECT_DESC: u64 = 1 << 28;
    /// Used and available event indices.
    pub const RING_EVENT_IDX: u64 = 1 << 29;
    /// The device follows virtio 1.0 or later (modern).
    pub const VERSION_1: u64 = 1 << 32;
    /// The device's DMA goes through the platform's address translation.
    pub const ACCESS_PLATFORM: u64 = 1 << 33;
}

/// A device type's name, as FreeBSD's `virtio_device_name` gives it.
#[must_use]
pub fn device_name(device_type: u32) -> Option<&'static str> {
    Some(match device_type {
        1 => "Network",
        2 => "Block",
        3 => "Console",
        4 => "Entropy",
        5 => "Balloon",
        6 => "IOMemory",
        7 => "Remote Processor Messaging",
        8 => "SCSI",
        9 => "9P Transport",
        11 => "Remote Processor Serial",
        12 => "CAIF",
        16 => "GPU",
        18 => "Input",
        19 => "VSOCK Transport",
        20 => "Crypto",
        23 => "IOMMU",
        25 => "Sound",
        26 => "Filesystem",
        27 => "Persistent Memory",
        28 => "RPMB",
        32 => "SCMI",
        41 => "GPIO",
        _ => return None,
    })
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests;
