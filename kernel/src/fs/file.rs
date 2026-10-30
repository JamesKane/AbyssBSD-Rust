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
use abi::ioctl::{FIOASYNC, FIODTYPE, FIONBIO, FIONREAD};
use abi::stat::{S_ISGID, S_ISUID, Stat};
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
    /// Opened for writing: counted as a writer while open, so no program
    /// runs the file meanwhile (FreeBSD's `v_writecount`).
    writer: bool,
    /// Opened with `FWRITE`, for the close's note.
    write: bool,
}

impl VnodeFile {
    /// `v` opened, for writing if `write`, without waiting if `nonblock`:
    /// a devfs node's device is looked up once, here. `ETXTBSY` to write a
    /// file a program runs.
    pub(crate) fn open(
        s: &Sleepable,
        v: &Arc<Vnode>,
        write: bool,
        nonblock: bool,
    ) -> Result<Self, Errno> {
        let device = match v.kind() {
            Kind::CharDevice => {
                let mut d = v.device(s)?;
                // At securelevel 2, no disk opens for writing
                // (`g_dev_open`).
                if write && d.ops.dtype() == vfs::D_DISK {
                    super::cred().securelevel_ge(2)?;
                }
                if let Some(ops) = d.ops.instance_open(s, nonblock, write)? {
                    d.ops = ops;
                }
                Some(d)
            }
            _ => None,
        };
        let writer = write && v.kind() == Kind::Regular;
        if writer {
            v.add_writer()?;
        }
        // `vop_open_post`.
        v.knote(abi::event::NOTE_OPEN);
        vfs::inotify::event(v, abi::inotify::IN_OPEN);
        Ok(Self {
            vnode: v.use_ref(),
            device,
            writer,
            write: true,
        }
        .opened_for(write))
    }

    fn opened_for(mut self, write: bool) -> Self {
        self.write = write;
        self
    }

    /// `v` as `O_PATH` opens it: a name only.
    pub(crate) fn path_only(v: &Arc<Vnode>) -> Self {
        // `vn_open_vnode` reports an `O_PATH` open too.
        vfs::inotify::event(v, abi::inotify::IN_OPEN);
        Self {
            vnode: v.use_ref(),
            device: None,
            writer: false,
            write: false,
        }
    }
}

/// A revoked node's read (`dead_read`): end of file for a terminal,
/// `ENXIO` otherwise.
fn dead_read(d: &Device) -> Result<usize, Errno> {
    if d.ops.dtype() == vfs::D_TTY {
        Ok(0)
    } else {
        Err(Errno::ENXIO)
    }
}

impl VnodeFile {
    /// A device file whose node was revoked, or went (`devfs_fp_check`
    /// failing): its calls go to the dead vnode's operations.
    fn dead(&self) -> bool {
        self.vnode.gone()
    }

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
        if self.writer {
            self.vnode.drop_writer();
        }
        // `vop_close_post`.
        self.vnode.knote(if self.write {
            abi::event::NOTE_CLOSE_WRITE
        } else {
            abi::event::NOTE_CLOSE
        });
        vfs::inotify::event(
            &self.vnode,
            if self.write {
                abi::inotify::IN_CLOSE_WRITE
            } else {
                abi::inotify::IN_CLOSE_NOWRITE
            },
        );
    }
}

/// What an open file is.
pub(crate) enum FileKind {
    Vnode(VnodeFile),
    Pipe(pipe::End),
    /// A process descriptor (`pdfork`).
    Process(crate::proc::ProcDesc),
    /// A jail descriptor (`JAIL_GET_DESC`, `JAIL_OWN_DESC`).
    Jail(crate::prison::JailDesc),
    /// A UNIX-domain socket.
    Socket(super::socket::SocketFile),
    /// An internet (`AF_INET`, `AF_INET6`) socket.
    Inet(super::inet::InetFile),
    /// A netlink socket.
    Netlink(crate::net::netlink::NetlinkFile),
    /// A named pipe.
    Fifo(super::fifo::FifoFile),
    /// POSIX shared memory.
    Shm(Arc<super::shm::Shm>),
    /// A kqueue.
    Kqueue(crate::event::KqueueFile),
    /// A `timerfd`.
    Timerfd(Arc<crate::timers::Timerfd>),
    /// An `eventfd`.
    Eventfd(Arc<super::eventfd::Eventfd>),
    /// An inotify descriptor.
    Inotify(super::inotify::InotifyFile),
    /// A pseudo-terminal's master.
    Pts(Arc<crate::tty::pts::Master>),
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
    /// The jail of whoever opened it (`f_cred->cr_prison`); `None` for
    /// the host.
    jail: Option<Arc<dyn security::Jail>>,
}

/// Open files system-wide (`openfiles`, `kern.openfiles`).
pub(crate) static OPEN_FILES: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(0);

impl Drop for File {
    fn drop(&mut self) {
        OPEN_FILES.fetch_sub(1, Ordering::Relaxed);
    }
}

impl File {
    pub(crate) fn new(kind: FileKind, flags: u32) -> Arc<Self> {
        OPEN_FILES.fetch_add(1, Ordering::Relaxed);
        // As `falloc` takes the opener's credential.
        let jail = crate::proc::Process::with_current(crate::proc::Process::jail).flatten();
        Arc::new(Self {
            kind,
            flags: AtomicU32::new(flags),
            offset: Mutex::new(&OFFSET_LOCK, 0),
            jail,
        })
    }

    /// Whether a descriptor for it received by `to` must stay beneath it
    /// (`restrict_rights`): it was opened in another jail with another
    /// root, and `to` is jailed.
    pub(crate) fn restricted_for(&self, to: &crate::cred::Ucred) -> bool {
        use crate::prison::{self, Prison};
        let Some(mine) = to.jail.as_ref() else {
            return false;
        };
        let theirs = self.jail.as_ref();
        if theirs.is_some_and(|t| t.id() == mine.id()) {
            return false;
        }
        let root = |j: Option<&Arc<dyn security::Jail>>| {
            j.and_then(|j| j.as_any().downcast_ref::<Prison>())
                .map_or_else(|| prison::prison0().root(), Prison::root)
        };
        !Arc::ptr_eq(&root(theirs), &root(Some(mine)))
    }

    pub(crate) const fn kind(&self) -> &FileKind {
        &self.kind
    }

    /// An internet socket's domain (`None` for another file, a UNIX one
    /// included).
    pub(crate) fn inet_family(&self) -> Option<inet::Family> {
        match &self.kind {
            FileKind::Inet(so) => Some(so.so.family),
            _ => None,
        }
    }

    /// The vnode, if this is a file system object.
    pub(crate) fn vnode(&self) -> Option<&Arc<Vnode>> {
        match &self.kind {
            FileKind::Vnode(f) => Some(f.vnode.vnode()),
            FileKind::Fifo(f) => Some(f.vnode.vnode()),
            FileKind::Pipe(_)
            | FileKind::Process(_)
            | FileKind::Jail(_)
            | FileKind::Socket(_)
            | FileKind::Inet(_)
            | FileKind::Netlink(_)
            | FileKind::Shm(_)
            | FileKind::Kqueue(_)
            | FileKind::Timerfd(_)
            | FileKind::Eventfd(_)
            | FileKind::Inotify(_)
            | FileKind::Pts(_) => None,
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
        // A device that signals hears of `O_ASYNC` (FreeBSD's `FIOASYNC`
        // to `fo_ioctl`).
        if mask & abi::fcntl::O_ASYNC != 0
            && let FileKind::Vnode(VnodeFile {
                device: Some(d), ..
            }) = &self.kind
        {
            d.ops.set_async(bits & abi::fcntl::O_ASYNC != 0);
        }
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
                    if f.dead() {
                        return dead_read(d);
                    }
                    // Not under the offset lock (`devfs_read_f`'s
                    // `FOF_NOLOCK`): a read waiting on a terminal must not
                    // hold up writes through the same file.
                    let at = *self.offset.lock(s);
                    let n = d.ops.read(s, uio, at, self.nonblock())?;
                    *self.offset.lock(s) = at + n as u64;
                    return Ok(n);
                }
                let mut off = self.offset.lock(s);
                let n = f.vnode.read_at(s, *off, uio)?;
                *off += n as u64;
                Ok(n)
            }
            FileKind::Pts(m) => m.read(s, uio, self.nonblock()),
            FileKind::Pipe(p) => p.read(s, uio, self.nonblock()),
            FileKind::Process(_) | FileKind::Jail(_) => Err(Errno::EOPNOTSUPP),
            FileKind::Socket(so) => so.read(s, uio, self.nonblock()),
            FileKind::Inet(so) => so.read(s, uio, self.nonblock()),
            FileKind::Netlink(nl) => nl.read(s, uio, self.nonblock()),
            FileKind::Fifo(f) => f.read(s, uio, self.nonblock()),
            FileKind::Shm(m) => {
                let mut off = self.offset.lock(s);
                let n = m.read(s, *off, uio)?;
                *off += n as u64;
                Ok(n)
            }
            FileKind::Kqueue(_) => Err(Errno::EOPNOTSUPP),
            FileKind::Timerfd(t) => t.read(s, uio, self.nonblock()),
            FileKind::Eventfd(e) => e.read(s, uio, self.nonblock()),
            FileKind::Inotify(i) => i.read(s, uio, self.nonblock()),
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
                let offset = u64::try_from(offset).map_err(|_| Errno::EINVAL)?;
                match &f.device {
                    Some(d) if f.dead() => dead_read(d),
                    Some(d) => d.ops.read(s, uio, offset, self.nonblock()),
                    None => f.vnode.read_at(s, offset, uio),
                }
            }
            FileKind::Shm(m) => m.read(s, u64::try_from(offset).map_err(|_| Errno::EINVAL)?, uio),
            FileKind::Pipe(_)
            | FileKind::Process(_)
            | FileKind::Jail(_)
            | FileKind::Socket(_)
            | FileKind::Inet(_)
            | FileKind::Netlink(_)
            | FileKind::Fifo(_)
            | FileKind::Kqueue(_)
            | FileKind::Timerfd(_)
            | FileKind::Eventfd(_)
            | FileKind::Inotify(_)
            | FileKind::Pts(_) => Err(Errno::ESPIPE),
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
                    if f.dead() {
                        return Err(Errno::ENXIO);
                    }
                    // As for reads (`devfs_write_f`'s `FOF_NOLOCK`).
                    let at = *self.offset.lock(s);
                    let n = d.ops.write(s, uio, at, self.nonblock())?;
                    *self.offset.lock(s) = at + n as u64;
                    return Ok(n);
                }
                let mut off = self.offset.lock(s);
                if self.flags() & O_APPEND != 0 {
                    *off = f.vnode.size(s)?;
                }
                let n = write_regular(s, &f.vnode, *off, uio)?;
                *off += n as u64;
                Ok(n)
            }
            FileKind::Pts(m) => m.write(s, uio, self.nonblock()),
            FileKind::Pipe(p) => p.write(s, uio, self.nonblock()),
            FileKind::Process(_) | FileKind::Jail(_) => Err(Errno::EOPNOTSUPP),
            FileKind::Socket(so) => so.write(s, uio, self.nonblock()),
            FileKind::Inet(so) => so.write(s, uio, self.nonblock()),
            FileKind::Netlink(nl) => nl.write(uio),
            FileKind::Fifo(f) => f.write(s, uio, self.nonblock()),
            FileKind::Shm(m) => {
                let mut off = self.offset.lock(s);
                let at = (self.flags() & O_APPEND == 0).then_some(*off);
                let (n, from) = m.write(s, at, uio)?;
                *off = from + n as u64;
                Ok(n)
            }
            FileKind::Kqueue(_) | FileKind::Timerfd(_) => Err(Errno::EOPNOTSUPP),
            FileKind::Eventfd(e) => e.write(s, uio, self.nonblock()),
            // `invfo_rdwr`.
            FileKind::Inotify(_) => Err(Errno::EOPNOTSUPP),
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
            FileKind::Pipe(_)
            | FileKind::Process(_)
            | FileKind::Jail(_)
            | FileKind::Socket(_)
            | FileKind::Inet(_)
            | FileKind::Netlink(_)
            | FileKind::Fifo(_)
            | FileKind::Kqueue(_)
            | FileKind::Timerfd(_)
            | FileKind::Eventfd(_)
            | FileKind::Inotify(_)
            | FileKind::Pts(_) => Err(Errno::ESPIPE),
            FileKind::Shm(m) => {
                let at = u64::try_from(offset).map_err(|_| Errno::EINVAL)?;
                let at = (self.flags() & O_APPEND == 0).then_some(at);
                Ok(m.write(s, at, uio)?.0)
            }
            FileKind::Vnode(f) => {
                if let Some(d) = &f.device {
                    if f.dead() {
                        return Err(Errno::ENXIO);
                    }
                    let at = u64::try_from(offset).map_err(|_| Errno::EINVAL)?;
                    return d.ops.write(s, uio, at, self.nonblock());
                }
                let at = if self.flags() & O_APPEND != 0 {
                    f.vnode.size(s)?
                } else {
                    u64::try_from(offset).map_err(|_| Errno::EINVAL)?
                };
                write_regular(s, &f.vnode, at, uio)
            }
        }
    }

    /// `lseek`: the new offset.
    pub(crate) fn seek(&self, s: &Sleepable, offset: i64, whence: u32) -> Result<i64, Errno> {
        if let FileKind::Shm(m) = &self.kind {
            let mut off = self.offset.lock(s);
            let new = m.seek(s, *off, offset, whence)?;
            *off = new;
            return i64::try_from(new).map_err(|_| Errno::EOVERFLOW);
        }
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
            FileKind::Jail(j) => Ok(j.stat()),
            FileKind::Socket(so) => Ok(so.stat()),
            FileKind::Inet(so) => Ok(so.stat()),
            FileKind::Netlink(nl) => Ok(nl.stat()),
            FileKind::Fifo(f) => f.vnode.stat(s),
            FileKind::Shm(m) => Ok(m.stat(s)),
            // `kqueue_stat`: a FIFO, otherwise empty.
            FileKind::Kqueue(_) => Ok(Stat {
                st_mode: abi::stat::S_IFIFO,
                ..Stat::default()
            }),
            FileKind::Timerfd(t) => Ok(t.stat()),
            FileKind::Eventfd(_) => Ok(super::eventfd::Eventfd::stat()),
            FileKind::Inotify(i) => Ok(i.stat()),
            FileKind::Pts(m) => Ok(m.stat()),
        }
    }

    /// The `poll` events of `events` that hold now (`fo_poll`). With a
    /// poller, it is registered first with what it would wait on, so a
    /// change after the look wakes it.
    pub(crate) fn poll(&self, s: &Sleepable, events: i16, w: Option<&Arc<dyn event::Wake>>) -> i16 {
        use abi::event::{POLLHUP, POLLIN, POLLOUT, POLLRDNORM, POLLSTANDARD, POLLWRNORM};
        match &self.kind {
            FileKind::Vnode(f) => match &f.device {
                // `dead_poll`: the descriptor is gone.
                Some(_) if f.dead() => {
                    if events & !POLLSTANDARD != 0 {
                        abi::event::POLLNVAL
                    } else {
                        POLLHUP | (events & (POLLIN | POLLRDNORM))
                    }
                }
                Some(d) => d.ops.poll(events, w),
                // `vop_stdpoll`: a file is always ready; anything beyond
                // the standard events waits for a note.
                None => {
                    if events & !POLLSTANDARD != 0 {
                        if let Some(w) = w {
                            f.vnode.notify().record(w);
                        }
                        0
                    } else {
                        events & (POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM)
                    }
                }
            },
            FileKind::Pipe(p) => p.poll(self.flags(), events, w),
            FileKind::Fifo(f) => f.poll(s, self.flags(), events, w),
            // `procdesc_poll`: hung up once the process exits.
            FileKind::Process(pd) => {
                let p = pd.process();
                if let Some(w) = w {
                    p.pd_sel.record(w);
                }
                if p.has_exited() { POLLHUP } else { 0 }
            }
            FileKind::Jail(j) => j.poll(w),
            FileKind::Socket(so) => so.so.poll(events, w),
            FileKind::Inet(so) => so.so.poll(events, w),
            FileKind::Netlink(nl) => nl.poll(events, w),
            FileKind::Shm(_) => vfs::node::poll_no_poll(events),
            FileKind::Kqueue(k) => k.poll(events, w),
            FileKind::Timerfd(t) => t.poll(events, w),
            FileKind::Eventfd(e) => e.poll(events, w),
            FileKind::Inotify(i) => i.poll(events, w),
            FileKind::Pts(m) => m.poll(events, w),
        }
    }

    /// The filter for a knote on this file (`fo_kqfilter`).
    pub(crate) fn kqfilter(
        this: &Arc<Self>,
        s: &Sleepable,
        kev: &abi::event::Kevent,
    ) -> Result<alloc::boxed::Box<dyn event::Filter>, Errno> {
        use abi::event::EVFILT_PROCDESC;
        match &this.kind {
            FileKind::Vnode(f) => match &f.device {
                Some(_) if f.dead() => Err(Errno::ENXIO),
                Some(d) => d.ops.kqfilter(kev),
                None => crate::event::vnode_filter(this, s, kev),
            },
            FileKind::Pipe(p) => p.kqfilter(this.flags(), kev),
            FileKind::Fifo(f) => match f.kqfilter(this.flags(), kev) {
                Some(r) => r,
                None => crate::event::vnode_filter(this, s, kev),
            },
            FileKind::Process(_) if kev.filter == EVFILT_PROCDESC => Ok(alloc::boxed::Box::new(
                crate::event::ProcdescFilter(Arc::clone(this)),
            )),
            FileKind::Socket(so) => socket::filter(&so.so, kev),
            FileKind::Inet(so) => inet::filter(&so.so, kev),
            FileKind::Netlink(nl) => nl.kqfilter(kev),
            // No `EVFILT_JAILDESC` yet (docs/compat-deviations.md).
            FileKind::Process(_) | FileKind::Jail(_) | FileKind::Shm(_) => Err(Errno::EINVAL),
            FileKind::Kqueue(k) if kev.filter == abi::event::EVFILT_READ => Ok(
                alloc::boxed::Box::new(event::KqueueFilter(Arc::clone(&k.kq))),
            ),
            FileKind::Kqueue(_) => Err(Errno::EINVAL),
            FileKind::Timerfd(t) => t.kqfilter(kev),
            FileKind::Eventfd(e) => e.kqfilter(kev),
            FileKind::Inotify(i) => i.kqfilter(kev),
            FileKind::Pts(m) => m.kqfilter(kev),
        }
    }

    /// The offset as it stands, without waiting for a transfer under way
    /// (a kqueue filter's racy read of `f_offset`).
    pub(crate) fn offset_hint(&self) -> u64 {
        self.offset.try_lock().map_or(0, |o| *o)
    }

    /// An `ioctl` request that is not about the descriptor itself; `arg`
    /// is its argument, copied in and out by the caller. Returns the value
    /// the call returns (0 for most requests).
    pub(crate) fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
        let r = self.ioctl_inner(s, req, arg);
        if r == Err(Errno::ENOTTY) {
            self.report_ioctl(req);
        }
        r
    }

    /// Reports a request this file did not know that FreeBSD may answer
    /// ([`crate::firstuse`]): any on a device but a terminal request
    /// (`isatty` sends `TIOCGETA` to everything, and FreeBSD's non-terminal
    /// devices refuse it too), and the generic file requests (`FIO*`) on
    /// anything else.
    fn report_ioctl(&self, req: u32) {
        let group = (req >> 8) & 0xff;
        let what = match &self.kind {
            FileKind::Vnode(VnodeFile {
                device: Some(_),
                vnode,
                ..
            }) if group != u32::from(b't') => vnode.named().map(|(_, n)| n),
            FileKind::Pipe(_) if group == u32::from(b'f') => Some(b"pipe".to_vec()),
            FileKind::Fifo(_) if group == u32::from(b'f') => Some(b"fifo".to_vec()),
            FileKind::Socket(_) | FileKind::Inet(_) | FileKind::Netlink(_)
                if group == u32::from(b'f') =>
            {
                Some(b"socket".to_vec())
            }
            // `ifioctl`'s and `rtioctl`'s, on any socket.
            FileKind::Socket(_) | FileKind::Inet(_) | FileKind::Netlink(_)
                if group == u32::from(b'i') || group == u32::from(b'r') =>
            {
                Some(b"socket".to_vec())
            }
            FileKind::Vnode(_) if group == u32::from(b'f') => Some(b"vnode".to_vec()),
            _ => None,
        };
        if let Some(what) = what {
            crate::firstuse::report(
                crate::firstuse::Kind::Ioctl,
                format_args!(
                    "{} on {}",
                    crate::firstuse::IoctlCmd(req),
                    core::str::from_utf8(&what).unwrap_or("?")
                ),
            );
        }
    }

    fn ioctl_inner(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
        let put = |arg: &mut [u8], v: usize| {
            let v = i32::try_from(v).unwrap_or(i32::MAX);
            arg.copy_from_slice(&v.to_ne_bytes());
        };
        match &self.kind {
            FileKind::Process(_) | FileKind::Jail(_) => Err(Errno::ENOTTY),
            // `soo_ioctl`: the interface and routing requests on any
            // socket (`ifioctl`), then the socket's own.
            FileKind::Socket(_) | FileKind::Inet(_) | FileKind::Netlink(_)
                if crate::net::ioctl::is_interface_request(req) =>
            {
                crate::net::ioctl::ifioctl(s, req, arg, self.inet_family())
            }
            FileKind::Inet(so) => crate::net::ioctl::soioctl(&so.so, req, arg),
            FileKind::Netlink(nl) => nl.ioctl(req, arg),
            FileKind::Socket(so) => match req {
                FIONBIO | FIOASYNC => Ok(0),
                FIONREAD => {
                    put(arg, so.so.nread());
                    Ok(0)
                }
                abi::ioctl::FIONWRITE => {
                    put(arg, 0);
                    Ok(0)
                }
                abi::ioctl::FIONSPACE => {
                    put(arg, so.so.send_space());
                    Ok(0)
                }
                _ => Err(Errno::ENOTTY),
            },
            // `kqueue_ioctl`, `timerfd_ioctl`, and `eventfd_ioctl`: the
            // generic layer already set the flag.
            FileKind::Kqueue(_) | FileKind::Timerfd(_) | FileKind::Eventfd(_) => match req {
                FIONBIO | FIOASYNC => Ok(0),
                _ => Err(Errno::ENOTTY),
            },
            // `inotify_ioctl`.
            FileKind::Inotify(i) => match req {
                FIONBIO | FIOASYNC => Ok(0),
                FIONREAD => {
                    put(arg, i.nread());
                    Ok(0)
                }
                _ => Err(Errno::ENOTTY),
            },
            // Non-blocking and asynchronous mean nothing to memory, but
            // `fcntl(F_SETFL)` must work (`shm_ioctl`).
            FileKind::Shm(_) => match req {
                FIONBIO | FIOASYNC => Ok(0),
                _ => Err(Errno::ENOTTY),
            },
            FileKind::Fifo(f) => match req {
                FIONREAD => {
                    put(arg, f.readable(s));
                    Ok(0)
                }
                _ => Err(Errno::ENOTTY),
            },
            FileKind::Pipe(p) => match req {
                FIONREAD => {
                    put(arg, p.readable());
                    Ok(0)
                }
                _ => Err(Errno::ENOTTY),
            },
            FileKind::Pts(m) => m.ioctl(s, req, arg, self.flags()),
            FileKind::Vnode(f) => match (&f.device, f.vnode.kind()) {
                // A revoked node's (`vn_ioctl` on a dead vnode).
                (Some(_), _) if f.dead() => Err(Errno::ENOTTY),
                // Every device answers its kind and its name (FreeBSD's
                // devfs does).
                (Some(d), _) if req == FIODTYPE => {
                    arg.copy_from_slice(&d.ops.dtype().to_ne_bytes());
                    Ok(0)
                }
                (Some(_), _) if req == abi::ioctl::FIODGNAME => {
                    let name = super::devfs::name_of(f.vnode.vnode()).ok_or(Errno::EINVAL)?;
                    crate::tty::pts::fiodgname(arg, &name)?;
                    Ok(0)
                }
                (Some(d), _) => d.ops.ioctl_file(s, req, arg, self.flags()),
                (None, Kind::Regular) if req == FIONREAD => {
                    let off = *self.offset.lock(s);
                    let size = f.vnode.size(s)?;
                    put(
                        arg,
                        usize::try_from(size.saturating_sub(off)).unwrap_or(usize::MAX),
                    );
                    Ok(0)
                }
                // devfs's rule requests (`devfs_rioctl`).
                (None, Kind::Directory) => super::devfs::rule_ioctl(f.vnode.vnode(), req, arg),
                _ => Err(Errno::ENOTTY),
            },
        }
    }
}

/// A transfer cut to its first `left` bytes.
struct Limited<'a> {
    uio: &'a mut dyn Uio,
    left: usize,
}

impl Uio for Limited<'_> {
    fn resid(&self) -> usize {
        self.uio.resid().min(self.left)
    }

    fn copy_out(&mut self, data: &[u8]) -> Result<usize, Errno> {
        let n = data.len().min(self.left);
        let n = self.uio.copy_out(&data[..n])?;
        self.left -= n;
        Ok(n)
    }

    fn copy_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let n = buf.len().min(self.left);
        let n = self.uio.copy_in(&mut buf[..n])?;
        self.left -= n;
        Ok(n)
    }
}

/// Writes `uio` into file `v` at `at`, held to `RLIMIT_FSIZE`: a write
/// past the limit stops there, and one starting at or past it is `EFBIG`
/// with `SIGXFSZ` to the process (FreeBSD's `vn_rlimit_fsizex`).
fn write_regular(s: &Sleepable, v: &Vnode, at: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
    let lim = crate::proc::Process::with_current(|p| {
        p.limits.lock()[abi::resource::rlimit::FSIZE as usize].rlim_cur
    })
    .unwrap_or(abi::resource::RLIM_INFINITY);
    let n = if v.kind() == vfs::Kind::Regular && at.saturating_add(uio.resid() as u64) > lim {
        if at >= lim {
            if let Some(p) = crate::proc::Process::current() {
                let _ = crate::signal::post(
                    &p,
                    crate::signal::user_info(abi::signal::SIGXFSZ, 0, abi::signal::code::SI_KERNEL),
                );
            }
            return Err(Errno::EFBIG);
        }
        let left = usize::try_from(lim - at).unwrap_or(usize::MAX);
        v.write_at(s, at, &mut Limited { uio, left })?
    } else {
        v.write_at(s, at, uio)?
    };
    clear_set_id(s, v, n)?;
    Ok(n)
}

/// After `written` bytes went into the file, by anyone without
/// `PRIV_VFS_RETAINSUGID`: its set-id bits go (FreeBSD's `ffs_write`,
/// `tmpfs_write`).
fn clear_set_id(s: &Sleepable, v: &Vnode, written: usize) -> Result<(), Errno> {
    if written == 0 || super::priv_check(crate::cred::Priv::VfsRetainsugid).is_ok() {
        return Ok(());
    }
    let mode = v.attr(s)?.mode;
    if mode & (S_ISUID | S_ISGID) == 0 {
        return Ok(());
    }
    let a = vfs::SetAttr {
        mode: Some(mode & !(S_ISUID | S_ISGID)),
        ..vfs::SetAttr::default()
    };
    // An append-only file refuses `setattr`; FreeBSD clears the bits
    // anyway, and the write itself has happened either way.
    match vfs::ops::setattr(s, v, &a, &vfs::Cred::root(), false) {
        Ok(()) | Err(Errno::EPERM) => Ok(()),
        Err(e) => Err(e),
    }
}

impl socket::Passable for File {
    fn unix(&self) -> Option<&Arc<socket::Socket>> {
        match &self.kind {
            FileKind::Socket(so) => Some(&so.so),
            _ => None,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
}

impl File {
    /// A file back from a message in flight.
    pub(crate) fn from_passable(f: Arc<dyn socket::Passable>) -> Option<Arc<Self>> {
        f.into_any().downcast::<Self>().ok()
    }
}
