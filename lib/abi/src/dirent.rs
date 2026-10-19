// SPDX-License-Identifier: BSD-2-Clause
//! Directory entries as `getdirentries` returns them (`sys/dirent.h`).
//!
//! `getdirentries` packs records of varying length: the fields before
//! `d_name` ([`NAME_OFFSET`] bytes), the name, a NUL, and padding to 8
//! bytes. `d_reclen` is the whole record ([`reclen`]).

use crate::layout::{Arches, ConstValue};

/// `struct dirent`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dirent {
    pub d_fileno: u64,
    /// The directory offset of the next entry.
    pub d_off: i64,
    pub d_reclen: u16,
    pub d_type: u8,
    pub d_pad0: u8,
    pub d_namlen: u16,
    pub d_pad1: u16,
    pub d_name: [u8; MAXNAMLEN + 1],
}

/// Where `d_name` starts: the length of a record's fixed part.
pub const NAME_OFFSET: usize = core::mem::offset_of!(Dirent, d_name);

/// The longest name (`MAXNAMLEN`).
pub const MAXNAMLEN: usize = 255;

pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;
pub const DT_WHT: u8 = 14;

/// A record's length for a name of `namlen` bytes (`_GENERIC_DIRLEN`).
#[must_use]
pub const fn reclen(namlen: usize) -> usize {
    (NAME_OFFSET + namlen + 1).next_multiple_of(8)
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("MAXNAMLEN", MAXNAMLEN as i64),
    c("DT_UNKNOWN", DT_UNKNOWN as i64),
    c("DT_FIFO", DT_FIFO as i64),
    c("DT_CHR", DT_CHR as i64),
    c("DT_DIR", DT_DIR as i64),
    c("DT_BLK", DT_BLK as i64),
    c("DT_REG", DT_REG as i64),
    c("DT_LNK", DT_LNK as i64),
    c("DT_SOCK", DT_SOCK as i64),
    c("DT_WHT", DT_WHT as i64),
    c("_GENERIC_DIRLEN(1)", reclen(1) as i64),
    c("_GENERIC_DIRLEN(255)", reclen(255) as i64),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "dirent.h",
        value,
        arches: Arches::All,
    }
}
