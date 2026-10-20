// SPDX-License-Identifier: BSD-2-Clause
//! Memory disks (`sys/mdioctl.h`): the requests `mdconfig` sends to
//! `/dev/mdctl`.

use crate::ioctl::{IOC_IN, IOC_OUT};
use crate::layout::{Arches, ConstValue};

/// `struct md_ioctl`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MdIoctl {
    pub md_version: u32,
    pub md_unit: u32,
    /// An `enum md_types`.
    pub md_type: u32,
    /// The backing file's path (`MD_VNODE`), a user pointer.
    pub md_file: u64,
    pub md_mediasize: i64,
    pub md_sectorsize: u32,
    pub md_options: u32,
    pub md_base: u64,
    pub md_fwheads: i32,
    pub md_fwsectors: i32,
    /// A label, a user pointer.
    pub md_label: u64,
    pub md_pad: [i32; MDNPAD],
}

pub const MDNPAD: usize = 96;
const _: () = assert!(MDNPAD == 96);
/// The one layout version (`MDIOVERSION`).
pub const MDIOVERSION: u32 = 0;

/// `enum md_types`.
pub const MD_MALLOC: u32 = 0;
pub const MD_PRELOAD: u32 = 1;
pub const MD_VNODE: u32 = 2;
pub const MD_SWAP: u32 = 3;
pub const MD_NULL: u32 = 4;

/// `md_options`.
pub const MD_CLUSTER: u32 = 0x01;
pub const MD_RESERVE: u32 = 0x02;
pub const MD_AUTOUNIT: u32 = 0x04;
pub const MD_READONLY: u32 = 0x08;
pub const MD_COMPRESS: u32 = 0x10;
pub const MD_FORCE: u32 = 0x20;
pub const MD_ASYNC: u32 = 0x40;
pub const MD_VERIFY: u32 = 0x80;
pub const MD_CACHE: u32 = 0x100;
pub const MD_MUSTDEALLOC: u32 = 0x200;

/// `sizeof(struct md_ioctl)`, which the requests encode.
const LEN: u32 = 448;
const _: () = assert!(size_of::<MdIoctl>() == LEN as usize);
const fn iowr(num: u32) -> u32 {
    IOC_IN | IOC_OUT | (LEN << 16) | (b'm' as u32) << 8 | num
}
pub const MDIOCATTACH: u32 = iowr(0);
pub const MDIOCDETACH: u32 = iowr(1);
pub const MDIOCQUERY: u32 = iowr(2);
pub const MDIOCRESIZE: u32 = iowr(4);

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("MDNPAD", 96),
    c("MDIOVERSION", MDIOVERSION),
    c("MD_MALLOC", MD_MALLOC),
    c("MD_PRELOAD", MD_PRELOAD),
    c("MD_VNODE", MD_VNODE),
    c("MD_SWAP", MD_SWAP),
    c("MD_NULL", MD_NULL),
    c("MD_CLUSTER", MD_CLUSTER),
    c("MD_RESERVE", MD_RESERVE),
    c("MD_AUTOUNIT", MD_AUTOUNIT),
    c("MD_READONLY", MD_READONLY),
    c("MD_COMPRESS", MD_COMPRESS),
    c("MD_FORCE", MD_FORCE),
    c("MD_ASYNC", MD_ASYNC),
    c("MD_VERIFY", MD_VERIFY),
    c("MD_CACHE", MD_CACHE),
    c("MD_MUSTDEALLOC", MD_MUSTDEALLOC),
    c("MDIOCATTACH", MDIOCATTACH),
    c("MDIOCDETACH", MDIOCDETACH),
    c("MDIOCQUERY", MDIOCQUERY),
    c("MDIOCRESIZE", MDIOCRESIZE),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/mdioctl.h",
        value: value as i64,
        arches: Arches::All,
    }
}
