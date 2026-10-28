// SPDX-License-Identifier: BSD-2-Clause
//! Extended attributes (docs/MULTIUSER.md §10.7): `extattr_get`, `_set`,
//! `_delete`, and `_list`, each by path (`_file`), without following a
//! last symbolic link (`_link`), or by descriptor (`_fd`, which needs the
//! matching `CAP_EXTATTR_*` right). The name is copied in first, then the
//! file found, as FreeBSD orders them.
//!
//! Behavior reference: sys/kern/vfs_extattr.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::capsicum::CapRights;
use abi::errno::Errno;
use sched::Sleepable;
use vfs::Vnode;
use vfs::ops::extattr::MAXNAMELEN;

use super::{SysResult, int, process};
use crate::capsicum::rights as R;
use crate::fs::{At, Walk};
use crate::uaccess;

/// The most a value may be: what tmpfs keeps for all of them
/// (`vfs.tmpfs.ea_memory_max`'s default); UFS2's area is smaller.
const MAX_VALUE: usize = 16 << 20;

/// How a call names its file.
#[derive(Clone, Copy)]
pub(super) enum By {
    File,
    Link,
    Fd,
}

/// The attribute name at `addr`: `ENAMETOOLONG` past `EXTATTR_MAXNAMELEN`.
fn name(addr: u64) -> Result<Vec<u8>, Errno> {
    let mut buf = [0u8; MAXNAMELEN + 1];
    let n = uaccess::copy_in_str(addr, &mut buf).map_err(|e| match e {
        uaccess::StrError::Fault => Errno::EFAULT,
        uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    })?;
    Ok(buf[..n].to_vec())
}

/// The file `by` names with `arg`.
pub(super) fn file(s: &Sleepable, by: By, arg: u64, need: &CapRights) -> Result<Arc<Vnode>, Errno> {
    let p = process()?;
    match by {
        By::Fd => super::io::file(s, &p, arg, need)?
            .vnode()
            .cloned()
            .ok_or(Errno::EINVAL),
        By::File | By::Link => {
            let path = super::path::path(arg)?;
            let walk = if matches!(by, By::Link) {
                Walk::NOFOLLOW
            } else {
                Walk::FOLLOW
            };
            crate::fs::lookup(s, &p, At::Cwd, &path, walk)
        }
    }
}

/// A namespace argument (`int`); anything else fails as the file system
/// would (`EPERM`).
fn ns(ns: u64) -> u8 {
    u8::try_from(int(ns)).unwrap_or(0)
}

/// `extattr_set_*(target, attrnamespace, attrname, data, nbytes)`: the
/// bytes set.
pub(super) fn set(
    s: &Sleepable,
    by: By,
    target: u64,
    namespace: u64,
    attrname: u64,
    data: u64,
    nbytes: u64,
) -> SysResult {
    let name = name(attrname)?;
    let len = usize::try_from(nbytes).map_err(|_| Errno::EINVAL)?;
    if i64::try_from(len).is_err() {
        return Err(Errno::EINVAL);
    }
    let v = file(s, by, target, &R::EXTATTR_SET)?;
    if len > MAX_VALUE {
        return Err(Errno::ENOSPC);
    }
    let mut value = alloc::vec![0u8; len];
    if len > 0 {
        uaccess::copy_in(&mut value, data).map_err(|_| Errno::EFAULT)?;
    }
    vfs::ops::set_extattr(s, &v, &crate::fs::cred(), ns(namespace), &name, &value)?;
    Ok(len as u64)
}

/// `extattr_get_*(target, attrnamespace, attrname, data, nbytes)`: with
/// no buffer, the value's size; else as much of it as fits, and how much
/// that was.
pub(super) fn get(
    s: &Sleepable,
    by: By,
    target: u64,
    namespace: u64,
    attrname: u64,
    data: u64,
    nbytes: u64,
) -> SysResult {
    let name = name(attrname)?;
    let v = file(s, by, target, &R::EXTATTR_GET)?;
    let value = vfs::ops::get_extattr(s, &v, &crate::fs::cred(), ns(namespace), &name)?;
    out(&value, data, nbytes)
}

/// `extattr_delete_*(target, attrnamespace, attrname)`.
pub(super) fn delete(
    s: &Sleepable,
    by: By,
    target: u64,
    namespace: u64,
    attrname: u64,
) -> SysResult {
    let name = name(attrname)?;
    let v = file(s, by, target, &R::EXTATTR_DELETE)?;
    vfs::ops::delete_extattr(s, &v, &crate::fs::cred(), ns(namespace), &name)?;
    Ok(0)
}

/// `extattr_list_*(target, attrnamespace, data, nbytes)`: the names, each
/// a length byte and the name, as `get` returns a value.
pub(super) fn list(
    s: &Sleepable,
    by: By,
    target: u64,
    namespace: u64,
    data: u64,
    nbytes: u64,
) -> SysResult {
    let v = file(s, by, target, &R::EXTATTR_LIST)?;
    let names = vfs::ops::list_extattr(s, &v, &crate::fs::cred(), ns(namespace))?;
    out(&names, data, nbytes)
}

/// Hands `bytes` back: their size with no buffer, else as much as fits.
fn out(bytes: &[u8], data: u64, nbytes: u64) -> SysResult {
    if data == 0 {
        return Ok(bytes.len() as u64);
    }
    let n = bytes
        .len()
        .min(usize::try_from(nbytes).unwrap_or(usize::MAX));
    uaccess::copy_out(data, &bytes[..n]).map_err(|_| Errno::EFAULT)?;
    Ok(n as u64)
}
