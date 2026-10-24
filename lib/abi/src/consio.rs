// SPDX-License-Identifier: BSD-2-Clause
//! The console's requests (`sys/consio.h`) that are here so far: its
//! version, and its mouse control (`CONS_MOUSECTL`), which is how
//! `moused` hands `/dev/sysmouse` what it reads, through
//! `/dev/consolectl`.

use crate::ioctl::{IOC_IN, IOC_OUT, IOCPARM_MASK};
use crate::layout::{Arches, ConstValue};

/// `mouse_info.operation`.
pub const MOUSE_SHOW: i32 = 0x01;
pub const MOUSE_HIDE: i32 = 0x02;
pub const MOUSE_MOVEABS: i32 = 0x03;
pub const MOUSE_MOVEREL: i32 = 0x04;
pub const MOUSE_GETINFO: i32 = 0x05;
pub const MOUSE_MODE: i32 = 0x06;
pub const MOUSE_ACTION: i32 = 0x07;
pub const MOUSE_MOTION_EVENT: i32 = 0x08;
pub const MOUSE_BUTTON_EVENT: i32 = 0x09;
pub const MOUSE_MOUSECHAR: i32 = 0x0a;

/// `mouse_info_t` (`CONS_MOUSECTL`): an operation and its argument, the
/// union of `mouse_data_t` (x, y, z, buttons), `mouse_mode_t`,
/// `mouse_event_t` (id, value), and `mouse_char`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseInfo {
    pub operation: i32,
    pub u: [i32; 4],
}

impl MouseInfo {
    pub const SIZE: usize = 20;

    /// From its bytes; `None` if `b` is short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        Some(Self {
            operation: i32_at(b, 0)?,
            u: [i32_at(b, 4)?, i32_at(b, 8)?, i32_at(b, 12)?, i32_at(b, 16)?],
        })
    }

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        ints([self.operation, self.u[0], self.u[1], self.u[2], self.u[3]])
    }

    /// `u.data`: motion and buttons.
    #[must_use]
    pub const fn data(&self) -> MouseMotion {
        MouseMotion {
            x: self.u[0],
            y: self.u[1],
            z: self.u[2],
            buttons: self.u[3],
        }
    }

    /// `u.event`: a button (`MOUSE_BUTTON?DOWN`) and its state.
    #[must_use]
    pub const fn event(&self) -> (i32, i32) {
        (self.u[0], self.u[1])
    }
}

/// `mouse_data_t`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMotion {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub buttons: i32,
}

const fn ioc(dir: u32, group: u8, num: u8, len: u32) -> u32 {
    dir | ((len & IOCPARM_MASK) << 16) | (group as u32) << 8 | num as u32
}

pub const CONS_MOUSECTL: u32 = ioc(IOC_IN | IOC_OUT, b'c', 10, 20);
pub const CONS_GETVERS: u32 = ioc(IOC_OUT, b'c', 74, 4);

fn ints<const N: usize, const B: usize>(v: [i32; N]) -> [u8; B] {
    let mut b = [0u8; B];
    for (chunk, x) in b.as_chunks_mut::<4>().0.iter_mut().zip(v) {
        *chunk = x.to_ne_bytes();
    }
    b
}

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("CONS_MOUSECTL", CONS_MOUSECTL),
    c("CONS_GETVERS", CONS_GETVERS),
    n("MOUSE_SHOW", MOUSE_SHOW),
    n("MOUSE_HIDE", MOUSE_HIDE),
    n("MOUSE_MOVEABS", MOUSE_MOVEABS),
    n("MOUSE_MOVEREL", MOUSE_MOVEREL),
    n("MOUSE_GETINFO", MOUSE_GETINFO),
    n("MOUSE_MODE", MOUSE_MODE),
    n("MOUSE_ACTION", MOUSE_ACTION),
    n("MOUSE_MOTION_EVENT", MOUSE_MOTION_EVENT),
    n("MOUSE_BUTTON_EVENT", MOUSE_BUTTON_EVENT),
    n("MOUSE_MOUSECHAR", MOUSE_MOUSECHAR),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/consio.h",
        value: value as i64,
        arches: Arches::All,
    }
}

const fn n(c_name: &'static str, value: i32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/consio.h",
        value: value as i64,
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ioctl::param_len;

    #[test]
    fn sizes_match_the_c_layouts() {
        assert_eq!(size_of::<MouseInfo>(), MouseInfo::SIZE);
        assert_eq!(
            usize::try_from(param_len(CONS_MOUSECTL)).unwrap(),
            MouseInfo::SIZE
        );
        let mi = MouseInfo {
            operation: MOUSE_ACTION,
            u: [1, -2, 3, 4],
        };
        assert_eq!(MouseInfo::from_bytes(&mi.to_bytes()), Some(mi));
    }
}
