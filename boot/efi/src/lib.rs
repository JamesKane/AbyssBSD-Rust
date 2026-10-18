// SPDX-License-Identifier: BSD-2-Clause
//! Minimal UEFI bindings for the loader.
//!
//! Raw `#[repr(C)]` layouts live in [`raw`]; the safe layer here is what the
//! loader uses. Boot services exist only as long as a [`SystemTable`] does:
//! [`SystemTable::exit_boot_services`] consumes it and returns a
//! [`RuntimeTable`], so code after the exit cannot call a boot service or
//! hold a [`File`] by construction.

#![no_std]

mod file;
pub mod memory;
pub mod raw;

use core::ffi::c_void;
use core::fmt::{self, Write as _};
use core::marker::PhantomData;

pub use file::{File, Mode};
pub use guid::Guid;
pub use memory::{MemoryDescriptor, MemoryMap, MemoryType};
pub use raw::{Handle, Status};

/// A UEFI result.
pub type Result<T> = core::result::Result<T, Status>;

/// The firmware's system table while boot services are active.
#[derive(Debug)]
pub struct SystemTable {
    raw: &'static raw::SystemTable,
}

impl SystemTable {
    /// Wraps the pointer firmware passed to the image entry point.
    ///
    /// # Safety
    ///
    /// `ptr` must be the system table pointer given to `efi_main`, boot
    /// services must not have been exited, and the caller must not create a
    /// second `SystemTable` that outlives an `exit_boot_services` call on
    /// this one.
    pub unsafe fn from_raw(ptr: *const raw::SystemTable) -> Option<Self> {
        // SAFETY: the caller guarantees `ptr` is null or the live system
        // table, which firmware keeps valid for as long as boot services run.
        let raw = unsafe { ptr.as_ref() }?;
        (raw.hdr.signature == raw::SystemTable::SIGNATURE).then_some(Self { raw })
    }

    fn bs(&self) -> &raw::BootServices {
        // SAFETY: a conforming system table has a valid boot services
        // pointer while boot services run, which `self` existing implies.
        unsafe { &*self.raw.boot_services }
    }

    /// The raw table address, for the kernel handoff.
    pub fn as_ptr(&self) -> *const raw::SystemTable {
        self.raw
    }

    /// UEFI revision, e.g. `0x0002_0046` for 2.70.
    pub fn revision(&self) -> u32 {
        self.raw.hdr.revision
    }

    /// The firmware vendor and revision.
    pub fn firmware(&self) -> Firmware {
        Firmware {
            vendor: self.raw.firmware_vendor,
            revision: self.raw.firmware_revision,
        }
    }

    /// The console output device.
    pub fn console(&self) -> Console<'_> {
        Console {
            out: self.raw.con_out,
            _st: PhantomData,
        }
    }

    /// The vendor configuration tables (ACPI, SMBIOS, …).
    pub fn configuration_tables(&self) -> &'static [raw::ConfigurationTable] {
        config_tables(self.raw)
    }

    /// The configuration table with `guid`, if present.
    pub fn configuration_table(&self, guid: Guid) -> Option<*const c_void> {
        self.configuration_tables()
            .iter()
            .find(|t| t.vendor_guid.get() == guid)
            .map(|t| t.vendor_table)
    }

    /// The runtime services table.
    pub fn runtime_services(&self) -> *const raw::RuntimeServices {
        self.raw.runtime_services
    }

    /// Allocates `pages` 4 KiB pages of `kind` memory.
    pub fn allocate_pages(&self, at: Allocate, kind: MemoryType, pages: usize) -> Result<u64> {
        let (ty, mut addr) = match at {
            Allocate::Anywhere => (0, 0),
            Allocate::Below(max) => (1, max),
            Allocate::At(a) => (2, a),
        };
        // SAFETY: `addr` is a valid out-pointer; the arguments follow the
        // AllocatePages contract.
        unsafe { (self.bs().allocate_pages)(ty, kind.0, pages, &raw mut addr) }.ok()?;
        Ok(addr)
    }

    /// Frees pages from [`SystemTable::allocate_pages`].
    ///
    /// # Safety
    ///
    /// Nothing may use the pages afterwards.
    pub unsafe fn free_pages(&self, addr: u64, pages: usize) -> Result<()> {
        // SAFETY: the caller gives up the pages.
        unsafe { (self.bs().free_pages)(addr, pages) }.ok()
    }

    /// Allocates `size` bytes of pool memory of `kind`, 8-byte aligned.
    pub fn allocate_pool(&self, kind: MemoryType, size: usize) -> Result<*mut u8> {
        let mut p = core::ptr::null_mut();
        // SAFETY: valid out-pointer.
        unsafe { (self.bs().allocate_pool)(kind.0, size, &raw mut p) }.ok()?;
        Ok(p)
    }

    /// Frees pool memory.
    ///
    /// # Safety
    ///
    /// `p` must come from [`SystemTable::allocate_pool`] and not be used
    /// afterwards.
    pub unsafe fn free_pool(&self, p: *mut u8) -> Result<()> {
        // SAFETY: per the caller's contract.
        unsafe { (self.bs().free_pool)(p) }.ok()
    }

    /// Bytes needed for the memory map right now, with slack for the
    /// allocation that will hold it.
    pub fn memory_map_size(&self) -> Result<usize> {
        let (mut size, mut key, mut desc_size, mut version) = (0, 0, 0, 0);
        // SAFETY: a zero-size query with a null buffer is allowed and only
        // writes the out-parameters.
        let status = unsafe {
            (self.bs().get_memory_map)(
                &raw mut size,
                core::ptr::null_mut(),
                &raw mut key,
                &raw mut desc_size,
                &raw mut version,
            )
        };
        match status {
            Status::BUFFER_TOO_SMALL => Ok(size + 8 * desc_size.max(48)),
            s => Err(if s.is_error() {
                s
            } else {
                Status::DEVICE_ERROR
            }),
        }
    }

    /// Reads the current memory map into `buf`.
    pub fn memory_map<'b>(&self, buf: &'b mut [u8]) -> Result<MemoryMap<'b>> {
        memory_map(self.bs(), buf)
    }

    /// The protocol `P` on `handle`.
    pub fn handle_protocol<P: Protocol>(&self, handle: Handle) -> Result<*mut P> {
        let guid = raw::RawGuid::new(P::GUID);
        let mut out = core::ptr::null_mut();
        // SAFETY: valid GUID and out-pointer.
        unsafe { (self.bs().handle_protocol)(handle, &raw const guid, &raw mut out) }.ok()?;
        if out.is_null() {
            return Err(Status::NOT_FOUND);
        }
        Ok(out.cast())
    }

    /// The first instance of protocol `P` in the system.
    pub fn locate_protocol<P: Protocol>(&self) -> Result<*mut P> {
        let guid = raw::RawGuid::new(P::GUID);
        let mut out = core::ptr::null_mut();
        // SAFETY: valid GUID and out-pointer; no registration key.
        unsafe {
            (self.bs().locate_protocol)(&raw const guid, core::ptr::null_mut(), &raw mut out)
        }
        .ok()?;
        if out.is_null() {
            return Err(Status::NOT_FOUND);
        }
        Ok(out.cast())
    }

    /// Opens the root directory of the volume `device` lives on.
    pub fn open_volume(&self, device: Handle) -> Result<File<'_>> {
        let fs = self.handle_protocol::<raw::SimpleFileSystem>(device)?;
        let mut root = core::ptr::null_mut();
        // SAFETY: `fs` is the protocol firmware returned for `device`.
        unsafe { ((*fs).open_volume)(fs, &raw mut root) }.ok()?;
        // SAFETY: firmware returned an open file handle.
        unsafe { File::from_raw(root) }
    }

    /// Reads variable `name` (NUL-terminated UCS-2, see [`ucs2`]) of
    /// `vendor` into `buf`, returning its size.
    pub fn get_variable(&self, name: &[u16], vendor: Guid, buf: &mut [u8]) -> Result<usize> {
        if name.last() != Some(&0) {
            return Err(Status::INVALID_PARAMETER);
        }
        let guid = raw::RawGuid::new(vendor);
        let mut size = buf.len();
        let mut attrs = 0;
        // SAFETY: valid NUL-terminated name, GUID, and buffer; the runtime
        // services table is valid while boot services run.
        unsafe {
            ((*self.raw.runtime_services).get_variable)(
                name.as_ptr(),
                &raw const guid,
                &raw mut attrs,
                &raw mut size,
                buf.as_mut_ptr(),
            )
        }
        .ok()?;
        Ok(size)
    }

    /// Writes non-volatile variable `name` of `vendor`, visible at boot and
    /// run time.
    pub fn set_variable(&self, name: &[u16], vendor: Guid, data: &[u8]) -> Result<()> {
        const NV_BS_RT: u32 = 0x7;
        if name.last() != Some(&0) {
            return Err(Status::INVALID_PARAMETER);
        }
        let guid = raw::RawGuid::new(vendor);
        // SAFETY: as for `get_variable`.
        unsafe {
            ((*self.raw.runtime_services).set_variable)(
                name.as_ptr(),
                &raw const guid,
                NV_BS_RT,
                data.len(),
                data.as_ptr(),
            )
        }
        .ok()
    }

    /// Disables the firmware watchdog (which otherwise resets the machine
    /// five minutes after boot).
    pub fn disable_watchdog(&self) -> Result<()> {
        // SAFETY: a zero timeout with no data disables the watchdog.
        unsafe { (self.bs().set_watchdog_timer)(0, 0, 0, core::ptr::null()) }.ok()
    }

    /// Busy-waits for `us` microseconds.
    pub fn stall(&self, us: usize) {
        // SAFETY: Stall has no preconditions.
        let _ = unsafe { (self.bs().stall)(us) };
    }

    /// Asks firmware to power the machine off or reset it.
    pub fn reset(&self, kind: ResetKind, status: Status) -> ! {
        reset(self.raw, kind, status)
    }

    /// Exits boot services, leaving the final memory map in `buf`.
    ///
    /// Retries once if the map changed between reading it and exiting, as
    /// the specification allows. On failure the firmware may be in a state
    /// where only `GetMemoryMap` and `ExitBootServices` work, so the error
    /// is final.
    pub fn exit_boot_services(
        self,
        image: Handle,
        buf: &mut [u8],
    ) -> core::result::Result<(RuntimeTable, MemoryMap<'_>), Status> {
        let bs = self.bs();
        for _ in 0..2 {
            let (key, len, desc_size) = {
                let map = memory_map(bs, buf)?;
                (map.key, map.byte_len(), map.descriptor_size())
            };
            // SAFETY: `image` is our image handle and `key` is current, or
            // firmware rejects the call and we retry with a fresh map.
            if unsafe { (bs.exit_boot_services)(image, key) } == Status::SUCCESS {
                let rt = RuntimeTable { raw: self.raw };
                // Boot services are gone; describe the map already read.
                let map = MemoryMap::new(&buf[..len], key, desc_size)?;
                return Ok((rt, map));
            }
        }
        Err(Status::ABORTED)
    }
}

fn config_tables(raw: &'static raw::SystemTable) -> &'static [raw::ConfigurationTable] {
    if raw.configuration_table.is_null() {
        return &[];
    }
    // SAFETY: firmware publishes `number_of_table_entries` entries at
    // `configuration_table`, in memory that stays valid after boot services
    // (runtime data).
    unsafe { core::slice::from_raw_parts(raw.configuration_table, raw.number_of_table_entries) }
}

fn memory_map<'b>(bs: &raw::BootServices, buf: &'b mut [u8]) -> Result<MemoryMap<'b>> {
    let (mut size, mut key, mut desc_size, mut version) = (buf.len(), 0, 0, 0);
    // SAFETY: `buf` is writable for `size` bytes; descriptors are written
    // with unaligned-safe stride handling on the read side.
    unsafe {
        (bs.get_memory_map)(
            &raw mut size,
            buf.as_mut_ptr().cast(),
            &raw mut key,
            &raw mut desc_size,
            &raw mut version,
        )
    }
    .ok()?;
    MemoryMap::new(&buf[..size], key, desc_size)
}

fn reset(raw: &raw::SystemTable, kind: ResetKind, status: Status) -> ! {
    // SAFETY: the runtime services table is valid for the life of the
    // system, before and after ExitBootServices.
    let rt = unsafe { &*raw.runtime_services };
    // SAFETY: `ResetSystem` takes no data when `data_size` is zero.
    unsafe { (rt.reset_system)(kind as u32, status, 0, core::ptr::null()) }
}

/// The system table after `ExitBootServices`: runtime services and
/// configuration tables only.
#[derive(Debug)]
pub struct RuntimeTable {
    raw: &'static raw::SystemTable,
}

impl RuntimeTable {
    /// Asks firmware to power the machine off or reset it.
    pub fn reset(&self, kind: ResetKind, status: Status) -> ! {
        reset(self.raw, kind, status)
    }
}

/// An ASCII string as NUL-terminated UCS-2, for variable names. `N` must
/// be the length plus one; anything else fails to compile when used in a
/// constant.
#[must_use]
pub const fn ucs2<const N: usize>(s: &str) -> [u16; N] {
    let b = s.as_bytes();
    assert!(
        b.len() + 1 == N,
        "ucs2: N must be the string length plus one"
    );
    let mut out = [0u16; N];
    let mut i = 0;
    while i < b.len() {
        assert!(b[i].is_ascii(), "ucs2: ASCII only");
        out[i] = b[i] as u16;
        i += 1;
    }
    out
}

/// Where [`SystemTable::allocate_pages`] may place memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allocate {
    /// Anywhere.
    Anywhere,
    /// Entirely below this address.
    Below(u64),
    /// Exactly here.
    At(u64),
}

/// A protocol interface type with its GUID.
///
/// # Safety
///
/// `GUID` must identify a protocol whose interface has the layout of `Self`.
pub unsafe trait Protocol {
    /// The protocol GUID.
    const GUID: Guid;
}

// SAFETY: GUIDs and layouts from UEFI 2.10 §9.1, §13.4, §12.9, §37.5.
unsafe impl Protocol for raw::LoadedImage {
    const GUID: Guid = Guid::must_parse("5B1B31A1-9562-11D2-8E3F-00A0C969723B");
}
// SAFETY: see above.
unsafe impl Protocol for raw::SimpleFileSystem {
    const GUID: Guid = Guid::must_parse("964E5B22-6459-11D2-8E39-00A0C969723B");
}
// SAFETY: see above.
unsafe impl Protocol for raw::GraphicsOutput {
    const GUID: Guid = Guid::must_parse("9042A9DE-23DC-4A38-96FB-7ADED080516A");
}
// SAFETY: see above.
unsafe impl Protocol for raw::Rng {
    const GUID: Guid = Guid::must_parse("3152BCA5-EADE-433D-862E-C01CDC291F44");
}

/// How [`SystemTable::reset`] should reset the platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ResetKind {
    /// Full power cycle of the platform.
    Cold = 0,
    /// Reset the processors and devices without a power cycle.
    Warm = 1,
    /// Power off (ACPI G2/S5).
    Shutdown = 2,
}

/// Firmware identification from the system table.
#[derive(Clone, Copy, Debug)]
pub struct Firmware {
    vendor: *const u16,
    /// Vendor-specific revision number.
    pub revision: u32,
}

impl fmt::Display for Firmware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.vendor.is_null() {
            return f.write_str("(unknown)");
        }
        let mut p = self.vendor;
        // Bounded so a missing terminator cannot run away.
        for _ in 0..256 {
            // SAFETY: the vendor string is a NUL-terminated UCS-2 string
            // owned by firmware; reading stops at the terminator.
            let unit = unsafe { p.read_unaligned() };
            if unit == 0 {
                break;
            }
            f.write_char(char::from_u32(u32::from(unit)).unwrap_or('?'))?;
            // SAFETY: the terminator has not been reached, so the next unit
            // is still inside the string.
            p = unsafe { p.add(1) };
        }
        Ok(())
    }
}

/// The firmware text console, as a [`fmt::Write`] sink.
///
/// Text is converted to UCS-2 and `\n` becomes `\r\n`.
#[derive(Debug)]
pub struct Console<'st> {
    out: *mut raw::SimpleTextOutput,
    _st: PhantomData<&'st SystemTable>,
}

impl fmt::Write for Console<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.out.is_null() {
            return Err(fmt::Error);
        }
        const CHUNK: usize = 128;
        let mut buf = [0u16; CHUNK + 1];
        let mut len = 0;
        let mut units = s.chars().flat_map(|c| {
            let (first, second) = match c {
                '\n' => ('\r', Some('\n')),
                c if u32::from(c) > 0xFFFF => ('\u{FFFD}', None),
                c => (c, None),
            };
            core::iter::once(first).chain(second)
        });
        loop {
            let next = units.next();
            if let Some(c) = next {
                // `c` is in the Basic Multilingual Plane after the mapping
                // above, so it fits in one UCS-2 unit.
                buf[len] = u16::try_from(u32::from(c)).unwrap_or(0xFFFD);
                len += 1;
            }
            if len == CHUNK || (next.is_none() && len > 0) {
                buf[len] = 0;
                // SAFETY: `out` is the console protocol from the system
                // table and `buf` is NUL-terminated.
                let status = unsafe { ((*self.out).output_string)(self.out, buf.as_ptr()) };
                if status.is_error() {
                    return Err(fmt::Error);
                }
                len = 0;
            }
            if next.is_none() {
                return Ok(());
            }
        }
    }
}

/// Well-known configuration table GUIDs.
pub mod tables {
    use super::Guid;

    /// ACPI 2.0+ RSDP.
    pub const ACPI_20: Guid = Guid::must_parse("8868E871-E4F1-11D3-BC22-0080C73C8881");
    /// SMBIOS 3.0 entry point.
    pub const SMBIOS3: Guid = Guid::must_parse("F2FD1544-9794-4A2C-992E-E5BBCF20E394");
}
