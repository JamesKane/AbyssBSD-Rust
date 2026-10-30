// SPDX-License-Identifier: BSD-2-Clause
//! `inotify_add_watch_at` and `inotify_rm_watch`, on a descriptor from
//! `__specialfd(SPECIALFD_INOTIFY)`.
//!
//! Behavior reference: sys/kern/vfs_inotify.c.

use alloc::sync::Arc;

use abi::capsicum::CapRights;
use abi::errno::Errno;
use abi::inotify::{
    IN_ALL_EVENTS, IN_ALL_FLAGS, IN_DONT_FOLLOW, IN_MASK_ADD, IN_MASK_CREATE, IN_ONLYDIR,
    IN_UNMOUNT,
};
use sched::Sleepable;
use vfs::inotify::Inotify;
use vfs::ops::VREAD;

use super::{SysResult, int, process};
use crate::fs::Walk;
use crate::fs::file::FileKind;

/// The inotify state behind `fd` (`fget_inotify`): `EINVAL` for another
/// kind of file.
fn inotify_of(s: &Sleepable, fd: u64, need: &CapRights) -> Result<Arc<Inotify>, Errno> {
    let f = super::io::current_file(s, fd, need)?;
    match f.kind() {
        FileKind::Inotify(i) => Ok(Arc::clone(&i.0)),
        _ => Err(Errno::EINVAL),
    }
}

/// `inotify_add_watch_at(fd, dfd, path, mask)`: the watch's number.
pub(super) fn inotify_add_watch_at(
    s: &Sleepable,
    fd: u64,
    dfd: u64,
    path_ptr: u64,
    mask: u64,
) -> SysResult {
    let mask = int(mask).cast_unsigned();
    if mask & IN_ALL_EVENTS == 0
        || mask & (IN_MASK_ADD | IN_MASK_CREATE) == IN_MASK_ADD | IN_MASK_CREATE
        || mask & !(IN_ALL_EVENTS | IN_ALL_FLAGS | IN_UNMOUNT) != 0
    {
        return Err(Errno::EINVAL);
    }
    let sc = inotify_of(s, fd, &crate::capsicum::rights::INOTIFY_ADD)?;
    let p = process()?;
    let path = super::path::path(path_ptr)?;
    let walk = Walk {
        follow: mask & IN_DONT_FOLLOW == 0,
        beneath: false,
        create: false,
    };
    let v = super::path::at_vnode(s, &p, dfd, &path, walk, false, &crate::capsicum::rights::NO)?;
    vfs::acl::access(s, &v, &p.cred(), VREAD)?;
    if mask & IN_ONLYDIR != 0 && !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    let wd = sc.add_watch(s, &v, mask)?;
    Ok(u64::from(wd.cast_unsigned()))
}

/// `inotify_rm_watch(fd, wd)`.
pub(super) fn inotify_rm_watch(s: &Sleepable, fd: u64, wd: u64) -> SysResult {
    let sc = inotify_of(s, fd, &crate::capsicum::rights::INOTIFY_RM)?;
    sc.rm_watch(int(wd))?;
    Ok(0)
}
