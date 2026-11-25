// SPDX-License-Identifier: BSD-2-Clause
//! Descriptors for dma-bufs: what the graphics core's buffer sharing
//! (`sys/drm`, `PRIME_HANDLE_TO_FD` and `PRIME_FD_TO_HANDLE`) needs from
//! the calling process's descriptor table.

use alloc::sync::Arc;

use abi::errno::Errno;
use abi::fcntl::{FREAD, FWRITE};
use sched::Sleepable;

use super::process;
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};

/// A new descriptor in the calling process for `buf`: close on exec if
/// `cloexec`, open for writing as well as reading if `rdwr`.
pub(crate) fn export(
    s: &Sleepable,
    buf: Arc<drm::DmaBuf>,
    cloexec: bool,
    rdwr: bool,
) -> Result<i32, Errno> {
    let p = process()?;
    let limit = super::io::max_files(&p);
    let status = FREAD | if rdwr { FWRITE } else { 0 };
    p.files.lock(s).alloc(
        File::new(FileKind::DmaBuf(buf), status)?,
        FdFlags {
            cloexec,
            clofork: false,
            resolve_beneath: false,
        },
        0,
        limit,
    )
}

/// The dma-buf the calling process's descriptor `fd` names.
///
/// # Errors
///
/// `EBADF` for no such descriptor (or rights it lacks); `EINVAL` for a
/// descriptor that is not a dma-buf.
pub(crate) fn import(s: &Sleepable, fd: i32) -> Result<Arc<drm::DmaBuf>, Errno> {
    let fd = u64::try_from(fd).map_err(|_| Errno::EBADF)?;
    let f = super::io::current_file(s, fd, &crate::capsicum::rights::MMAP)?;
    match f.kind() {
        FileKind::DmaBuf(d) => Ok(Arc::clone(d)),
        _ => Err(Errno::EINVAL),
    }
}

/// A new descriptor in the calling process for the sync file `file`: close
/// on exec if `cloexec`.
pub(crate) fn export_syncfile(
    s: &Sleepable,
    file: Arc<drm::SyncFile>,
    cloexec: bool,
) -> Result<i32, Errno> {
    let p = process()?;
    let limit = super::io::max_files(&p);
    p.files.lock(s).alloc(
        File::new(FileKind::SyncFile(file), FREAD)?,
        FdFlags {
            cloexec,
            clofork: false,
            resolve_beneath: false,
        },
        0,
        limit,
    )
}

/// The sync file the calling process's descriptor `fd` names.
///
/// # Errors
///
/// `EBADF` for no such descriptor; `EINVAL` for one that is not a sync
/// file.
pub(crate) fn import_syncfile(s: &Sleepable, fd: i32) -> Result<Arc<drm::SyncFile>, Errno> {
    let fd = u64::try_from(fd).map_err(|_| Errno::EBADF)?;
    let f = super::io::current_file(s, fd, &crate::capsicum::rights::EVENT)?;
    match f.kind() {
        FileKind::SyncFile(d) => Ok(Arc::clone(d)),
        _ => Err(Errno::EINVAL),
    }
}
