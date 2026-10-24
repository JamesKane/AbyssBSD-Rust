// SPDX-License-Identifier: BSD-2-Clause
//! The console's requests (`sys/consio.h`): the virtual terminals'
//! (`VT_*`), the console's (`CONS_*`), among them its mouse control
//! (`CONS_MOUSECTL`), which is how `moused` hands `/dev/sysmouse` what
//! it reads, through `/dev/consolectl`.

use crate::ioctl::{IOC_IN, IOC_OUT, IOC_VOID, IOCPARM_MASK};
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

/// `KDSETMODE`'s modes.
pub const KD_TEXT: i32 = 0;
pub const KD_GRAPHICS: i32 = 1;
pub const KD_TEXT1: i32 = 2;
pub const KD_PIXEL: i32 = 3;

/// `CONS_BELLTYPE`'s bits.
pub const CONS_VISUAL_BELL: i32 = 1 << 0;
pub const CONS_QUIET_BELL: i32 = 1 << 1;

/// `CONS_GET`'s answer under `vt`: 640×480 graphics (`sys/fbio.h`).
pub const M_CG640X480: i32 = 27;

/// `vt_mode.mode`.
pub const VT_AUTO: u8 = 0;
pub const VT_PROCESS: u8 = 1;
pub const VT_KERNEL: u8 = 255;

/// `VT_RELDISP`'s answers.
pub const VT_FALSE: i32 = 0;
pub const VT_TRUE: i32 = 1;
pub const VT_ACKACQ: i32 = 2;

pub const GIO_SCRNMAP: u32 = ioc(IOC_OUT, b'k', 2, 256);
pub const CONS_GET: u32 = ioc(IOC_OUT, b'c', 2, 4);
pub const CONS_BLANKTIME: u32 = ioc(IOC_IN, b'c', 4, 4);
pub const CONS_BELLTYPE: u32 = ioc(IOC_IN, b'c', 8, 4);
pub const CONS_HISTORY: u32 = ioc(IOC_IN, b'c', 9, 4);
pub const CONS_CLRHIST: u32 = ioc(IOC_VOID, b'c', 10, 0);
pub const CONS_MOUSECTL: u32 = ioc(IOC_IN | IOC_OUT, b'c', 10, 20);
pub const PIO_VFONT_DEFAULT: u32 = ioc(IOC_VOID, b'c', 72, 0);
pub const CONS_GETINFO: u32 = ioc(IOC_IN | IOC_OUT, b'c', 73, 24);
pub const CONS_GETVERS: u32 = ioc(IOC_OUT, b'c', 74, 4);
pub const CONS_SETKBD: u32 = ioc(IOC_VOID, b'c', 110, 4);
pub const CONS_RELKBD: u32 = ioc(IOC_VOID, b'c', 111, 0);
pub const VT_OPENQRY: u32 = ioc(IOC_OUT, b'v', 1, 4);
pub const VT_SETMODE: u32 = ioc(IOC_IN, b'v', 2, 8);
pub const VT_GETMODE: u32 = ioc(IOC_OUT, b'v', 3, 8);
pub const VT_RELDISP: u32 = ioc(IOC_VOID, b'v', 4, 4);
pub const VT_ACTIVATE: u32 = ioc(IOC_VOID, b'v', 5, 4);
pub const VT_WAITACTIVE: u32 = ioc(IOC_VOID, b'v', 6, 4);
pub const VT_GETACTIVE: u32 = ioc(IOC_OUT, b'v', 7, 4);
pub const VT_GETINDEX: u32 = ioc(IOC_OUT, b'v', 8, 4);
pub const VT_LOCKSWITCH: u32 = ioc(IOC_IN, b'v', 9, 4);

/// `vid_info_t` (`CONS_GETINFO`): the caller sets `size`; these are the
/// offsets of the fields `vt` fills.
pub mod vid_info {
    pub const SIZE: usize = 24;
    pub const M_NUM: usize = 2;
    pub const MV_ROW: usize = 6;
    pub const MV_COL: usize = 8;
    pub const MV_RSZ: usize = 10;
    pub const MV_CSZ: usize = 12;
    pub const MV_NORM: usize = 16;
    pub const MV_REV: usize = 18;
    pub const MK_KEYLOCK: usize = 23;
}

/// `struct vt_mode` (`VT_SETMODE`, `VT_GETMODE`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VtMode {
    pub mode: u8,
    pub waitv: u8,
    pub relsig: i16,
    pub acqsig: i16,
    pub frsig: i16,
}

impl VtMode {
    pub const SIZE: usize = 8;

    /// From its bytes; `None` if `b` is short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let h = |o: usize| Some(i16::from_ne_bytes(b.get(o..o + 2)?.try_into().ok()?));
        Some(Self {
            mode: *b.first()?,
            waitv: *b.get(1)?,
            relsig: h(2)?,
            acqsig: h(4)?,
            frsig: h(6)?,
        })
    }

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0] = self.mode;
        b[1] = self.waitv;
        b[2..4].copy_from_slice(&self.relsig.to_ne_bytes());
        b[4..6].copy_from_slice(&self.acqsig.to_ne_bytes());
        b[6..8].copy_from_slice(&self.frsig.to_ne_bytes());
        b
    }
}

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
    c("GIO_SCRNMAP", GIO_SCRNMAP),
    c("CONS_GET", CONS_GET),
    c("CONS_BLANKTIME", CONS_BLANKTIME),
    c("CONS_BELLTYPE", CONS_BELLTYPE),
    c("CONS_HISTORY", CONS_HISTORY),
    c("CONS_CLRHIST", CONS_CLRHIST),
    c("CONS_MOUSECTL", CONS_MOUSECTL),
    c("PIO_VFONT_DEFAULT", PIO_VFONT_DEFAULT),
    c("CONS_GETINFO", CONS_GETINFO),
    c("CONS_GETVERS", CONS_GETVERS),
    c("CONS_SETKBD", CONS_SETKBD),
    c("CONS_RELKBD", CONS_RELKBD),
    c("VT_OPENQRY", VT_OPENQRY),
    c("VT_SETMODE", VT_SETMODE),
    c("VT_GETMODE", VT_GETMODE),
    c("VT_RELDISP", VT_RELDISP),
    c("VT_ACTIVATE", VT_ACTIVATE),
    c("VT_WAITACTIVE", VT_WAITACTIVE),
    c("VT_GETACTIVE", VT_GETACTIVE),
    c("VT_GETINDEX", VT_GETINDEX),
    c("VT_LOCKSWITCH", VT_LOCKSWITCH),
    n("KD_TEXT", KD_TEXT),
    n("KD_GRAPHICS", KD_GRAPHICS),
    n("KD_TEXT1", KD_TEXT1),
    n("KD_PIXEL", KD_PIXEL),
    n("CONS_VISUAL_BELL", CONS_VISUAL_BELL),
    n("CONS_QUIET_BELL", CONS_QUIET_BELL),
    ConstValue {
        c_name: "M_CG640x480",
        header: "sys/fbio.h",
        value: M_CG640X480 as i64,
        arches: Arches::All,
    },
    n("VT_AUTO", VT_AUTO as i32),
    n("VT_PROCESS", VT_PROCESS as i32),
    n("VT_KERNEL", VT_KERNEL as i32),
    n("VT_FALSE", VT_FALSE),
    n("VT_TRUE", VT_TRUE),
    n("VT_ACKACQ", VT_ACKACQ),
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
