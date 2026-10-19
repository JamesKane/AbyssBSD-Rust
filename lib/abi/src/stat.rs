// SPDX-License-Identifier: BSD-2-Clause
//! File status. Behavior reference: sys/sys/stat.h (the 64-bit layout, without
//! i386's `__STAT_TIME_T_EXT` padding).

use crate::layout::{Arches, ConstValue};
use crate::time::Timespec;

/// `dev_t`.
pub type DevT = u64;
/// `ino_t`.
pub type InoT = u64;
/// `nlink_t`.
pub type NlinkT = u64;
/// `mode_t`.
pub type ModeT = u16;
/// `uid_t`.
pub type UidT = u32;
/// `gid_t`.
pub type GidT = u32;
/// `off_t`.
pub type OffT = i64;
/// `blkcnt_t`.
pub type BlkcntT = i64;
/// `blksize_t`.
pub type BlksizeT = i32;
/// `fflags_t`.
pub type FflagsT = u32;

/// `struct stat`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub st_dev: DevT,
    pub st_ino: InoT,
    pub st_nlink: NlinkT,
    pub st_mode: ModeT,
    pub st_bsdflags: i16,
    pub st_uid: UidT,
    pub st_gid: GidT,
    pub st_padding1: i32,
    pub st_rdev: DevT,
    pub st_atim: Timespec,
    pub st_mtim: Timespec,
    pub st_ctim: Timespec,
    pub st_birthtim: Timespec,
    pub st_size: OffT,
    pub st_blocks: BlkcntT,
    pub st_blksize: BlksizeT,
    pub st_flags: FflagsT,
    pub st_gen: u64,
    pub st_filerev: u64,
    pub st_spare: [u64; 9],
}

/// The file type bits of `st_mode`, and the types.
pub const S_IFMT: u16 = 0o170_000;
pub const S_IFIFO: u16 = 0o010_000;
pub const S_IFCHR: u16 = 0o020_000;
pub const S_IFDIR: u16 = 0o040_000;
pub const S_IFBLK: u16 = 0o060_000;
pub const S_IFREG: u16 = 0o100_000;
pub const S_IFLNK: u16 = 0o120_000;
pub const S_IFSOCK: u16 = 0o140_000;
/// The permission bits, set-id and sticky bits included (`ALLPERMS`).
pub const ALLPERMS: u16 = 0o7777;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("S_IFMT", S_IFMT),
    c("S_IFIFO", S_IFIFO),
    c("S_IFCHR", S_IFCHR),
    c("S_IFDIR", S_IFDIR),
    c("S_IFBLK", S_IFBLK),
    c("S_IFREG", S_IFREG),
    c("S_IFLNK", S_IFLNK),
    c("S_IFSOCK", S_IFSOCK),
    c("ALLPERMS", ALLPERMS),
];

const fn c(c_name: &'static str, value: u16) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/stat.h",
        value: value as i64,
        arches: Arches::All,
    }
}
