// SPDX-License-Identifier: BSD-2-Clause
//! The safe layer against an EFI 1.10 firmware: a system table of revision
//! 1.10 whose boot and runtime services tables are 1.10-sized, with every
//! slot past that size poisoned (ADR-0023; the Mac Pro (Late 2013) reports
//! 1.10).

extern crate std;

use core::ffi::c_void;
use core::mem::offset_of;
use std::boxed::Box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec;

use crate::raw::{
    self, BootServices, ConfigurationTable, Handle, MemoryDescriptor, RawGuid, RuntimeServices,
    Status, TableHeader, Time, TimeCapabilities,
};
use crate::{Protocol, SystemTable};

/// What a slot past a 1.10 table's end holds: never a callable address.
const POISON: usize = 0xDEAD_0000_0000_0110;

/// The one interface the fake firmware hands out.
static INTERFACE: u64 = 0x1_10;
/// The fake firmware's handles.
static HANDLES: [usize; 2] = [0x1000, 0x2000];
/// Pool frees, to check `HandleBuffer` returns its buffer.
static FREES: AtomicUsize = AtomicUsize::new(0);

fn is_loaded_image(guid: *const RawGuid) -> bool {
    // SAFETY: the safe layer passes a valid GUID.
    let guid = unsafe { (*guid).get() };
    guid == raw::LoadedImage::GUID
}

extern "efiapi" fn allocate_pages(_: u32, _: u32, pages: usize, at: *mut u64) -> Status {
    let mem = Box::leak(vec![0u8; pages * 4096].into_boxed_slice());
    // SAFETY: the safe layer passes a valid out-pointer.
    unsafe { *at = mem.as_ptr() as u64 };
    Status::SUCCESS
}

extern "efiapi" fn free_pages(_: u64, _: usize) -> Status {
    Status::SUCCESS
}

extern "efiapi" fn get_memory_map(
    size: *mut usize,
    map: *mut MemoryDescriptor,
    key: *mut usize,
    desc_size: *mut usize,
    version: *mut u32,
) -> Status {
    let one = core::mem::size_of::<MemoryDescriptor>();
    // SAFETY: the safe layer passes valid out-pointers and a buffer of
    // `*size` bytes.
    unsafe {
        *desc_size = one;
        *version = 1;
        *key = 7;
        if *size < one {
            *size = one;
            return Status::BUFFER_TOO_SMALL;
        }
        *size = one;
        map.write_unaligned(MemoryDescriptor {
            kind: 7,
            physical_start: 0x10_0000,
            virtual_start: 0,
            number_of_pages: 256,
            attribute: 0xF,
        });
    }
    Status::SUCCESS
}

extern "efiapi" fn allocate_pool(_: u32, size: usize, out: *mut *mut u8) -> Status {
    let mem = Box::leak(vec![0u8; size].into_boxed_slice());
    // SAFETY: the safe layer passes a valid out-pointer.
    unsafe { *out = mem.as_mut_ptr() };
    Status::SUCCESS
}

extern "efiapi" fn free_pool(_: *mut u8) -> Status {
    FREES.fetch_add(1, Ordering::Relaxed);
    Status::SUCCESS
}

extern "efiapi" fn handle_protocol(
    _: Handle,
    guid: *const RawGuid,
    out: *mut *mut c_void,
) -> Status {
    if !is_loaded_image(guid) {
        return Status::UNSUPPORTED;
    }
    // SAFETY: the safe layer passes a valid out-pointer.
    unsafe { *out = (&raw const INTERFACE).cast_mut().cast() };
    Status::SUCCESS
}

extern "efiapi" fn exit_boot_services(_: Handle, _: usize) -> Status {
    Status::SUCCESS
}

extern "efiapi" fn stall(_: usize) -> Status {
    Status::SUCCESS
}

extern "efiapi" fn set_watchdog_timer(_: usize, _: u64, _: usize, _: *const u16) -> Status {
    Status::SUCCESS
}

extern "efiapi" fn locate_handle_buffer(
    _: u32,
    guid: *const RawGuid,
    _: *mut c_void,
    n: *mut usize,
    out: *mut *mut Handle,
) -> Status {
    if !is_loaded_image(guid) {
        return Status::NOT_FOUND;
    }
    let buf: Box<[Handle]> = HANDLES.iter().map(|&h| Handle(h as *mut c_void)).collect();
    // SAFETY: the safe layer passes valid out-pointers.
    unsafe {
        *n = buf.len();
        *out = Box::leak(buf).as_mut_ptr();
    }
    Status::SUCCESS
}

extern "efiapi" fn locate_protocol(
    guid: *const RawGuid,
    _: *mut c_void,
    out: *mut *mut c_void,
) -> Status {
    handle_protocol(Handle(core::ptr::null_mut()), guid, out)
}

extern "efiapi" fn get_time(_: *mut Time, _: *mut TimeCapabilities) -> Status {
    Status::UNSUPPORTED
}

extern "efiapi" fn set_time(_: *const Time) -> Status {
    Status::UNSUPPORTED
}

extern "efiapi" fn get_wakeup_time(_: *mut u8, _: *mut u8, _: *mut Time) -> Status {
    Status::UNSUPPORTED
}

extern "efiapi" fn set_wakeup_time(_: u8, _: *const Time) -> Status {
    Status::UNSUPPORTED
}

extern "efiapi" fn get_variable(
    _: *const u16,
    _: *const RawGuid,
    attrs: *mut u32,
    size: *mut usize,
    data: *mut u8,
) -> Status {
    let value = *b"1.10";
    // SAFETY: the safe layer passes valid out-pointers and a buffer of
    // `*size` bytes.
    unsafe {
        if *size < value.len() {
            *size = value.len();
            return Status::BUFFER_TOO_SMALL;
        }
        *attrs = 7;
        *size = value.len();
        core::ptr::copy_nonoverlapping(value.as_ptr(), data, value.len());
    }
    Status::SUCCESS
}

extern "efiapi" fn get_next_variable_name(_: *mut usize, _: *mut u16, _: *mut RawGuid) -> Status {
    Status::NOT_FOUND
}

extern "efiapi" fn set_variable(
    _: *const u16,
    _: *const RawGuid,
    _: u32,
    _: usize,
    _: *const u8,
) -> Status {
    Status::SUCCESS
}

extern "efiapi" fn reset_system(_: u32, _: Status, _: usize, _: *const c_void) -> ! {
    std::process::abort()
}

fn header(signature: u64, size: usize) -> TableHeader {
    TableHeader {
        signature,
        revision: raw::EFI_1_10_REVISION,
        header_size: u32::try_from(size).expect("table size"),
        crc32: 0,
        reserved: 0,
    }
}

/// An EFI 1.10 system table, leaked for the `'static` the wrapper needs.
fn firmware_1_10() -> &'static raw::SystemTable {
    let boot = Box::leak(Box::new(BootServices {
        hdr: header(0x5652_4553_544f_4f42, BootServices::EFI_1_10_SIZE),
        raise_tpl: 0,
        restore_tpl: 0,
        allocate_pages,
        free_pages,
        get_memory_map,
        allocate_pool,
        free_pool,
        create_event: 0,
        set_timer: 0,
        wait_for_event: 0,
        signal_event: 0,
        close_event: 0,
        check_event: 0,
        install_protocol_interface: 0,
        reinstall_protocol_interface: 0,
        uninstall_protocol_interface: 0,
        handle_protocol,
        reserved: 0,
        register_protocol_notify: 0,
        locate_handle: 0,
        locate_device_path: 0,
        install_configuration_table: 0,
        load_image: 0,
        start_image: 0,
        exit: 0,
        unload_image: 0,
        exit_boot_services,
        get_next_monotonic_count: 0,
        stall,
        set_watchdog_timer,
        connect_controller: 0,
        disconnect_controller: 0,
        open_protocol: 0,
        close_protocol: 0,
        open_protocol_information: 0,
        protocols_per_handle: 0,
        locate_handle_buffer,
        locate_protocol,
        install_multiple_protocol_interfaces: 0,
        uninstall_multiple_protocol_interfaces: 0,
        calculate_crc32: 0,
        copy_mem: 0,
        set_mem: 0,
        // UEFI 2.0.
        create_event_ex: POISON,
    }));
    let runtime = Box::leak(Box::new(RuntimeServices {
        hdr: header(0x5652_4553_544e_5552, RuntimeServices::EFI_1_10_SIZE),
        get_time,
        set_time,
        get_wakeup_time,
        set_wakeup_time,
        set_virtual_address_map: 0,
        convert_pointer: 0,
        get_variable,
        get_next_variable_name,
        set_variable,
        get_next_high_monotonic_count: 0,
        reset_system,
        // UEFI 2.0.
        update_capsule: POISON,
        query_capsule_capabilities: POISON,
        query_variable_info: POISON,
    }));
    let vendor: &'static [u16] =
        Box::leak("Apple".encode_utf16().chain([0]).collect::<Box<[u16]>>());
    let tables: &'static [ConfigurationTable] = Box::leak(Box::new([ConfigurationTable {
        vendor_guid: RawGuid::new(crate::tables::ACPI_20),
        vendor_table: (&raw const INTERFACE).cast(),
    }]));
    Box::leak(Box::new(raw::SystemTable {
        hdr: header(
            raw::SystemTable::SIGNATURE,
            core::mem::size_of::<raw::SystemTable>(),
        ),
        firmware_vendor: vendor.as_ptr(),
        firmware_revision: 0x0001_0000,
        console_in_handle: Handle(core::ptr::null_mut()),
        con_in: core::ptr::null_mut(),
        console_out_handle: Handle(core::ptr::null_mut()),
        con_out: core::ptr::null_mut(),
        standard_error_handle: Handle(core::ptr::null_mut()),
        std_err: core::ptr::null_mut(),
        runtime_services: runtime,
        boot_services: boot,
        number_of_table_entries: tables.len(),
        configuration_table: tables.as_ptr(),
    }))
}

fn system() -> SystemTable {
    // SAFETY: a well-formed (fake) system table that lives forever, and no
    // test exits its boot services through a second wrapper.
    unsafe { SystemTable::from_raw(firmware_1_10()) }.expect("1.10 table accepted")
}

#[test]
fn the_1_10_tables_end_where_the_specification_says() {
    // EFI 1.10 §4.4: the boot services end with `SetMem` (UEFI 2.0 put
    // `CreateEventEx` at 0x170); §4.5: the runtime services end with
    // `ResetSystem` at 0x68.
    assert_eq!(BootServices::EFI_1_10_SIZE, 0x170);
    assert_eq!(RuntimeServices::EFI_1_10_SIZE, 0x70);
    assert_eq!(offset_of!(RuntimeServices, reset_system), 0x68);
    assert_eq!(raw::EFI_1_10_REVISION, 0x0001_000A);
}

#[test]
fn a_1_10_header_does_not_provide_the_uefi_2_services() {
    let st = firmware_1_10();
    // SAFETY: the fake table's pointers are valid.
    let (bs, rt) = unsafe { (&*st.boot_services, &*st.runtime_services) };
    assert!(bs.hdr.provides(offset_of!(BootServices, set_mem)));
    assert!(!bs.hdr.provides(offset_of!(BootServices, create_event_ex)));
    assert!(rt.hdr.provides(offset_of!(RuntimeServices, reset_system)));
    assert!(!rt.hdr.provides(offset_of!(RuntimeServices, update_capsule)));
    assert!(
        !rt.hdr
            .provides(offset_of!(RuntimeServices, query_variable_info))
    );
}

#[test]
fn every_called_service_is_in_the_1_10_tables() {
    let st = firmware_1_10();
    // SAFETY: the fake table's pointers are valid.
    let (bs, rt) = unsafe { (&*st.boot_services, &*st.runtime_services) };
    for &offset in crate::BOOT_SERVICES_CALLED {
        assert!(bs.hdr.provides(offset), "boot service at {offset:#x}");
    }
    for &offset in crate::RUNTIME_SERVICES_CALLED {
        assert!(rt.hdr.provides(offset), "runtime service at {offset:#x}");
    }
}

#[test]
fn the_loaders_lookups_work_on_1_10_firmware() {
    let st = system();
    assert_eq!(st.revision(), raw::EFI_1_10_REVISION);
    assert!(st.configuration_table(crate::tables::ACPI_20).is_some());

    let image = st
        .locate_protocol::<raw::LoadedImage>()
        .expect("LocateProtocol");
    assert_eq!(image.cast::<u64>().cast_const(), &raw const INTERFACE);
    let image = st
        .handle_protocol::<raw::LoadedImage>(Handle(0x1000 as *mut c_void))
        .expect("HandleProtocol");
    assert_eq!(image.cast::<u64>().cast_const(), &raw const INTERFACE);
    assert!(
        st.handle_protocol::<raw::BlockIo>(Handle(0x1000 as *mut c_void))
            .is_err()
    );

    let before = FREES.load(Ordering::Relaxed);
    {
        let handles = st
            .handles_with::<raw::LoadedImage>()
            .expect("LocateHandleBuffer");
        assert_eq!(handles.handles().len(), HANDLES.len());
    }
    assert!(
        FREES.load(Ordering::Relaxed) > before,
        "the buffer goes back to the pool"
    );

    let mut buf = [0u8; 256];
    let map = st.memory_map(&mut buf).expect("GetMemoryMap");
    assert_eq!(map.iter().count(), 1);

    let mut value = [0u8; 8];
    let name: [u16; 2] = [u16::from(b'x'), 0];
    let n = st
        .get_variable(&name, crate::tables::ACPI_20, &mut value)
        .expect("GetVariable");
    assert_eq!(&value[..n], b"1.10");
    st.set_variable(&name, crate::tables::ACPI_20, b"1")
        .expect("SetVariable");

    assert!(st.allocate_pool(crate::MemoryType::LOADER_DATA, 64).is_ok());
    st.disable_watchdog().expect("SetWatchdogTimer");
    st.stall(1);
}
