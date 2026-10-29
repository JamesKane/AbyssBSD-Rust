// SPDX-License-Identifier: BSD-2-Clause
//! devfs rules (`fs/devfs/devfs.h`): `struct devfs_rule` and the
//! `DEVFSIO_*` requests `devfs(8)` makes on a devfs directory.
//!
//! A rule's id is its ruleset's number in the high 16 bits and its own
//! number in the low 16 ([`mkrid`]); ruleset 0 means the mount's current
//! one, rule number 0 "the next free one".

use crate::ioctl::{IOC_IN, IOC_OUT};
use crate::layout::{Arches, ConstValue};

/// `DEVFS_MAGIC`: the version a rule must carry.
pub const DEVFS_MAGIC: u32 = 0xdb0a_087a;
/// `DEVFS_MAXPTRNLEN`: the path pattern's room, its NUL included.
pub const DEVFS_MAXPTRNLEN: usize = 200;

/// `dr_icond`: match `dr_dswflags` against the device's `d_flags`.
pub const DRC_DSWFLAGS: i32 = 0x001;
/// `dr_icond`: match `dr_pathptrn` against the node's path.
pub const DRC_PATHPTRN: i32 = 0x002;
/// `dr_iacts`: do `dr_bacts`.
pub const DRA_BACTS: i32 = 0x001;
/// `dr_iacts`: set the owner.
pub const DRA_UID: i32 = 0x002;
/// `dr_iacts`: set the group.
pub const DRA_GID: i32 = 0x004;
/// `dr_iacts`: set the mode.
pub const DRA_MODE: i32 = 0x008;
/// `dr_iacts`: run ruleset `dr_incset`.
pub const DRA_INCSET: i32 = 0x010;
/// `dr_bacts`: hide the node.
pub const DRB_HIDE: i32 = 0x001;
/// `dr_bacts`: show it again.
pub const DRB_UNHIDE: i32 = 0x002;

/// `cdevsw` `d_flags` a rule may match (`sys/conf.h`'s `D_TYPEMASK`).
pub const D_TAPE: i32 = 0x0001;
pub const D_DISK: i32 = 0x0002;
pub const D_TTY: i32 = 0x0004;
pub const D_MEM: i32 = 0x0008;

/// A rule id from a ruleset and rule number (`mkrid`).
#[must_use]
pub const fn mkrid(rsnum: u16, rnum: u16) -> u32 {
    (rsnum as u32) << 16 | rnum as u32
}

/// The ruleset of a rule id (`rid2rsn`).
#[must_use]
pub const fn rid2rsn(rid: u32) -> u16 {
    (rid >> 16) as u16
}

/// The rule number of a rule id (`rid2rn`).
#[must_use]
pub const fn rid2rn(rid: u32) -> u16 {
    (rid & 0xffff) as u16
}

/// `struct devfs_rule`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevfsRule {
    pub dr_magic: u32,
    pub dr_id: u32,
    pub dr_icond: i32,
    pub dr_dswflags: i32,
    pub dr_pathptrn: [u8; DEVFS_MAXPTRNLEN],
    pub dr_iacts: i32,
    pub dr_bacts: i32,
    pub dr_uid: u32,
    pub dr_gid: u32,
    pub dr_mode: u16,
    pub dr_incset: u16,
}

impl Default for DevfsRule {
    fn default() -> Self {
        Self {
            dr_magic: DEVFS_MAGIC,
            dr_id: 0,
            dr_icond: 0,
            dr_dswflags: 0,
            dr_pathptrn: [0; DEVFS_MAXPTRNLEN],
            dr_iacts: 0,
            dr_bacts: 0,
            dr_uid: 0,
            dr_gid: 0,
            dr_mode: 0,
            dr_incset: 0,
        }
    }
}

/// `sizeof(struct devfs_rule)`, which the requests encode.
pub const DEVFS_RULE_LEN: u32 = 236;
const _: () = assert!(size_of::<DevfsRule>() == DEVFS_RULE_LEN as usize);

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .and_then(|c| c.try_into().ok())
        .map_or(0, u32::from_ne_bytes)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .and_then(|c| c.try_into().ok())
        .map_or(0, u16::from_ne_bytes)
}

impl DevfsRule {
    /// The rule in a request's argument, if it is one's size.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        use core::mem::offset_of;
        if b.len() != size_of::<Self>() {
            return None;
        }
        let mut pathptrn = [0; DEVFS_MAXPTRNLEN];
        let at = offset_of!(Self, dr_pathptrn);
        pathptrn.copy_from_slice(&b[at..at + DEVFS_MAXPTRNLEN]);
        Some(Self {
            dr_magic: u32_at(b, offset_of!(Self, dr_magic)),
            dr_id: u32_at(b, offset_of!(Self, dr_id)),
            dr_icond: u32_at(b, offset_of!(Self, dr_icond)).cast_signed(),
            dr_dswflags: u32_at(b, offset_of!(Self, dr_dswflags)).cast_signed(),
            dr_pathptrn: pathptrn,
            dr_iacts: u32_at(b, offset_of!(Self, dr_iacts)).cast_signed(),
            dr_bacts: u32_at(b, offset_of!(Self, dr_bacts)).cast_signed(),
            dr_uid: u32_at(b, offset_of!(Self, dr_uid)),
            dr_gid: u32_at(b, offset_of!(Self, dr_gid)),
            dr_mode: u16_at(b, offset_of!(Self, dr_mode)),
            dr_incset: u16_at(b, offset_of!(Self, dr_incset)),
        })
    }

    /// The rule as a request's argument: `out` must be its size.
    pub fn to_bytes(&self, out: &mut [u8]) {
        use core::mem::offset_of;
        let mut put = |at: usize, v: &[u8]| {
            if let Some(o) = out.get_mut(at..at + v.len()) {
                o.copy_from_slice(v);
            }
        };
        put(offset_of!(Self, dr_magic), &self.dr_magic.to_ne_bytes());
        put(offset_of!(Self, dr_id), &self.dr_id.to_ne_bytes());
        put(offset_of!(Self, dr_icond), &self.dr_icond.to_ne_bytes());
        put(
            offset_of!(Self, dr_dswflags),
            &self.dr_dswflags.to_ne_bytes(),
        );
        put(offset_of!(Self, dr_pathptrn), &self.dr_pathptrn);
        put(offset_of!(Self, dr_iacts), &self.dr_iacts.to_ne_bytes());
        put(offset_of!(Self, dr_bacts), &self.dr_bacts.to_ne_bytes());
        put(offset_of!(Self, dr_uid), &self.dr_uid.to_ne_bytes());
        put(offset_of!(Self, dr_gid), &self.dr_gid.to_ne_bytes());
        put(offset_of!(Self, dr_mode), &self.dr_mode.to_ne_bytes());
        put(offset_of!(Self, dr_incset), &self.dr_incset.to_ne_bytes());
    }

    /// The path pattern, up to its NUL.
    #[must_use]
    pub fn pattern(&self) -> &[u8] {
        let n = self
            .dr_pathptrn
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(DEVFS_MAXPTRNLEN);
        &self.dr_pathptrn[..n]
    }
}

const fn ioc(dir: u32, num: u32, len: u32) -> u32 {
    dir | (len << 16) | (b'D' as u32) << 8 | num
}

pub const DEVFSIO_RADD: u32 = ioc(IOC_IN | IOC_OUT, 0, DEVFS_RULE_LEN);
pub const DEVFSIO_RDEL: u32 = ioc(IOC_IN, 1, 4);
pub const DEVFSIO_RAPPLY: u32 = ioc(IOC_IN, 2, DEVFS_RULE_LEN);
pub const DEVFSIO_RAPPLYID: u32 = ioc(IOC_IN, 3, 4);
pub const DEVFSIO_RGETNEXT: u32 = ioc(IOC_IN | IOC_OUT, 4, DEVFS_RULE_LEN);
pub const DEVFSIO_SUSE: u32 = ioc(IOC_IN, 10, 2);
pub const DEVFSIO_SAPPLY: u32 = ioc(IOC_IN, 11, 2);
pub const DEVFSIO_SGETNEXT: u32 = ioc(IOC_IN | IOC_OUT, 12, 2);

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("DEVFS_MAGIC", DEVFS_MAGIC as i64),
    c("DEVFS_MAXPTRNLEN", DEVFS_MAXPTRNLEN as i64),
    c("DRC_DSWFLAGS", DRC_DSWFLAGS as i64),
    c("DRC_PATHPTRN", DRC_PATHPTRN as i64),
    c("DRA_BACTS", DRA_BACTS as i64),
    c("DRA_UID", DRA_UID as i64),
    c("DRA_GID", DRA_GID as i64),
    c("DRA_MODE", DRA_MODE as i64),
    c("DRA_INCSET", DRA_INCSET as i64),
    c("DRB_HIDE", DRB_HIDE as i64),
    c("DRB_UNHIDE", DRB_UNHIDE as i64),
    c("DEVFSIO_RADD", DEVFSIO_RADD as i64),
    c("DEVFSIO_RDEL", DEVFSIO_RDEL as i64),
    c("DEVFSIO_RAPPLY", DEVFSIO_RAPPLY as i64),
    c("DEVFSIO_RAPPLYID", DEVFSIO_RAPPLYID as i64),
    c("DEVFSIO_RGETNEXT", DEVFSIO_RGETNEXT as i64),
    c("DEVFSIO_SUSE", DEVFSIO_SUSE as i64),
    c("DEVFSIO_SAPPLY", DEVFSIO_SAPPLY as i64),
    c("DEVFSIO_SGETNEXT", DEVFSIO_SGETNEXT as i64),
    ConstValue {
        c_name: "D_TAPE",
        header: "sys/conf.h",
        value: D_TAPE as i64,
        arches: Arches::All,
    },
    ConstValue {
        c_name: "D_DISK",
        header: "sys/conf.h",
        value: D_DISK as i64,
        arches: Arches::All,
    },
    ConstValue {
        c_name: "D_TTY",
        header: "sys/conf.h",
        value: D_TTY as i64,
        arches: Arches::All,
    },
    ConstValue {
        c_name: "D_MEM",
        header: "sys/conf.h",
        value: D_MEM as i64,
        arches: Arches::All,
    },
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "fs/devfs/devfs.h",
        value,
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut r = DevfsRule {
            dr_id: mkrid(4, 300),
            dr_icond: DRC_PATHPTRN,
            dr_iacts: DRA_BACTS | DRA_MODE,
            dr_bacts: DRB_UNHIDE,
            dr_mode: 0o660,
            dr_incset: 7,
            dr_uid: 5,
            ..DevfsRule::default()
        };
        r.dr_pathptrn[..5].copy_from_slice(b"pts/*");
        let mut b = [0u8; DEVFS_RULE_LEN as usize];
        r.to_bytes(&mut b);
        assert_eq!(DevfsRule::from_bytes(&b), Some(r));
        assert_eq!(r.pattern(), b"pts/*");
        assert_eq!((rid2rsn(r.dr_id), rid2rn(r.dr_id)), (4, 300));
        assert_eq!(DevfsRule::from_bytes(&b[1..]), None);
    }
}
