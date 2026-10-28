// SPDX-License-Identifier: BSD-2-Clause
//! Access control lists (docs/MULTIUSER.md §10.7): `__acl_get`, `_set`,
//! `_delete`, and `_aclcheck`, each by path (`_file`), without following a
//! last symbolic link (`_link`), or by descriptor (`_fd`, which needs the
//! matching `CAP_ACL_*` right). The old types (`ACL_TYPE_ACCESS_OLD`,
//! `ACL_TYPE_DEFAULT_OLD`) take a `struct oldacl`, the rest a `struct acl`.
//!
//! Behavior reference: sys/kern/vfs_acl.c.

use alloc::vec::Vec;

use abi::acl::{
    ACL_MAX_COUNT, ACL_MAX_ENTRIES, ACL_TYPE_ACCESS_OLD, ACL_TYPE_DEFAULT_OLD, Acl as RawAcl,
    AclEntry, OLDACL_MAX_ENTRIES,
};
use abi::errno::Errno;
use sched::Sleepable;
use vfs::acl::{self, Acl, Type};

use super::SysResult;
use super::extattr::{By, file};
use crate::capsicum::rights as R;
use crate::uaccess;

/// `struct acl`'s size: two counts, four spare words, and its entries.
const ACL_SIZE: usize = core::mem::size_of::<RawAcl>();
const ENTRY_SIZE: usize = core::mem::size_of::<AclEntry>();

fn is_old(t: u32) -> bool {
    matches!(t, ACL_TYPE_ACCESS_OLD | ACL_TYPE_DEFAULT_OLD)
}

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The ACL at `addr`, as type `t` lays it out (`acl_copyin`): `EINVAL`
/// for a `struct acl` whose `acl_maxcnt` is not `ACL_MAX_ENTRIES`, or a
/// count past what it holds.
fn copy_in(addr: u64, t: u32) -> Result<Acl, Errno> {
    if is_old(t) {
        let mut b = alloc::vec![0u8; acl::OLDACL_SIZE];
        uaccess::copy_in(&mut b, addr).map_err(|_| Errno::EFAULT)?;
        return match Acl::decode(&b) {
            Err(Errno::EPERM) => Err(Errno::EINVAL),
            r => r,
        };
    }
    let mut b = alloc::vec![0u8; ACL_SIZE];
    uaccess::copy_in(&mut b, addr).map_err(|_| Errno::EFAULT)?;
    if word(&b, 0) != ACL_MAX_COUNT {
        return Err(Errno::EINVAL);
    }
    let n = usize::try_from(word(&b, 4))
        .ok()
        .filter(|&n| n <= ACL_MAX_ENTRIES)
        .ok_or(Errno::EINVAL)?;
    Ok(Acl((0..n)
        .map(|i| {
            let at = 24 + i * ENTRY_SIZE;
            AclEntry {
                ae_tag: word(&b, at),
                ae_id: word(&b, at + 4),
                ae_perm: word(&b, at + 8),
                ae_entry_type: u16::from_ne_bytes([b[at + 12], b[at + 13]]),
                ae_flags: u16::from_ne_bytes([b[at + 14], b[at + 15]]),
            }
        })
        .collect()))
}

/// Copies `a` out to `addr` as type `t` lays it out (`acl_copyout`): the
/// caller's `struct acl` must say it holds `ACL_MAX_ENTRIES`.
fn copy_out(addr: u64, t: u32, a: &Acl) -> Result<(), Errno> {
    if is_old(t) {
        if a.0.len() > OLDACL_MAX_ENTRIES {
            return Err(Errno::EINVAL);
        }
        let b = a.encode()?;
        return uaccess::copy_out(addr, &b).map_err(|_| Errno::EFAULT);
    }
    let mut max = [0u8; 4];
    uaccess::copy_in(&mut max, addr).map_err(|_| Errno::EFAULT)?;
    if u32::from_ne_bytes(max) != ACL_MAX_COUNT {
        return Err(Errno::EINVAL);
    }
    let mut b: Vec<u8> = alloc::vec![0u8; ACL_SIZE];
    let n = u32::try_from(a.0.len()).map_err(|_| Errno::EINVAL)?;
    b[..4].copy_from_slice(&ACL_MAX_COUNT.to_ne_bytes());
    b[4..8].copy_from_slice(&n.to_ne_bytes());
    for (i, e) in a.0.iter().enumerate() {
        let at = 24 + i * ENTRY_SIZE;
        b[at..at + 4].copy_from_slice(&e.ae_tag.to_ne_bytes());
        b[at + 4..at + 8].copy_from_slice(&e.ae_id.to_ne_bytes());
        b[at + 8..at + 12].copy_from_slice(&e.ae_perm.to_ne_bytes());
        b[at + 12..at + 14].copy_from_slice(&e.ae_entry_type.to_ne_bytes());
        b[at + 14..at + 16].copy_from_slice(&e.ae_flags.to_ne_bytes());
    }
    uaccess::copy_out(addr, &b).map_err(|_| Errno::EFAULT)
}

fn ty(t: u64) -> Result<(u32, Type), Errno> {
    let t = u32::try_from(t).map_err(|_| Errno::EINVAL)?;
    Ok((t, Type::from_raw(t)?))
}

/// `__acl_get_*(target, type, aclp)`.
pub(super) fn get(s: &Sleepable, by: By, target: u64, t: u64, aclp: u64) -> SysResult {
    let (raw, t) = ty(t)?;
    let v = file(s, by, target, &R::ACL_GET)?;
    let a = acl::get(s, &v, t)?;
    copy_out(aclp, raw, &a)?;
    Ok(0)
}

/// `__acl_set_*(target, type, aclp)`.
pub(super) fn set(s: &Sleepable, by: By, target: u64, t: u64, aclp: u64) -> SysResult {
    let (raw, t) = ty(t)?;
    let v = file(s, by, target, &R::ACL_SET)?;
    let a = copy_in(aclp, raw)?;
    acl::set(s, &v, &crate::fs::cred(), t, Some(&a))?;
    Ok(0)
}

/// `__acl_delete_*(target, type)`.
pub(super) fn delete(s: &Sleepable, by: By, target: u64, t: u64) -> SysResult {
    let (_, t) = ty(t)?;
    let v = file(s, by, target, &R::ACL_DELETE)?;
    acl::set(s, &v, &crate::fs::cred(), t, None)?;
    Ok(0)
}

/// `__acl_aclcheck_*(target, type, aclp)`.
pub(super) fn check(s: &Sleepable, by: By, target: u64, t: u64, aclp: u64) -> SysResult {
    let (raw, t) = ty(t)?;
    let v = file(s, by, target, &R::ACL_CHECK)?;
    let mut a = copy_in(aclp, raw)?;
    acl::check(&v, t, &mut a)?;
    Ok(0)
}
