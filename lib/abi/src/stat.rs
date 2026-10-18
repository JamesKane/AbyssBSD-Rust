// SPDX-License-Identifier: BSD-2-Clause
//! File status. Behavior reference: sys/sys/stat.h (the 64-bit layout, without
//! i386's `__STAT_TIME_T_EXT` padding).

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
