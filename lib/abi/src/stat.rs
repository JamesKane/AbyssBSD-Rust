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
pub const S_ISUID: u16 = 0o4000;
pub const S_ISGID: u16 = 0o2000;
/// On a directory: only an entry's owner (or the directory's) may remove
/// or rename it.
pub const S_ISVTX: u16 = 0o1000;

/// `chflags` flags: the owner may set these.
pub const UF_SETTABLE: u32 = 0x0000_ffff;
pub const UF_NODUMP: u32 = 0x0000_0001;
pub const UF_IMMUTABLE: u32 = 0x0000_0002;
pub const UF_APPEND: u32 = 0x0000_0004;
pub const UF_OPAQUE: u32 = 0x0000_0008;
pub const UF_NOUNLINK: u32 = 0x0000_0010;
/// The DOS and Windows attributes (msdosfs keeps them).
pub const UF_SYSTEM: u32 = 0x0000_0080;
pub const UF_ARCHIVE: u32 = 0x0000_0800;
pub const UF_READONLY: u32 = 0x0000_1000;
pub const UF_HIDDEN: u32 = 0x0000_8000;
/// Only the superuser may set these.
pub const SF_SETTABLE: u32 = 0xffff_0000;
pub const SF_ARCHIVED: u32 = 0x0001_0000;
pub const SF_IMMUTABLE: u32 = 0x0002_0000;
pub const SF_APPEND: u32 = 0x0004_0000;
pub const SF_NOUNLINK: u32 = 0x0010_0000;
pub const SF_SNAPSHOT: u32 = 0x0020_0000;
/// Flags that forbid changing a file.
pub const IMMUTABLE: u32 = UF_IMMUTABLE | SF_IMMUTABLE;
/// Flags that allow only appending.
pub const APPEND: u32 = UF_APPEND | SF_APPEND;
/// Flags that forbid removing or renaming a file.
pub const NOUNLINK: u32 = UF_NOUNLINK | SF_NOUNLINK;

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
    c("S_ISUID", S_ISUID),
    c("S_ISGID", S_ISGID),
    c("S_ISVTX", S_ISVTX),
    f("UF_SETTABLE", UF_SETTABLE),
    f("UF_NODUMP", UF_NODUMP),
    f("UF_IMMUTABLE", UF_IMMUTABLE),
    f("UF_APPEND", UF_APPEND),
    f("UF_OPAQUE", UF_OPAQUE),
    f("UF_NOUNLINK", UF_NOUNLINK),
    f("UF_SYSTEM", UF_SYSTEM),
    f("UF_ARCHIVE", UF_ARCHIVE),
    f("UF_READONLY", UF_READONLY),
    f("UF_HIDDEN", UF_HIDDEN),
    f("SF_SETTABLE", SF_SETTABLE),
    f("SF_ARCHIVED", SF_ARCHIVED),
    f("SF_IMMUTABLE", SF_IMMUTABLE),
    f("SF_APPEND", SF_APPEND),
    f("SF_NOUNLINK", SF_NOUNLINK),
    f("SF_SNAPSHOT", SF_SNAPSHOT),
];

const fn f(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/stat.h",
        value: value as i64,
        arches: Arches::All,
    }
}

const fn c(c_name: &'static str, value: u16) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/stat.h",
        value: value as i64,
        arches: Arches::All,
    }
}
