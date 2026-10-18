// SPDX-License-Identifier: BSD-2-Clause
//! Raw UEFI table layouts (UEFI 2.10 §4).

use core::ffi::c_void;

use guid::Guid;

/// An opaque firmware object handle.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle(pub *mut c_void);

/// A UEFI status code. The high bit marks errors.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub usize);

impl Status {
    const ERROR_BIT: usize = 1 << (usize::BITS - 1);

    /// Success.
    pub const SUCCESS: Self = Self(0);
    /// A generic device or firmware failure.
    pub const ABORTED: Self = Self(Self::ERROR_BIT | 21);
    /// Firmware reports an unexpected internal error.
    pub const DEVICE_ERROR: Self = Self(Self::ERROR_BIT | 7);

    /// Whether this status is an error rather than success or a warning.
    pub const fn is_error(self) -> bool {
        self.0 & Self::ERROR_BIT != 0
    }
}

/// An unused service slot. Present only to keep table offsets right.
pub type Slot = usize;

/// Common header of every UEFI table.
#[repr(C)]
#[derive(Debug)]
pub struct TableHeader {
    /// Table-specific signature.
    pub signature: u64,
    /// UEFI specification revision the table conforms to.
    pub revision: u32,
    /// Size of the whole table in bytes.
    pub header_size: u32,
    /// CRC-32 of the table.
    pub crc32: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// `EFI_SYSTEM_TABLE`.
#[repr(C)]
#[derive(Debug)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut SimpleTextOutput,
    pub standard_error_handle: Handle,
    pub std_err: *mut SimpleTextOutput,
    pub runtime_services: *const RuntimeServices,
    pub boot_services: *const c_void,
    pub number_of_table_entries: usize,
    pub configuration_table: *const ConfigurationTable,
}

impl SystemTable {
    /// `"IBI SYST"` in little-endian.
    pub const SIGNATURE: u64 = 0x5453_5953_2049_4249;
}

/// `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
pub struct SimpleTextOutput {
    pub reset: Slot,
    pub output_string: unsafe extern "efiapi" fn(*mut SimpleTextOutput, *const u16) -> Status,
    pub test_string: Slot,
    pub query_mode: Slot,
    pub set_mode: Slot,
    pub set_attribute: Slot,
    pub clear_screen: Slot,
    pub set_cursor_position: Slot,
    pub enable_cursor: Slot,
    pub mode: *mut c_void,
}

/// `EFI_RUNTIME_SERVICES`.
#[repr(C)]
#[derive(Debug)]
pub struct RuntimeServices {
    pub hdr: TableHeader,
    pub get_time: Slot,
    pub set_time: Slot,
    pub get_wakeup_time: Slot,
    pub set_wakeup_time: Slot,
    pub set_virtual_address_map: Slot,
    pub convert_pointer: Slot,
    pub get_variable: Slot,
    pub get_next_variable_name: Slot,
    pub set_variable: Slot,
    pub get_next_high_monotonic_count: Slot,
    pub reset_system: unsafe extern "efiapi" fn(u32, Status, usize, *const c_void) -> !,
    pub update_capsule: Slot,
    pub query_capsule_capabilities: Slot,
    pub query_variable_info: Slot,
}

/// One `EFI_CONFIGURATION_TABLE` entry.
#[repr(C)]
#[derive(Debug)]
pub struct ConfigurationTable {
    /// Identifies the table.
    pub vendor_guid: RawGuid,
    /// Address of the table.
    pub vendor_table: *const c_void,
}

/// A GUID in its in-memory UEFI encoding.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawGuid(pub [u8; 16]);

impl RawGuid {
    /// Decodes to a [`Guid`].
    pub const fn get(self) -> Guid {
        Guid::from_uefi_bytes(self.0)
    }
}
