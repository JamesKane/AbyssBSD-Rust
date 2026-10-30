// SPDX-License-Identifier: BSD-2-Clause
//! The kernel linker's and module system's records (`sys/linker.h`,
//! `sys/module.h`): what `kldstat(2)` and `modstat(2)` fill in.

use crate::layout::{Arches, ConstValue};

/// `MAXPATHLEN`.
pub const MAXPATHLEN: usize = 1024;
/// `MAXMODNAME` (`MAXMODNAMEV3`), and the first two versions' name
/// length, `MAXMODNAMEV1V2`.
pub const MAXMODNAME: usize = MAXPATHLEN;
pub const MAXMODNAMEV1V2: usize = 32;

/// `struct kld_file_stat`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct KldFileStat {
    pub version: i32,
    pub name: [u8; MAXPATHLEN],
    pub refs: i32,
    pub id: i32,
    /// Alignment for `address`.
    pub _pad0: [u8; 4],
    pub address: u64,
    pub size: u64,
    pub pathname: [u8; MAXPATHLEN],
}

/// `sizeof(struct kld_file_stat_1)`: the record before `pathname`.
pub const KLD_FILE_STAT_1_SIZE: usize = core::mem::offset_of!(KldFileStat, pathname);

/// `struct module_stat`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ModuleStat {
    pub version: i32,
    pub name: [u8; MAXMODNAME],
    pub refs: i32,
    pub id: i32,
    /// Alignment for `data`.
    pub _pad0: [u8; 4],
    /// `modspecific_t`, a union of `int`, `u_int`, `long`, and `u_long`.
    pub data: u64,
}

/// `struct module_stat_v2`, the record before names grew to
/// `MAXPATHLEN` (`kern_module.c`'s own, in no header).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ModuleStatV2 {
    pub version: i32,
    pub name: [u8; MAXMODNAMEV1V2],
    pub refs: i32,
    pub id: i32,
    /// Alignment for `data`.
    pub _pad0: [u8; 4],
    pub data: u64,
}

/// `sizeof(struct module_stat_v1)` (the second version's record before
/// `data`) and `sizeof(struct module_stat_v2)`.
pub const MODULE_STAT_V1_SIZE: usize = core::mem::offset_of!(ModuleStatV2, id) + 4;
pub const MODULE_STAT_V2_SIZE: usize = size_of::<ModuleStatV2>();
const _: () = assert!(MODULE_STAT_V1_SIZE == 44 && MODULE_STAT_V2_SIZE == 56);

const fn c(c_name: &'static str, value: usize) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/module.h",
        value: value as i64,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("MAXMODNAME", MAXMODNAME),
    c("MAXMODNAMEV1V2", MAXMODNAMEV1V2),
];
