// SPDX-License-Identifier: BSD-2-Clause
//! GPIO pins (`sys/gpio.h`): the flags a pin's configuration is made of,
//! and the requests `gpioctl(8)` and `libgpio` send to `/dev/gpiocN`.
//!
//! Behavior reference: sys/sys/gpio.h, sys/dev/gpio/gpiobus.c
//! (`gpio_check_flags`).

use crate::ioctl::{IOC_IN, IOC_OUT};
use crate::layout::{Arches, ConstValue};

/// `GPIOMAXNAME`: a pin name's buffer, its NUL included.
pub const GPIOMAXNAME: usize = 64;
const GPIOMAXNAME_U32: u32 = 64;
const _: () = assert!(GPIOMAXNAME_U32 as usize == GPIOMAXNAME);

/// `GPIO_PIN_*`: a pin's configuration and capabilities.
pub mod pin {
    pub const LOW: u32 = 0x00;
    pub const HIGH: u32 = 0x01;
    pub const INPUT: u32 = 0x0000_0001;
    pub const OUTPUT: u32 = 0x0000_0002;
    pub const OPENDRAIN: u32 = 0x0000_0004;
    pub const PUSHPULL: u32 = 0x0000_0008;
    pub const TRISTATE: u32 = 0x0000_0010;
    pub const PULLUP: u32 = 0x0000_0020;
    pub const PULLDOWN: u32 = 0x0000_0040;
    pub const INVIN: u32 = 0x0000_0080;
    pub const INVOUT: u32 = 0x0000_0100;
    pub const PULSATE: u32 = 0x0000_0200;
    pub const PRESET_LOW: u32 = 0x0000_0400;
    pub const PRESET_HIGH: u32 = 0x0000_0800;
}

/// `GPIO_INTR_*`: a pin's interrupt configuration, in the same word.
pub mod intr {
    pub const NONE: u32 = 0x0000_0000;
    pub const LEVEL_LOW: u32 = 0x0001_0000;
    pub const LEVEL_HIGH: u32 = 0x0002_0000;
    pub const EDGE_RISING: u32 = 0x0004_0000;
    pub const EDGE_FALLING: u32 = 0x0008_0000;
    pub const EDGE_BOTH: u32 = 0x0010_0000;
    pub const ATTACHED: u32 = 0x0020_0000;
    pub const EDGE_MASK: u32 = EDGE_RISING | EDGE_FALLING | EDGE_BOTH;
    pub const MASK: u32 = LEVEL_LOW | LEVEL_HIGH | EDGE_MASK | ATTACHED;
}

/// `struct gpio_pin`: `GPIOGETCONFIG`, `GPIOSETCONFIG`, `GPIOSETNAME`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpioPin {
    pub gp_pin: u32,
    pub gp_name: [u8; GPIOMAXNAME],
    pub gp_caps: u32,
    pub gp_flags: u32,
}

/// `struct gpio_req`: `GPIOGET`, `GPIOSET`, `GPIOTOGGLE`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpioReq {
    pub gp_pin: u32,
    pub gp_value: u32,
}

/// `struct gpio_access_32`: `GPIOACCESS32`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpioAccess32 {
    pub first_pin: u32,
    pub clear_pins: u32,
    pub change_pins: u32,
    pub orig_pins: u32,
}

/// `struct gpio_config_32`: `GPIOCONFIG32`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpioConfig32 {
    pub first_pin: u32,
    pub num_pins: u32,
    pub pin_flags: [u32; 32],
}

/// `struct gpio_event_config`: `GPIOCONFIGEVENTS`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpioEventConfig {
    pub gp_report_type: u32,
    pub gp_fifo_size: u32,
}

pub const GPIO_PIN_LEN: usize = 76;
pub const GPIO_REQ_LEN: usize = 8;
pub const GPIO_ACCESS_32_LEN: usize = 16;
pub const GPIO_CONFIG_32_LEN: usize = 136;
pub const GPIO_EVENT_CONFIG_LEN: usize = 8;
const _: () = assert!(size_of::<GpioPin>() == GPIO_PIN_LEN);
const _: () = assert!(size_of::<GpioReq>() == GPIO_REQ_LEN);
const _: () = assert!(size_of::<GpioAccess32>() == GPIO_ACCESS_32_LEN);
const _: () = assert!(size_of::<GpioConfig32>() == GPIO_CONFIG_32_LEN);
const _: () = assert!(size_of::<GpioEventConfig>() == GPIO_EVENT_CONFIG_LEN);

// Every length is a few dozen bytes, well within `IOCPARM_MASK`.
#[allow(clippy::cast_possible_truncation)]
const fn ioc(dir: u32, num: u32, len: usize) -> u32 {
    dir | ((len as u32) << 16) | (b'G' as u32) << 8 | num
}

pub const GPIOMAXPIN: u32 = ioc(IOC_OUT, 0, 4);
pub const GPIOGETCONFIG: u32 = ioc(IOC_IN | IOC_OUT, 1, GPIO_PIN_LEN);
pub const GPIOSETCONFIG: u32 = ioc(IOC_IN, 2, GPIO_PIN_LEN);
pub const GPIOGET: u32 = ioc(IOC_IN | IOC_OUT, 3, GPIO_REQ_LEN);
pub const GPIOSET: u32 = ioc(IOC_IN, 4, GPIO_REQ_LEN);
pub const GPIOTOGGLE: u32 = ioc(IOC_IN | IOC_OUT, 5, GPIO_REQ_LEN);
pub const GPIOSETNAME: u32 = ioc(IOC_IN, 6, GPIO_PIN_LEN);
pub const GPIOACCESS32: u32 = ioc(IOC_IN | IOC_OUT, 7, GPIO_ACCESS_32_LEN);
pub const GPIOCONFIG32: u32 = ioc(IOC_IN, 8, GPIO_CONFIG_32_LEN);
pub const GPIOCONFIGEVENTS: u32 = ioc(IOC_IN, 9, GPIO_EVENT_CONFIG_LEN);

/// Reads a `u32` at byte `at` of an ioctl argument, if it is there.
fn word(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

impl GpioPin {
    /// The structure from its bytes, if there are enough.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let mut gp_name = [0; GPIOMAXNAME];
        gp_name.copy_from_slice(b.get(4..4 + GPIOMAXNAME)?);
        Some(Self {
            gp_pin: word(b, 0)?,
            gp_name,
            gp_caps: word(b, 4 + GPIOMAXNAME)?,
            gp_flags: word(b, 8 + GPIOMAXNAME)?,
        })
    }

    /// Writes the structure over the first [`GPIO_PIN_LEN`] bytes of `b`.
    pub fn store(&self, b: &mut [u8]) {
        if b.len() < GPIO_PIN_LEN {
            return;
        }
        b[0..4].copy_from_slice(&self.gp_pin.to_ne_bytes());
        b[4..4 + GPIOMAXNAME].copy_from_slice(&self.gp_name);
        b[4 + GPIOMAXNAME..8 + GPIOMAXNAME].copy_from_slice(&self.gp_caps.to_ne_bytes());
        b[8 + GPIOMAXNAME..12 + GPIOMAXNAME].copy_from_slice(&self.gp_flags.to_ne_bytes());
    }

    /// The name, up to its NUL.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        let n = self
            .gp_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(GPIOMAXNAME);
        &self.gp_name[..n]
    }
}

impl GpioReq {
    /// The structure from its bytes, if there are enough.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        Some(Self {
            gp_pin: word(b, 0)?,
            gp_value: word(b, 4)?,
        })
    }

    /// Writes the structure over the first [`GPIO_REQ_LEN`] bytes of `b`.
    pub fn store(&self, b: &mut [u8]) {
        if b.len() < GPIO_REQ_LEN {
            return;
        }
        b[0..4].copy_from_slice(&self.gp_pin.to_ne_bytes());
        b[4..8].copy_from_slice(&self.gp_value.to_ne_bytes());
    }
}

/// `gpio_check_flags`: whether `flags`, less what `caps` lacks, is a
/// configuration a pin can have.
#[must_use]
pub fn flags_valid(caps: u32, flags: u32) -> bool {
    let f = flags & caps;
    let intr_bits = f & intr::MASK;
    !(f & pin::INPUT != 0 && f & pin::OUTPUT != 0
        || f & pin::PULLUP != 0 && f & pin::PULLDOWN != 0
        || f & pin::OUTPUT != 0 && intr_bits != 0
        || intr_bits & intr_bits.wrapping_sub(1) != 0
        || f & intr::ATTACHED != 0)
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("GPIOMAXNAME", GPIOMAXNAME_U32),
    c("GPIO_PIN_INPUT", pin::INPUT),
    c("GPIO_PIN_OUTPUT", pin::OUTPUT),
    c("GPIO_PIN_OPENDRAIN", pin::OPENDRAIN),
    c("GPIO_PIN_PUSHPULL", pin::PUSHPULL),
    c("GPIO_PIN_TRISTATE", pin::TRISTATE),
    c("GPIO_PIN_PULLUP", pin::PULLUP),
    c("GPIO_PIN_PULLDOWN", pin::PULLDOWN),
    c("GPIO_PIN_INVIN", pin::INVIN),
    c("GPIO_PIN_INVOUT", pin::INVOUT),
    c("GPIO_PIN_PULSATE", pin::PULSATE),
    c("GPIO_PIN_PRESET_LOW", pin::PRESET_LOW),
    c("GPIO_PIN_PRESET_HIGH", pin::PRESET_HIGH),
    c("GPIO_INTR_LEVEL_LOW", intr::LEVEL_LOW),
    c("GPIO_INTR_LEVEL_HIGH", intr::LEVEL_HIGH),
    c("GPIO_INTR_EDGE_RISING", intr::EDGE_RISING),
    c("GPIO_INTR_EDGE_FALLING", intr::EDGE_FALLING),
    c("GPIO_INTR_EDGE_BOTH", intr::EDGE_BOTH),
    c("GPIO_INTR_ATTACHED", intr::ATTACHED),
    c("GPIOMAXPIN", GPIOMAXPIN),
    c("GPIOGETCONFIG", GPIOGETCONFIG),
    c("GPIOSETCONFIG", GPIOSETCONFIG),
    c("GPIOGET", GPIOGET),
    c("GPIOSET", GPIOSET),
    c("GPIOTOGGLE", GPIOTOGGLE),
    c("GPIOSETNAME", GPIOSETNAME),
    c("GPIOACCESS32", GPIOACCESS32),
    c("GPIOCONFIG32", GPIOCONFIG32),
    c("GPIOCONFIGEVENTS", GPIOCONFIGEVENTS),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/gpio.h",
        value: value as i64,
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_freebsds() {
        // _IOWR('G', 1, struct gpio_pin) and _IOR('G', 0, int).
        assert_eq!(GPIOGETCONFIG, 0xc04c_4701);
        assert_eq!(GPIOMAXPIN, 0x4004_4700);
        assert_eq!(GPIOSET, 0x8008_4704);
    }

    #[test]
    fn a_pin_round_trips() {
        let mut p = GpioPin {
            gp_pin: 7,
            gp_name: [0; GPIOMAXNAME],
            gp_caps: pin::INPUT | pin::OUTPUT,
            gp_flags: pin::OUTPUT,
        };
        p.gp_name[..7].copy_from_slice(b"gpio0.7");
        let mut b = [0u8; GPIO_PIN_LEN];
        p.store(&mut b);
        let q = GpioPin::from_bytes(&b).unwrap();
        assert_eq!(q, p);
        assert_eq!(q.name(), b"gpio0.7");
    }

    #[test]
    fn flags_are_checked_as_gpio_check_flags() {
        let caps = pin::INPUT | pin::OUTPUT | pin::PULLUP | pin::PULLDOWN | intr::EDGE_MASK;
        assert!(flags_valid(caps, pin::INPUT | pin::PULLUP));
        assert!(!flags_valid(caps, pin::INPUT | pin::OUTPUT));
        assert!(!flags_valid(caps, pin::PULLUP | pin::PULLDOWN));
        assert!(!flags_valid(caps, pin::OUTPUT | intr::EDGE_RISING));
        assert!(!flags_valid(caps, intr::EDGE_RISING | intr::EDGE_FALLING));
        // What the pin cannot do is dropped first, as FreeBSD filters it.
        assert!(flags_valid(pin::INPUT, pin::INPUT | pin::OUTPUT));
    }
}
