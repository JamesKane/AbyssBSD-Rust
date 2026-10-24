// SPDX-License-Identifier: BSD-2-Clause
//! Mice (`sys/mouse.h`): the `MOUSE_*` requests and the structures they
//! carry, and the MouseSystems and sysmouse packet formats.

use crate::ioctl::{IOC_IN, IOC_OUT, IOCPARM_MASK};
use crate::layout::{Arches, ConstValue};

/// `mousestatus_t.button`: buttons 1 (left), 2 (middle), and 3 (right).
pub const MOUSE_BUTTON1DOWN: i32 = 0x0001;
pub const MOUSE_BUTTON2DOWN: i32 = 0x0002;
pub const MOUSE_BUTTON3DOWN: i32 = 0x0004;
pub const MOUSE_STDBUTTONS: i32 = 0x0007;
pub const MOUSE_EXTBUTTONS: i32 = 0x7fff_fff8;
/// `mousestatus_t.flags`: the pointer moved (the other bits are the
/// buttons that changed).
pub const MOUSE_POSCHANGED: i32 = i32::MIN;

/// `mousehw_t.iftype`, `.type`, and `.model`.
pub const MOUSE_IF_SYSMOUSE: i32 = 4;
pub const MOUSE_MOUSE: i32 = 0;
pub const MOUSE_MODEL_GENERIC: i32 = 0;

/// `mousemode_t.protocol`.
pub const MOUSE_PROTO_MSC: i32 = 1;
pub const MOUSE_PROTO_SYSMOUSE: i32 = 12;

/// The MouseSystems packet (level 0): five bytes, the buttons in the
/// first, active low.
pub const MOUSE_MSC_PACKETSIZE: usize = 5;
pub const MOUSE_MSC_SYNCMASK: u8 = 0xf8;
pub const MOUSE_MSC_SYNC: u8 = 0x80;
pub const MOUSE_MSC_BUTTON1UP: u8 = 0x04;
pub const MOUSE_MSC_BUTTON2UP: u8 = 0x02;
pub const MOUSE_MSC_BUTTON3UP: u8 = 0x01;

/// The sysmouse packet (level 1): the MouseSystems packet, then the
/// wheel and buttons 4 to 10.
pub const MOUSE_SYS_PACKETSIZE: usize = 8;
pub const MOUSE_SYS_SYNCMASK: u8 = 0xf8;
pub const MOUSE_SYS_SYNC: u8 = 0x80;

/// `mousestatus_t`: the buttons, those before the last
/// `MOUSE_GETSTATUS`, and the motion since.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseStatus {
    pub flags: i32,
    pub button: i32,
    pub obutton: i32,
    pub dx: i32,
    pub dy: i32,
    pub dz: i32,
}

impl MouseStatus {
    pub const SIZE: usize = 24;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        ints([
            self.flags,
            self.button,
            self.obutton,
            self.dx,
            self.dy,
            self.dz,
        ])
    }
}

/// `mousehw_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseHw {
    pub buttons: i32,
    pub iftype: i32,
    pub r#type: i32,
    pub model: i32,
    pub hwid: i32,
}

impl MouseHw {
    pub const SIZE: usize = 20;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        ints([
            self.buttons,
            self.iftype,
            self.r#type,
            self.model,
            self.hwid,
        ])
    }
}

/// `mousemode_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMode {
    pub protocol: i32,
    pub rate: i32,
    pub resolution: i32,
    pub accelfactor: i32,
    pub level: i32,
    pub packetsize: i32,
    pub syncmask: [u8; 2],
}

impl MouseMode {
    pub const SIZE: usize = 28;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        let head: [u8; 24] = ints([
            self.protocol,
            self.rate,
            self.resolution,
            self.accelfactor,
            self.level,
            self.packetsize,
        ]);
        b[..24].copy_from_slice(&head);
        b[24..26].copy_from_slice(&self.syncmask);
        b
    }

    /// From its bytes; `None` if `b` is short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let syncmask = b.get(24..26)?;
        Some(Self {
            protocol: i32_at(b, 0)?,
            rate: i32_at(b, 4)?,
            resolution: i32_at(b, 8)?,
            accelfactor: i32_at(b, 12)?,
            level: i32_at(b, 16)?,
            packetsize: i32_at(b, 20)?,
            syncmask: [syncmask[0], syncmask[1]],
        })
    }
}

/// `mousedata_t` (`MOUSE_READSTATE`, `MOUSE_READDATA`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseData {
    pub len: i32,
    pub buf: [i32; 16],
}

const fn ioc(dir: u32, group: u8, num: u8, len: u32) -> u32 {
    dir | ((len & IOCPARM_MASK) << 16) | (group as u32) << 8 | num as u32
}

pub const MOUSE_GETSTATUS: u32 = ioc(IOC_OUT, b'M', 0, 24);
pub const MOUSE_GETHWINFO: u32 = ioc(IOC_OUT, b'M', 1, 20);
pub const MOUSE_GETMODE: u32 = ioc(IOC_OUT, b'M', 2, 28);
pub const MOUSE_SETMODE: u32 = ioc(IOC_IN, b'M', 3, 28);
pub const MOUSE_GETLEVEL: u32 = ioc(IOC_OUT, b'M', 4, 4);
pub const MOUSE_SETLEVEL: u32 = ioc(IOC_IN, b'M', 5, 4);
pub const MOUSE_READSTATE: u32 = ioc(IOC_IN | IOC_OUT, b'M', 8, 68);
pub const MOUSE_READDATA: u32 = ioc(IOC_IN | IOC_OUT, b'M', 9, 68);

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
    n("MOUSE_BUTTON1DOWN", MOUSE_BUTTON1DOWN as i64),
    n("MOUSE_BUTTON2DOWN", MOUSE_BUTTON2DOWN as i64),
    n("MOUSE_BUTTON3DOWN", MOUSE_BUTTON3DOWN as i64),
    n("MOUSE_STDBUTTONS", MOUSE_STDBUTTONS as i64),
    n("MOUSE_EXTBUTTONS", MOUSE_EXTBUTTONS as i64),
    n("MOUSE_POSCHANGED", MOUSE_POSCHANGED.cast_unsigned() as i64),
    n("MOUSE_IF_SYSMOUSE", MOUSE_IF_SYSMOUSE as i64),
    n("MOUSE_MOUSE", MOUSE_MOUSE as i64),
    n("MOUSE_MODEL_GENERIC", MOUSE_MODEL_GENERIC as i64),
    n("MOUSE_PROTO_MSC", MOUSE_PROTO_MSC as i64),
    n("MOUSE_PROTO_SYSMOUSE", MOUSE_PROTO_SYSMOUSE as i64),
    n("MOUSE_MSC_PACKETSIZE", MOUSE_MSC_PACKETSIZE as i64),
    n("MOUSE_MSC_SYNCMASK", MOUSE_MSC_SYNCMASK as i64),
    n("MOUSE_MSC_SYNC", MOUSE_MSC_SYNC as i64),
    n("MOUSE_MSC_BUTTON1UP", MOUSE_MSC_BUTTON1UP as i64),
    n("MOUSE_MSC_BUTTON2UP", MOUSE_MSC_BUTTON2UP as i64),
    n("MOUSE_MSC_BUTTON3UP", MOUSE_MSC_BUTTON3UP as i64),
    n("MOUSE_SYS_PACKETSIZE", MOUSE_SYS_PACKETSIZE as i64),
    n("MOUSE_SYS_SYNCMASK", MOUSE_SYS_SYNCMASK as i64),
    n("MOUSE_SYS_SYNC", MOUSE_SYS_SYNC as i64),
    c("MOUSE_GETSTATUS", MOUSE_GETSTATUS),
    c("MOUSE_GETHWINFO", MOUSE_GETHWINFO),
    c("MOUSE_GETMODE", MOUSE_GETMODE),
    c("MOUSE_SETMODE", MOUSE_SETMODE),
    c("MOUSE_GETLEVEL", MOUSE_GETLEVEL),
    c("MOUSE_SETLEVEL", MOUSE_SETLEVEL),
    c("MOUSE_READSTATE", MOUSE_READSTATE),
    c("MOUSE_READDATA", MOUSE_READDATA),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/mouse.h",
        value: value as i64,
        arches: Arches::All,
    }
}

const fn n(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/mouse.h",
        value,
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ioctl::param_len;

    #[test]
    fn sizes_match_the_c_layouts() {
        assert_eq!(size_of::<MouseStatus>(), MouseStatus::SIZE);
        assert_eq!(size_of::<MouseHw>(), MouseHw::SIZE);
        assert_eq!(size_of::<MouseMode>(), MouseMode::SIZE);
        let len = |req| usize::try_from(param_len(req)).unwrap();
        assert_eq!(len(MOUSE_GETSTATUS), size_of::<MouseStatus>());
        assert_eq!(len(MOUSE_GETHWINFO), size_of::<MouseHw>());
        assert_eq!(len(MOUSE_SETMODE), size_of::<MouseMode>());
        assert_eq!(len(MOUSE_GETLEVEL), size_of::<i32>());
        assert_eq!(len(MOUSE_READDATA), size_of::<MouseData>());
    }

    #[test]
    fn mode_round_trips() {
        let m = MouseMode {
            protocol: MOUSE_PROTO_SYSMOUSE,
            rate: -1,
            resolution: -1,
            accelfactor: 0,
            level: 1,
            packetsize: 8,
            syncmask: [MOUSE_SYS_SYNCMASK, MOUSE_SYS_SYNC],
        };
        assert_eq!(MouseMode::from_bytes(&m.to_bytes()), Some(m));
    }
}
