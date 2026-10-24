// SPDX-License-Identifier: BSD-2-Clause
//! Keyboards (`sys/kbio.h`): the kbd layer's modes, lock and LED bits,
//! the `KD*` and keymap requests, the keymap, accent map, and function
//! key structures they carry, and the values `K_XLATE` reads return.
//!
//! The whole keymap and accent map are too big for an `ioctl` argument,
//! so `GIO_KEYMAP` and its kin pass a pointer instead ([`indirect`]).

use crate::ioctl::{IOC_IN, IOC_OUT, IOC_VOID, IOCPARM_MASK};
use crate::layout::{Arches, ConstValue};

/// Key input modes: AT scancodes, characters, keycodes.
pub const K_RAW: i32 = 0;
pub const K_XLATE: i32 = 1;
pub const K_CODE: i32 = 2;

/// Lock key state (`KDGKBSTATE`): Caps, Num, Scroll, AltGr locked.
pub const CLKED: i32 = 1;
pub const NLKED: i32 = 2;
pub const SLKED: i32 = 4;
pub const ALKED: i32 = 8;
pub const LOCK_MASK: i32 = CLKED | NLKED | SLKED | ALKED;

/// LEDs (`KDGETLED`): the same bits as the lock state.
pub const LED_CAP: i32 = 1;
pub const LED_NUM: i32 = 2;
pub const LED_SCR: i32 = 4;
pub const LED_MASK: i32 = LED_CAP | LED_NUM | LED_SCR;

/// Keyboard models (`KDGKBTYPE`).
pub const KB_OTHER: i32 = 0;
pub const KB_84: i32 = 1;
pub const KB_101: i32 = 2;

/// Repeat delays and periods, in ms, by `KDSETRAD`'s bits 5-6 and 0-4.
pub const KBDELAYS: [i32; 4] = [250, 500, 750, 1000];
pub const KBRATES: [i32; 32] = [
    34, 38, 42, 46, 50, 55, 59, 63, 68, 76, 84, 92, 100, 110, 118, 126, 136, 152, 168, 184, 200,
    220, 236, 252, 272, 304, 336, 368, 400, 440, 472, 504,
];

/// Table sizes.
pub const NUM_KEYS: usize = 256;
pub const NUM_STATES: usize = 8;
pub const ALTGR_OFFSET: usize = 128;
pub const NUM_DEADKEYS: usize = 15;
pub const NUM_ACCENTCHARS: usize = 52;
pub const NUM_FKEYS: usize = 96;
pub const MAXFK: usize = 16;

/// `keyent_t.flgs`: Caps Lock or Num Lock shifts the key.
pub const FLAG_LOCK_O: u8 = 0;
pub const FLAG_LOCK_C: u8 = 1;
pub const FLAG_LOCK_N: u8 = 2;

/// Special key actions (a key's `spcl` bit set for its state).
pub mod spcl {
    pub const NOP: u32 = 0x00;
    pub const LSH: u32 = 0x02;
    pub const RSH: u32 = 0x03;
    pub const CLK: u32 = 0x04;
    pub const NLK: u32 = 0x05;
    pub const SLK: u32 = 0x06;
    pub const LALT: u32 = 0x07;
    pub const BTAB: u32 = 0x08;
    pub const LCTR: u32 = 0x09;
    pub const NEXT: u32 = 0x0a;
    pub const F_SCR: u32 = 0x0b;
    pub const L_SCR: u32 = 0x1a;
    pub const F_FN: u32 = 0x1b;
    pub const L_FN: u32 = 0x7a;
    pub const RCTR: u32 = 0x80;
    pub const RALT: u32 = 0x81;
    pub const ALK: u32 = 0x82;
    pub const ASH: u32 = 0x83;
    pub const META: u32 = 0x84;
    pub const RBT: u32 = 0x85;
    pub const DBG: u32 = 0x86;
    pub const SUSP: u32 = 0x87;
    pub const SPSC: u32 = 0x88;
    pub const F_ACC: u32 = 0x89;
    pub const DGRA: u32 = 0x89;
    pub const DACU: u32 = 0x8a;
    pub const DCIR: u32 = 0x8b;
    pub const DTIL: u32 = 0x8c;
    pub const DMAC: u32 = 0x8d;
    pub const DBRE: u32 = 0x8e;
    pub const DDOT: u32 = 0x8f;
    pub const DUML: u32 = 0x90;
    pub const DSLA: u32 = 0x91;
    pub const DRIN: u32 = 0x92;
    pub const DCED: u32 = 0x93;
    pub const DAPO: u32 = 0x94;
    pub const DDAC: u32 = 0x95;
    pub const DOGO: u32 = 0x96;
    pub const DCAR: u32 = 0x97;
    pub const L_ACC: u32 = 0x97;
    pub const STBY: u32 = 0x98;
    pub const PREV: u32 = 0x99;
    pub const PNC: u32 = 0x9a;
    pub const LSHA: u32 = 0x9b;
    pub const RSHA: u32 = 0x9c;
    pub const LCTRA: u32 = 0x9d;
    pub const RCTRA: u32 = 0x9e;
    pub const LALTA: u32 = 0x9f;
    pub const RALTA: u32 = 0xa0;
    pub const HALT: u32 = 0xa1;
    pub const PDWN: u32 = 0xa2;
    pub const PASTE: u32 = 0xa3;

    /// Function key `x` (1-based).
    #[must_use]
    pub const fn f(x: u32) -> u32 {
        x + F_FN - 1
    }

    /// Switch to screen `x` (1-based).
    #[must_use]
    pub const fn s(x: u32) -> u32 {
        x + F_SCR - 1
    }
}

/// Flags in a `K_XLATE` read's value, above its 24-bit character.
pub const NOKEY: u32 = 0x0100_0000;
pub const FKEY: u32 = 0x0200_0000;
pub const MKEY: u32 = 0x0400_0000;
pub const BKEY: u32 = 0x0800_0000;
pub const SPCLKEY: u32 = 0x8000_0000;
pub const RELKEY: u32 = 0x4000_0000;
pub const ERRKEY: u32 = 0x2000_0000;

/// The character in a `K_XLATE` value.
#[must_use]
pub const fn keychar(c: u32) -> u32 {
    c & 0x00ff_ffff
}

/// The flags in a `K_XLATE` value.
#[must_use]
pub const fn keyflags(c: u32) -> u32 {
    c & !0x00ff_ffff
}

/// `struct keyboard_info` (`KDGKBINFO`, `KBADDKBD`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardInfo {
    pub kb_index: i32,
    pub kb_name: [u8; 16],
    pub kb_unit: i32,
    pub kb_type: i32,
    pub kb_config: i32,
    pub kb_flags: i32,
}

impl KeyboardInfo {
    pub const SIZE: usize = 36;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[..4].copy_from_slice(&self.kb_index.to_ne_bytes());
        b[4..20].copy_from_slice(&self.kb_name);
        b[20..24].copy_from_slice(&self.kb_unit.to_ne_bytes());
        b[24..28].copy_from_slice(&self.kb_type.to_ne_bytes());
        b[28..32].copy_from_slice(&self.kb_config.to_ne_bytes());
        b[32..36].copy_from_slice(&self.kb_flags.to_ne_bytes());
        b
    }

    /// From its bytes; `None` if `b` is short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let mut name = [0u8; 16];
        name.copy_from_slice(b.get(4..20)?);
        Some(Self {
            kb_index: i32_at(b, 0)?,
            kb_name: name,
            kb_unit: i32_at(b, 20)?,
            kb_type: i32_at(b, 24)?,
            kb_config: i32_at(b, 28)?,
            kb_flags: i32_at(b, 32)?,
        })
    }

    /// `kb_name` up to its NUL.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        let n = self.kb_name.iter().position(|&c| c == 0).unwrap_or(16);
        &self.kb_name[..n]
    }
}

/// `struct keyent_t`: what a key does in each of the eight shift states
/// (base, Shift, Ctrl, Ctrl-Shift, then the same with Alt), which of
/// them are special actions (`spcl`, bit 7 for state 0), and which lock
/// shifts it (`flgs`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyEnt {
    pub map: [u32; NUM_STATES],
    pub spcl: u8,
    pub flgs: u8,
}

impl KeyEnt {
    pub const SIZE: usize = 36;

    /// Writes its bytes to the front of `b`, which holds at least
    /// [`Self::SIZE`].
    pub fn write(&self, b: &mut [u8]) {
        for (i, m) in self.map.iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&m.to_ne_bytes());
        }
        b[32] = self.spcl;
        b[33] = self.flgs;
    }

    /// From its bytes; `None` if `b` is short.
    #[must_use]
    pub fn read(b: &[u8]) -> Option<Self> {
        let mut map = [0u32; NUM_STATES];
        for (i, m) in map.iter_mut().enumerate() {
            *m = u32_at(b, i * 4)?;
        }
        Some(Self {
            map,
            spcl: *b.get(32)?,
            flgs: *b.get(33)?,
        })
    }
}

/// `keymap_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keymap {
    pub n_keys: u16,
    pub key: [KeyEnt; NUM_KEYS],
}

impl Keymap {
    /// Its size: `n_keys` and padding, then the entries, each
    /// [`KeyEnt::SIZE`] bytes. (A whole one is too big for a kernel
    /// stack; the kernel converts it entry by entry.)
    pub const SIZE: usize = 4 + NUM_KEYS * KeyEnt::SIZE;
}

/// `struct acc_t`: a dead key's own character, and pairs of a character
/// and its accented form.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccT {
    pub accchar: u32,
    pub map: [[u32; 2]; NUM_ACCENTCHARS],
}

impl AccT {
    pub const SIZE: usize = 4 + NUM_ACCENTCHARS * 8;

    /// An unused entry.
    pub const EMPTY: Self = Self {
        accchar: 0,
        map: [[0; 2]; NUM_ACCENTCHARS],
    };

    /// Writes its bytes to the front of `b`, which holds at least
    /// [`Self::SIZE`].
    pub fn write(&self, b: &mut [u8]) {
        b[..4].copy_from_slice(&self.accchar.to_ne_bytes());
        for (j, [from, to]) in self.map.iter().enumerate() {
            let p = 4 + j * 8;
            b[p..p + 4].copy_from_slice(&from.to_ne_bytes());
            b[p + 4..p + 8].copy_from_slice(&to.to_ne_bytes());
        }
    }

    /// From its bytes; `None` if `b` is short.
    #[must_use]
    pub fn read(b: &[u8]) -> Option<Self> {
        let mut a = Self {
            accchar: u32_at(b, 0)?,
            ..Self::EMPTY
        };
        for (j, pair) in a.map.iter_mut().enumerate() {
            let p = 4 + j * 8;
            *pair = [u32_at(b, p)?, u32_at(b, p + 4)?];
        }
        Some(a)
    }
}

/// `accentmap_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccentMap {
    pub n_accs: u16,
    pub acc: [AccT; NUM_DEADKEYS],
}

impl AccentMap {
    /// Its size: `n_accs` and padding, then the entries, each
    /// [`AccT::SIZE`] bytes.
    pub const SIZE: usize = 4 + NUM_DEADKEYS * AccT::SIZE;
}

/// `keyarg_t` (`GIO_KEYMAPENT`, `PIO_KEYMAPENT`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyArg {
    pub keynum: u16,
    pub key: KeyEnt,
}

impl KeyArg {
    pub const SIZE: usize = 4 + KeyEnt::SIZE;

    /// The key number in its bytes.
    #[must_use]
    pub fn keynum(b: &[u8]) -> Option<u16> {
        Some(u16::from_ne_bytes(b.get(..2)?.try_into().ok()?))
    }

    /// The entry in its bytes.
    #[must_use]
    pub fn key(b: &[u8]) -> Option<KeyEnt> {
        KeyEnt::read(b.get(4..)?)
    }

    /// Writes `key` as its entry.
    pub fn set_key(b: &mut [u8], key: &KeyEnt) {
        if b.len() >= Self::SIZE {
            key.write(&mut b[4..]);
        }
    }
}

/// `fkeytab_t`: a function key's string.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FkeyTab {
    pub str: [u8; MAXFK],
    pub len: u8,
}

/// `fkeyarg_t` (`GETFKEY`, `SETFKEY`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FkeyArg {
    pub keynum: u16,
    pub keydef: [u8; MAXFK],
    pub flen: u8,
}

impl FkeyArg {
    pub const SIZE: usize = 20;
}

/// `keyboard_repeat_t` (`KDSETREPEAT`, `KDGETREPEAT`): delay and period
/// in ms.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardRepeat {
    pub kb_repeat: [i32; 2],
}

const fn ioc(dir: u32, group: u8, num: u8, len: u32) -> u32 {
    dir | ((len & IOCPARM_MASK) << 16) | (group as u32) << 8 | num as u32
}

pub const KDGKBMODE: u32 = ioc(IOC_OUT, b'K', 6, 4);
pub const KDSKBMODE: u32 = ioc(IOC_VOID, b'K', 7, 4);
pub const KDMKTONE: u32 = ioc(IOC_VOID, b'K', 8, 4);
pub const KDGETMODE: u32 = ioc(IOC_OUT, b'K', 9, 4);
pub const KDSETMODE: u32 = ioc(IOC_VOID, b'K', 10, 4);
pub const KDSBORDER: u32 = ioc(IOC_VOID, b'K', 13, 4);
pub const KDGKBSTATE: u32 = ioc(IOC_OUT, b'K', 19, 4);
pub const KDSKBSTATE: u32 = ioc(IOC_VOID, b'K', 20, 4);
pub const KDENABIO: u32 = ioc(IOC_VOID, b'K', 60, 0);
pub const KDDISABIO: u32 = ioc(IOC_VOID, b'K', 61, 0);
pub const KIOCSOUND: u32 = ioc(IOC_VOID, b'K', 63, 4);
pub const KDGKBTYPE: u32 = ioc(IOC_OUT, b'K', 64, 4);
pub const KDGETLED: u32 = ioc(IOC_OUT, b'K', 65, 4);
pub const KDSETLED: u32 = ioc(IOC_VOID, b'K', 66, 4);
pub const KDSETRAD: u32 = ioc(IOC_VOID, b'K', 67, 4);
pub const KBADDKBD: u32 = ioc(IOC_IN, b'K', 68, 36);
pub const KBRELKBD: u32 = ioc(IOC_IN, b'K', 69, 36);
pub const KDGKBINFO: u32 = ioc(IOC_OUT, b'K', 101, 36);
pub const KDSETREPEAT: u32 = ioc(IOC_IN, b'K', 102, 8);
pub const KDGETREPEAT: u32 = ioc(IOC_OUT, b'K', 103, 8);
pub const GETFKEY: u32 = ioc(IOC_IN | IOC_OUT, b'k', 0, 20);
pub const SETFKEY: u32 = ioc(IOC_IN | IOC_OUT, b'k', 1, 20);
pub const GIO_KEYMAP: u32 = ioc(IOC_VOID, b'k', 6, 0);
pub const PIO_KEYMAP: u32 = ioc(IOC_VOID, b'k', 7, 0);
pub const GIO_DEADKEYMAP: u32 = ioc(IOC_VOID, b'k', 8, 0);
pub const PIO_DEADKEYMAP: u32 = ioc(IOC_VOID, b'k', 9, 0);
pub const GIO_KEYMAPENT: u32 = ioc(IOC_IN | IOC_OUT, b'k', 10, 40);
pub const PIO_KEYMAPENT: u32 = ioc(IOC_IN, b'k', 11, 40);

/// A request whose argument is a pointer to a structure too big to be an
/// `ioctl` argument: the structure's size, and whether the kernel reads
/// it (`copyin`) or fills it (`copyout`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Indirect {
    pub len: usize,
    pub copy_in: bool,
    pub copy_out: bool,
}

/// Whether `req` passes its structure by pointer, and how.
#[must_use]
pub const fn indirect(req: u32) -> Option<Indirect> {
    let (len, copy_in) = match req {
        GIO_KEYMAP => (Keymap::SIZE, false),
        PIO_KEYMAP => (Keymap::SIZE, true),
        GIO_DEADKEYMAP => (AccentMap::SIZE, false),
        PIO_DEADKEYMAP => (AccentMap::SIZE, true),
        _ => return None,
    };
    Some(Indirect {
        len,
        copy_in,
        copy_out: !copy_in,
    })
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    u32_at(b, at).map(u32::cast_signed)
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    n("K_RAW", K_RAW as i64),
    n("K_XLATE", K_XLATE as i64),
    n("K_CODE", K_CODE as i64),
    n("CLKED", CLKED as i64),
    n("NLKED", NLKED as i64),
    n("SLKED", SLKED as i64),
    n("ALKED", ALKED as i64),
    n("LED_CAP", LED_CAP as i64),
    n("LED_NUM", LED_NUM as i64),
    n("LED_SCR", LED_SCR as i64),
    n("KB_OTHER", KB_OTHER as i64),
    n("KB_84", KB_84 as i64),
    n("KB_101", KB_101 as i64),
    n("NUM_KEYS", NUM_KEYS as i64),
    n("NUM_STATES", NUM_STATES as i64),
    n("ALTGR_OFFSET", ALTGR_OFFSET as i64),
    n("NUM_DEADKEYS", NUM_DEADKEYS as i64),
    n("NUM_ACCENTCHARS", NUM_ACCENTCHARS as i64),
    n("NUM_FKEYS", NUM_FKEYS as i64),
    n("MAXFK", MAXFK as i64),
    n("NOP", spcl::NOP as i64),
    n("BTAB", spcl::BTAB as i64),
    n("F_SCR", spcl::F_SCR as i64),
    n("L_SCR", spcl::L_SCR as i64),
    n("F_FN", spcl::F_FN as i64),
    n("L_FN", spcl::L_FN as i64),
    n("RCTR", spcl::RCTR as i64),
    n("RBT", spcl::RBT as i64),
    n("F_ACC", spcl::F_ACC as i64),
    n("L_ACC", spcl::L_ACC as i64),
    n("PASTE", spcl::PASTE as i64),
    n("NOKEY", NOKEY as i64),
    n("FKEY", FKEY as i64),
    n("MKEY", MKEY as i64),
    n("BKEY", BKEY as i64),
    n("SPCLKEY", SPCLKEY as i64),
    n("RELKEY", RELKEY as i64),
    n("ERRKEY", ERRKEY as i64),
    c("KDGKBMODE", KDGKBMODE),
    c("KDSKBMODE", KDSKBMODE),
    c("KDMKTONE", KDMKTONE),
    c("KDGKBSTATE", KDGKBSTATE),
    c("KDSKBSTATE", KDSKBSTATE),
    c("KDGKBTYPE", KDGKBTYPE),
    c("KDGETLED", KDGETLED),
    c("KDSETLED", KDSETLED),
    c("KDSETRAD", KDSETRAD),
    c("KBADDKBD", KBADDKBD),
    c("KDGETMODE", KDGETMODE),
    c("KDSETMODE", KDSETMODE),
    c("KDSBORDER", KDSBORDER),
    c("KDENABIO", KDENABIO),
    c("KDDISABIO", KDDISABIO),
    c("KIOCSOUND", KIOCSOUND),
    c("KBRELKBD", KBRELKBD),
    c("KDGKBINFO", KDGKBINFO),
    c("KDSETREPEAT", KDSETREPEAT),
    c("KDGETREPEAT", KDGETREPEAT),
    c("GETFKEY", GETFKEY),
    c("SETFKEY", SETFKEY),
    c("GIO_KEYMAP", GIO_KEYMAP),
    c("PIO_KEYMAP", PIO_KEYMAP),
    c("GIO_DEADKEYMAP", GIO_DEADKEYMAP),
    c("PIO_DEADKEYMAP", PIO_DEADKEYMAP),
    c("GIO_KEYMAPENT", GIO_KEYMAPENT),
    c("PIO_KEYMAPENT", PIO_KEYMAPENT),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/kbio.h",
        value: value as i64,
        arches: Arches::All,
    }
}

const fn n(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/kbio.h",
        value,
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_the_c_layouts() {
        assert_eq!(size_of::<KeyboardInfo>(), 36);
        assert_eq!(size_of::<KeyEnt>(), KeyEnt::SIZE);
        assert_eq!(size_of::<Keymap>(), Keymap::SIZE);
        assert_eq!(size_of::<AccT>(), AccT::SIZE);
        assert_eq!(size_of::<AccentMap>(), AccentMap::SIZE);
        assert_eq!(size_of::<KeyArg>(), 40);
        assert_eq!(size_of::<FkeyArg>(), 20);
    }

    #[test]
    fn entries_round_trip() {
        let k = KeyEnt {
            map: [1, 2, 3, 4, 5, 6, 7, 0x00ff_ffff],
            spcl: 0x33,
            flgs: FLAG_LOCK_C,
        };
        let mut b = [0u8; KeyEnt::SIZE];
        k.write(&mut b);
        assert_eq!(KeyEnt::read(&b), Some(k));
        let mut a = AccT::EMPTY;
        a.accchar = u32::from(b'`');
        a.map[0] = [u32::from(b'a'), 0xe0];
        let mut b = [0u8; AccT::SIZE];
        a.write(&mut b);
        assert_eq!(AccT::read(&b), Some(a));
    }
}
