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

/// The keys consumer-control and system-control collections map to
/// (`hcons`, `hsctrl`), from `input-event-codes.h`.
pub const KEY_ADDRESSBOOK: u16 = 0x1ad;
pub const KEY_APPSELECT: u16 = 0x244;
pub const KEY_ASPECT_RATIO: u16 = 0x177;
pub const KEY_ASSISTANT: u16 = 0x247;
pub const KEY_AUDIO: u16 = 0x188;
pub const KEY_BACK: u16 = 0x09e;
pub const KEY_BASSBOOST: u16 = 0x0d1;
pub const KEY_BLUE: u16 = 0x191;
pub const KEY_BOOKMARKS: u16 = 0x09c;
pub const KEY_BRIGHTNESSDOWN: u16 = 0x0e0;
pub const KEY_BRIGHTNESSUP: u16 = 0x0e1;
pub const KEY_BRIGHTNESS_AUTO: u16 = 0x0f4;
pub const KEY_BRIGHTNESS_MAX: u16 = 0x251;
pub const KEY_BRIGHTNESS_MIN: u16 = 0x250;
pub const KEY_BRIGHTNESS_TOGGLE: u16 = 0x1af;
pub const KEY_BUTTONCONFIG: u16 = 0x240;
pub const KEY_CALC: u16 = 0x08c;
pub const KEY_CALENDAR: u16 = 0x18d;
pub const KEY_CAMERA: u16 = 0x0d4;
pub const KEY_CANCEL: u16 = 0x0df;
pub const KEY_CD: u16 = 0x17f;
pub const KEY_CHANNELDOWN: u16 = 0x193;
pub const KEY_CHANNELUP: u16 = 0x192;
pub const KEY_CHAT: u16 = 0x0d8;
pub const KEY_CLOSE: u16 = 0x0ce;
pub const KEY_COFFEE: u16 = 0x098;
pub const KEY_CONFIG: u16 = 0x0ab;
pub const KEY_CONTEXT_MENU: u16 = 0x1b6;
pub const KEY_CONTROLPANEL: u16 = 0x243;
pub const KEY_COPY: u16 = 0x085;
pub const KEY_CUT: u16 = 0x089;
pub const KEY_DATABASE: u16 = 0x1aa;
pub const KEY_DELETE: u16 = 0x06f;
pub const KEY_DOCUMENTS: u16 = 0x0eb;
pub const KEY_DOWN: u16 = 0x06c;
pub const KEY_DVD: u16 = 0x185;
pub const KEY_EDIT: u16 = 0x0b0;
pub const KEY_EDITOR: u16 = 0x1a6;
pub const KEY_EJECTCD: u16 = 0x0a1;
pub const KEY_ENTER: u16 = 0x01c;
pub const KEY_ESC: u16 = 0x001;
pub const KEY_EXIT: u16 = 0x0ae;
pub const KEY_FASTFORWARD: u16 = 0x0d0;
pub const KEY_FILE: u16 = 0x090;
pub const KEY_FINANCE: u16 = 0x0db;
pub const KEY_FIND: u16 = 0x088;
pub const KEY_FORWARD: u16 = 0x09f;
pub const KEY_FORWARDMAIL: u16 = 0x0e9;
pub const KEY_FULL_SCREEN: u16 = 0x174;
pub const KEY_GAMES: u16 = 0x1a1;
pub const KEY_GOTO: u16 = 0x162;
pub const KEY_GRAPHICSEDITOR: u16 = 0x1a8;
pub const KEY_GREEN: u16 = 0x18f;
pub const KEY_HELP: u16 = 0x08a;
pub const KEY_HOMEPAGE: u16 = 0x0ac;
pub const KEY_IMAGES: u16 = 0x1ba;
pub const KEY_INFO: u16 = 0x166;
pub const KEY_INSERT: u16 = 0x06e;
pub const KEY_JOURNAL: u16 = 0x242;
pub const KEY_KBDILLUMDOWN: u16 = 0x0e5;
pub const KEY_KBDILLUMTOGGLE: u16 = 0x0e4;
pub const KEY_KBDILLUMUP: u16 = 0x0e6;
pub const KEY_KBDINPUTASSIST_ACCEPT: u16 = 0x264;
pub const KEY_KBDINPUTASSIST_CANCEL: u16 = 0x265;
pub const KEY_KBDINPUTASSIST_NEXT: u16 = 0x261;
pub const KEY_KBDINPUTASSIST_NEXTGROUP: u16 = 0x263;
pub const KEY_KBDINPUTASSIST_PREV: u16 = 0x260;
pub const KEY_KBDINPUTASSIST_PREVGROUP: u16 = 0x262;
pub const KEY_KBD_LAYOUT_NEXT: u16 = 0x248;
pub const KEY_KEYBOARD: u16 = 0x176;
pub const KEY_KPMINUS: u16 = 0x04a;
pub const KEY_KPPLUS: u16 = 0x04e;
pub const KEY_LAST: u16 = 0x195;
pub const KEY_LEFT: u16 = 0x069;
pub const KEY_LOGOFF: u16 = 0x1b1;
pub const KEY_MAIL: u16 = 0x09b;
pub const KEY_MEDIA_REPEAT: u16 = 0x1b7;
pub const KEY_MEMO: u16 = 0x18c;
pub const KEY_MENU: u16 = 0x08b;
pub const KEY_MESSENGER: u16 = 0x1ae;
pub const KEY_MUTE: u16 = 0x071;
pub const KEY_NEW: u16 = 0x0b5;
pub const KEY_NEWS: u16 = 0x1ab;
pub const KEY_NEXT: u16 = 0x197;
pub const KEY_NEXTSONG: u16 = 0x0a3;
pub const KEY_OPEN: u16 = 0x086;
pub const KEY_PASTE: u16 = 0x087;
pub const KEY_PAUSE: u16 = 0x077;
pub const KEY_PC: u16 = 0x178;
pub const KEY_PHONE: u16 = 0x0a9;
pub const KEY_PLAY: u16 = 0x0cf;
pub const KEY_PLAYER: u16 = 0x183;
pub const KEY_PLAYPAUSE: u16 = 0x0a4;
pub const KEY_POWER: u16 = 0x074;
pub const KEY_POWER2: u16 = 0x164;
pub const KEY_PRESENTATION: u16 = 0x1a9;
pub const KEY_PREVIOUS: u16 = 0x19c;
pub const KEY_PREVIOUSSONG: u16 = 0x0a5;
pub const KEY_PRINT: u16 = 0x0d2;
pub const KEY_PROG1: u16 = 0x094;
pub const KEY_PROGRAM: u16 = 0x16a;
pub const KEY_PROPS: u16 = 0x082;
pub const KEY_PVR: u16 = 0x16e;
pub const KEY_RECORD: u16 = 0x0a7;
pub const KEY_RED: u16 = 0x18e;
pub const KEY_REDO: u16 = 0x0b6;
pub const KEY_REFRESH: u16 = 0x0ad;
pub const KEY_REPLY: u16 = 0x0e8;
pub const KEY_RESTART: u16 = 0x198;
pub const KEY_REWIND: u16 = 0x0a8;
pub const KEY_RIGHT: u16 = 0x06a;
pub const KEY_SAT: u16 = 0x17d;
pub const KEY_SAVE: u16 = 0x0ea;
pub const KEY_SCALE: u16 = 0x078;
pub const KEY_SCREENSAVER: u16 = 0x245;
pub const KEY_SCROLLDOWN: u16 = 0x0b2;
pub const KEY_SCROLLUP: u16 = 0x0b1;
pub const KEY_SEARCH: u16 = 0x0d9;
pub const KEY_SELECT: u16 = 0x161;
pub const KEY_SEND: u16 = 0x0e7;
pub const KEY_SHUFFLE: u16 = 0x19a;
pub const KEY_SLEEP: u16 = 0x08e;
pub const KEY_SLOW: u16 = 0x199;
pub const KEY_SPELLCHECK: u16 = 0x1b0;
pub const KEY_SPREADSHEET: u16 = 0x1a7;
pub const KEY_STOP: u16 = 0x080;
pub const KEY_STOPCD: u16 = 0x0a6;
pub const KEY_SUBTITLE: u16 = 0x172;
pub const KEY_TAPE: u16 = 0x180;
pub const KEY_TASKMANAGER: u16 = 0x241;
pub const KEY_TUNER: u16 = 0x182;
pub const KEY_TV: u16 = 0x179;
pub const KEY_TV2: u16 = 0x17a;
pub const KEY_UNDO: u16 = 0x083;
pub const KEY_UP: u16 = 0x067;
pub const KEY_VCR: u16 = 0x17b;
pub const KEY_VCR2: u16 = 0x17c;
pub const KEY_VIDEO: u16 = 0x189;
pub const KEY_VIDEOPHONE: u16 = 0x1a0;
pub const KEY_VIDEO_NEXT: u16 = 0x0f1;
pub const KEY_VOICECOMMAND: u16 = 0x246;
pub const KEY_VOICEMAIL: u16 = 0x1ac;
pub const KEY_VOLUMEDOWN: u16 = 0x072;
pub const KEY_VOLUMEUP: u16 = 0x073;
pub const KEY_WAKEUP: u16 = 0x08f;
pub const KEY_WORDPROCESSOR: u16 = 0x1a5;
pub const KEY_WWW: u16 = 0x096;
pub const KEY_YELLOW: u16 = 0x190;
pub const KEY_ZOOMIN: u16 = 0x1a2;
pub const KEY_ZOOMOUT: u16 = 0x1a3;
pub const KEY_ZOOMRESET: u16 = 0x1a4;

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
/// The volume (`hcons`).
pub const ABS_VOLUME: u16 = 0x20;

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
    n("ABS_VOLUME", ABS_VOLUME as i64),
    n("KEY_ADDRESSBOOK", KEY_ADDRESSBOOK as i64),
    n("KEY_APPSELECT", KEY_APPSELECT as i64),
    n("KEY_ASPECT_RATIO", KEY_ASPECT_RATIO as i64),
    n("KEY_ASSISTANT", KEY_ASSISTANT as i64),
    n("KEY_AUDIO", KEY_AUDIO as i64),
    n("KEY_BACK", KEY_BACK as i64),
    n("KEY_BASSBOOST", KEY_BASSBOOST as i64),
    n("KEY_BLUE", KEY_BLUE as i64),
    n("KEY_BOOKMARKS", KEY_BOOKMARKS as i64),
    n("KEY_BRIGHTNESSDOWN", KEY_BRIGHTNESSDOWN as i64),
    n("KEY_BRIGHTNESSUP", KEY_BRIGHTNESSUP as i64),
    n("KEY_BRIGHTNESS_AUTO", KEY_BRIGHTNESS_AUTO as i64),
    n("KEY_BRIGHTNESS_MAX", KEY_BRIGHTNESS_MAX as i64),
    n("KEY_BRIGHTNESS_MIN", KEY_BRIGHTNESS_MIN as i64),
    n("KEY_BRIGHTNESS_TOGGLE", KEY_BRIGHTNESS_TOGGLE as i64),
    n("KEY_BUTTONCONFIG", KEY_BUTTONCONFIG as i64),
    n("KEY_CALC", KEY_CALC as i64),
    n("KEY_CALENDAR", KEY_CALENDAR as i64),
    n("KEY_CAMERA", KEY_CAMERA as i64),
    n("KEY_CANCEL", KEY_CANCEL as i64),
    n("KEY_CD", KEY_CD as i64),
    n("KEY_CHANNELDOWN", KEY_CHANNELDOWN as i64),
    n("KEY_CHANNELUP", KEY_CHANNELUP as i64),
    n("KEY_CHAT", KEY_CHAT as i64),
    n("KEY_CLOSE", KEY_CLOSE as i64),
    n("KEY_COFFEE", KEY_COFFEE as i64),
    n("KEY_CONFIG", KEY_CONFIG as i64),
    n("KEY_CONTEXT_MENU", KEY_CONTEXT_MENU as i64),
    n("KEY_CONTROLPANEL", KEY_CONTROLPANEL as i64),
    n("KEY_COPY", KEY_COPY as i64),
    n("KEY_CUT", KEY_CUT as i64),
    n("KEY_DATABASE", KEY_DATABASE as i64),
    n("KEY_DELETE", KEY_DELETE as i64),
    n("KEY_DOCUMENTS", KEY_DOCUMENTS as i64),
    n("KEY_DOWN", KEY_DOWN as i64),
    n("KEY_DVD", KEY_DVD as i64),
    n("KEY_EDIT", KEY_EDIT as i64),
    n("KEY_EDITOR", KEY_EDITOR as i64),
    n("KEY_EJECTCD", KEY_EJECTCD as i64),
    n("KEY_ENTER", KEY_ENTER as i64),
    n("KEY_ESC", KEY_ESC as i64),
    n("KEY_EXIT", KEY_EXIT as i64),
    n("KEY_FASTFORWARD", KEY_FASTFORWARD as i64),
    n("KEY_FILE", KEY_FILE as i64),
    n("KEY_FINANCE", KEY_FINANCE as i64),
    n("KEY_FIND", KEY_FIND as i64),
    n("KEY_FORWARD", KEY_FORWARD as i64),
    n("KEY_FORWARDMAIL", KEY_FORWARDMAIL as i64),
    n("KEY_FULL_SCREEN", KEY_FULL_SCREEN as i64),
    n("KEY_GAMES", KEY_GAMES as i64),
    n("KEY_GOTO", KEY_GOTO as i64),
    n("KEY_GRAPHICSEDITOR", KEY_GRAPHICSEDITOR as i64),
    n("KEY_GREEN", KEY_GREEN as i64),
    n("KEY_HELP", KEY_HELP as i64),
    n("KEY_HOMEPAGE", KEY_HOMEPAGE as i64),
    n("KEY_IMAGES", KEY_IMAGES as i64),
    n("KEY_INFO", KEY_INFO as i64),
    n("KEY_INSERT", KEY_INSERT as i64),
    n("KEY_JOURNAL", KEY_JOURNAL as i64),
    n("KEY_KBDILLUMDOWN", KEY_KBDILLUMDOWN as i64),
    n("KEY_KBDILLUMTOGGLE", KEY_KBDILLUMTOGGLE as i64),
    n("KEY_KBDILLUMUP", KEY_KBDILLUMUP as i64),
    n(
        "KEY_KBDINPUTASSIST_ACCEPT",
        KEY_KBDINPUTASSIST_ACCEPT as i64,
    ),
    n(
        "KEY_KBDINPUTASSIST_CANCEL",
        KEY_KBDINPUTASSIST_CANCEL as i64,
    ),
    n("KEY_KBDINPUTASSIST_NEXT", KEY_KBDINPUTASSIST_NEXT as i64),
    n(
        "KEY_KBDINPUTASSIST_NEXTGROUP",
        KEY_KBDINPUTASSIST_NEXTGROUP as i64,
    ),
    n("KEY_KBDINPUTASSIST_PREV", KEY_KBDINPUTASSIST_PREV as i64),
    n(
        "KEY_KBDINPUTASSIST_PREVGROUP",
        KEY_KBDINPUTASSIST_PREVGROUP as i64,
    ),
    n("KEY_KBD_LAYOUT_NEXT", KEY_KBD_LAYOUT_NEXT as i64),
    n("KEY_KEYBOARD", KEY_KEYBOARD as i64),
    n("KEY_KPMINUS", KEY_KPMINUS as i64),
    n("KEY_KPPLUS", KEY_KPPLUS as i64),
    n("KEY_LAST", KEY_LAST as i64),
    n("KEY_LEFT", KEY_LEFT as i64),
    n("KEY_LOGOFF", KEY_LOGOFF as i64),
    n("KEY_MAIL", KEY_MAIL as i64),
    n("KEY_MEDIA_REPEAT", KEY_MEDIA_REPEAT as i64),
    n("KEY_MEMO", KEY_MEMO as i64),
    n("KEY_MENU", KEY_MENU as i64),
    n("KEY_MESSENGER", KEY_MESSENGER as i64),
    n("KEY_MUTE", KEY_MUTE as i64),
    n("KEY_NEW", KEY_NEW as i64),
    n("KEY_NEWS", KEY_NEWS as i64),
    n("KEY_NEXT", KEY_NEXT as i64),
    n("KEY_NEXTSONG", KEY_NEXTSONG as i64),
    n("KEY_OPEN", KEY_OPEN as i64),
    n("KEY_PASTE", KEY_PASTE as i64),
    n("KEY_PAUSE", KEY_PAUSE as i64),
    n("KEY_PC", KEY_PC as i64),
    n("KEY_PHONE", KEY_PHONE as i64),
    n("KEY_PLAY", KEY_PLAY as i64),
    n("KEY_PLAYER", KEY_PLAYER as i64),
    n("KEY_PLAYPAUSE", KEY_PLAYPAUSE as i64),
    n("KEY_POWER", KEY_POWER as i64),
    n("KEY_POWER2", KEY_POWER2 as i64),
    n("KEY_PRESENTATION", KEY_PRESENTATION as i64),
    n("KEY_PREVIOUS", KEY_PREVIOUS as i64),
    n("KEY_PREVIOUSSONG", KEY_PREVIOUSSONG as i64),
    n("KEY_PRINT", KEY_PRINT as i64),
    n("KEY_PROG1", KEY_PROG1 as i64),
    n("KEY_PROGRAM", KEY_PROGRAM as i64),
    n("KEY_PROPS", KEY_PROPS as i64),
    n("KEY_PVR", KEY_PVR as i64),
    n("KEY_RECORD", KEY_RECORD as i64),
    n("KEY_RED", KEY_RED as i64),
    n("KEY_REDO", KEY_REDO as i64),
    n("KEY_REFRESH", KEY_REFRESH as i64),
    n("KEY_REPLY", KEY_REPLY as i64),
    n("KEY_RESTART", KEY_RESTART as i64),
    n("KEY_REWIND", KEY_REWIND as i64),
    n("KEY_RIGHT", KEY_RIGHT as i64),
    n("KEY_SAT", KEY_SAT as i64),
    n("KEY_SAVE", KEY_SAVE as i64),
    n("KEY_SCALE", KEY_SCALE as i64),
    n("KEY_SCREENSAVER", KEY_SCREENSAVER as i64),
    n("KEY_SCROLLDOWN", KEY_SCROLLDOWN as i64),
    n("KEY_SCROLLUP", KEY_SCROLLUP as i64),
    n("KEY_SEARCH", KEY_SEARCH as i64),
    n("KEY_SELECT", KEY_SELECT as i64),
    n("KEY_SEND", KEY_SEND as i64),
    n("KEY_SHUFFLE", KEY_SHUFFLE as i64),
    n("KEY_SLEEP", KEY_SLEEP as i64),
    n("KEY_SLOW", KEY_SLOW as i64),
    n("KEY_SPELLCHECK", KEY_SPELLCHECK as i64),
    n("KEY_SPREADSHEET", KEY_SPREADSHEET as i64),
    n("KEY_STOP", KEY_STOP as i64),
    n("KEY_STOPCD", KEY_STOPCD as i64),
    n("KEY_SUBTITLE", KEY_SUBTITLE as i64),
    n("KEY_TAPE", KEY_TAPE as i64),
    n("KEY_TASKMANAGER", KEY_TASKMANAGER as i64),
    n("KEY_TUNER", KEY_TUNER as i64),
    n("KEY_TV", KEY_TV as i64),
    n("KEY_TV2", KEY_TV2 as i64),
    n("KEY_UNDO", KEY_UNDO as i64),
    n("KEY_UP", KEY_UP as i64),
    n("KEY_VCR", KEY_VCR as i64),
    n("KEY_VCR2", KEY_VCR2 as i64),
    n("KEY_VIDEO", KEY_VIDEO as i64),
    n("KEY_VIDEOPHONE", KEY_VIDEOPHONE as i64),
    n("KEY_VIDEO_NEXT", KEY_VIDEO_NEXT as i64),
    n("KEY_VOICECOMMAND", KEY_VOICECOMMAND as i64),
    n("KEY_VOICEMAIL", KEY_VOICEMAIL as i64),
    n("KEY_VOLUMEDOWN", KEY_VOLUMEDOWN as i64),
    n("KEY_VOLUMEUP", KEY_VOLUMEUP as i64),
    n("KEY_WAKEUP", KEY_WAKEUP as i64),
    n("KEY_WORDPROCESSOR", KEY_WORDPROCESSOR as i64),
    n("KEY_WWW", KEY_WWW as i64),
    n("KEY_YELLOW", KEY_YELLOW as i64),
    n("KEY_ZOOMIN", KEY_ZOOMIN as i64),
    n("KEY_ZOOMOUT", KEY_ZOOMOUT as i64),
    n("KEY_ZOOMRESET", KEY_ZOOMRESET as i64),
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
