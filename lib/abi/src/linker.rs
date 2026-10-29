// SPDX-License-Identifier: BSD-2-Clause
//! The kernel linker's and module system's records (`sys/linker.h`,
//! `sys/module.h`): what `kldstat(2)` and `modstat(2)` fill in.

use crate::layout::{Arches, ConstValue};

/// `MAXPATHLEN`.
pub const MAXPATHLEN: usize = 1024;
/// `MAXMODNAME`.
pub const MAXMODNAME: usize = 32;

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

/// `sizeof(struct module_stat_v1)`: the record before `data`.
pub const MODULE_STAT_V1_SIZE: usize = core::mem::offset_of!(ModuleStat, id) + 4;

pub(crate) const CONSTANTS: &[ConstValue] = &[ConstValue {
    c_name: "MAXMODNAME",
    header: "sys/module.h",
    value: MAXMODNAME as i64,
    arches: Arches::All,
}];
