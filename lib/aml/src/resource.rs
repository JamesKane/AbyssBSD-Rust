// SPDX-License-Identifier: BSD-2-Clause
//! Resource templates (ACPI 6.5 §6.4): decoding the buffers `_CRS`, `_PRS`,
//! and `_AEI` return into typed descriptors.
//!
//! A template is a sequence of small and large descriptors ending in an end
//! tag. Every descriptor's length is checked against its type's minimum and
//! against the buffer; internal offsets (GPIO pin tables, resource source
//! names) must stay inside their descriptor. The end tag's checksum, when
//! non-zero, must make the template sum to zero. Pin function, pin
//! configuration, pin group, and clock descriptors are returned as
//! [`Resource::Unknown`] with their bytes.
//!
//! Behavior reference: `sys/dev/acpica/acpi_resource.c` (which descriptors
//! FreeBSD consumes and how it reads their flags).

use alloc::string::String;
use alloc::vec::Vec;

use crate::Error;

/// Interrupt trigger mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Level triggered.
    Level,
    /// Edge triggered.
    Edge,
}

/// Interrupt polarity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    /// Active high (rising edge).
    ActiveHigh,
    /// Active low (falling edge).
    ActiveLow,
    /// Both edges (GPIO interrupts only).
    ActiveBoth,
}

/// A resource source: the device that produces a resource, by path, and an
/// index into its resources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceSource {
    /// Index into the source device's resources.
    pub index: u8,
    /// Path of the source device, as written (may be relative).
    pub name: String,
}

/// `IRQ` / `IRQNoFlags` (small item 0x04).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Irq {
    /// Bit mask of ISA IRQs 0–15.
    pub mask: u16,
    /// Trigger mode (edge when the descriptor has no flags byte).
    pub trigger: Trigger,
    /// Polarity (active high when the descriptor has no flags byte).
    pub polarity: Polarity,
    /// Shared.
    pub shared: bool,
    /// Wake capable.
    pub wake: bool,
}

/// `DMA` (small item 0x05).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dma {
    /// Bit mask of channels 0–7.
    pub channels: u8,
    /// Transfer size code (0: 8-bit, 1: 8- and 16-bit, 2: 16-bit).
    pub transfer: u8,
    /// Bus master.
    pub bus_master: bool,
    /// Speed code (0: compatibility, 1: type A, 2: type B, 3: type F).
    pub speed: u8,
}

/// `IO` (small item 0x08).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Io {
    /// Decodes 16 address bits (otherwise 10).
    pub decode16: bool,
    /// Lowest base address.
    pub min: u16,
    /// Highest base address.
    pub max: u16,
    /// Base alignment.
    pub alignment: u8,
    /// Number of ports.
    pub length: u8,
}

/// `Memory24`, `Memory32`, or `FixedMemory32`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    /// Writeable.
    pub writeable: bool,
    /// Lowest base address.
    pub min: u32,
    /// Highest base address.
    pub max: u32,
    /// Base alignment (0 for a fixed range).
    pub alignment: u32,
    /// Length in bytes.
    pub length: u32,
}

/// Memory caching attribute of an address space descriptor (`_MEM`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caching {
    /// Non-cacheable.
    NonCacheable,
    /// Cacheable.
    Cacheable,
    /// Write-combining.
    WriteCombining,
    /// Prefetchable.
    Prefetchable,
}

/// The kind of an address space descriptor, with its type-specific flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressKind {
    /// A memory range.
    Memory {
        /// Writeable (`_RW`).
        writeable: bool,
        /// Caching (`_MEM`).
        caching: Caching,
        /// Memory type (`_MTP`: 0 memory, 1 reserved, 2 ACPI, 3 NVS).
        memory_type: u8,
        /// Type translation (`_TTP`): the range is I/O on the other side.
        translation: bool,
    },
    /// An I/O range.
    Io {
        /// ISA range (`_RNG`: 1 non-ISA only, 2 ISA only, 3 entire range).
        range: u8,
        /// Type translation (`_TTP`): the range is memory on the other side.
        translation: bool,
        /// Sparse translation (`_TRS`).
        sparse: bool,
    },
    /// A bus number range.
    BusNumber,
    /// A vendor-defined type (192–255) or reserved type, with its raw
    /// type-specific flags.
    Other {
        /// The resource type byte.
        kind: u8,
        /// The type-specific flags byte.
        flags: u8,
    },
}

/// The size of an address space descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressWidth {
    /// `WordSpace` (large item 0x08).
    Word,
    /// `DWordSpace` (0x07).
    DWord,
    /// `QWordSpace` (0x0A).
    QWord,
    /// `ExtendedSpace` (0x0B).
    Extended,
}

/// An address space descriptor (`WordIO`, `DWordMemory`, `QWordSpace`,
/// `WordBusNumber`, `ExtendedMemory`, …).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    /// Descriptor size.
    pub width: AddressWidth,
    /// Kind and type-specific flags.
    pub kind: AddressKind,
    /// The device consumes the range (otherwise produces it, as a bridge
    /// window does).
    pub consumer: bool,
    /// Subtractive decode (`_DEC`).
    pub subtractive: bool,
    /// Minimum address is fixed (`_MIF`).
    pub min_fixed: bool,
    /// Maximum address is fixed (`_MAF`).
    pub max_fixed: bool,
    /// Address granularity (`_GRA`).
    pub granularity: u64,
    /// Minimum address (`_MIN`).
    pub min: u64,
    /// Maximum address (`_MAX`).
    pub max: u64,
    /// Translation offset (`_TRA`): add to a primary-side address to get
    /// the secondary-side one.
    pub translation: u64,
    /// Length (`_LEN`).
    pub length: u64,
    /// Type-specific attributes (`ExtendedSpace` only).
    pub attributes: Option<u64>,
    /// Resource source, if present.
    pub source: Option<ResourceSource>,
}

/// `Interrupt` (extended interrupt, large item 0x09).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtendedIrq {
    /// The device consumes the interrupts (otherwise produces them).
    pub consumer: bool,
    /// Trigger mode.
    pub trigger: Trigger,
    /// Polarity.
    pub polarity: Polarity,
    /// Shared.
    pub shared: bool,
    /// Wake capable.
    pub wake: bool,
    /// Interrupt numbers (GSIs unless a source is given).
    pub interrupts: Vec<u32>,
    /// The interrupt controller producing them, if not the system one.
    pub source: Option<ResourceSource>,
}

/// `Register` (generic register, large item 0x02).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenericRegister {
    /// Address space ID.
    pub space: u8,
    /// Register width in bits.
    pub bit_width: u8,
    /// Register offset in bits.
    pub bit_offset: u8,
    /// Access size code.
    pub access_size: u8,
    /// Address.
    pub address: u64,
}

/// What a GPIO connection is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioKind {
    /// `GpioInt`.
    Interrupt {
        /// Trigger mode.
        trigger: Trigger,
        /// Polarity.
        polarity: Polarity,
        /// Shared.
        shared: bool,
        /// Wake capable.
        wake: bool,
    },
    /// `GpioIo`.
    Io {
        /// I/O restriction (0 none, 1 input only, 2 output only, 3 preserve).
        restriction: u8,
        /// Shared.
        shared: bool,
    },
}

/// `GpioInt` or `GpioIo` (large item 0x0C).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gpio {
    /// Descriptor revision.
    pub revision: u8,
    /// Interrupt or I/O, with flags.
    pub kind: GpioKind,
    /// The device consumes the pins.
    pub consumer: bool,
    /// Pin configuration (0 default, 1 pull-up, 2 pull-down, 3 no pull,
    /// 0x80 and up vendor-defined).
    pub pin_config: u8,
    /// Output drive strength, in 0.01 mA.
    pub drive_strength: u16,
    /// Debounce timeout, in 0.01 ms.
    pub debounce: u16,
    /// Pin numbers on the controller.
    pub pins: Vec<u16>,
    /// The GPIO controller.
    pub source: ResourceSource,
    /// Vendor data.
    pub vendor: Vec<u8>,
}

/// The bus-specific part of a serial bus connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SerialBusKind {
    /// `I2cSerialBusV2`.
    I2c {
        /// 10-bit addressing.
        ten_bit: bool,
        /// Connection speed in Hz.
        speed: u32,
        /// Slave address.
        address: u16,
    },
    /// `SpiSerialBusV2`.
    Spi {
        /// Three-wire mode.
        three_wire: bool,
        /// Chip select active high.
        cs_active_high: bool,
        /// Connection speed in Hz.
        speed: u32,
        /// Bits per data word.
        data_bits: u8,
        /// Clock phase (0: first edge, 1: second).
        phase: u8,
        /// Clock polarity (0: idle low, 1: idle high).
        polarity: u8,
        /// Chip select line.
        device_select: u16,
    },
    /// `UartSerialBusV2`.
    Uart {
        /// The raw type-specific flags: flow control (bits 0–1), stop bits
        /// (2–3), data bits (4–6), big endian (7).
        flags: u16,
        /// Baud rate.
        baud: u32,
        /// Receive FIFO size.
        rx_fifo: u16,
        /// Transmit FIFO size.
        tx_fifo: u16,
        /// Parity (0 none, 1 even, 2 odd, 3 mark, 4 space).
        parity: u8,
        /// Serial lines enabled (bit mask).
        lines: u8,
    },
    /// Another bus type (CSI-2, reserved), with its type-specific data.
    Other {
        /// Serial bus type byte.
        kind: u8,
        /// Type-specific flags.
        flags: u16,
        /// Type-specific data.
        data: Vec<u8>,
    },
}

/// A serial bus connection (large item 0x0E).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialBus {
    /// Descriptor revision.
    pub revision: u8,
    /// Type-specific revision.
    pub type_revision: u8,
    /// The device is the bus slave (`ControllerInitiated` when false).
    pub slave_mode: bool,
    /// The device consumes the connection.
    pub consumer: bool,
    /// Shared connection.
    pub shared: bool,
    /// The bus kind and its parameters.
    pub bus: SerialBusKind,
    /// Vendor data after the bus-specific fields.
    pub vendor: Vec<u8>,
    /// The bus controller.
    pub source: ResourceSource,
}

/// One decoded resource descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    /// `IRQ` or `IRQNoFlags`.
    Irq(Irq),
    /// `DMA`.
    Dma(Dma),
    /// `StartDependentFn`: compatibility and performance priorities.
    StartDependent {
        /// Compatibility priority (0 good, 1 acceptable, 2 sub-optimal).
        compatibility: u8,
        /// Performance/robustness priority.
        performance: u8,
    },
    /// `EndDependentFn`.
    EndDependent,
    /// `IO`.
    Io(Io),
    /// `FixedIO`.
    FixedIo {
        /// Base port (10 bits).
        base: u16,
        /// Number of ports.
        length: u8,
    },
    /// `FixedDMA`.
    FixedDma {
        /// DMA request line.
        request: u16,
        /// DMA channel.
        channel: u16,
        /// Transfer width code (0: 8-bit … 5: 256-bit).
        width: u8,
    },
    /// `VendorShort` data.
    VendorShort(Vec<u8>),
    /// `VendorLong` data (a UUID subtype and UUID, then data).
    VendorLong(Vec<u8>),
    /// `Memory24` (addresses and length already scaled to bytes).
    Memory24(Memory),
    /// `Memory32`.
    Memory32(Memory),
    /// `Memory32Fixed`.
    FixedMemory32(Memory),
    /// An address space descriptor.
    Address(Address),
    /// `Interrupt`.
    ExtendedIrq(ExtendedIrq),
    /// `Register`.
    GenericRegister(GenericRegister),
    /// `GpioInt` or `GpioIo`.
    Gpio(Gpio),
    /// `I2cSerialBusV2`, `SpiSerialBusV2`, `UartSerialBusV2`, or another
    /// serial bus.
    SerialBus(SerialBus),
    /// A descriptor not decoded here (pin function, pin configuration, pin
    /// groups, clock input, reserved types): its type byte and the bytes
    /// after its header.
    Unknown {
        /// The first byte of the descriptor.
        tag: u8,
        /// The descriptor's data.
        data: Vec<u8>,
    },
}

const END_TAG: u8 = 0x0f;

fn bad(what: &'static str) -> Error {
    Error::BadResource(what)
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, Error> {
    let s = b.get(o..o + 2).ok_or(bad("truncated field"))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], o: usize) -> Result<u32, Error> {
    let s = b.get(o..o + 4).ok_or(bad("truncated field"))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_at(b: &[u8], o: usize) -> Result<u64, Error> {
    let s = b.get(o..o + 8).ok_or(bad("truncated field"))?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(u64::from_le_bytes(a))
}

fn byte(b: &[u8], o: usize) -> Result<u8, Error> {
    b.get(o).copied().ok_or(bad("truncated field"))
}

/// A NUL-terminated (or unterminated, to the end) name.
fn name_from(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    crate::value::string_from_bytes(&b[..n])
}

/// The optional resource source after a fixed part: an index byte and a
/// name. Absent when nothing follows; a lone index byte has no name.
fn optional_source(rest: &[u8]) -> Option<ResourceSource> {
    let (&index, name) = rest.split_first()?;
    let name = name_from(name);
    if name.is_empty() {
        return None;
    }
    Some(ResourceSource { index, name })
}

/// One descriptor: its header length and total length at `off`.
fn descriptor_at(buf: &[u8], off: usize) -> Result<(u8, usize, usize), Error> {
    let tag = byte(buf, off)?;
    let (hdr, len) = if tag & 0x80 == 0 {
        (1, usize::from(tag & 7))
    } else {
        (3, usize::from(u16_at(buf, off + 1)?))
    };
    let total = hdr + len;
    if off.checked_add(total).is_none_or(|e| e > buf.len()) {
        return Err(bad("descriptor runs past the buffer"));
    }
    Ok((tag, hdr, total))
}

/// Offset of the end tag (the template without it is `buf[..offset]`).
/// An empty buffer counts as an empty template.
///
/// # Errors
///
/// [`Error::BadResource`] for a malformed template or a missing end tag.
pub fn end_tag_offset(buf: &[u8]) -> Result<usize, Error> {
    if buf.is_empty() {
        return Ok(0);
    }
    let mut off = 0;
    while off < buf.len() {
        let (tag, _, total) = descriptor_at(buf, off)?;
        if tag & 0x80 == 0 && (tag >> 3) & 0x0f == END_TAG {
            return Ok(off);
        }
        off += total;
    }
    Err(bad("no end tag"))
}

/// Decodes a resource template.
///
/// # Errors
///
/// [`Error::BadResource`] for a descriptor that is too short, runs past
/// the buffer, or has inconsistent internal offsets; a missing end tag; or
/// a bad checksum.
pub fn parse(buf: &[u8]) -> Result<Vec<Resource>, Error> {
    let mut out = Vec::new();
    let mut off = 0;
    loop {
        if off >= buf.len() {
            return Err(bad("no end tag"));
        }
        let (tag, hdr, total) = descriptor_at(buf, off)?;
        let d = &buf[off + hdr..off + total];
        if tag & 0x80 == 0 {
            let kind = (tag >> 3) & 0x0f;
            if kind == END_TAG {
                if d.len() > 1 {
                    return Err(bad("end tag length"));
                }
                if let Some(&sum) = d.first()
                    && sum != 0
                    && buf[..off + total]
                        .iter()
                        .fold(0u8, |a, &x| a.wrapping_add(x))
                        != 0
                {
                    return Err(bad("checksum"));
                }
                return Ok(out);
            }
            out.push(small(kind, tag, d)?);
        } else {
            out.push(large(tag, &buf[off..off + total], d)?);
        }
        off += total;
    }
}

fn small(kind: u8, tag: u8, d: &[u8]) -> Result<Resource, Error> {
    let need = |ok: bool, what| if ok { Ok(()) } else { Err(bad(what)) };
    Ok(match kind {
        0x04 => {
            need(d.len() == 2 || d.len() == 3, "IRQ length")?;
            let mask = u16_at(d, 0)?;
            let (trigger, polarity, shared, wake) = match d.get(2) {
                Some(&f) => (
                    if f & 1 != 0 {
                        Trigger::Edge
                    } else {
                        Trigger::Level
                    },
                    if f & 8 != 0 {
                        Polarity::ActiveLow
                    } else {
                        Polarity::ActiveHigh
                    },
                    f & 0x10 != 0,
                    f & 0x20 != 0,
                ),
                None => (Trigger::Edge, Polarity::ActiveHigh, false, false),
            };
            Resource::Irq(Irq {
                mask,
                trigger,
                polarity,
                shared,
                wake,
            })
        }
        0x05 => {
            need(d.len() == 2, "DMA length")?;
            Resource::Dma(Dma {
                channels: d[0],
                transfer: d[1] & 3,
                bus_master: d[1] & 4 != 0,
                speed: (d[1] >> 5) & 3,
            })
        }
        0x06 => {
            need(d.len() <= 1, "StartDependentFn length")?;
            let p = d.first().copied().unwrap_or(0x05);
            Resource::StartDependent {
                compatibility: p & 3,
                performance: (p >> 2) & 3,
            }
        }
        0x07 => {
            need(d.is_empty(), "EndDependentFn length")?;
            Resource::EndDependent
        }
        0x08 => {
            need(d.len() == 7, "IO length")?;
            Resource::Io(Io {
                decode16: d[0] & 1 != 0,
                min: u16_at(d, 1)?,
                max: u16_at(d, 3)?,
                alignment: d[5],
                length: d[6],
            })
        }
        0x09 => {
            need(d.len() == 3, "FixedIO length")?;
            Resource::FixedIo {
                base: u16_at(d, 0)? & 0x3ff,
                length: d[2],
            }
        }
        0x0a => {
            need(d.len() == 5, "FixedDMA length")?;
            Resource::FixedDma {
                request: u16_at(d, 0)?,
                channel: u16_at(d, 2)?,
                width: d[4],
            }
        }
        0x0e => {
            need(!d.is_empty(), "VendorShort length")?;
            Resource::VendorShort(d.to_vec())
        }
        _ => Resource::Unknown {
            tag,
            data: d.to_vec(),
        },
    })
}

fn large(tag: u8, full: &[u8], d: &[u8]) -> Result<Resource, Error> {
    let need = |ok: bool, what| if ok { Ok(()) } else { Err(bad(what)) };
    Ok(match tag {
        0x81 => {
            need(d.len() == 9, "Memory24 length")?;
            Resource::Memory24(Memory {
                writeable: d[0] & 1 != 0,
                min: u32::from(u16_at(d, 1)?) << 8,
                max: u32::from(u16_at(d, 3)?) << 8,
                alignment: u32::from(u16_at(d, 5)?),
                length: u32::from(u16_at(d, 7)?) << 8,
            })
        }
        0x82 => {
            need(d.len() == 12, "Register length")?;
            Resource::GenericRegister(GenericRegister {
                space: d[0],
                bit_width: d[1],
                bit_offset: d[2],
                access_size: d[3],
                address: u64_at(d, 4)?,
            })
        }
        0x84 => Resource::VendorLong(d.to_vec()),
        0x85 => {
            need(d.len() == 17, "Memory32 length")?;
            Resource::Memory32(Memory {
                writeable: d[0] & 1 != 0,
                min: u32_at(d, 1)?,
                max: u32_at(d, 5)?,
                alignment: u32_at(d, 9)?,
                length: u32_at(d, 13)?,
            })
        }
        0x86 => {
            need(d.len() == 9, "Memory32Fixed length")?;
            let base = u32_at(d, 1)?;
            let length = u32_at(d, 5)?;
            Resource::FixedMemory32(Memory {
                writeable: d[0] & 1 != 0,
                min: base,
                max: base,
                alignment: 0,
                length,
            })
        }
        0x87 => Resource::Address(address(AddressWidth::DWord, d)?),
        0x88 => Resource::Address(address(AddressWidth::Word, d)?),
        0x8a => Resource::Address(address(AddressWidth::QWord, d)?),
        0x8b => Resource::Address(address(AddressWidth::Extended, d)?),
        0x89 => {
            need(d.len() >= 2, "Interrupt length")?;
            let f = d[0];
            let count = usize::from(d[1]);
            need(count >= 1 && 2 + 4 * count <= d.len(), "Interrupt count")?;
            let interrupts = (0..count)
                .map(|i| u32_at(d, 2 + 4 * i))
                .collect::<Result<Vec<_>, _>>()?;
            Resource::ExtendedIrq(ExtendedIrq {
                consumer: f & 1 != 0,
                trigger: if f & 2 != 0 {
                    Trigger::Edge
                } else {
                    Trigger::Level
                },
                polarity: if f & 4 != 0 {
                    Polarity::ActiveLow
                } else {
                    Polarity::ActiveHigh
                },
                shared: f & 8 != 0,
                wake: f & 0x10 != 0,
                interrupts,
                source: optional_source(&d[2 + 4 * count..]),
            })
        }
        0x8c => Resource::Gpio(gpio(full)?),
        0x8e => Resource::SerialBus(serial_bus(full)?),
        _ => Resource::Unknown {
            tag,
            data: d.to_vec(),
        },
    })
}

fn address(width: AddressWidth, d: &[u8]) -> Result<Address, Error> {
    let (w, fixed) = match width {
        AddressWidth::Word => (2, 3 + 5 * 2),
        AddressWidth::DWord => (4, 3 + 5 * 4),
        AddressWidth::QWord => (8, 3 + 5 * 8),
        AddressWidth::Extended => (8, 53),
    };
    if d.len() < fixed {
        return Err(bad("address descriptor length"));
    }
    let field = |i: usize| -> Result<u64, Error> {
        let base = if width == AddressWidth::Extended {
            5
        } else {
            3
        };
        let o = base + i * w;
        Ok(match w {
            2 => u64::from(u16_at(d, o)?),
            4 => u64::from(u32_at(d, o)?),
            _ => u64_at(d, o)?,
        })
    };
    let (ty, gf, tf) = (d[0], d[1], d[2]);
    let kind = match ty {
        0 => AddressKind::Memory {
            writeable: tf & 1 != 0,
            caching: match (tf >> 1) & 3 {
                0 => Caching::NonCacheable,
                1 => Caching::Cacheable,
                2 => Caching::WriteCombining,
                _ => Caching::Prefetchable,
            },
            memory_type: (tf >> 3) & 3,
            translation: tf & 0x20 != 0,
        },
        1 => AddressKind::Io {
            range: tf & 3,
            translation: tf & 0x10 != 0,
            sparse: tf & 0x20 != 0,
        },
        2 => AddressKind::BusNumber,
        k => AddressKind::Other { kind: k, flags: tf },
    };
    let (attributes, source) = if width == AddressWidth::Extended {
        (Some(u64_at(d, 45)?), None)
    } else {
        (None, optional_source(&d[fixed..]))
    };
    Ok(Address {
        width,
        kind,
        consumer: gf & 1 != 0,
        subtractive: gf & 2 != 0,
        min_fixed: gf & 4 != 0,
        max_fixed: gf & 8 != 0,
        granularity: field(0)?,
        min: field(1)?,
        max: field(2)?,
        translation: field(3)?,
        length: field(4)?,
        attributes,
        source,
    })
}

/// A sub-range of a descriptor given by offsets from its first byte.
fn span(full: &[u8], start: usize, end: usize) -> Result<&[u8], Error> {
    if start > end || end > full.len() {
        return Err(bad("descriptor offset out of range"));
    }
    Ok(&full[start..end])
}

fn gpio(full: &[u8]) -> Result<Gpio, Error> {
    if full.len() < 23 {
        return Err(bad("GPIO length"));
    }
    let revision = full[3];
    let ty = full[4];
    let general = u16_at(full, 5)?;
    let flags = u16_at(full, 7)?;
    let pin_off = usize::from(u16_at(full, 14)?);
    let index = full[16];
    let name_off = usize::from(u16_at(full, 17)?);
    let vendor_off = usize::from(u16_at(full, 19)?);
    let vendor_len = usize::from(u16_at(full, 21)?);
    let pins_raw = span(full, pin_off, name_off)?;
    if pins_raw.len() % 2 != 0 {
        return Err(bad("GPIO pin table"));
    }
    let pins = pins_raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let name_end = if vendor_len > 0 {
        vendor_off
    } else {
        full.len()
    };
    let name = name_from(span(full, name_off, name_end)?);
    let vendor = if vendor_len > 0 {
        span(full, vendor_off, vendor_off + vendor_len)?.to_vec()
    } else {
        Vec::new()
    };
    let lo = flags.to_le_bytes()[0];
    let kind = match ty {
        0 => GpioKind::Interrupt {
            trigger: if lo & 1 != 0 {
                Trigger::Edge
            } else {
                Trigger::Level
            },
            polarity: match (lo >> 1) & 3 {
                0 => Polarity::ActiveHigh,
                1 => Polarity::ActiveLow,
                _ => Polarity::ActiveBoth,
            },
            shared: lo & 8 != 0,
            wake: lo & 0x10 != 0,
        },
        1 => GpioKind::Io {
            restriction: lo & 3,
            shared: lo & 8 != 0,
        },
        _ => return Err(bad("GPIO connection type")),
    };
    Ok(Gpio {
        revision,
        kind,
        consumer: general & 1 != 0,
        pin_config: full[9],
        drive_strength: u16_at(full, 10)?,
        debounce: u16_at(full, 12)?,
        pins,
        source: ResourceSource { index, name },
        vendor,
    })
}

fn serial_bus(full: &[u8]) -> Result<SerialBus, Error> {
    if full.len() < 12 {
        return Err(bad("serial bus length"));
    }
    let revision = full[3];
    let index = full[4];
    let ty = full[5];
    let general = full[6];
    let flags = u16_at(full, 7)?;
    let type_revision = full[9];
    let tdl = usize::from(u16_at(full, 10)?);
    let data = span(full, 12, 12 + tdl)?;
    let name = name_from(&full[12 + tdl..]);
    let (bus, fixed) = match ty {
        1 => {
            if data.len() < 6 {
                return Err(bad("I2C data length"));
            }
            (
                SerialBusKind::I2c {
                    ten_bit: flags & 1 != 0,
                    speed: u32_at(data, 0)?,
                    address: u16_at(data, 4)?,
                },
                6,
            )
        }
        2 => {
            if data.len() < 9 {
                return Err(bad("SPI data length"));
            }
            (
                SerialBusKind::Spi {
                    three_wire: flags & 1 != 0,
                    cs_active_high: flags & 2 != 0,
                    speed: u32_at(data, 0)?,
                    data_bits: data[4],
                    phase: data[5],
                    polarity: data[6],
                    device_select: u16_at(data, 7)?,
                },
                9,
            )
        }
        3 => {
            if data.len() < 10 {
                return Err(bad("UART data length"));
            }
            (
                SerialBusKind::Uart {
                    flags,
                    baud: u32_at(data, 0)?,
                    rx_fifo: u16_at(data, 4)?,
                    tx_fifo: u16_at(data, 6)?,
                    parity: data[8],
                    lines: data[9],
                },
                10,
            )
        }
        k => (
            SerialBusKind::Other {
                kind: k,
                flags,
                data: data.to_vec(),
            },
            data.len(),
        ),
    };
    Ok(SerialBus {
        revision,
        type_revision,
        slave_mode: general & 1 != 0,
        consumer: general & 2 != 0,
        shared: general & 4 != 0,
        bus,
        vendor: data[fixed..].to_vec(),
        source: ResourceSource { index, name },
    })
}
