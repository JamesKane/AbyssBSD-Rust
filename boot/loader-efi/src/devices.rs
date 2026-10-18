// SPDX-License-Identifier: BSD-2-Clause
//! Finding the early console devices: the serial port from ACPI (SPCR, then
//! DBG2) and the GOP framebuffer.

use acpi_tables::{AddressSpace, PhysMem, Tables, UartKind};
use bootproto::PixelFormat;
use efi::{SystemTable, raw};

/// ACPI table memory, reached through the firmware's identity map.
#[derive(Clone, Copy, Debug)]
struct Identity;

impl PhysMem<'static> for Identity {
    fn bytes(&self, pa: u64, len: usize) -> Option<&'static [u8]> {
        if pa == 0 {
            return None;
        }
        let pa = usize::try_from(pa).ok()?;
        pa.checked_add(len)?;
        // SAFETY: UEFI identity-maps all memory while boot services run,
        // and firmware tables stay put; the parser bounds every access by
        // the table lengths it validated.
        Some(unsafe { core::slice::from_raw_parts(pa as *const u8, len) })
    }
}

/// A serial port before it has a virtual address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SerialPort {
    /// 16550 at an I/O port.
    Port(u16),
    /// 16550 in MMIO.
    Ns16550 { phys: u64, shift: u8, width: u8 },
    /// PL011 or SBSA UART.
    Pl011 { phys: u64 },
}

/// Why no serial port was chosen, for the loader's log.
#[derive(Clone, Copy, Debug)]
pub(crate) enum NoSerial {
    Acpi(acpi_tables::Error),
    NotDescribed,
    Unsupported(UartKind, AddressSpace),
}

impl core::fmt::Display for NoSerial {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Acpi(e) => write!(f, "ACPI tables unusable: {e:?}"),
            Self::NotDescribed => f.write_str("no SPCR or DBG2 serial port"),
            Self::Unsupported(kind, space) => write!(f, "unsupported UART {kind:?} in {space:?}"),
        }
    }
}

/// The console UART ACPI describes. On x86 with no SPCR or DBG2, COM1.
pub(crate) fn serial(rsdp: u64) -> Result<SerialPort, NoSerial> {
    let tables = Tables::new(Identity, rsdp).map_err(NoSerial::Acpi)?;
    let Some(port) = acpi_tables::console_uart(&tables) else {
        return if cfg!(target_arch = "x86_64") {
            Ok(SerialPort::Port(0x3f8))
        } else {
            Err(NoSerial::NotDescribed)
        };
    };
    let width = port.base.access_bytes().unwrap_or(1);
    match (port.kind, port.base.space) {
        (UartKind::Ns16550, AddressSpace::Io) => u16::try_from(port.base.address)
            .map(SerialPort::Port)
            .map_err(|_| NoSerial::Unsupported(port.kind, port.base.space)),
        (UartKind::Ns16550, AddressSpace::Memory) => Ok(SerialPort::Ns16550 {
            phys: port.base.address,
            // Registers are as far apart as they are wide.
            shift: match width {
                4 => 2,
                2 => 1,
                _ => 0,
            },
            width,
        }),
        (UartKind::Pl011 | UartKind::SbsaGeneric, AddressSpace::Memory) => Ok(SerialPort::Pl011 {
            phys: port.base.address,
        }),
        (kind, space) => Err(NoSerial::Unsupported(kind, space)),
    }
}

/// The GOP framebuffer, before it has a virtual address.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Framebuffer {
    pub(crate) phys: u64,
    pub(crate) size: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
    pub(crate) format: PixelFormat,
}

/// The current GOP mode, if it has a linear framebuffer.
pub(crate) fn framebuffer(st: &SystemTable) -> Option<Framebuffer> {
    let gop = st.locate_protocol::<raw::GraphicsOutput>().ok()?;
    // SAFETY: firmware returned a live protocol; its mode and info
    // structures stay valid while boot services run.
    let (mode, info) = unsafe {
        let mode = (*gop).mode.as_ref()?;
        (mode, mode.info.as_ref()?)
    };
    let format = match info.pixel_format {
        0 => PixelFormat::Rgbx,
        1 => PixelFormat::Bgrx,
        2 => PixelFormat::Masks {
            red: info.red_mask,
            green: info.green_mask,
            blue: info.blue_mask,
        },
        _ => return None, // Blt-only: no linear framebuffer.
    };
    if mode.frame_buffer_base == 0 || info.pixels_per_scan_line < info.horizontal_resolution {
        return None;
    }
    Some(Framebuffer {
        phys: mode.frame_buffer_base,
        size: u64::try_from(mode.frame_buffer_size).ok()?,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        stride: info.pixels_per_scan_line,
        format,
    })
}
