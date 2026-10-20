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
use abi::fcntl::{
    FREAD, FWRITE, O_APPEND, O_NONBLOCK, SEEK_CUR, SEEK_DATA, SEEK_END, SEEK_HOLE, SEEK_SET,
};
use abi::ioctl::FIONREAD;
use abi::stat::Stat;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use vfs::{Device, Kind, Uio, Vnode, VnodeUse};

use super::pipe;

static OFFSET_LOCK: LockClass = LockClass::new("file offset", 300);

/// An open file system object: the vnode in use, and for a device, the
/// device it reached when opened (its I/O does not go through the vnode).
pub(crate) struct VnodeFile {
    pub(crate) vnode: VnodeUse,
    pub(crate) device: Option<Device>,
}

impl VnodeFile {
    /// `v` opened: a devfs node's device is looked up once, here.
    pub(crate) fn open(s: &Sleepable, v: &Arc<Vnode>) -> Result<Self, Errno> {
        let device = match v.kind() {
            Kind::CharDevice => Some(v.device(s)?),
            _ => None,
        };
        Ok(Self {
            vnode: v.use_ref(),
            device,
        })
    }
}

impl VnodeFile {
    /// This open file as the owner of `flock` locks.
    pub(crate) fn lock_owner(&self) -> vfs::lockf::Owner {
        vfs::lockf::Owner::File(core::ptr::from_ref(self).addr())
    }
}

impl Drop for VnodeFile {
    fn drop(&mut self) {
        // The last descriptor for this open file closed: its `flock` locks
        // go.
        let owner = self.lock_owner();
        self.vnode.lockf().release(owner);
    }
}

/// What an open file is.
pub(crate) enum FileKind {
    Vnode(VnodeFile),
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
    pub(crate) fn vnode(&self) -> Option<&Arc<Vnode>> {
        match &self.kind {
            FileKind::Vnode(f) => Some(f.vnode.vnode()),
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
            FileKind::Vnode(f) => {
                if let Some(d) = &f.device {
                    return d.ops.read(s, uio, self.nonblock());
                }
                let mut off = self.offset.lock(s);
                let n = f.vnode.read_at(s, *off, uio)?;
                *off += n as u64;
                Ok(n)
            }
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
            FileKind::Vnode(f) => {
                if let Some(d) = &f.device {
                    return d.ops.read(s, uio, self.nonblock());
                }
                let offset = u64::try_from(offset).map_err(|_| Errno::EINVAL)?;
                f.vnode.read_at(s, offset, uio)
            }
            FileKind::Pipe(_) | FileKind::Process(_) => Err(Errno::ESPIPE),
        }
    }

    /// `write`: at the offset (the end with `O_APPEND`), which advances.
    pub(crate) fn write(&self, s: &Sleepable, uio: &mut dyn Uio) -> Result<usize, Errno> {
        if self.flags() & FWRITE == 0 {
            return Err(Errno::EBADF);
        }
        match &self.kind {
            FileKind::Vnode(f) => {
                if let Some(d) = &f.device {
                    return d.ops.write(s, uio, self.nonblock());
                }
                let mut off = self.offset.lock(s);
                if self.flags() & O_APPEND != 0 {
                    *off = f.vnode.size(s)?;
                }
                let n = f.vnode.write_at(s, *off, uio)?;
                *off += n as u64;
                Ok(n)
            }
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
        if self.flags() & FWRITE == 0 {
            return Err(Errno::EBADF);
        }
        match &self.kind {
            FileKind::Pipe(_) | FileKind::Process(_) => Err(Errno::ESPIPE),
            FileKind::Vnode(f) => {
                if let Some(d) = &f.device {
                    return d.ops.write(s, uio, self.nonblock());
                }
                let at = if self.flags() & O_APPEND != 0 {
                    f.vnode.size(s)?
                } else {
                    u64::try_from(offset).map_err(|_| Errno::EINVAL)?
                };
                f.vnode.write_at(s, at, uio)
            }
        }
    }

    /// `lseek`: the new offset.
    pub(crate) fn seek(&self, s: &Sleepable, offset: i64, whence: u32) -> Result<i64, Errno> {
        let FileKind::Vnode(f) = &self.kind else {
            return Err(Errno::ESPIPE);
        };
        let mut off = self.offset.lock(s);
        if whence == SEEK_DATA || whence == SEEK_HOLE {
            // No file system reports holes yet: a file is all data, and its
            // end is its only hole (FreeBSD's answer without hole support).
            if f.vnode.kind() != Kind::Regular {
                return Err(Errno::EINVAL);
            }
            let size = f.vnode.size(s)?;
            let at = u64::try_from(offset).map_err(|_| Errno::ENXIO)?;
            if at >= size {
                return Err(Errno::ENXIO);
            }
            let to = if whence == SEEK_DATA { at } else { size };
            *off = to;
            return i64::try_from(to).map_err(|_| Errno::EOVERFLOW);
        }
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => i64::try_from(*off).map_err(|_| Errno::EOVERFLOW)?,
            SEEK_END => i64::try_from(f.vnode.size(s)?).map_err(|_| Errno::EOVERFLOW)?,
            _ => return Err(Errno::EINVAL),
        };
        let new = base.checked_add(offset).ok_or(Errno::EOVERFLOW)?;
        // Devices take any offset; files and directories none before 0.
        if new < 0 && f.device.is_none() {
            return Err(Errno::EINVAL);
        }
        *off = new.cast_unsigned();
        Ok(new)
    }

    /// The offset, held: `getdirentries` reads and advances it in place.
    pub(crate) fn offset(&self, s: &Sleepable) -> sched::lock::MutexGuard<'_, u64> {
        self.offset.lock(s)
    }

    pub(crate) fn stat(&self, s: &Sleepable) -> Result<Stat, Errno> {
        match &self.kind {
            FileKind::Vnode(f) => f.vnode.stat(s),
            FileKind::Pipe(p) => Ok(p.stat(s)),
            FileKind::Process(pd) => Ok(pd.stat()),
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
            FileKind::Vnode(f) => match (&f.device, f.vnode.kind()) {
                (Some(d), _) => d.ops.ioctl(s, req, arg),
                (None, Kind::Regular) if req == FIONREAD => {
                    let off = *self.offset.lock(s);
                    let size = f.vnode.size(s)?;
                    put(
                        arg,
                        usize::try_from(size.saturating_sub(off)).unwrap_or(usize::MAX),
                    );
                    Ok(())
                }
                _ => Err(Errno::ENOTTY),
            },
        }
    }
}
