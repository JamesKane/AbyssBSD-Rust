// SPDX-License-Identifier: BSD-2-Clause
//! Raw UEFI table and protocol layouts (UEFI 2.10 §4, §7, §9, §12, §13,
//! §37).
//!
//! Only the services the loader calls have function-pointer types; the rest
//! are [`Slot`]s that keep the offsets right.

use core::ffi::c_void;

use guid::Guid;

/// An opaque firmware object handle.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle(pub *mut c_void);

/// A UEFI status code. The high bit marks errors.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Status(pub usize);

impl Status {
    const ERROR_BIT: usize = 1 << (usize::BITS - 1);

    /// Success.
    pub const SUCCESS: Self = Self(0);
    /// `EFI_LOAD_ERROR`.
    pub const LOAD_ERROR: Self = Self::error(1);
    /// `EFI_INVALID_PARAMETER`.
    pub const INVALID_PARAMETER: Self = Self::error(2);
    /// `EFI_UNSUPPORTED`.
    pub const UNSUPPORTED: Self = Self::error(3);
    /// `EFI_BUFFER_TOO_SMALL`.
    pub const BUFFER_TOO_SMALL: Self = Self::error(5);
    /// `EFI_NOT_READY`.
    pub const NOT_READY: Self = Self::error(6);
    /// `EFI_DEVICE_ERROR`.
    pub const DEVICE_ERROR: Self = Self::error(7);
    /// `EFI_OUT_OF_RESOURCES`.
    pub const OUT_OF_RESOURCES: Self = Self::error(9);
    /// `EFI_VOLUME_CORRUPTED`.
    pub const VOLUME_CORRUPTED: Self = Self::error(10);
    /// `EFI_NOT_FOUND`.
    pub const NOT_FOUND: Self = Self::error(14);
    /// `EFI_ABORTED`.
    pub const ABORTED: Self = Self::error(21);

    const fn error(code: usize) -> Self {
        Self(Self::ERROR_BIT | code)
    }

    /// Whether this status is an error rather than success or a warning.
    pub const fn is_error(self) -> bool {
        self.0 & Self::ERROR_BIT != 0
    }

    /// `Ok(())` for success and warnings, `Err(self)` for errors.
    pub const fn ok(self) -> Result<(), Self> {
        if self.is_error() { Err(self) } else { Ok(()) }
    }
}

impl core::fmt::Debug for Status {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match *self {
            Self::SUCCESS => "SUCCESS",
            Self::LOAD_ERROR => "LOAD_ERROR",
            Self::INVALID_PARAMETER => "INVALID_PARAMETER",
            Self::UNSUPPORTED => "UNSUPPORTED",
            Self::BUFFER_TOO_SMALL => "BUFFER_TOO_SMALL",
            Self::NOT_READY => "NOT_READY",
            Self::DEVICE_ERROR => "DEVICE_ERROR",
            Self::OUT_OF_RESOURCES => "OUT_OF_RESOURCES",
            Self::VOLUME_CORRUPTED => "VOLUME_CORRUPTED",
            Self::NOT_FOUND => "NOT_FOUND",
            Self::ABORTED => "ABORTED",
            _ => return write!(f, "Status({:#x})", self.0),
        };
        f.write_str(name)
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
#[allow(missing_docs)]
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
    pub boot_services: *const BootServices,
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
#[allow(missing_docs)]
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
#[allow(missing_docs)]
pub struct RuntimeServices {
    pub hdr: TableHeader,
    pub get_time: Slot,
    pub set_time: Slot,
    pub get_wakeup_time: Slot,
    pub set_wakeup_time: Slot,
    pub set_virtual_address_map: Slot,
    pub convert_pointer: Slot,
    pub get_variable: unsafe extern "efiapi" fn(
        *const u16,
        *const RawGuid,
        *mut u32,
        *mut usize,
        *mut u8,
    ) -> Status,
    pub get_next_variable_name: Slot,
    pub set_variable:
        unsafe extern "efiapi" fn(*const u16, *const RawGuid, u32, usize, *const u8) -> Status,
    pub get_next_high_monotonic_count: Slot,
    pub reset_system: unsafe extern "efiapi" fn(u32, Status, usize, *const c_void) -> !,
    pub update_capsule: Slot,
    pub query_capsule_capabilities: Slot,
    pub query_variable_info: Slot,
}

/// `EFI_BOOT_SERVICES`.
#[repr(C)]
#[derive(Debug)]
#[allow(missing_docs)]
pub struct BootServices {
    pub hdr: TableHeader,
    pub raise_tpl: Slot,
    pub restore_tpl: Slot,
    pub allocate_pages: unsafe extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status,
    pub free_pages: unsafe extern "efiapi" fn(u64, usize) -> Status,
    pub get_memory_map: unsafe extern "efiapi" fn(
        *mut usize,
        *mut MemoryDescriptor,
        *mut usize,
        *mut usize,
        *mut u32,
    ) -> Status,
    pub allocate_pool: unsafe extern "efiapi" fn(u32, usize, *mut *mut u8) -> Status,
    pub free_pool: unsafe extern "efiapi" fn(*mut u8) -> Status,
    pub create_event: Slot,
    pub set_timer: Slot,
    pub wait_for_event: Slot,
    pub signal_event: Slot,
    pub close_event: Slot,
    pub check_event: Slot,
    pub install_protocol_interface: Slot,
    pub reinstall_protocol_interface: Slot,
    pub uninstall_protocol_interface: Slot,
    pub handle_protocol:
        unsafe extern "efiapi" fn(Handle, *const RawGuid, *mut *mut c_void) -> Status,
    pub reserved: Slot,
    pub register_protocol_notify: Slot,
    pub locate_handle: Slot,
    pub locate_device_path: Slot,
    pub install_configuration_table: Slot,
    pub load_image: Slot,
    pub start_image: Slot,
    pub exit: Slot,
    pub unload_image: Slot,
    pub exit_boot_services: unsafe extern "efiapi" fn(Handle, usize) -> Status,
    pub get_next_monotonic_count: Slot,
    pub stall: unsafe extern "efiapi" fn(usize) -> Status,
    pub set_watchdog_timer: unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> Status,
    pub connect_controller: Slot,
    pub disconnect_controller: Slot,
    pub open_protocol: Slot,
    pub close_protocol: Slot,
    pub open_protocol_information: Slot,
    pub protocols_per_handle: Slot,
    pub locate_handle_buffer: Slot,
    pub locate_protocol:
        unsafe extern "efiapi" fn(*const RawGuid, *mut c_void, *mut *mut c_void) -> Status,
    pub install_multiple_protocol_interfaces: Slot,
    pub uninstall_multiple_protocol_interfaces: Slot,
    pub calculate_crc32: Slot,
    pub copy_mem: Slot,
    pub set_mem: Slot,
    pub create_event_ex: Slot,
}

/// `EFI_MEMORY_DESCRIPTOR`. Firmware may use a larger stride than this
/// struct; always step by the descriptor size `GetMemoryMap` returns.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct MemoryDescriptor {
    pub kind: u32,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
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

    /// Encodes a [`Guid`].
    pub const fn new(g: Guid) -> Self {
        Self(g.to_uefi_bytes())
    }
}

/// `EFI_LOADED_IMAGE_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
#[allow(missing_docs)]
pub struct LoadedImage {
    pub revision: u32,
    pub parent_handle: Handle,
    pub system_table: *const SystemTable,
    pub device_handle: Handle,
    pub file_path: *const c_void,
    pub reserved: *const c_void,
    pub load_options_size: u32,
    pub load_options: *const c_void,
    pub image_base: *const c_void,
    pub image_size: u64,
    pub image_code_type: u32,
    pub image_data_type: u32,
    pub unload: Slot,
}

/// `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
#[allow(missing_docs)]
pub struct SimpleFileSystem {
    pub revision: u64,
    pub open_volume: unsafe extern "efiapi" fn(*mut SimpleFileSystem, *mut *mut File) -> Status,
}

/// `EFI_FILE_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
#[allow(missing_docs)]
pub struct File {
    pub revision: u64,
    pub open: unsafe extern "efiapi" fn(*mut File, *mut *mut File, *const u16, u64, u64) -> Status,
    pub close: unsafe extern "efiapi" fn(*mut File) -> Status,
    pub delete: unsafe extern "efiapi" fn(*mut File) -> Status,
    pub read: unsafe extern "efiapi" fn(*mut File, *mut usize, *mut c_void) -> Status,
    pub write: unsafe extern "efiapi" fn(*mut File, *mut usize, *const c_void) -> Status,
    pub get_position: Slot,
    pub set_position: Slot,
    pub get_info:
        unsafe extern "efiapi" fn(*mut File, *const RawGuid, *mut usize, *mut c_void) -> Status,
    pub set_info: Slot,
    pub flush: unsafe extern "efiapi" fn(*mut File) -> Status,
}

/// File open modes.
pub mod file_mode {
    /// `EFI_FILE_MODE_READ`.
    pub const READ: u64 = 1;
    /// `EFI_FILE_MODE_WRITE`.
    pub const WRITE: u64 = 2;
    /// `EFI_FILE_MODE_CREATE`.
    pub const CREATE: u64 = 1 << 63;
}

/// The fixed part of `EFI_FILE_INFO`; the name follows.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct FileInfo {
    pub size: u64,
    pub file_size: u64,
    pub physical_size: u64,
    pub create_time: [u8; 16],
    pub last_access_time: [u8; 16],
    pub modification_time: [u8; 16],
    pub attribute: u64,
}

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
#[allow(missing_docs)]
pub struct GraphicsOutput {
    pub query_mode: Slot,
    pub set_mode: Slot,
    pub blt: Slot,
    pub mode: *const GraphicsMode,
}

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE`.
#[repr(C)]
#[derive(Debug)]
#[allow(missing_docs)]
pub struct GraphicsMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *const GraphicsModeInfo,
    pub size_of_info: usize,
    pub frame_buffer_base: u64,
    pub frame_buffer_size: usize,
}

/// `EFI_GRAPHICS_OUTPUT_MODE_INFORMATION`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct GraphicsModeInfo {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    pub pixel_format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub reserved_mask: u32,
    pub pixels_per_scan_line: u32,
}

/// `EFI_RNG_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
#[allow(missing_docs)]
pub struct Rng {
    pub get_info: Slot,
    pub get_rng: unsafe extern "efiapi" fn(*mut Rng, *const RawGuid, usize, *mut u8) -> Status,
}
