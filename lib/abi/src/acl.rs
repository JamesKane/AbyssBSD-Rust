// SPDX-License-Identifier: BSD-2-Clause
//! Access control lists (`sys/acl.h`): the `__acl_*` system calls'
//! `struct acl`, the old ABI's `struct oldacl`, and their constants.

use crate::layout::{Arches, ConstValue};

/// The most entries a `struct acl` holds (`ACL_MAX_ENTRIES`).
pub const ACL_MAX_ENTRIES: usize = 254;
/// The same, as `acl_maxcnt` holds it.
pub const ACL_MAX_COUNT: u32 = 254;
/// The most a `struct oldacl` holds (`OLDACL_MAX_ENTRIES`).
pub const OLDACL_MAX_ENTRIES: usize = 32;

/// Where UFS keeps each kind (`POSIX1E_ACL_*_EXTATTR_NAME`,
/// `NFS4_ACL_EXTATTR_NAME`), all in the system namespace.
pub const POSIX1E_ACL_ACCESS_EXTATTR_NAME: &[u8] = b"posix1e.acl_access";
pub const POSIX1E_ACL_DEFAULT_EXTATTR_NAME: &[u8] = b"posix1e.acl_default";
pub const NFS4_ACL_EXTATTR_NAME: &[u8] = b"nfs4.acl";

/// Tags (`ACL_*`).
pub const ACL_UNDEFINED_TAG: u32 = 0x00;
pub const ACL_USER_OBJ: u32 = 0x01;
pub const ACL_USER: u32 = 0x02;
pub const ACL_GROUP_OBJ: u32 = 0x04;
pub const ACL_GROUP: u32 = 0x08;
pub const ACL_MASK: u32 = 0x10;
pub const ACL_OTHER: u32 = 0x20;
pub const ACL_EVERYONE: u32 = 0x40;

/// NFSv4 entry types.
pub const ACL_ENTRY_TYPE_ALLOW: u16 = 0x0100;
pub const ACL_ENTRY_TYPE_DENY: u16 = 0x0200;
pub const ACL_ENTRY_TYPE_AUDIT: u16 = 0x0400;
pub const ACL_ENTRY_TYPE_ALARM: u16 = 0x0800;

/// What an `__acl_*` call names (`acl_type_t`).
pub const ACL_TYPE_ACCESS_OLD: u32 = 0;
pub const ACL_TYPE_DEFAULT_OLD: u32 = 1;
pub const ACL_TYPE_ACCESS: u32 = 2;
pub const ACL_TYPE_DEFAULT: u32 = 3;
pub const ACL_TYPE_NFS4: u32 = 4;

/// POSIX.1e permissions.
pub const ACL_EXECUTE: u32 = 0x1;
pub const ACL_WRITE: u32 = 0x2;
pub const ACL_READ: u32 = 0x4;
pub const ACL_PERM_BITS: u32 = ACL_EXECUTE | ACL_WRITE | ACL_READ;

/// NFSv4 permissions.
pub const ACL_READ_DATA: u32 = 0x0000_0008;
pub const ACL_WRITE_DATA: u32 = 0x0000_0010;
pub const ACL_APPEND_DATA: u32 = 0x0000_0020;
pub const ACL_READ_NAMED_ATTRS: u32 = 0x0000_0040;
pub const ACL_WRITE_NAMED_ATTRS: u32 = 0x0000_0080;
pub const ACL_DELETE_CHILD: u32 = 0x0000_0100;
pub const ACL_READ_ATTRIBUTES: u32 = 0x0000_0200;
pub const ACL_WRITE_ATTRIBUTES: u32 = 0x0000_0400;
pub const ACL_DELETE: u32 = 0x0000_0800;
pub const ACL_READ_ACL: u32 = 0x0000_1000;
pub const ACL_WRITE_ACL: u32 = 0x0000_2000;
pub const ACL_WRITE_OWNER: u32 = 0x0000_4000;
pub const ACL_SYNCHRONIZE: u32 = 0x0000_8000;

/// NFSv4 inheritance flags.
pub const ACL_ENTRY_FILE_INHERIT: u16 = 0x0001;
pub const ACL_ENTRY_DIRECTORY_INHERIT: u16 = 0x0002;
pub const ACL_ENTRY_NO_PROPAGATE_INHERIT: u16 = 0x0004;
pub const ACL_ENTRY_INHERIT_ONLY: u16 = 0x0008;
pub const ACL_ENTRY_INHERITED: u16 = 0x0080;

/// `ACL_UNDEFINED_ID`.
pub const ACL_UNDEFINED_ID: u32 = u32::MAX;

/// `struct acl_entry`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct AclEntry {
    pub ae_tag: u32,
    pub ae_id: u32,
    pub ae_perm: u32,
    pub ae_entry_type: u16,
    pub ae_flags: u16,
}

/// `struct acl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Acl {
    pub acl_maxcnt: u32,
    pub acl_cnt: u32,
    pub acl_spare: [i32; 4],
    pub acl_entry: [AclEntry; ACL_MAX_ENTRIES],
}

impl Default for Acl {
    fn default() -> Self {
        Self {
            acl_maxcnt: ACL_MAX_COUNT,
            acl_cnt: 0,
            acl_spare: [0; 4],
            acl_entry: [AclEntry::default(); ACL_MAX_ENTRIES],
        }
    }
}

/// `struct oldacl_entry`: `mode_t` permissions, padded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct OldAclEntry {
    pub ae_tag: i32,
    pub ae_id: u32,
    pub ae_perm: u16,
    pub pad: u16,
}

/// `struct oldacl`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct OldAcl {
    pub acl_cnt: i32,
    pub acl_entry: [OldAclEntry; OLDACL_MAX_ENTRIES],
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("ACL_MAX_ENTRIES", ACL_MAX_ENTRIES as i64),
    c("OLDACL_MAX_ENTRIES", OLDACL_MAX_ENTRIES as i64),
    c("ACL_USER_OBJ", ACL_USER_OBJ as i64),
    c("ACL_USER", ACL_USER as i64),
    c("ACL_GROUP_OBJ", ACL_GROUP_OBJ as i64),
    c("ACL_GROUP", ACL_GROUP as i64),
    c("ACL_MASK", ACL_MASK as i64),
    c("ACL_OTHER", ACL_OTHER as i64),
    c("ACL_EVERYONE", ACL_EVERYONE as i64),
    c("ACL_ENTRY_TYPE_ALLOW", ACL_ENTRY_TYPE_ALLOW as i64),
    c("ACL_ENTRY_TYPE_DENY", ACL_ENTRY_TYPE_DENY as i64),
    c("ACL_TYPE_ACCESS_OLD", ACL_TYPE_ACCESS_OLD as i64),
    c("ACL_TYPE_DEFAULT_OLD", ACL_TYPE_DEFAULT_OLD as i64),
    c("ACL_TYPE_ACCESS", ACL_TYPE_ACCESS as i64),
    c("ACL_TYPE_DEFAULT", ACL_TYPE_DEFAULT as i64),
    c("ACL_TYPE_NFS4", ACL_TYPE_NFS4 as i64),
    c("ACL_EXECUTE", ACL_EXECUTE as i64),
    c("ACL_WRITE", ACL_WRITE as i64),
    c("ACL_READ", ACL_READ as i64),
    c("ACL_READ_DATA", ACL_READ_DATA as i64),
    c("ACL_WRITE_DATA", ACL_WRITE_DATA as i64),
    c("ACL_APPEND_DATA", ACL_APPEND_DATA as i64),
    c("ACL_DELETE_CHILD", ACL_DELETE_CHILD as i64),
    c("ACL_DELETE", ACL_DELETE as i64),
    c("ACL_READ_ACL", ACL_READ_ACL as i64),
    c("ACL_WRITE_ACL", ACL_WRITE_ACL as i64),
    c("ACL_WRITE_OWNER", ACL_WRITE_OWNER as i64),
    c("ACL_SYNCHRONIZE", ACL_SYNCHRONIZE as i64),
    c("ACL_ENTRY_FILE_INHERIT", ACL_ENTRY_FILE_INHERIT as i64),
    c(
        "ACL_ENTRY_DIRECTORY_INHERIT",
        ACL_ENTRY_DIRECTORY_INHERIT as i64,
    ),
    c("ACL_ENTRY_INHERIT_ONLY", ACL_ENTRY_INHERIT_ONLY as i64),
    c("ACL_ENTRY_INHERITED", ACL_ENTRY_INHERITED as i64),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/acl.h",
        value,
        arches: Arches::All,
    }
}
