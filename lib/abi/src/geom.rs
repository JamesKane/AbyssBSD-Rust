// SPDX-License-Identifier: BSD-2-Clause
//! GEOM's control requests (`geom/geom_ctl.h`): what libgeom's
//! `gctl_issue` sends to `/dev/geom.ctl`.

use crate::ioctl::IOC_IN;
use crate::layout::{Arches, ConstValue};

/// The request layout's version (`GCTL_VERSION`).
pub const GCTL_VERSION: u32 = 2;

/// `struct gctl_req_arg`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GctlReqArg {
    /// The name's length, its NUL included.
    pub nlen: u32,
    /// The name, a user pointer.
    pub name: u64,
    pub offset: i64,
    /// `GCTL_PARAM_*`.
    pub flag: i32,
    /// The value's length.
    pub len: i32,
    /// The value, a user pointer.
    pub value: u64,
    /// The kernel's copy (kernel only).
    pub kvalue: u64,
}

/// `struct gctl_req`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GctlReq {
    pub version: u32,
    pub serial: u32,
    pub narg: u32,
    /// The arguments, a user pointer to `narg` of them.
    pub arg: u64,
    /// The error buffer's length.
    pub lerror: u32,
    /// The error buffer, a user pointer.
    pub error: u64,
    pub reqt: u64,
    /// Kernel only.
    pub nerror: i32,
    /// Kernel only.
    pub serror: u64,
}

/// `GCTL_PARAM_*`.
pub const GCTL_PARAM_RD: i32 = 1;
pub const GCTL_PARAM_WR: i32 = 2;
pub const GCTL_PARAM_ASCII: i32 = 4;

/// The most arguments a request may have (`GEOM_CTL_ARG_MAX`).
pub const GEOM_CTL_ARG_MAX: u32 = 2048;

/// `sizeof(struct gctl_req)`.
const LEN: u32 = 64;
const _: () = assert!(size_of::<GctlReq>() == LEN as usize);
const _: () = assert!(size_of::<GctlReqArg>() == 48);

/// `_IOW('G', GCTL_VERSION, struct gctl_req)`.
pub const GEOM_CTL: u32 = IOC_IN | (LEN << 16) | (b'G' as u32) << 8 | GCTL_VERSION;

/// The node, under `/dev` (`PATH_GEOM_CTL`).
pub const PATH_GEOM_CTL: &str = "geom.ctl";

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("GCTL_VERSION", GCTL_VERSION as i64),
    c("GCTL_PARAM_RD", GCTL_PARAM_RD as i64),
    c("GCTL_PARAM_WR", GCTL_PARAM_WR as i64),
    c("GCTL_PARAM_ASCII", GCTL_PARAM_ASCII as i64),
    c("GEOM_CTL_ARG_MAX", GEOM_CTL_ARG_MAX as i64),
    c("GEOM_CTL", GEOM_CTL as i64),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "geom/geom_ctl.h",
        value,
        arches: Arches::All,
    }
}
