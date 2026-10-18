// SPDX-License-Identifier: BSD-2-Clause
//! Minimal UEFI bindings for the loader.
//!
//! Raw `#[repr(C)]` tables mirror UEFI 2.10 §4; the safe layer on top
//! ([`SystemTable`], [`Console`]) is what the loader uses. Only the services
//! the loader needs are given types; the rest are opaque slots that keep the
//! table layouts correct.

#![no_std]

pub mod raw;

use core::fmt::{self, Write as _};

pub use guid::Guid;
pub use raw::{Handle, Status};

/// The firmware's system table, valid while boot services are active.
#[derive(Clone, Copy, Debug)]
pub struct SystemTable {
    raw: &'static raw::SystemTable,
}

impl SystemTable {
    /// Wraps the pointer firmware passed to the image entry point.
    ///
    /// # Safety
    ///
    /// `ptr` must be the system table pointer given to `efi_main`, and boot
    /// services must not have been exited.
    pub unsafe fn from_raw(ptr: *const raw::SystemTable) -> Option<Self> {
        // SAFETY: the caller guarantees `ptr` is null or the live system
        // table, which firmware keeps valid and unaliased by writers for as
        // long as boot services run.
        let raw = unsafe { ptr.as_ref() }?;
        (raw.hdr.signature == raw::SystemTable::SIGNATURE).then_some(Self { raw })
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
    pub fn console(&self) -> Console {
        Console {
            out: self.raw.con_out,
        }
    }

    /// The vendor configuration tables (ACPI, SMBIOS, …).
    pub fn configuration_tables(&self) -> &'static [raw::ConfigurationTable] {
        if self.raw.configuration_table.is_null() {
            return &[];
        }
        // SAFETY: firmware publishes `number_of_table_entries` entries at
        // `configuration_table` and keeps them while boot services run.
        unsafe {
            core::slice::from_raw_parts(
                self.raw.configuration_table,
                self.raw.number_of_table_entries,
            )
        }
    }

    /// Asks firmware to power the machine off or reset it.
    pub fn reset(&self, kind: ResetKind, status: Status) -> ! {
        // SAFETY: the runtime services table is valid while boot services
        // run; it is never null in a conforming system table.
        let rt = unsafe { &*self.raw.runtime_services };
        // SAFETY: `ResetSystem` takes no data when `data_size` is zero.
        unsafe { (rt.reset_system)(kind as u32, status, 0, core::ptr::null()) }
    }
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
pub struct Console {
    out: *mut raw::SimpleTextOutput,
}

impl fmt::Write for Console {
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
