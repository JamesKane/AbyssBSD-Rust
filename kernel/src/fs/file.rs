// SPDX-License-Identifier: BSD-2-Clause
//! Open files (FreeBSD's `struct file`): what a descriptor refers to, its
//! access mode and status flags, and its offset. Descriptors that `dup`
//! or `fork` copied share one `File`, and so its offset and flags; it
//! closes when the last of them does.
//!
//! Behavior reference: sys/kern/kern_descrip.c, sys/kern/vfs_vnops.c
//! (`vn_read`, `vn_seek`, `vn_statfile`), sys/kern/sys_generic.c.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use abi::errno::Errno;
use abi::fcntl::{FREAD, FWRITE, O_APPEND, O_NONBLOCK, SEEK_CUR, SEEK_END, SEEK_SET};
use abi::ioctl::FIONREAD;
use abi::stat::Stat;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use vfs::{Uio, Vnode, VnodeKind};

use super::pipe;

static OFFSET_LOCK: LockClass = LockClass::new("file offset", 300);

/// What an open file is.
pub(crate) enum FileKind {
    Vnode(Arc<Vnode>),
    Pipe(pipe::End),
    /// A process descriptor (`pdfork`).
    Process(crate::proc::ProcDesc),
}

/// An open file.
pub(crate) struct File {
    kind: FileKind,
    /// `FREAD` and `FWRITE`, and the status flags (`O_NONBLOCK`,
    /// `O_APPEND`, ...).
    flags: AtomicU32,
    /// The offset, held across a transfer so concurrent ones each see
    /// and advance it whole (FreeBSD's `foffset_lock`).
    offset: Mutex<u64>,
}

impl File {
    pub(crate) fn new(kind: FileKind, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            kind,
            flags: AtomicU32::new(flags),
            offset: Mutex::new(&OFFSET_LOCK, 0),
        })
    }

    pub(crate) const fn kind(&self) -> &FileKind {
        &self.kind
    }

    /// The vnode, if this is a file system object.
    pub(crate) const fn vnode(&self) -> Option<&Arc<Vnode>> {
        match &self.kind {
            FileKind::Vnode(v) => Some(v),
            FileKind::Pipe(_) | FileKind::Process(_) => None,
        }
    }

    pub(crate) fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// Replaces the status flags in `mask` with `bits`.
    pub(crate) fn set_flags(&self, mask: u32, bits: u32) {
        // Relaxed: the flags are independent bits, read without ordering.
        let _ = self
            .flags
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |f| {
                Some(f & !mask | bits & mask)
            });
    }

    fn nonblock(&self) -> bool {
        self.flags() & O_NONBLOCK != 0
    }

    /// `read`: from the offset, which advances.
    pub(crate) fn read(&self, s: &Sleepable, uio: &mut dyn Uio) -> Result<usize, Errno> {
        if self.flags() & FREAD == 0 {
            return Err(Errno::EBADF);
        }
        match &self.kind {
            FileKind::Vnode(v) => match v.kind() {
                VnodeKind::Regular(f) => {
                    let mut off = self.offset.lock(s);
                    let n = f.read(*off, uio)?;
                    *off += n as u64;
                    Ok(n)
                }
                VnodeKind::Directory(_) => Err(Errno::EISDIR),
                VnodeKind::Symlink(_) => Err(Errno::EINVAL),
                VnodeKind::CharDevice(d) => d.ops.read(s, uio, self.nonblock()),
            },
            FileKind::Pipe(p) => p.read(s, uio, self.nonblock()),
            FileKind::Process(_) => Err(Errno::EOPNOTSUPP),
        }
    }

    /// `pread`: at `offset`, which the file's own does not follow.
    pub(crate) fn pread(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        offset: i64,
    ) -> Result<usize, Errno> {
        if self.flags() & FREAD == 0 {
            return Err(Errno::EBADF);
        }
        match &self.kind {
            FileKind::Vnode(v) => {
                let offset = u64::try_from(offset).map_err(|_| Errno::EINVAL)?;
                match v.kind() {
                    VnodeKind::Regular(f) => f.read(offset, uio),
                    VnodeKind::Directory(_) => Err(Errno::EISDIR),
                    VnodeKind::Symlink(_) => Err(Errno::EINVAL),
                    VnodeKind::CharDevice(d) => d.ops.read(s, uio, self.nonblock()),
                }
            }
            FileKind::Pipe(_) | FileKind::Process(_) => Err(Errno::ESPIPE),
        }
    }

    /// `write`. Regular files are read-only until Phase 5's writable file
    /// systems, so they cannot be open for writing.
    pub(crate) fn write(&self, s: &Sleepable, uio: &mut dyn Uio) -> Result<usize, Errno> {
        if self.flags() & FWRITE == 0 {
            return Err(Errno::EBADF);
        }
        match &self.kind {
            FileKind::Vnode(v) => match v.kind() {
                VnodeKind::CharDevice(d) => d.ops.write(s, uio, self.nonblock()),
                VnodeKind::Directory(_) => Err(Errno::EISDIR),
                VnodeKind::Regular(_) | VnodeKind::Symlink(_) => Err(Errno::EROFS),
            },
            FileKind::Pipe(p) => p.write(s, uio, self.nonblock()),
            FileKind::Process(_) => Err(Errno::EOPNOTSUPP),
        }
    }

    /// `pwrite` (with `O_APPEND`, FreeBSD writes at the end regardless).
    pub(crate) fn pwrite(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        offset: i64,
    ) -> Result<usize, Errno> {
        match &self.kind {
            FileKind::Pipe(_) | FileKind::Process(_) => Err(Errno::ESPIPE),
            FileKind::Vnode(_) if offset < 0 && self.flags() & O_APPEND == 0 => Err(Errno::EINVAL),
            FileKind::Vnode(_) => self.write(s, uio),
        }
    }

    /// `lseek`: the new offset.
    pub(crate) fn seek(&self, s: &Sleepable, offset: i64, whence: u32) -> Result<i64, Errno> {
        let FileKind::Vnode(v) = &self.kind else {
            return Err(Errno::ESPIPE);
        };
        let mut off = self.offset.lock(s);
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => i64::try_from(*off).map_err(|_| Errno::EOVERFLOW)?,
            SEEK_END => i64::try_from(v.size()).map_err(|_| Errno::EOVERFLOW)?,
            _ => return Err(Errno::EINVAL),
        };
        let new = base.checked_add(offset).ok_or(Errno::EOVERFLOW)?;
        // Devices take any offset; files and directories none before 0.
        if new < 0 && !matches!(v.kind(), VnodeKind::CharDevice(_)) {
            return Err(Errno::EINVAL);
        }
        *off = new.cast_unsigned();
        Ok(new)
    }

    /// The offset, held: `getdirentries` reads and advances it in place.
    pub(crate) fn offset(&self, s: &Sleepable) -> sched::lock::MutexGuard<'_, u64> {
        self.offset.lock(s)
    }

    pub(crate) fn stat(&self, s: &Sleepable) -> Stat {
        match &self.kind {
            FileKind::Vnode(v) => v.stat(),
            FileKind::Pipe(p) => p.stat(s),
            FileKind::Process(pd) => pd.stat(),
        }
    }

    /// An `ioctl` request that is not about the descriptor itself; `arg`
    /// is its argument, copied in and out by the caller.
    pub(crate) fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let put = |arg: &mut [u8], v: usize| {
            let v = i32::try_from(v).unwrap_or(i32::MAX);
            arg.copy_from_slice(&v.to_ne_bytes());
        };
        match &self.kind {
            FileKind::Process(_) => Err(Errno::ENOTTY),
            FileKind::Pipe(p) => match req {
                FIONREAD => {
                    put(arg, p.readable());
                    Ok(())
                }
                _ => Err(Errno::ENOTTY),
            },
            FileKind::Vnode(v) => match v.kind() {
                VnodeKind::CharDevice(d) => d.ops.ioctl(s, req, arg),
                VnodeKind::Regular(f) if req == FIONREAD => {
                    let off = *self.offset.lock(s);
                    put(
                        arg,
                        usize::try_from(f.size().saturating_sub(off)).unwrap_or(usize::MAX),
                    );
                    Ok(())
                }
                _ => Err(Errno::ENOTTY),
            },
        }
    }
}
