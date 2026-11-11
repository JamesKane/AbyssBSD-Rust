// SPDX-License-Identifier: BSD-2-Clause
//! Device statistics (`sys/devicestat.h`): `struct devstat`, which
//! `kern.devstat.all` returns and `/dev/devstat`'s pages hold, read by
//! `devstat(3)` (`iostat`, `systat`) and `libgeom`'s `geom_stats_*`
//! (`gstat`).

use crate::layout::{Arches, ConstValue};

/// `struct bintime`: seconds and a binary fraction of one.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bintime {
    pub sec: i64,
    pub frac: u64,
}

impl Bintime {
    /// `ns` nanoseconds.
    #[must_use]
    pub fn from_ns(ns: u64) -> Self {
        let rem = u128::from(ns % 1_000_000_000);
        Self {
            sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
            // 2^64 / 10^9 per nanosecond, as `nstobt` scales.
            frac: u64::try_from((rem << 64) / 1_000_000_000).unwrap_or(u64::MAX),
        }
    }
}

/// `DEVSTAT_VERSION`.
pub const VERSION: i32 = 6;
/// `DEVSTAT_NAME_LEN`.
pub const NAME_LEN: usize = 16;
/// `DEVSTAT_N_TRANS_FLAGS`.
pub const N_TRANS_FLAGS: usize = 4;
/// `DEVSTAT_DEVICE_NAME`: `/dev/devstat`.
pub const DEVICE_NAME: &str = "devstat";

/// `devstat_trans_flags`.
pub mod trans {
    pub const NO_DATA: usize = 0x00;
    pub const READ: usize = 0x01;
    pub const WRITE: usize = 0x02;
    pub const FREE: usize = 0x03;
}

/// `devstat_tag_type`.
pub mod tag {
    pub const SIMPLE: usize = 0x00;
    pub const ORDERED: usize = 0x02;
}

/// `devstat_type_flags`.
pub mod ty {
    pub const DIRECT: u32 = 0x000;
    pub const IF_SCSI: u32 = 0x010;
    pub const IF_IDE: u32 = 0x020;
    pub const IF_OTHER: u32 = 0x030;
    pub const IF_NVME: u32 = 0x040;
    pub const PASS: u32 = 0x100;
}

/// `devstat_priority`.
pub mod priority {
    pub const DISK: u32 = 0x110;
    pub const MAX: u32 = 0xfff;
}

/// `devstat_support_flags`: `DEVSTAT_ALL_SUPPORTED`.
pub const ALL_SUPPORTED: u32 = 0;

/// `struct devstat`, with its padding spelled out.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Devstat {
    pub sequence0: u32,
    pub allocated: i32,
    pub start_count: u32,
    pub end_count: u32,
    pub busy_from: Bintime,
    /// `dev_links` (`STAILQ_ENTRY`): a kernel pointer.
    pub dev_links: u64,
    pub device_number: u32,
    pub device_name: [u8; NAME_LEN],
    pub unit_number: i32,
    pub bytes: [u64; N_TRANS_FLAGS],
    pub operations: [u64; N_TRANS_FLAGS],
    pub duration: [Bintime; N_TRANS_FLAGS],
    pub busy_time: Bintime,
    pub creation_time: Bintime,
    pub block_size: u32,
    pub pad0: u32,
    pub tag_types: [u64; 3],
    pub flags: u32,
    pub device_type: u32,
    pub priority: u32,
    pub pad1: u32,
    /// `id`: for GEOM's entries, the provider (its id in
    /// `kern.geom.confxml`).
    pub id: u64,
    pub sequence1: u32,
    pub pad2: u32,
}

/// `sizeof(struct devstat)` (`debug.sizeof.devstat`).
pub const SIZE: usize = 288;
const _: () = assert!(size_of::<Devstat>() == SIZE);

pub const CONSTANTS: &[ConstValue] = &[
    c("DEVSTAT_VERSION", VERSION as i64),
    c("DEVSTAT_NAME_LEN", NAME_LEN as i64),
    c("DEVSTAT_N_TRANS_FLAGS", N_TRANS_FLAGS as i64),
    c("DEVSTAT_READ", trans::READ as i64),
    c("DEVSTAT_WRITE", trans::WRITE as i64),
    c("DEVSTAT_FREE", trans::FREE as i64),
    c("DEVSTAT_NO_DATA", trans::NO_DATA as i64),
    c("DEVSTAT_TAG_SIMPLE", tag::SIMPLE as i64),
    c("DEVSTAT_TAG_ORDERED", tag::ORDERED as i64),
    c("DEVSTAT_TYPE_DIRECT", ty::DIRECT as i64),
    c("DEVSTAT_TYPE_IF_SCSI", ty::IF_SCSI as i64),
    c("DEVSTAT_TYPE_IF_IDE", ty::IF_IDE as i64),
    c("DEVSTAT_TYPE_IF_OTHER", ty::IF_OTHER as i64),
    c("DEVSTAT_TYPE_IF_NVME", ty::IF_NVME as i64),
    c("DEVSTAT_TYPE_PASS", ty::PASS as i64),
    c("DEVSTAT_PRIORITY_DISK", priority::DISK as i64),
    c("DEVSTAT_PRIORITY_MAX", priority::MAX as i64),
    c("DEVSTAT_ALL_SUPPORTED", ALL_SUPPORTED as i64),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/devicestat.h",
        value,
        arches: Arches::All,
    }
}
