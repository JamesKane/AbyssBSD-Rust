// SPDX-License-Identifier: BSD-2-Clause
//! Vnodes (docs/VFS.md §4): the one in-memory object for an inode while
//! anything refers to it.
//!
//! An `Arc<Vnode>` keeps the vnode valid; a [`VnodeUse`] keeps the file in
//! use (open, a working directory, mapped). The node the file system
//! loaded sits behind the vnode's lock in [`State::Live`] until the vnode
//! is reclaimed or doomed (forced unmount), after which every operation
//! fails as FreeBSD's dead vnodes do.
//!
//! Behavior reference: sys/kern/vfs_subr.c (`vget`, `vrele`, `vinactive`,
//! `vgone`), sys/kern/vfs_vnops.c (`vn_stat`).

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::stat::Stat;
use sched::Sleepable;
use sched::lock::{LockClass, ReadGuard, SxLock, WriteGuard};
use sync::PreemptLock;

use crate::mount::Mount;
use crate::node::{Attr, Device, DirEntry, Kind, NodeOps};
use crate::{NodeId, Uio};

/// The entry naming a directory: its parent and its name.
type Named = Option<(Weak<Vnode>, Box<[u8]>)>;

/// Vnode locks nest parent before child, and unrelated ones by address
/// (docs/VFS.md §5), which the witness cannot see.
static VNODE: LockClass = LockClass::new_dup_ok("vnode", 340);

/// What a vnode holds.
#[derive(Debug)]
pub enum State {
    Live(NodeOps),
    /// Reclaimed or forcibly unmounted: nothing left to operate on.
    Doomed,
}

impl State {
    /// The node, or `EBADF` for a doomed vnode.
    pub fn live(&self) -> Result<&NodeOps, Errno> {
        match self {
            Self::Live(n) => Ok(n),
            Self::Doomed => Err(Errno::EBADF),
        }
    }

    pub fn live_mut(&mut self) -> Result<&mut NodeOps, Errno> {
        match self {
            Self::Live(n) => Ok(n),
            Self::Doomed => Err(Errno::EBADF),
        }
    }
}

/// A file system object in core.
pub struct Vnode {
    mount: Arc<Mount>,
    /// Its mount's id: with `ino` and `kind`, what a lockless lookup reads
    /// (plain fields, never changed, so readable without a reference; see
    /// [`Peek`]).
    fs: u64,
    ino: u64,
    kind: Kind,
    state: SxLock<State>,
    uses: AtomicU32,
    /// The file system mounted on this directory, if any.
    mounted: PreemptLock<Option<Arc<Mount>>>,
    /// That file system's root vnode, for lockless lookups to cross into
    /// it: published by mounting, cleared by unmounting before the root can
    /// go (the unmount waits out lockless readers).
    mounted_root: AtomicPtr<Vnode>,
    /// The entry that names this directory: its parent and its name, for
    /// `getcwd` (FreeBSD's `v_cache_dd`).
    named: PreemptLock<Named>,
    /// Its name-cache entries, so a purge finds them.
    pub(crate) cache_links: PreemptLock<crate::cache::Links>,
    /// Advisory locks (`fcntl`, `flock`).
    lockf: crate::lockf::Lockf,
    /// When it was last looked up, in [`tick`]s: the reclaimer takes the
    /// longest unused first.
    used: AtomicU64,
    /// The reclaimer is taking it ([`Mount::prune`](crate::Mount::prune)):
    /// a weak reference upgraded meanwhile (the name cache's) must let it
    /// go, as FreeBSD's `vget` fails on a vnode being doomed.
    dying: core::sync::atomic::AtomicBool,
    /// Open files writing it (positive) or programs running it
    /// (negative), never both (FreeBSD's `v_writecount`).
    writecount: core::sync::atomic::AtomicI64,
}

/// A counter standing in for time, for vnode recency.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// The next recency tick (a relaxed counter: an approximate order is
/// enough, and no locked instruction is spent per lookup).
fn tick() -> u64 {
    let t = TICKS.load(Ordering::Relaxed) + 1;
    TICKS.store(t, Ordering::Relaxed);
    t
}

impl core::fmt::Debug for Vnode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Vnode({:#x}:{} {:?})",
            self.mount.id, self.ino, self.kind
        )
    }
}

impl Vnode {
    pub(crate) fn new(mount: Arc<Mount>, ino: u64, ops: NodeOps) -> Self {
        Self {
            kind: ops.kind(),
            fs: mount.id,
            mount,
            ino,
            state: SxLock::new(&VNODE, State::Live(ops)),
            uses: AtomicU32::new(0),
            mounted: PreemptLock::new(None),
            mounted_root: AtomicPtr::new(core::ptr::null_mut()),
            named: PreemptLock::new(None),
            cache_links: PreemptLock::new(crate::cache::Links::new()),
            lockf: crate::lockf::Lockf::new(),
            used: AtomicU64::new(tick()),
            dying: core::sync::atomic::AtomicBool::new(false),
            writecount: core::sync::atomic::AtomicI64::new(0),
        }
    }

    /// Whether the reclaimer is taking it (see `dying`).
    pub(crate) fn is_dying(&self) -> bool {
        self.dying.load(Ordering::SeqCst)
    }

    /// Counts an open file writing it: `ETXTBSY` while a program runs it.
    pub fn add_writer(&self) -> Result<(), Errno> {
        self.writecount
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n >= 0).then_some(n + 1)
            })
            .map(drop)
            .map_err(|_| Errno::ETXTBSY)
    }

    /// An open file writing it closed.
    pub fn drop_writer(&self) {
        self.writecount.fetch_sub(1, Ordering::AcqRel);
    }

    /// Counts a program running it: `ETXTBSY` while a file writes it
    /// (FreeBSD's `VOP_SET_TEXT`).
    pub fn add_text(&self) -> Result<(), Errno> {
        self.writecount
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n <= 0).then_some(n - 1)
            })
            .map(drop)
            .map_err(|_| Errno::ETXTBSY)
    }

    /// A program running it ended or replaced itself.
    pub fn drop_text(&self) {
        self.writecount.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether a program runs it (FreeBSD's `VV_TEXT`).
    #[must_use]
    pub fn is_text(&self) -> bool {
        self.writecount.load(Ordering::Acquire) < 0
    }

    pub(crate) fn set_dying(&self, dying: bool) {
        self.dying.store(dying, Ordering::SeqCst);
    }

    /// Records a use, for the reclaimer's order.
    pub fn touch(&self) {
        self.used.store(tick(), Ordering::Relaxed);
    }

    /// When it was last used.
    #[must_use]
    pub fn last_used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// The advisory locks on this file.
    #[must_use]
    pub fn lockf(&self) -> &crate::lockf::Lockf {
        &self.lockf
    }

    #[must_use]
    pub const fn ino(&self) -> u64 {
        self.ino
    }

    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    #[must_use]
    pub const fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Directory)
    }

    #[must_use]
    pub fn mount(&self) -> &Arc<Mount> {
        &self.mount
    }

    #[must_use]
    pub fn id(&self) -> NodeId {
        NodeId {
            fs: self.fs,
            ino: self.ino,
        }
    }

    /// Publishes (or with `None`, withdraws) the root vnode of the file
    /// system mounted here, for lockless lookups. A withdrawn root may go
    /// only after [`crate::cache::synchronize`].
    pub(crate) fn set_mounted_root(&self, root: Option<&Arc<Vnode>>) {
        let p = root.map_or(core::ptr::null_mut(), |r| Arc::as_ptr(r).cast_mut());
        self.mounted_root.store(p, Ordering::Release);
    }

    /// Whether this is its file system's root.
    #[must_use]
    pub fn is_mount_root(&self) -> bool {
        self.ino == self.mount.fs().root()
    }

    /// Use references held.
    #[must_use]
    pub fn uses(&self) -> u32 {
        self.uses.load(Ordering::Relaxed)
    }

    /// Shared access to the node.
    pub fn read(&self, s: &Sleepable) -> ReadGuard<'_, State> {
        self.state.read(s)
    }

    /// Exclusive access to the node.
    pub fn write(&self, s: &Sleepable) -> WriteGuard<'_, State> {
        self.state.write(s)
    }

    /// Exclusive access if nobody holds the lock now.
    pub fn try_write(&self) -> Option<WriteGuard<'_, State>> {
        self.state.try_write()
    }

    /// After an unlink: with no use left, the file goes now.
    pub(crate) fn release_if_unlinked(self: &Arc<Self>, s: &Sleepable) {
        if self.uses() == 0 {
            self.inactive(s);
        }
    }

    /// A use reference: the file is in use (FreeBSD's `vref`).
    #[must_use]
    pub fn use_ref(self: &Arc<Self>) -> VnodeUse {
        self.uses.fetch_add(1, Ordering::Relaxed);
        VnodeUse(Arc::clone(self))
    }

    /// The file system mounted here.
    #[must_use]
    pub fn mounted(&self) -> Option<Arc<Mount>> {
        self.mounted.lock().clone()
    }

    pub(crate) fn set_mounted(&self, m: Option<Arc<Mount>>) {
        *self.mounted.lock() = m;
    }

    /// The parent and name recorded for this directory.
    #[must_use]
    pub fn named(&self) -> Option<(Arc<Vnode>, Vec<u8>)> {
        let n = self.named.lock();
        let (p, name) = n.as_ref()?;
        let p = p.upgrade().filter(|p| !p.is_dying())?;
        Some((p, name.to_vec()))
    }

    pub(crate) fn set_named(&self, parent: &Arc<Vnode>, name: &[u8]) {
        *self.named.lock() = Some((Arc::downgrade(parent), name.into()));
    }

    pub(crate) fn clear_named(&self) {
        *self.named.lock() = None;
    }

    pub fn attr(&self, s: &Sleepable) -> Result<Attr, Errno> {
        Ok(self.read(s).live()?.node().attr())
    }

    /// `struct stat` (FreeBSD's `vn_stat`).
    pub fn stat(&self, s: &Sleepable) -> Result<Stat, Errno> {
        Ok(self.stat_of(&self.attr(s)?))
    }

    /// `struct stat` from attributes already read.
    #[must_use]
    pub fn stat_of(&self, a: &Attr) -> Stat {
        let blksize = if a.blksize == 0 {
            self.mount.info().io_size
        } else {
            a.blksize
        };
        Stat {
            st_dev: self.mount.id,
            st_ino: if a.fileid != 0 { a.fileid } else { self.ino },
            st_nlink: a.nlink,
            st_mode: self.kind.ifmt() | (a.mode & 0o7777),
            st_uid: a.uid,
            st_gid: a.gid,
            st_rdev: a.rdev,
            st_atim: a.atime,
            st_mtim: a.mtime,
            st_ctim: a.ctime,
            st_birthtim: a.birthtime,
            st_size: i64::try_from(a.size).unwrap_or(i64::MAX),
            st_blocks: i64::try_from(a.bytes.div_ceil(512)).unwrap_or(i64::MAX),
            st_blksize: i32::try_from(blksize).unwrap_or(i32::MAX),
            st_flags: a.flags,
            st_gen: a.generation,
            ..Stat::default()
        }
    }

    /// The size `stat` reports.
    pub fn size(&self, s: &Sleepable) -> Result<u64, Errno> {
        Ok(self.attr(s)?.size)
    }

    /// Reads a regular file at `offset`.
    pub fn read_at(&self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        match self.read(s).live()? {
            NodeOps::Regular(f) => f.read(s, offset, uio),
            NodeOps::Directory(_) => Err(Errno::EISDIR),
            _ => Err(Errno::EINVAL),
        }
    }

    /// Writes a regular file at `offset`.
    pub fn write_at(&self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        if self.mount.read_only() {
            return Err(Errno::EROFS);
        }
        match self.write(s).live_mut()? {
            NodeOps::Regular(f) => f.write(s, offset, uio),
            NodeOps::Directory(_) => Err(Errno::EISDIR),
            _ => Err(Errno::EINVAL),
        }
    }

    /// The contents of a memory-resident regular file.
    pub fn resident(&self, s: &Sleepable) -> Option<&'static [u8]> {
        match self.read(s).live().ok()? {
            NodeOps::Regular(f) => f.resident(),
            _ => None,
        }
    }

    /// The VM object holding a regular file's pages, if its file system
    /// keeps one.
    pub fn object(&self, s: &Sleepable) -> Option<Arc<vm::object::Object>> {
        match self.read(s).live().ok()? {
            NodeOps::Regular(f) => f.object(),
            _ => None,
        }
    }

    /// Writes a file's dirty pages, then its metadata (`fsync`).
    pub fn fsync(&self, s: &Sleepable) -> Result<(), Errno> {
        // The pages go with the vnode unlocked: a pager may lock it.
        if let Some(o) = self.object(s) {
            o.sync(0, u64::MAX).map_err(|_| Errno::EIO)?;
        }
        if self.mount().read_only() {
            return Ok(());
        }
        let mut st = self.write(s);
        st.live_mut()?.node_mut().fsync(s)
    }

    /// A symbolic link's target.
    pub fn readlink(&self, s: &Sleepable) -> Result<Vec<u8>, Errno> {
        match self.read(s).live()? {
            NodeOps::Symlink(l) => l.target(s),
            _ => Err(Errno::EINVAL),
        }
    }

    /// The device behind a devfs node.
    pub fn device(&self, s: &Sleepable) -> Result<Device, Errno> {
        match self.read(s).live()? {
            NodeOps::Device(d) => Ok(d.device()),
            // A device node outside devfs (FreeBSD: `ENXIO`).
            NodeOps::Other(Kind::CharDevice | Kind::BlockDevice, _) => Err(Errno::ENXIO),
            _ => Err(Errno::ENODEV),
        }
    }

    /// Calls `f` with a directory's entries from cookie `from` on.
    pub fn readdir(
        &self,
        s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        match self.read(s).live()? {
            NodeOps::Directory(d) => d.readdir(s, from, f),
            _ => Err(Errno::ENOTDIR),
        }
    }

    /// Reclaims the vnode: the node goes back to its file system, the
    /// vnode leaves its mount's table and the name cache, and holders of
    /// references find it doomed (FreeBSD's `vgone`).
    pub fn reclaim(self: &Arc<Self>, s: &Sleepable) {
        crate::cache::purge(self);
        let ops = {
            let mut st = self.write(s);
            core::mem::replace(&mut *st, State::Doomed)
        };
        if let State::Live(ops) = ops {
            ops.reclaim(s);
        }
        self.mount.forget(self);
    }

    /// Whether it was doomed.
    pub fn is_doomed(&self, s: &Sleepable) -> bool {
        matches!(*self.read(s), State::Doomed)
    }

    /// The last use went (FreeBSD's `vinactive`): the file system frees an
    /// unlinked file, and an unlinked vnode is reclaimed at once, since no
    /// name can find it again.
    fn inactive(self: &Arc<Self>, s: &Sleepable) {
        let unlinked = {
            let mut st = self.write(s);
            let Ok(ops) = st.live_mut() else {
                return;
            };
            let node = ops.node_mut();
            node.inactive(s);
            node.attr().nlink == 0
        };
        // A use taken meanwhile keeps it.
        if unlinked && self.uses() == 0 {
            self.reclaim(s);
        }
    }
}

/// What a lockless lookup reads of a vnode it holds no reference to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Peek {
    pub(crate) id: NodeId,
    pub(crate) kind: Kind,
    pub(crate) dying: bool,
    /// The root of what is mounted here.
    pub(crate) mounted_root: *const Vnode,
}

impl Peek {
    /// Reads `v`'s fields that never change, and its atomics.
    ///
    /// # Safety
    ///
    /// `v`'s allocation must stay for the call: a weak reference (a
    /// name-cache entry's, within an epoch section) is enough. Its value
    /// may already be dropped: only plain fields and atomics, which a drop
    /// leaves as they were, are read, through the pointer.
    #[allow(unsafe_code)]
    pub(crate) unsafe fn of(v: *const Vnode) -> Self {
        // SAFETY: the allocation stays (the caller's contract); the fields
        // read are `Copy` or atomic, set at creation, and no reference to
        // the whole (possibly dropped) value is made.
        unsafe {
            Self {
                id: NodeId {
                    fs: core::ptr::addr_of!((*v).fs).read(),
                    ino: core::ptr::addr_of!((*v).ino).read(),
                },
                kind: core::ptr::addr_of!((*v).kind).read(),
                dying: (*core::ptr::addr_of!((*v).dying)).load(Ordering::Acquire),
                mounted_root: (*core::ptr::addr_of!((*v).mounted_root))
                    .load(Ordering::Acquire)
                    .cast_const(),
            }
        }
    }
}

/// A use reference (FreeBSD's `v_usecount`): held by open files, working
/// and root directories, and mappings. The last one going runs the file
/// system's `inactive`.
#[derive(Debug)]
pub struct VnodeUse(Arc<Vnode>);

impl VnodeUse {
    #[must_use]
    pub fn vnode(&self) -> &Arc<Vnode> {
        &self.0
    }
}

impl core::ops::Deref for VnodeUse {
    type Target = Arc<Vnode>;
    fn deref(&self) -> &Arc<Vnode> {
        &self.0
    }
}

impl Clone for VnodeUse {
    fn clone(&self) -> Self {
        self.0.use_ref()
    }
}

impl Drop for VnodeUse {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if self.0.uses.fetch_sub(1, Ordering::AcqRel) == 1 {
            // SAFETY: use references are dropped in thread context (closing
            // a file, changing directory, exiting), where the caller could
            // sleep.
            let s = unsafe { Sleepable::assume() };
            self.0.inactive(&s);
        }
    }
}
