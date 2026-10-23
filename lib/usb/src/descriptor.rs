// SPDX-License-Identifier: BSD-2-Clause
//! Standard descriptors (USB 2.0 §9.6, USB 3.2 §9.6).

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use crate::{le16, lo};

/// Descriptor types.
pub mod kind {
    /// Device.
    pub const DEVICE: u8 = 1;
    /// Configuration.
    pub const CONFIGURATION: u8 = 2;
    /// String.
    pub const STRING: u8 = 3;
    /// Interface.
    pub const INTERFACE: u8 = 4;
    /// Endpoint.
    pub const ENDPOINT: u8 = 5;
    /// Device qualifier (high-speed capable devices).
    pub const DEVICE_QUALIFIER: u8 = 6;
    /// Interface association.
    pub const INTERFACE_ASSOCIATION: u8 = 11;
    /// Binary object store.
    pub const BOS: u8 = 15;
    /// Device capability (inside the BOS).
    pub const DEVICE_CAPABILITY: u8 = 16;
    /// HID class descriptor.
    pub const HID: u8 = 0x21;
    /// HID report descriptor.
    pub const REPORT: u8 = 0x22;
    /// USB 2 hub.
    pub const HUB: u8 = 0x29;
    /// SuperSpeed hub.
    pub const SS_HUB: u8 = 0x2a;
    /// SuperSpeed endpoint companion.
    pub const SS_ENDPOINT_COMPANION: u8 = 0x30;
}

/// Device and interface classes this stack knows by name.
pub mod class {
    /// The class is per interface.
    pub const PER_INTERFACE: u8 = 0;
    /// Human interface device.
    pub const HID: u8 = 3;
    /// Mass storage.
    pub const MASS_STORAGE: u8 = 8;
    /// Hub.
    pub const HUB: u8 = 9;
    /// Vendor specific.
    pub const VENDOR: u8 = 0xff;
}

/// Why bytes were not a descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shorter than the descriptor needs, or than its length says.
    Short,
    /// The type byte is not the one expected.
    WrongType(u8),
    /// A length byte below 2, which would never advance.
    BadLength,
}

/// The header every descriptor starts with: its length and type, checked
/// against `b` and `want`.
fn header(b: &[u8], want: u8, min: usize) -> Result<(), Error> {
    let len = usize::from(*b.first().ok_or(Error::Short)?);
    let t = *b.get(1).ok_or(Error::Short)?;
    if t != want {
        return Err(Error::WrongType(t));
    }
    if len < min || b.len() < min {
        return Err(Error::Short);
    }
    Ok(())
}

/// The device descriptor (USB 2.0 §9.6.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceDescriptor {
    /// `bcdUSB`.
    pub usb: u16,
    /// `bDeviceClass`.
    pub class: u8,
    /// `bDeviceSubClass`.
    pub subclass: u8,
    /// `bDeviceProtocol`.
    pub protocol: u8,
    /// `bMaxPacketSize0`: bytes, or on USB 3 an exponent (9 for 512).
    pub max_packet0: u8,
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
    /// `bcdDevice`.
    pub release: u16,
    /// `iManufacturer`.
    pub manufacturer: u8,
    /// `iProduct`.
    pub product_string: u8,
    /// `iSerialNumber`.
    pub serial: u8,
    /// `bNumConfigurations`.
    pub configurations: u8,
}

impl DeviceDescriptor {
    /// Its size.
    pub const LEN: usize = 18;

    /// Parses the first eight bytes only, as read before the default
    /// control endpoint's max packet size is known: everything after
    /// `bMaxPacketSize0` is left 0.
    ///
    /// # Errors
    ///
    /// If `b` is not the start of a device descriptor.
    pub fn parse_prefix(b: &[u8]) -> Result<Self, Error> {
        header(b, kind::DEVICE, 8)?;
        Ok(Self {
            usb: le16(b, 2).ok_or(Error::Short)?,
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            max_packet0: b[7],
            ..Self::default()
        })
    }

    /// Parses a whole device descriptor.
    ///
    /// # Errors
    ///
    /// If `b` is not one.
    pub fn parse(b: &[u8]) -> Result<Self, Error> {
        header(b, kind::DEVICE, Self::LEN)?;
        let p = Self::parse_prefix(b)?;
        Ok(Self {
            vendor: le16(b, 8).ok_or(Error::Short)?,
            product: le16(b, 10).ok_or(Error::Short)?,
            release: le16(b, 12).ok_or(Error::Short)?,
            manufacturer: b[14],
            product_string: b[15],
            serial: b[16],
            configurations: b[17],
            ..p
        })
    }

    /// The default control endpoint's max packet size in bytes.
    #[must_use]
    pub fn ep0_max_packet(&self) -> u16 {
        if self.usb >= 0x0300 {
            // An exponent; 9 is the only valid value.
            1u16 << self.max_packet0.min(15)
        } else {
            u16::from(self.max_packet0)
        }
    }
}

/// An endpoint (USB 2.0 §9.6.6), with its SuperSpeed companion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// `bEndpointAddress`: number in bits 3:0, IN in bit 7.
    pub address: u8,
    /// `bmAttributes`: transfer type in bits 1:0.
    pub attributes: u8,
    /// `wMaxPacketSize`, whole.
    pub max_packet_raw: u16,
    /// `bInterval`.
    pub interval: u8,
    /// The SuperSpeed endpoint companion, on USB 3 devices.
    pub companion: Option<Companion>,
}

/// Transfer types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferType {
    /// Control.
    Control,
    /// Isochronous.
    Isochronous,
    /// Bulk.
    Bulk,
    /// Interrupt.
    Interrupt,
}

impl Endpoint {
    /// Whether data flows to the host.
    #[must_use]
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    /// The endpoint number.
    #[must_use]
    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }

    /// The transfer type.
    #[must_use]
    pub fn transfer_type(&self) -> TransferType {
        match self.attributes & 3 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }

    /// The max packet size in bytes (bits 10:0).
    #[must_use]
    pub fn max_packet(&self) -> u16 {
        self.max_packet_raw & 0x7ff
    }

    /// High-speed periodic endpoints: extra transactions per microframe
    /// (bits 12:11, 0 to 2).
    #[must_use]
    pub fn extra_transactions(&self) -> u8 {
        lo((self.max_packet_raw >> 11) & 3).min(2)
    }
}

/// A SuperSpeed endpoint companion (USB 3.2 §9.6.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Companion {
    /// `bMaxBurst`: packets per burst minus one.
    pub max_burst: u8,
    /// `bmAttributes`: for bulk, log2 of the streams (bits 4:0); for
    /// isochronous, `Mult` (bits 1:0).
    pub attributes: u8,
    /// `wBytesPerInterval`.
    pub bytes_per_interval: u16,
}

impl Companion {
    /// Bulk endpoints: the streams supported (0 for none).
    #[must_use]
    pub fn max_streams(&self) -> u32 {
        match self.attributes & 0x1f {
            0 => 0,
            n => 1 << n.min(16),
        }
    }
}

/// One alternate setting of an interface, with its endpoints and the
/// class-specific descriptors that followed it (a HID descriptor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    /// `bInterfaceNumber`.
    pub number: u8,
    /// `bAlternateSetting`.
    pub alternate: u8,
    /// `bInterfaceClass`.
    pub class: u8,
    /// `bInterfaceSubClass`.
    pub subclass: u8,
    /// `bInterfaceProtocol`.
    pub protocol: u8,
    /// `iInterface`.
    pub string: u8,
    /// Its endpoints.
    pub endpoints: Vec<Endpoint>,
    /// Other descriptors after it, as `(type, byte range in the
    /// configuration descriptor)`.
    pub extra: Vec<(u8, Range<usize>)>,
}

/// A configuration descriptor and everything under it (USB 2.0 §9.6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    /// `bConfigurationValue`.
    pub value: u8,
    /// `iConfiguration`.
    pub string: u8,
    /// `bmAttributes` (self-powered in bit 6, remote wakeup in bit 5).
    pub attributes: u8,
    /// `bMaxPower`, in 2 mA units (8 mA on SuperSpeed).
    pub max_power: u8,
    /// `bNumInterfaces`.
    pub interfaces_declared: u8,
    /// Every interface's alternate settings, in order.
    pub interfaces: Vec<Interface>,
}

/// The configuration descriptor's own size.
pub const CONFIGURATION_LEN: usize = 9;

/// `wTotalLength` from a configuration descriptor's first nine bytes:
/// how many bytes to read for all of it.
///
/// # Errors
///
/// If `b` is not the start of a configuration descriptor.
pub fn configuration_total(b: &[u8]) -> Result<u16, Error> {
    header(b, kind::CONFIGURATION, CONFIGURATION_LEN)?;
    le16(b, 2).ok_or(Error::Short)
}

impl Configuration {
    /// Parses a configuration descriptor and its interfaces, endpoints,
    /// and companions. Descriptors of unknown types are kept in the
    /// current interface's `extra`; endpoints before any interface, and
    /// a truncated last descriptor, are dropped.
    ///
    /// # Errors
    ///
    /// If `b` does not start with a configuration descriptor, or a
    /// descriptor's length byte is below 2.
    pub fn parse(b: &[u8]) -> Result<Self, Error> {
        header(b, kind::CONFIGURATION, CONFIGURATION_LEN)?;
        let total = usize::from(le16(b, 2).ok_or(Error::Short)?).min(b.len());
        let mut c = Self {
            value: b[5],
            string: b[6],
            attributes: b[7],
            max_power: b[8],
            interfaces_declared: b[4],
            interfaces: Vec::new(),
        };
        let mut at = usize::from(b[0]);
        while at + 2 <= total {
            let len = usize::from(b[at]);
            if len < 2 {
                return Err(Error::BadLength);
            }
            let end = at + len;
            if end > total {
                break;
            }
            let d = &b[at..end];
            match d[1] {
                kind::INTERFACE if len >= 9 => c.interfaces.push(Interface {
                    number: d[2],
                    alternate: d[3],
                    class: d[5],
                    subclass: d[6],
                    protocol: d[7],
                    string: d[8],
                    endpoints: Vec::new(),
                    extra: Vec::new(),
                }),
                kind::ENDPOINT if len >= 7 => {
                    if let Some(i) = c.interfaces.last_mut() {
                        i.endpoints.push(Endpoint {
                            address: d[2],
                            attributes: d[3],
                            max_packet_raw: le16(d, 4).unwrap_or(0),
                            interval: d[6],
                            companion: None,
                        });
                    }
                }
                kind::SS_ENDPOINT_COMPANION if len >= 6 => {
                    let e = c.interfaces.last_mut().and_then(|i| i.endpoints.last_mut());
                    if let Some(e) = e {
                        e.companion = Some(Companion {
                            max_burst: d[2].min(15),
                            attributes: d[3],
                            bytes_per_interval: le16(d, 4).unwrap_or(0),
                        });
                    }
                }
                t => {
                    if let Some(i) = c.interfaces.last_mut() {
                        i.extra.push((t, at..end));
                    }
                }
            }
            at = end;
        }
        Ok(c)
    }

    /// The interface `number`'s alternate setting `alternate`.
    #[must_use]
    pub fn interface(&self, number: u8, alternate: u8) -> Option<&Interface> {
        self.interfaces
            .iter()
            .find(|i| i.number == number && i.alternate == alternate)
    }

    /// Each interface's default (alternate 0) setting, by number.
    pub fn defaults(&self) -> impl Iterator<Item = &Interface> {
        self.interfaces.iter().filter(|i| i.alternate == 0)
    }
}

/// A string descriptor's text (UTF-16LE, USB 2.0 §9.6.7); unpaired
/// surrogates become U+FFFD.
///
/// # Errors
///
/// If `b` is not a string descriptor.
pub fn string(b: &[u8]) -> Result<String, Error> {
    header(b, kind::STRING, 2)?;
    let len = usize::from(b[0]).min(b.len());
    let units = b[2..len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c));
    Ok(char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect())
}

/// String descriptor 0's language IDs.
///
/// # Errors
///
/// If `b` is not a string descriptor.
pub fn languages(b: &[u8]) -> Result<Vec<u16>, Error> {
    header(b, kind::STRING, 2)?;
    let len = usize::from(b[0]).min(b.len());
    Ok(b[2..len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect())
}

/// US English, the language FreeBSD asks for first.
pub const LANG_EN_US: u16 = 0x0409;

/// A device capability in the BOS (USB 3.2 §9.6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// USB 2.0 extension (LPM support in bit 1 of the attributes).
    Usb2Extension {
        /// `bmAttributes`.
        attributes: u32,
    },
    /// SuperSpeed USB.
    SuperSpeed {
        /// `wSpeedsSupported`.
        speeds: u16,
        /// U1 exit latency (µs).
        u1_exit: u8,
        /// U2 exit latency (µs).
        u2_exit: u16,
    },
    /// SuperSpeedPlus.
    SuperSpeedPlus,
    /// Container ID.
    ContainerId,
    /// Anything else, by its type.
    Other(u8),
}

/// The BOS descriptor's `wTotalLength`, from its first five bytes.
///
/// # Errors
///
/// If `b` is not the start of a BOS descriptor.
pub fn bos_total(b: &[u8]) -> Result<u16, Error> {
    header(b, kind::BOS, 5)?;
    le16(b, 2).ok_or(Error::Short)
}

/// The device capabilities in a BOS descriptor.
///
/// # Errors
///
/// If `b` is not a BOS descriptor, or a length byte is below 2.
pub fn bos(b: &[u8]) -> Result<Vec<Capability>, Error> {
    header(b, kind::BOS, 5)?;
    let total = usize::from(le16(b, 2).ok_or(Error::Short)?).min(b.len());
    let mut caps = Vec::new();
    let mut at = usize::from(b[0]);
    while at + 3 <= total {
        let len = usize::from(b[at]);
        if len < 2 {
            return Err(Error::BadLength);
        }
        let end = at + len;
        if end > total {
            break;
        }
        let d = &b[at..end];
        if d[1] == kind::DEVICE_CAPABILITY && len >= 3 {
            caps.push(match d[2] {
                2 if len >= 7 => Capability::Usb2Extension {
                    attributes: u32::from_le_bytes([d[3], d[4], d[5], d[6]]),
                },
                3 if len >= 10 => Capability::SuperSpeed {
                    speeds: le16(d, 4).unwrap_or(0),
                    u1_exit: d[7],
                    u2_exit: le16(d, 8).unwrap_or(0),
                },
                4 => Capability::ContainerId,
                10 => Capability::SuperSpeedPlus,
                t => Capability::Other(t),
            });
        }
        at = end;
    }
    Ok(caps)
}
