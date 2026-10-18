// SPDX-License-Identifier: BSD-2-Clause
//! The interface between the interpreter and the operating system: region
//! space accesses, timing, notifications, and the global lock.
//!
//! The kernel implements [`Host`]; host tests implement it with memory-backed
//! regions. Every hook except the plain region read and write has a default
//! that is safe for an OS that does not support it yet (the access fails
//! with [`HostError::Unsupported`], or the event is ignored).
//!
//! Behavior reference: `sys/dev/acpica/Osd/OsdHardware.c`,
//! `OsdMemory.c`, `OsdSchedule.c`, `OsdSynch.c` (the OS services ACPICA asks
//! FreeBSD for).

use alloc::vec::Vec;

use crate::namespace::NodeId;
use crate::value::Value;

/// Operation region address spaces (ACPI 6.5 §19.6.100, Table 19.36).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegionSpace {
    /// System memory (0).
    SystemMemory,
    /// System I/O ports (1).
    SystemIo,
    /// PCI configuration space (2).
    PciConfig,
    /// Embedded controller (3).
    EmbeddedControl,
    /// SMBus (4), accessed with buffers.
    SmBus,
    /// CMOS RAM (5).
    SystemCmos,
    /// A PCI BAR (6).
    PciBarTarget,
    /// IPMI (7), accessed with buffers.
    Ipmi,
    /// GPIO pins (8), through a `Connection`.
    GeneralPurposeIo,
    /// I2C, SPI, or UART devices (9), accessed with buffers through a
    /// `Connection`.
    GenericSerialBus,
    /// Platform Communications Channel (0x0A); the region offset is the PCC
    /// subspace.
    Pcc,
    /// Functional fixed hardware (0x7F).
    FunctionalFixedHw,
    /// OEM-defined (0x80–0xFF).
    Oem(u8),
    /// Reserved values.
    Reserved(u8),
}

impl RegionSpace {
    /// Decodes a `RegionSpace` byte.
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::SystemMemory,
            1 => Self::SystemIo,
            2 => Self::PciConfig,
            3 => Self::EmbeddedControl,
            4 => Self::SmBus,
            5 => Self::SystemCmos,
            6 => Self::PciBarTarget,
            7 => Self::Ipmi,
            8 => Self::GeneralPurposeIo,
            9 => Self::GenericSerialBus,
            0x0a => Self::Pcc,
            0x7f => Self::FunctionalFixedHw,
            0x80..=0xff => Self::Oem(v),
            _ => Self::Reserved(v),
        }
    }

    /// The `RegionSpace` byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::SystemMemory => 0,
            Self::SystemIo => 1,
            Self::PciConfig => 2,
            Self::EmbeddedControl => 3,
            Self::SmBus => 4,
            Self::SystemCmos => 5,
            Self::PciBarTarget => 6,
            Self::Ipmi => 7,
            Self::GeneralPurposeIo => 8,
            Self::GenericSerialBus => 9,
            Self::Pcc => 0x0a,
            Self::FunctionalFixedHw => 0x7f,
            Self::Oem(v) | Self::Reserved(v) => v,
        }
    }

    /// True for the spaces whose fields transfer buffers through
    /// [`Host::transfer`] rather than integers.
    #[must_use]
    pub const fn is_buffer_protocol(self) -> bool {
        matches!(self, Self::SmBus | Self::Ipmi | Self::GenericSerialBus)
    }
}

/// The width of one region access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessWidth {
    /// 8 bits.
    U8,
    /// 16 bits.
    U16,
    /// 32 bits.
    U32,
    /// 64 bits.
    U64,
}

impl AccessWidth {
    /// Bytes per access.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
            Self::U64 => 8,
        }
    }

    /// Bits per access.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.bytes() * 8
    }
}

/// A PCI function address, as derived for a `PCI_Config` region from
/// `_SEG`, `_BBN`, `_ADR`, and the bridges in between.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PciAddress {
    /// Segment group.
    pub segment: u16,
    /// Bus.
    pub bus: u8,
    /// Device.
    pub device: u8,
    /// Function.
    pub function: u8,
}

/// One integer access to an operation region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionAccess {
    /// The region's space.
    pub space: RegionSpace,
    /// The region node, so a host can tell embedded controllers or PCC
    /// channels apart.
    pub region: NodeId,
    /// The region's offset operand: an address for memory and I/O, a
    /// configuration offset for PCI, the subspace for PCC.
    pub base: u64,
    /// Byte offset of this access within the region.
    pub offset: u64,
    /// Access width.
    pub width: AccessWidth,
    /// For `PCI_Config` and `PciBarTarget`, the function.
    pub pci: Option<PciAddress>,
}

impl RegionAccess {
    /// `base + offset`, the address for memory, I/O, and PCI configuration
    /// accesses (wrapping, as the hardware address does).
    #[must_use]
    pub const fn address(&self) -> u64 {
        self.base.wrapping_add(self.offset)
    }
}

/// A buffer-protocol field access (SMBus, IPMI, `GenericSerialBus`).
#[derive(Clone, Copy, Debug)]
pub struct BufferTransfer<'a> {
    /// The region's space.
    pub space: RegionSpace,
    /// The region node.
    pub region: NodeId,
    /// The field's byte offset in the region: the SMBus or serial-bus
    /// command, or the IPMI network function and command.
    pub command: u64,
    /// The access attribute byte (`AttribByte`, `AttribBlock`, …).
    pub attrib: u8,
    /// The access length for the attributes that take one.
    pub access_len: u8,
    /// The `Connection` resource descriptor, for `GenericSerialBus`.
    pub connection: Option<&'a [u8]>,
    /// The data written, or `None` for a read.
    pub write: Option<&'a [u8]>,
}

/// A GPIO field access.
#[derive(Clone, Copy, Debug)]
pub struct GpioAccess<'a> {
    /// The region node.
    pub region: NodeId,
    /// The `Connection` resource descriptor (a `GpioIo`).
    pub connection: &'a [u8],
    /// Index of the first pin in the connection's pin table.
    pub pin: u64,
    /// Number of pins.
    pub count: u64,
}

/// Why a host operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostError {
    /// The host does not implement this space or operation.
    Unsupported,
    /// The host refused the access (for example a protected range).
    Denied,
    /// The access was attempted and failed.
    Failed,
}

/// Operating system services the interpreter needs.
pub trait Host {
    /// Reads from an operation region.
    ///
    /// # Errors
    ///
    /// Whatever [`HostError`] describes the failure; the AML operation that
    /// made the access then fails.
    fn read(&mut self, access: &RegionAccess) -> Result<u64, HostError>;

    /// Writes to an operation region.
    ///
    /// # Errors
    ///
    /// Whatever [`HostError`] describes the failure.
    fn write(&mut self, access: &RegionAccess, value: u64) -> Result<(), HostError>;

    /// Performs a buffer-protocol transfer and returns the result buffer
    /// (status, length, data for SMBus).
    ///
    /// # Errors
    ///
    /// [`HostError::Unsupported`] by default.
    fn transfer(&mut self, req: &BufferTransfer<'_>) -> Result<Vec<u8>, HostError> {
        let _ = req;
        Err(HostError::Unsupported)
    }

    /// Reads GPIO pins.
    ///
    /// # Errors
    ///
    /// [`HostError::Unsupported`] by default.
    fn gpio_read(&mut self, access: &GpioAccess<'_>) -> Result<u64, HostError> {
        let _ = access;
        Err(HostError::Unsupported)
    }

    /// Writes GPIO pins.
    ///
    /// # Errors
    ///
    /// [`HostError::Unsupported`] by default.
    fn gpio_write(&mut self, access: &GpioAccess<'_>, value: u64) -> Result<(), HostError> {
        let _ = (access, value);
        Err(HostError::Unsupported)
    }

    /// `Sleep`: yields for at least `ms` milliseconds.
    fn sleep(&mut self, ms: u64) {
        let _ = ms;
    }

    /// `Stall`: busy-waits for `us` microseconds.
    fn stall(&mut self, us: u64) {
        let _ = us;
    }

    /// `Timer`: a monotonic count in 100 ns units. The default of 0
    /// disables the loop timeout.
    fn timer(&mut self) -> u64 {
        0
    }

    /// `Notify(node, value)`.
    fn notify(&mut self, node: NodeId, value: u64) {
        let _ = (node, value);
    }

    /// Acquires the ACPI global lock, waiting up to `timeout_ms`
    /// (0xFFFF waits forever). Returns false on timeout.
    fn acquire_global_lock(&mut self, timeout_ms: u16) -> bool {
        let _ = timeout_ms;
        true
    }

    /// Releases the ACPI global lock.
    fn release_global_lock(&mut self) {}

    /// `Fatal(type, code, arg)`: firmware reports a fatal condition.
    fn fatal(&mut self, kind: u8, code: u32, arg: u64) {
        let _ = (kind, code, arg);
    }

    /// A value stored to the Debug object.
    fn debug(&mut self, value: &Value) {
        let _ = value;
    }

    /// Finds an ACPI table for `DataTableRegion`: its physical address and
    /// length.
    fn data_table(
        &mut self,
        signature: &[u8],
        oem_id: &[u8],
        oem_table_id: &[u8],
    ) -> Option<(u64, u64)> {
        let _ = (signature, oem_id, oem_table_id);
        None
    }
}
