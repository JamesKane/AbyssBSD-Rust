// SPDX-License-Identifier: BSD-2-Clause
//! evdev (`dev/evdev/input.h`, `dev/evdev/input-event-codes.h`): the
//! events `/dev/input/event*` reads return, the device's identity and
//! axis ranges, the `EVIOC*` requests, and the event types and codes
//! this kernel's drivers report.

use crate::ioctl::{IOC_IN, IOC_OUT, IOC_VOID, IOCPARM_MASK};
use crate::layout::{Arches, ConstValue};
use crate::time::Timeval;

/// `struct input_event`: one event, stamped when its report completed.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEvent {
    pub time: Timeval,
    /// `type` in C.
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    /// Its bytes, as `read` returns them.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[..8].copy_from_slice(&self.time.tv_sec.to_ne_bytes());
        b[8..16].copy_from_slice(&self.time.tv_usec.to_ne_bytes());
        b[16..18].copy_from_slice(&self.kind.to_ne_bytes());
        b[18..20].copy_from_slice(&self.code.to_ne_bytes());
        b[20..24].copy_from_slice(&self.value.to_ne_bytes());
        b
    }

    /// The type, code, and value of an event as `write` passes it.
    #[must_use]
    pub fn parse(b: &[u8; 24]) -> (u16, u16, i32) {
        (
            u16::from_ne_bytes([b[16], b[17]]),
            u16::from_ne_bytes([b[18], b[19]]),
            i32::from_ne_bytes([b[20], b[21], b[22], b[23]]),
        )
    }
}

/// `struct input_id`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

impl InputId {
    /// Its bytes, as `EVIOCGID` and `kern.evdev.input.N.id` return them.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..2].copy_from_slice(&self.bustype.to_ne_bytes());
        b[2..4].copy_from_slice(&self.vendor.to_ne_bytes());
        b[4..6].copy_from_slice(&self.product.to_ne_bytes());
        b[6..].copy_from_slice(&self.version.to_ne_bytes());
        b
    }
}

/// `struct input_absinfo`: an absolute axis's value and range.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputAbsinfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

impl InputAbsinfo {
    /// Its bytes, as `EVIOCGABS` returns them.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        let v = [
            self.value,
            self.minimum,
            self.maximum,
            self.fuzz,
            self.flat,
            self.resolution,
        ];
        for (chunk, x) in b.as_chunks_mut::<4>().0.iter_mut().zip(v) {
            *chunk = x.to_ne_bytes();
        }
        b
    }

    /// From `EVIOCSABS`'s argument.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let w = |i: usize| -> Option<i32> {
            Some(i32::from_ne_bytes(
                b.get(4 * i..4 * i + 4)?.try_into().ok()?,
            ))
        };
        Some(Self {
            value: w(0)?,
            minimum: w(1)?,
            maximum: w(2)?,
            fuzz: w(3)?,
            flat: w(4)?,
            resolution: w(5)?,
        })
    }
}

/// The version `EVIOCGVERSION` reports.
pub const EV_VERSION: i32 = 0x01_0001;

const fn ioc(dir: u32, num: u32, len: u32) -> u32 {
    dir | ((len & IOCPARM_MASK) << 16) | (b'E' as u32) << 8 | num
}

pub const EVIOCGVERSION: u32 = ioc(IOC_OUT, 0x01, 4);
pub const EVIOCGID: u32 = ioc(IOC_OUT, 0x02, 8);
pub const EVIOCGREP: u32 = ioc(IOC_OUT, 0x03, 8);
pub const EVIOCSREP: u32 = ioc(IOC_IN, 0x03, 8);
pub const EVIOCGKEYCODE: u32 = ioc(IOC_IN | IOC_OUT, 0x04, 8);
/// `struct input_keymap_entry` is 40 bytes.
pub const EVIOCGKEYCODE_V2: u32 = ioc(IOC_IN | IOC_OUT, 0x04, 40);
pub const EVIOCSKEYCODE: u32 = ioc(IOC_IN, 0x04, 8);
pub const EVIOCSKEYCODE_V2: u32 = ioc(IOC_IN, 0x04, 40);
/// `struct ff_effect` is 48 bytes on 64-bit architectures.
pub const EVIOCSFF: u32 = ioc(IOC_IN, 0x80, 48);
pub const EVIOCRMFF: u32 = ioc(IOC_VOID, 0x81, 4);
pub const EVIOCGEFFECTS: u32 = ioc(IOC_OUT, 0x84, 4);
pub const EVIOCGRAB: u32 = ioc(IOC_VOID, 0x90, 4);
pub const EVIOCREVOKE: u32 = ioc(IOC_VOID, 0x91, 4);
pub const EVIOCSCLOCKID: u32 = ioc(IOC_IN, 0xa0, 4);

/// `EVIOCGABS(abs)`: request numbers 0x40 + `abs`.
#[must_use]
pub const fn eviocgabs(abs: u32) -> u32 {
    ioc(IOC_OUT, 0x40 + abs, 24)
}

/// `EVIOCSABS(abs)`: request numbers 0xc0 + `abs`.
#[must_use]
pub const fn eviocsabs(abs: u32) -> u32 {
    ioc(IOC_IN, 0xc0 + abs, 24)
}

/// The variable-length requests, by number (`IOCBASECMD`: the request
/// with its length cleared).
pub mod var {
    pub const EVIOCGNAME: u32 = 0x06;
    pub const EVIOCGPHYS: u32 = 0x07;
    pub const EVIOCGUNIQ: u32 = 0x08;
    pub const EVIOCGPROP: u32 = 0x09;
    pub const EVIOCGMTSLOTS: u32 = 0x0a;
    pub const EVIOCGKEY: u32 = 0x18;
    pub const EVIOCGLED: u32 = 0x19;
    pub const EVIOCGSND: u32 = 0x1a;
    pub const EVIOCGSW: u32 = 0x1b;
    /// `EVIOCGBIT(ev, len)`: 0x20 + `ev`.
    pub const EVIOCGBIT: u32 = 0x20;
}

/// `EVIOCGNAME(len)` and the other requests of [`var`], for `len`
/// bytes.
#[must_use]
pub const fn eviocvar(num: u32, len: u32) -> u32 {
    ioc(IOC_OUT, num, len)
}

/// Event types.
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const EV_SW: u16 = 0x05;
pub const EV_LED: u16 = 0x11;
pub const EV_SND: u16 = 0x12;
pub const EV_REP: u16 = 0x14;
pub const EV_FF: u16 = 0x15;
pub const EV_CNT: u16 = 0x20;

/// `EV_SYN` codes.
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;
pub const SYN_CNT: u16 = 0x10;

/// Code counts per type.
pub const KEY_CNT: u16 = 0x300;
pub const REL_CNT: u16 = 0x10;
pub const ABS_CNT: u16 = 0x40;
pub const MSC_CNT: u16 = 0x08;
pub const SW_CNT: u16 = 0x12;
pub const LED_CNT: u16 = 0x10;
pub const SND_CNT: u16 = 0x08;
pub const REP_CNT: u16 = 0x02;
pub const FF_CNT: u16 = 0x80;
pub const INPUT_PROP_CNT: u16 = 0x20;

/// Key values.
pub const KEY_EVENT_UP: i32 = 0;
pub const KEY_EVENT_DOWN: i32 = 1;
pub const KEY_EVENT_REPEAT: i32 = 2;

/// Key and button codes.
pub const KEY_RESERVED: u16 = 0;
pub const BTN_MISC: u16 = 0x100;
pub const BTN_MOUSE: u16 = 0x110;
pub const KEY_OK: u16 = 0x160;

/// Relative axes.
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_Z: u16 = 0x02;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;

/// Absolute axes.
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;

/// LEDs.
pub const LED_NUML: u16 = 0x00;
pub const LED_CAPSL: u16 = 0x01;
pub const LED_SCROLLL: u16 = 0x02;

/// Repeat parameters.
pub const REP_DELAY: u16 = 0x00;
pub const REP_PERIOD: u16 = 0x01;

/// Device properties.
pub const INPUT_PROP_POINTER: u16 = 0x00;
pub const INPUT_PROP_DIRECT: u16 = 0x01;

/// Bus types (`input_id.bustype`).
pub const BUS_USB: u16 = 0x03;
pub const BUS_VIRTUAL: u16 = 0x06;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    n("EV_VERSION", EV_VERSION as i64),
    c("EVIOCGVERSION", EVIOCGVERSION),
    c("EVIOCGID", EVIOCGID),
    c("EVIOCGREP", EVIOCGREP),
    c("EVIOCSREP", EVIOCSREP),
    c("EVIOCGKEYCODE", EVIOCGKEYCODE),
    c("EVIOCGKEYCODE_V2", EVIOCGKEYCODE_V2),
    c("EVIOCSKEYCODE", EVIOCSKEYCODE),
    c("EVIOCSKEYCODE_V2", EVIOCSKEYCODE_V2),
    c("EVIOCSFF", EVIOCSFF),
    c("EVIOCRMFF", EVIOCRMFF),
    c("EVIOCGEFFECTS", EVIOCGEFFECTS),
    c("EVIOCGRAB", EVIOCGRAB),
    c("EVIOCREVOKE", EVIOCREVOKE),
    c("EVIOCSCLOCKID", EVIOCSCLOCKID),
    c("EVIOCGABS(0)", eviocgabs(0)),
    c("EVIOCSABS(0)", eviocsabs(0)),
    c("EVIOCGNAME(0)", eviocvar(var::EVIOCGNAME, 0)),
    c("EVIOCGPHYS(0)", eviocvar(var::EVIOCGPHYS, 0)),
    c("EVIOCGUNIQ(0)", eviocvar(var::EVIOCGUNIQ, 0)),
    c("EVIOCGPROP(0)", eviocvar(var::EVIOCGPROP, 0)),
    c("EVIOCGKEY(0)", eviocvar(var::EVIOCGKEY, 0)),
    c("EVIOCGLED(0)", eviocvar(var::EVIOCGLED, 0)),
    c("EVIOCGSND(0)", eviocvar(var::EVIOCGSND, 0)),
    c("EVIOCGSW(0)", eviocvar(var::EVIOCGSW, 0)),
    c("EVIOCGBIT(0, 0)", eviocvar(var::EVIOCGBIT, 0)),
    n("EV_SYN", EV_SYN as i64),
    n("EV_KEY", EV_KEY as i64),
    n("EV_REL", EV_REL as i64),
    n("EV_ABS", EV_ABS as i64),
    n("EV_MSC", EV_MSC as i64),
    n("EV_SW", EV_SW as i64),
    n("EV_LED", EV_LED as i64),
    n("EV_SND", EV_SND as i64),
    n("EV_REP", EV_REP as i64),
    n("EV_FF", EV_FF as i64),
    n("EV_CNT", EV_CNT as i64),
    n("SYN_REPORT", SYN_REPORT as i64),
    n("SYN_DROPPED", SYN_DROPPED as i64),
    n("SYN_CNT", SYN_CNT as i64),
    n("KEY_CNT", KEY_CNT as i64),
    n("REL_CNT", REL_CNT as i64),
    n("ABS_CNT", ABS_CNT as i64),
    n("MSC_CNT", MSC_CNT as i64),
    n("SW_CNT", SW_CNT as i64),
    n("LED_CNT", LED_CNT as i64),
    n("SND_CNT", SND_CNT as i64),
    n("REP_CNT", REP_CNT as i64),
    n("FF_CNT", FF_CNT as i64),
    n("INPUT_PROP_CNT", INPUT_PROP_CNT as i64),
    n("BTN_MISC", BTN_MISC as i64),
    n("BTN_MOUSE", BTN_MOUSE as i64),
    n("KEY_OK", KEY_OK as i64),
    n("REL_X", REL_X as i64),
    n("REL_Y", REL_Y as i64),
    n("REL_Z", REL_Z as i64),
    n("REL_HWHEEL", REL_HWHEEL as i64),
    n("REL_WHEEL", REL_WHEEL as i64),
    n("ABS_X", ABS_X as i64),
    n("ABS_Y", ABS_Y as i64),
    n("ABS_Z", ABS_Z as i64),
    n("LED_NUML", LED_NUML as i64),
    n("LED_CAPSL", LED_CAPSL as i64),
    n("LED_SCROLLL", LED_SCROLLL as i64),
    n("REP_DELAY", REP_DELAY as i64),
    n("REP_PERIOD", REP_PERIOD as i64),
    n("INPUT_PROP_POINTER", INPUT_PROP_POINTER as i64),
    n("INPUT_PROP_DIRECT", INPUT_PROP_DIRECT as i64),
    n("BUS_USB", BUS_USB as i64),
    n("BUS_VIRTUAL", BUS_VIRTUAL as i64),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "dev/evdev/input.h",
        value: value as i64,
        arches: Arches::All,
    }
}

const fn n(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "dev/evdev/input.h",
        value,
        arches: Arches::All,
    }
}
