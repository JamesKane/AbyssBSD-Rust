// SPDX-License-Identifier: BSD-2-Clause
//! Control requests (USB 2.0 §9.3, §9.4).

/// A setup packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    /// `bmRequestType`: direction (bit 7), type (6:5), recipient (4:0).
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// `wLength`.
    pub length: u16,
}

/// `bmRequestType` parts.
pub mod rt {
    /// Device to host.
    pub const IN: u8 = 0x80;
    /// Standard request.
    pub const STANDARD: u8 = 0x00;
    /// Class request.
    pub const CLASS: u8 = 0x20;
    /// Vendor request.
    pub const VENDOR: u8 = 0x40;
    /// To the device.
    pub const DEVICE: u8 = 0x00;
    /// To an interface.
    pub const INTERFACE: u8 = 0x01;
    /// To an endpoint.
    pub const ENDPOINT: u8 = 0x02;
    /// To something else (a hub's port).
    pub const OTHER: u8 = 0x03;
}

/// Standard request codes.
pub mod code {
    /// `GET_STATUS`.
    pub const GET_STATUS: u8 = 0;
    /// `CLEAR_FEATURE`.
    pub const CLEAR_FEATURE: u8 = 1;
    /// `SET_FEATURE`.
    pub const SET_FEATURE: u8 = 3;
    /// `SET_ADDRESS`.
    pub const SET_ADDRESS: u8 = 5;
    /// `GET_DESCRIPTOR`.
    pub const GET_DESCRIPTOR: u8 = 6;
    /// `GET_CONFIGURATION`.
    pub const GET_CONFIGURATION: u8 = 8;
    /// `SET_CONFIGURATION`.
    pub const SET_CONFIGURATION: u8 = 9;
    /// `GET_INTERFACE`.
    pub const GET_INTERFACE: u8 = 10;
    /// `SET_INTERFACE`.
    pub const SET_INTERFACE: u8 = 11;
    /// `SET_SEL` (USB 3).
    pub const SET_SEL: u8 = 48;
}

/// The `ENDPOINT_HALT` feature selector.
pub const ENDPOINT_HALT: u16 = 0;

impl Setup {
    /// Whether the data stage is device to host.
    #[must_use]
    pub fn is_in(&self) -> bool {
        self.request_type & rt::IN != 0
    }

    /// `GET_DESCRIPTOR` for descriptor `kind` index `index` (and language
    /// `lang` for strings), reading `length` bytes.
    #[must_use]
    pub fn get_descriptor(kind: u8, index: u8, lang: u16, length: u16) -> Self {
        Self {
            request_type: rt::IN | rt::STANDARD | rt::DEVICE,
            request: code::GET_DESCRIPTOR,
            value: (u16::from(kind) << 8) | u16::from(index),
            index: lang,
            length,
        }
    }

    /// `GET_DESCRIPTOR` for a class descriptor of an interface (a HID
    /// report descriptor).
    #[must_use]
    pub fn get_interface_descriptor(kind: u8, index: u8, interface: u8, length: u16) -> Self {
        Self {
            request_type: rt::IN | rt::STANDARD | rt::INTERFACE,
            index: u16::from(interface),
            ..Self::get_descriptor(kind, index, 0, length)
        }
    }

    /// `SET_CONFIGURATION`.
    #[must_use]
    pub fn set_configuration(value: u8) -> Self {
        Self {
            request_type: rt::STANDARD | rt::DEVICE,
            request: code::SET_CONFIGURATION,
            value: u16::from(value),
            index: 0,
            length: 0,
        }
    }

    /// `SET_INTERFACE`.
    #[must_use]
    pub fn set_interface(interface: u8, alternate: u8) -> Self {
        Self {
            request_type: rt::STANDARD | rt::INTERFACE,
            request: code::SET_INTERFACE,
            value: u16::from(alternate),
            index: u16::from(interface),
            length: 0,
        }
    }

    /// `CLEAR_FEATURE(ENDPOINT_HALT)` on endpoint `address`.
    #[must_use]
    pub fn clear_halt(address: u8) -> Self {
        Self {
            request_type: rt::STANDARD | rt::ENDPOINT,
            request: code::CLEAR_FEATURE,
            value: ENDPOINT_HALT,
            index: u16::from(address),
            length: 0,
        }
    }

    /// `GET_STATUS` of the device.
    #[must_use]
    pub fn get_status() -> Self {
        Self {
            request_type: rt::IN | rt::STANDARD | rt::DEVICE,
            request: code::GET_STATUS,
            value: 0,
            index: 0,
            length: 2,
        }
    }
}
