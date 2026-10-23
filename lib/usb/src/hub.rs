// SPDX-License-Identifier: BSD-2-Clause
//! Hubs (USB 2.0 ch. 11, USB 3.2 ch. 10): the hub descriptors, port
//! status and change bits, and the class requests a hub driver sends.

use crate::descriptor::{Error, kind};
use crate::request::{Setup, rt};
use crate::{le16, lo};

/// Hub class requests.
pub mod code {
    /// `GET_STATUS`.
    pub const GET_STATUS: u8 = 0;
    /// `CLEAR_FEATURE`.
    pub const CLEAR_FEATURE: u8 = 1;
    /// `SET_FEATURE`.
    pub const SET_FEATURE: u8 = 3;
    /// `GET_DESCRIPTOR`.
    pub const GET_DESCRIPTOR: u8 = 6;
    /// `SET_HUB_DEPTH` (USB 3).
    pub const SET_HUB_DEPTH: u8 = 12;
}

/// Port feature selectors.
pub mod feature {
    /// Enable (clear to disable).
    pub const PORT_ENABLE: u16 = 1;
    /// Suspend.
    pub const PORT_SUSPEND: u16 = 2;
    /// Reset.
    pub const PORT_RESET: u16 = 4;
    /// USB 3: link state.
    pub const PORT_LINK_STATE: u16 = 5;
    /// Power.
    pub const PORT_POWER: u16 = 8;
    /// Connection change.
    pub const C_PORT_CONNECTION: u16 = 16;
    /// Enable change.
    pub const C_PORT_ENABLE: u16 = 17;
    /// Suspend change.
    pub const C_PORT_SUSPEND: u16 = 18;
    /// Over-current change.
    pub const C_PORT_OVER_CURRENT: u16 = 19;
    /// Reset change.
    pub const C_PORT_RESET: u16 = 20;
    /// USB 3: link state change.
    pub const C_PORT_LINK_STATE: u16 = 25;
    /// USB 3: config error change.
    pub const C_PORT_CONFIG_ERROR: u16 = 26;
    /// USB 3: warm reset.
    pub const BH_PORT_RESET: u16 = 28;
    /// USB 3: warm reset change.
    pub const C_BH_PORT_RESET: u16 = 29;
}

/// A hub's descriptor, either generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubDescriptor {
    /// Downstream ports.
    pub ports: u8,
    /// `wHubCharacteristics`.
    pub characteristics: u16,
    /// Time from power on to power good, in ms.
    pub power_on_ms: u16,
    /// USB 3: the hub's header decode latency, and its delay in ns.
    pub ss: Option<(u8, u16)>,
}

impl HubDescriptor {
    /// Parses a USB 2 (`0x29`) or SuperSpeed (`0x2a`) hub descriptor.
    ///
    /// # Errors
    ///
    /// If `b` is neither, or short.
    pub fn parse(b: &[u8]) -> Result<Self, Error> {
        let t = *b.get(1).ok_or(Error::Short)?;
        let min = if t == kind::SS_HUB { 12 } else { 7 };
        if t != kind::HUB && t != kind::SS_HUB {
            return Err(Error::WrongType(t));
        }
        if usize::from(b[0]) < min || b.len() < min {
            return Err(Error::Short);
        }
        Ok(Self {
            ports: b[2],
            characteristics: le16(b, 3).ok_or(Error::Short)?,
            power_on_ms: u16::from(b[5]) * 2,
            ss: (t == kind::SS_HUB).then(|| (b[7], le16(b, 8).unwrap_or(0))),
        })
    }

    /// USB 2 high-speed hubs: the TT think time, in the slot context's
    /// encoding (0 to 3: 8 to 32 FS bit times).
    #[must_use]
    pub fn tt_think_time(&self) -> u8 {
        lo((self.characteristics >> 5) & 3)
    }
}

/// `GET_DESCRIPTOR` for a hub's descriptor.
#[must_use]
pub fn get_hub_descriptor(super_speed: bool, length: u16) -> Setup {
    let k = if super_speed { kind::SS_HUB } else { kind::HUB };
    Setup {
        request_type: rt::IN | rt::CLASS | rt::DEVICE,
        request: code::GET_DESCRIPTOR,
        value: u16::from(k) << 8,
        index: 0,
        length,
    }
}

/// `GET_STATUS` of port `port` (from 1): four bytes, status then change.
#[must_use]
pub fn get_port_status(port: u8) -> Setup {
    Setup {
        request_type: rt::IN | rt::CLASS | rt::OTHER,
        request: code::GET_STATUS,
        value: 0,
        index: u16::from(port),
        length: 4,
    }
}

/// `SET_FEATURE(f)` on port `port`.
#[must_use]
pub fn set_port_feature(port: u8, f: u16) -> Setup {
    Setup {
        request_type: rt::CLASS | rt::OTHER,
        request: code::SET_FEATURE,
        value: f,
        index: u16::from(port),
        length: 0,
    }
}

/// `CLEAR_FEATURE(f)` on port `port`.
#[must_use]
pub fn clear_port_feature(port: u8, f: u16) -> Setup {
    Setup {
        request: code::CLEAR_FEATURE,
        ..set_port_feature(port, f)
    }
}

/// `SET_HUB_DEPTH`: a SuperSpeed hub's tier below the root hub's ports
/// (0 for a hub on a root port).
#[must_use]
pub fn set_hub_depth(depth: u8) -> Setup {
    Setup {
        request_type: rt::CLASS | rt::DEVICE,
        request: code::SET_HUB_DEPTH,
        value: u16::from(depth),
        index: 0,
        length: 0,
    }
}

/// A port's status and change words, decoded for either hub generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    /// `wPortStatus`.
    pub status: u16,
    /// `wPortChange`.
    pub change: u16,
    /// A SuperSpeed hub's port.
    pub super_speed: bool,
}

/// Port speeds a hub reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortSpeed {
    /// Low speed.
    Low,
    /// Full speed.
    Full,
    /// High speed.
    High,
    /// SuperSpeed (a USB 3 hub's ports are all SuperSpeed).
    Super,
}

impl PortStatus {
    /// From the four bytes of `GET_STATUS`.
    #[must_use]
    pub fn parse(b: [u8; 4], super_speed: bool) -> Self {
        Self {
            status: u16::from_le_bytes([b[0], b[1]]),
            change: u16::from_le_bytes([b[2], b[3]]),
            super_speed,
        }
    }

    /// A device is connected.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.status & 1 != 0
    }

    /// The port is enabled.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.status & (1 << 1) != 0
    }

    /// A reset is in progress.
    #[must_use]
    pub fn resetting(&self) -> bool {
        self.status & (1 << 4) != 0
    }

    /// The port is powered.
    #[must_use]
    pub fn powered(&self) -> bool {
        let bit = if self.super_speed { 9 } else { 8 };
        self.status & (1 << bit) != 0
    }

    /// The attached device's speed.
    #[must_use]
    pub fn speed(&self) -> PortSpeed {
        if self.super_speed {
            PortSpeed::Super
        } else if self.status & (1 << 9) != 0 {
            PortSpeed::Low
        } else if self.status & (1 << 10) != 0 {
            PortSpeed::High
        } else {
            PortSpeed::Full
        }
    }

    /// USB 3: the link state (bits 8:5).
    #[must_use]
    pub fn link_state(&self) -> u8 {
        lo((self.status >> 5) & 0xf)
    }

    /// The change bits, as the `C_PORT_*` features that clear them.
    pub fn changes(&self) -> impl Iterator<Item = u16> + '_ {
        let table: &[(u16, u16)] = if self.super_speed {
            &[
                (0, feature::C_PORT_CONNECTION),
                (3, feature::C_PORT_OVER_CURRENT),
                (4, feature::C_PORT_RESET),
                (5, feature::C_BH_PORT_RESET),
                (6, feature::C_PORT_LINK_STATE),
                (7, feature::C_PORT_CONFIG_ERROR),
            ]
        } else {
            &[
                (0, feature::C_PORT_CONNECTION),
                (1, feature::C_PORT_ENABLE),
                (2, feature::C_PORT_SUSPEND),
                (3, feature::C_PORT_OVER_CURRENT),
                (4, feature::C_PORT_RESET),
            ]
        };
        table
            .iter()
            .filter(|(bit, _)| self.change & (1 << bit) != 0)
            .map(|&(_, f)| f)
    }
}
