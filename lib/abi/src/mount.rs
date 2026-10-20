// SPDX-License-Identifier: BSD-2-Clause
//! File system statistics (`sys/mount.h`): `struct statfs` and the mount
//! flags it reports.

use crate::layout::{Arches, ConstValue};

/// `fsid_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fsid {
    pub val: [i32; 2],
}

/// `MFSNAMELEN`: a type name, NUL included.
pub const MFSNAMELEN: usize = 16;
/// `MNAMELEN`: a mount's on and from names, NUL included.
pub const MNAMELEN: usize = 1024;
/// `STATFS_VERSION`.
pub const STATFS_VERSION: u32 = 0x2014_0518;

/// `struct statfs`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Statfs {
    pub f_version: u32,
    pub f_type: u32,
    pub f_flags: u64,
    pub f_bsize: u64,
    pub f_iosize: u64,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: i64,
    pub f_files: u64,
    pub f_ffree: i64,
    pub f_syncwrites: u64,
    pub f_asyncwrites: u64,
    pub f_syncreads: u64,
    pub f_asyncreads: u64,
    pub f_nvnodelistsize: u32,
    pub f_spare0: u32,
    pub f_spare: [u64; 9],
    pub f_namemax: u32,
    pub f_owner: u32,
    pub f_fsid: Fsid,
    pub f_charspare: [u8; 80],
    pub f_fstypename: [u8; MFSNAMELEN],
    pub f_mntfromname: [u8; MNAMELEN],
    pub f_mntonname: [u8; MNAMELEN],
}

impl Default for Statfs {
    fn default() -> Self {
        Self {
            f_version: STATFS_VERSION,
            f_type: 0,
            f_flags: 0,
            f_bsize: 0,
            f_iosize: 0,
            f_blocks: 0,
            f_bfree: 0,
            f_bavail: 0,
            f_files: 0,
            f_ffree: 0,
            f_syncwrites: 0,
            f_asyncwrites: 0,
            f_syncreads: 0,
            f_asyncreads: 0,
            f_nvnodelistsize: 0,
            f_spare0: 0,
            f_spare: [0; 9],
            f_namemax: 0,
            f_owner: 0,
            f_fsid: Fsid::default(),
            f_charspare: [0; 80],
            f_fstypename: [0; MFSNAMELEN],
            f_mntfromname: [0; MNAMELEN],
            f_mntonname: [0; MNAMELEN],
        }
    }
}

pub const MNT_RDONLY: u64 = 0x0000_0001;
pub const MNT_SYNCHRONOUS: u64 = 0x0000_0002;
pub const MNT_NOEXEC: u64 = 0x0000_0004;
pub const MNT_NOSUID: u64 = 0x0000_0008;
pub const MNT_LOCAL: u64 = 0x0000_1000;
pub const MNT_ROOTFS: u64 = 0x0000_4000;
pub const MNT_IGNORE: u64 = 0x0080_0000;
pub const MNT_ASYNC: u64 = 0x0000_0040;
pub const MNT_SUIDDIR: u64 = 0x0010_0000;
pub const MNT_SOFTDEP: u64 = 0x0020_0000;
pub const MNT_NOATIME: u64 = 0x1000_0000;
pub const MNT_NOCLUSTERR: u64 = 0x4000_0000;
pub const MNT_NOCLUSTERW: u64 = 0x8000_0000;
/// `nmount`/`unmount` command flags.
pub const MNT_UPDATE: u64 = 0x0001_0000;
pub const MNT_FORCE: u64 = 0x0008_0000;

/// `getfsstat` modes.
pub const MNT_WAIT: u32 = 1;
pub const MNT_NOWAIT: u32 = 2;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("MFSNAMELEN", MFSNAMELEN as i64),
    c("MNAMELEN", MNAMELEN as i64),
    c("STATFS_VERSION", STATFS_VERSION as i64),
    c("MNT_RDONLY", MNT_RDONLY as i64),
    c("MNT_NOEXEC", MNT_NOEXEC as i64),
    c("MNT_NOSUID", MNT_NOSUID as i64),
    c("MNT_LOCAL", MNT_LOCAL as i64),
    c("MNT_ROOTFS", MNT_ROOTFS as i64),
    c("MNT_IGNORE", MNT_IGNORE as i64),
    c("MNT_SYNCHRONOUS", MNT_SYNCHRONOUS as i64),
    c("MNT_ASYNC", MNT_ASYNC as i64),
    c("MNT_SUIDDIR", MNT_SUIDDIR as i64),
    c("MNT_SOFTDEP", MNT_SOFTDEP as i64),
    c("MNT_NOATIME", MNT_NOATIME as i64),
    c("MNT_NOCLUSTERR", MNT_NOCLUSTERR as i64),
    c("MNT_NOCLUSTERW", MNT_NOCLUSTERW as i64),
    c("MNT_UPDATE", MNT_UPDATE as i64),
    c("MNT_FORCE", MNT_FORCE as i64),
    c("MNT_WAIT", MNT_WAIT as i64),
    c("MNT_NOWAIT", MNT_NOWAIT as i64),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/mount.h",
        value,
        arches: Arches::All,
    }
}
