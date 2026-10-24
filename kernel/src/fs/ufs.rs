// SPDX-License-Identifier: BSD-2-Clause
//! UFS2 (P5.4, P5.5; docs/VFS.md §10.3): `lib/ufs` over a disk, read-only
//! or read-write.
//!
//! Metadata (the superblock, groups, inodes, indirect and directory
//! blocks) goes through the disk's buffer cache. File data goes through
//! the page cache: each regular file's VM object has a pager that maps
//! file blocks with `bmap_run` and reads each contiguous run straight from
//! the disk, and writes pages back the same way (allocating a hole a
//! mapping wrote into), as FreeBSD's vnode pager does over `VOP_BMAP`.
//!
//! An in-core inode lives in a sleepable lock that its vnode's node and
//! its file's pager share. Lock order: the vnode lock, then inode locks,
//! then the allocation summary, then buffers. The pager takes only its
//! own inode's lock, so `write` allocates under it and copies through the
//! page cache with it released.
//!
//! A read-write mount clears the clean flag (refusing an unclean file
//! system unless forced, or unless it has soft updates, as FreeBSD does)
//! and sets it again at unmount. Changed inodes are written lazily: by
//! `fsync`, `sync`, the syncer, or when the vnode goes.
//!
//! A file system with soft updates on (`FS_DOSOFTDEP`, as `newfs -U` and
//! `tunefs -n enable` set) writes all metadata delayed, its order kept by
//! `lib/ufs`'s tracker (docs/VFS.md §10.3): the buffer cache asks it before
//! each write. What waits for the disk (frees, link counts dropping) is
//! done by the syncer once a second, through in-core inodes shared with
//! the vnodes'. Without soft updates, ordered metadata is written
//! synchronously, as FreeBSD's UFS does without them.

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::time::Timespec;
use sched::lock::{LockClass, Mutex, MutexGuard};
use sched::{Sleepable, WaitQueue};
use sync::{Once, PreemptLock};
use ufs::ops::{Maker, NewKind};
use ufs::{Mode, Reserve, Timestamp};

use crate::fs::block::{DiskIo, SoftDeps};
use vfs::{
    Attr, Create, DirEntry, DirectoryNode, FileSystem, FsStats, Kind, Made, NewNode, Node, NodeOps,
    Rename, SetAttr, SymlinkNode, Uio,
};
use vm::object::{FreshPages, IoBuf, Object, PageIndex, Pager, PagerError, PendingFill};

/// Doing soft-updates work: under vnode locks, before inodes.
static WORK: LockClass = LockClass::new("ufs work", 342);
/// [`Shared::flush_frees`], one at a time: after the vnode locks, before
/// the work.
static FLUSHING: LockClass = LockClass::new("ufs flush", 341);
/// In-core inodes: under vnode locks, before the summary and buffers.
static INODE: LockClass = LockClass::new_dup_ok("ufs inode", 350);
/// The allocation summary (FreeBSD's `UFS_LOCK`).
static SUMMARY: LockClass = LockClass::new("ufs summary", 370);

/// Soft-updates items past which a writer waits for them (FreeBSD's
/// `max_softdeps`, `softdep_slowdown`): four per vnode, as FreeBSD sizes it.
fn softdep_limit() -> usize {
    super::max_vnodes().saturating_mul(4)
}

/// Soft-updates items past which the syncer writes everything each second
/// (FreeBSD's `softdep_speedup`).
fn softdep_speedup() -> usize {
    softdep_limit() / 4
}

/// An in-core inode, shared by its node and its file's pager. In order
/// (`repr(C)`): what a `stat` reads first, then the lock and the inode.
#[repr(C)]
struct InodeCell {
    stat: StatCopy,
    /// Taken through [`Self::lock`], which keeps `stat` up to date.
    state: Mutex<InodeState>,
    ino: u32,
    /// The size, readable with no lock (the VM asks a pager for it under
    /// the map lock); published whenever a regular file's size changes.
    size: AtomicU64,
    /// Its data writes under way.
    writing: Writing,
}

/// Bytes of file data being written without waiting, on every UFS mount
/// (FreeBSD's `runningbufspace`), and the most a writer may start more
/// past (`hirunningspace`): beyond it, it waits for some to finish.
static RUNNING: AtomicU64 = AtomicU64::new(0);
static RUNNING_ROOM: WaitQueue = WaitQueue::new();
const HI_RUNNING: u64 = 16 << 20;

/// Waits while too much is being written (FreeBSD's
/// `waitrunningbufspace`). The caller holds no lock.
fn wait_running(s: &Sleepable) {
    RUNNING_ROOM.wait_until(s, || RUNNING.load(Ordering::Acquire) < HI_RUNNING);
}

/// A file's block writes started without waiting and not done yet
/// (FreeBSD's busy buffers under a `bawrite`): a block has one at a time,
/// a later write of it waits for the earlier to finish, and nothing frees
/// blocks under one.
struct Writing {
    blocks: PreemptLock<BTreeSet<u64>>,
    done: WaitQueue,
}

impl Writing {
    const fn new() -> Self {
        Self {
            blocks: PreemptLock::new(BTreeSet::new()),
            done: WaitQueue::new(),
        }
    }

    /// Claims blocks `lbns` for a write; none if any is already being
    /// written.
    fn start(&self, lbns: core::ops::RangeInclusive<u64>) -> bool {
        let mut b = self.blocks.lock();
        if b.range(lbns.clone()).next().is_some() {
            return false;
        }
        b.extend(lbns);
        true
    }

    /// The write of `lbns` is done. Any context.
    fn finish(&self, lbns: core::ops::RangeInclusive<u64>) {
        {
            let mut b = self.blocks.lock();
            for lbn in lbns {
                b.remove(&lbn);
            }
        }
        self.done.wake_all();
    }

    /// Waits until no write of `lbns` is under way.
    fn wait(&self, s: &Sleepable, lbns: core::ops::RangeInclusive<u64>) {
        self.done.wait_until(s, || {
            self.blocks.lock().range(lbns.clone()).next().is_none()
        });
    }

    /// Waits until none of the file's writes is under way.
    fn wait_all(&self, s: &Sleepable) {
        self.wait(s, 0..=u64::MAX);
    }
}

impl InodeCell {
    fn publish(&self, st: &InodeState) {
        self.size.store(st.ip.size, Ordering::Release);
    }

    /// The inode, locked; what changes in it reaches [`Self::stat`] as the
    /// lock is let go.
    fn lock<'a>(&'a self, s: &Sleepable) -> StateGuard<'a> {
        StateGuard {
            cell: self,
            st: self.state.lock(s),
            changed: false,
        }
    }
}

/// What `stat` reads of an inode, kept beside it so that reading it takes
/// no lock (FreeBSD's `ufs_getattr` reads the inode under the vnode lock
/// alone, which our deferred work does not take): rewritten under the
/// inode's lock by whoever may have changed it, behind a sequence count
/// that is odd while it changes, which readers retry on.
struct StatCopy {
    seq: AtomicU64,
    words: [AtomicU64; STAT_WORDS],
}

const STAT_WORDS: usize = 12;

impl StatCopy {
    fn new(ip: &ufs::Inode) -> Self {
        Self {
            seq: AtomicU64::new(0),
            words: Self::encode(ip).map(AtomicU64::new),
        }
    }

    fn encode(ip: &ufs::Inode) -> [u64; STAT_WORDS] {
        let rdev = match ip.kind() {
            ufs::Kind::CharDevice | ufs::Kind::BlockDevice => ip.rdev(),
            _ => 0,
        };
        let pair = |lo: u32, hi: u32| u64::from(lo) | u64::from(hi) << 32;
        [
            pair(ip.uid, ip.gid),
            pair(u32::from(ip.mode), ip.flags),
            pair(
                u32::from(ip.effnlink) | u32::from(ip.nlink) << 16,
                ip.generation,
            ),
            ip.size,
            ip.blocks,
            ip.atime.sec.cast_unsigned(),
            ip.mtime.sec.cast_unsigned(),
            ip.ctime.sec.cast_unsigned(),
            ip.birthtime.sec.cast_unsigned(),
            pair(ip.atime.nsec, ip.mtime.nsec),
            pair(ip.ctime.nsec, ip.birthtime.nsec),
            rdev,
        ]
    }

    /// Rewrites it from `ip`; the caller holds the inode's lock, so
    /// writers take turns.
    fn store(&self, ip: &ufs::Inode) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq + 1, Ordering::Relaxed);
        core::sync::atomic::fence(Ordering::Release);
        for (w, v) in self.words.iter().zip(Self::encode(ip)) {
            w.store(v, Ordering::Relaxed);
        }
        self.seq.store(seq + 2, Ordering::Release);
    }

    /// A consistent copy of the words.
    fn load(&self) -> [u64; STAT_WORDS] {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 0 {
                let words = self.words.each_ref().map(|w| w.load(Ordering::Relaxed));
                core::sync::atomic::fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == before {
                    return words;
                }
            }
            core::hint::spin_loop();
        }
    }
}

/// An inode locked by [`InodeCell::lock`]: let go after any mutable use,
/// it rewrites the cell's [`StatCopy`] first, still holding the lock.
struct StateGuard<'a> {
    cell: &'a InodeCell,
    st: MutexGuard<'a, InodeState>,
    changed: bool,
}

impl core::ops::Deref for StateGuard<'_> {
    type Target = InodeState;

    fn deref(&self) -> &InodeState {
        &self.st
    }
}

impl core::ops::DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut InodeState {
        self.changed = true;
        &mut self.st
    }
}

impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        if self.changed {
            self.cell.stat.store(&self.st.ip);
        }
    }
}

/// The inode first (`repr(C)`), its attributes right after the lock.
#[repr(C)]
struct InodeState {
    ip: ufs::Inode,
    /// Changed since last written.
    dirty: bool,
    /// Nodes (vnodes) using it; deferred work can hold it with none.
    nodes: u32,
    /// Bumped by whatever may change the file's data on the disk or where
    /// it lies (writes, truncation, write-back), so read-ahead that went
    /// out before is dropped rather than put in stale.
    data_gen: u64,
    /// Its writes' cluster, for write-behind.
    cluster: Cluster,
    /// File byte ranges of blocks a write just allocated and has not yet
    /// filled: they read as zeros, never from the disk under them (the
    /// write's own pages take its bytes straight, unzeroed, as FreeBSD's
    /// `ffs_write` skips `BA_CLRBUF` for a block it covers).
    unwritten: Vec<(u64, u64)>,
}

/// The run of a file's blocks filled one after another and not written
/// yet, and where its last write ended (FreeBSD's `v_cstart`, `v_clen`,
/// and `seqcount`): what [`Self::filled`] says goes to the disk now.
#[derive(Default)]
struct Cluster {
    start: u64,
    len: u64,
    next: u64,
}

impl Cluster {
    /// A write filled block `lbn` (FreeBSD's `cluster_write`): the runs
    /// to write now, as (first block, blocks), go into `out`. A block
    /// next in the run joins it, and a run `max` long goes; a block at the
    /// end of the file goes at once, there being nothing after it to
    /// cluster with; any other starts a new run, the old one going if the
    /// writes were sequential (else it waits for a sync, as random
    /// writes' blocks do).
    fn filled(
        &mut self,
        lbn: u64,
        at_end: bool,
        sequential: bool,
        max: u64,
        out: &mut Vec<(u64, u64)>,
    ) {
        if self.len > 0 && lbn == self.start + self.len {
            self.len += 1;
            if self.len >= max {
                out.push((self.start, self.len));
                self.len = 0;
            }
            return;
        }
        if self.len > 0 && sequential {
            out.push((self.start, self.len));
        }
        self.len = 0;
        if at_end {
            out.push((lbn, 1));
        } else {
            self.start = lbn;
            self.len = 1;
        }
    }
}

/// An in-core inode, and its object if it is a regular file.
type InCore = (Weak<InodeCell>, Option<Weak<Object>>);

/// What every node of one mount shares.
struct Shared {
    ufs: ufs::Ufs<DiskIo>,
    /// The allocation state, loaded when first mounted read-write.
    sum: Once<Mutex<ufs::Summary>>,
    /// Mounted read-write now (`MNT_UPDATE` changes it).
    writable: AtomicBool,
    /// The in-core inodes, and the regular files' objects, for `sync`.
    inodes: PreemptLock<intmap::IntMap<u32, InCore>>,
    /// When the syncer last wrote every file's dirty pages.
    data_synced: AtomicU64,
    /// Bumped with every change to `inodes` (under its lock): one who
    /// read an inode from the disk with none in core puts it in only if
    /// nothing changed meanwhile, so one inode is never in core twice and
    /// never stale (FreeBSD's `vfs_hash_insert` races the insert the
    /// same way, with no lock held across the read).
    changes: AtomicU64,
    /// Held while doing soft-updates work, so a writer short of space
    /// waits for frees another thread has taken but not yet done.
    working: Mutex<()>,
    /// Set while work is being done, before it is taken.
    applying: AtomicBool,
    /// Held by [`Shared::flush_frees`]: its writing can come back to it
    /// (a page written into a hole, out of space), which must not go round
    /// again.
    flushing: Mutex<()>,
    /// With soft updates, the tracker (idle while read-only).
    deps: Option<Arc<SoftDeps>>,
    /// The superblock asked for a journal, which this mount turned off.
    journal_dropped: bool,
    /// Its disk went away and a forced unmount is under way (FreeBSD's
    /// `UM_FSFAIL_CLEANUP`): writing out fails, and that is no error.
    failing: AtomicBool,
}

/// A mounted UFS2 file system.
pub(crate) struct UfsFs {
    shared: Arc<Shared>,
}

impl core::fmt::Debug for UfsFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "UfsFs({})", self.shared.ufs.io().disk.name())
    }
}

fn errno(e: ufs::Error) -> Errno {
    match e {
        ufs::Error::Io => Errno::EIO,
        ufs::Error::NotUfs2 => Errno::EINVAL,
        ufs::Error::Corrupt(what) => {
            kprintln!("ufs: corrupt: {what}");
            Errno::EIO
        }
        ufs::Error::Integrity(what) => {
            kprintln!("ufs: {what} failed");
            Errno::EINTEGRITY
        }
        ufs::Error::ReadOnly => Errno::EROFS,
        ufs::Error::NoSpace => Errno::ENOSPC,
        ufs::Error::TooBig => Errno::EFBIG,
        ufs::Error::Exists => Errno::EEXIST,
        ufs::Error::NotFound => Errno::ENOENT,
        ufs::Error::NotEmpty => Errno::ENOTEMPTY,
        ufs::Error::NameTooLong => Errno::ENAMETOOLONG,
        ufs::Error::TooManyLinks => Errno::EMLINK,
    }
}

fn stamp(t: Timespec) -> Timestamp {
    Timestamp {
        sec: t.tv_sec,
        nsec: u32::try_from(t.tv_nsec).unwrap_or(0),
    }
}

fn now() -> Timestamp {
    stamp(vfs::now())
}

fn random() -> u32 {
    let mut b = [0u8; 4];
    let _ = crate::random::fill(&mut b);
    u32::from_le_bytes(b)
}

/// Seconds since the epoch, for the superblock's `fs_time`.
fn wall_seconds() -> i64 {
    vfs::now().tv_sec
}

impl UfsFs {
    /// Mounts the UFS2 file system on `disk`; read-write unless
    /// `read_only`. An unclean file system mounts read-write only when
    /// `force`d (FreeBSD's `ffs_mountfs`).
    pub(crate) fn mount(
        s: &Sleepable,
        disk: Arc<storage::Disk>,
        read_only: bool,
        force: bool,
    ) -> Result<Self, Errno> {
        let sector = disk.sector_size();
        let io = DiskIo::new(disk);
        let mut ufs = ufs::Ufs::open(s, io, sector).map_err(errno)?;
        // Soft updates follow the superblock from the start, so a mount
        // made read-write later (`mount -u`) has its tracker.
        let mut deps = None;
        let journal_dropped = ufs.superblock().flags & ufs::sb::FS_SUJ != 0;
        if ufs.superblock().flags & ufs::sb::FS_DOSOFTDEP != 0 {
            // No journal here: soft updates alone, and the journal flag
            // off (written once read-write) so nothing replays a journal
            // this mount left stale.
            let flags = ufs.superblock().flags;
            if flags & ufs::sb::FS_SUJ != 0 {
                ufs.set_flags(flags & !ufs::sb::FS_SUJ);
            }
            deps = Some(SoftDeps::new(ufs.superblock()));
        }
        ufs.io().use_ufs(ufs.superblock(), deps.as_ref());
        if deps.is_some() {
            ufs.use_soft_updates(s);
        }
        let fs = Self {
            shared: Arc::new(Shared {
                ufs,
                sum: Once::new(),
                writable: AtomicBool::new(false),
                inodes: PreemptLock::new(intmap::IntMap::new()),
                data_synced: AtomicU64::new(0),
                changes: AtomicU64::new(0),
                working: Mutex::new(&WORK, ()),
                flushing: Mutex::new(&FLUSHING, ()),
                applying: AtomicBool::new(false),
                deps,
                journal_dropped,
                failing: AtomicBool::new(false),
            }),
        };
        let shared = Arc::downgrade(&fs.shared);
        fs.shared.ufs.io().on_gone(move || {
            if let Some(sh) = shared.upgrade() {
                sh.fsfail();
            }
        });
        if !read_only {
            fs.shared.make_writable(s, force)?;
        }
        Ok(fs)
    }

    /// Whether writes go with soft updates (`MNT_SOFTDEP`): on a
    /// read-write mount of a file system that has them.
    pub(crate) fn soft_updates(&self) -> bool {
        self.shared.deps.is_some() && self.shared.writable()
    }

    /// The block size, for the mount's I/O size.
    pub(crate) fn block_size(&self) -> u32 {
        self.shared.ufs.superblock().bsize
    }
}

impl Shared {
    /// The disk is gone: unmounts the file system forcibly, from a thread
    /// of its own, once (FreeBSD's `ffs_fsfail_cleanup`).
    fn fsfail(self: &Arc<Self>) {
        if self.failing.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = Arc::downgrade(self);
        let spawned = sched::Builder::new("ufsfail")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| {
                let mount = crate::fs::namespace().mounts().into_iter().find(|m| {
                    m.fs()
                        .as_any()
                        .downcast_ref::<UfsFs>()
                        .is_some_and(|u| core::ptr::eq(Arc::as_ptr(&u.shared), me.as_ptr()))
                });
                let Some(m) = mount else { return };
                if let Some(sh) = me.upgrade() {
                    kprintln!(
                        "UFS: forcibly unmounting /dev/{} from {}",
                        sh.ufs.io().disk.name(),
                        alloc::string::String::from_utf8_lossy(&m.on())
                    );
                }
                let _ = crate::fs::namespace().unmount(s, &m, true);
            });
        if spawned.is_err() {
            kprintln!("ufs: no thread for the forced unmount");
        }
    }

    /// Whether its disk went away.
    fn is_failing(&self) -> bool {
        self.failing.load(Ordering::Acquire)
    }

    fn sum(&self) -> Result<&Mutex<ufs::Summary>, Errno> {
        if !self.writable() {
            return Err(Errno::EROFS);
        }
        self.sum.get().ok_or(Errno::EROFS)
    }

    fn writable(&self) -> bool {
        self.writable.load(Ordering::Acquire)
    }

    /// Goes read-write: the summary loaded (counted from the groups with
    /// soft updates, whose summary may lag), and the file system marked
    /// dirty until unmounted cleanly. An unclean file system is refused
    /// unless `force`d or it has soft updates without a journal or an
    /// owed fsck (FreeBSD's `ffs_mountfs` and `ffs_reload` rules).
    fn make_writable(&self, s: &Sleepable, force: bool) -> Result<(), Errno> {
        if self.writable() {
            return Ok(());
        }
        let ufs = &self.ufs;
        let sb = ufs.superblock();
        let name = ufs.io().disk.name();
        let soft = self.deps.is_some();
        // With soft updates an unclean file system holds nothing but
        // leaks: it mounts, and fsck runs in the background (FreeBSD's
        // rule), unless a journal or fsck is owed first.
        let unclean_ok = soft && !self.journal_dropped && sb.flags & ufs::sb::FS_NEEDSFSCK == 0;
        if !sb.clean {
            if !force && !unclean_ok {
                kprintln!("ufs: {name} was not properly dismounted: read-write mount denied");
                return Err(Errno::EPERM);
            }
            kprintln!("ufs: WARNING: {name} was not properly dismounted");
        }
        if self.journal_dropped {
            kprintln!("ufs: {name}: journaled soft updates not supported; journal disabled");
        }
        let mut sum = if soft {
            ufs.summary_from_groups(s).map_err(errno)?
        } else {
            ufs.load_summary(s).map_err(errno)?
        };
        // Dirty until unmounted cleanly.
        ufs.write_summary(s, &mut sum, false, wall_seconds())
            .map_err(errno)?;
        match self.sum.get() {
            Some(m) => *m.lock(s) = sum,
            None => {
                let _ = self.sum.set(Mutex::new(&SUMMARY, sum));
            }
        }
        self.writable.store(true, Ordering::Release);
        Ok(())
    }

    /// Goes read-only: everything written, and the file system marked
    /// clean (FreeBSD's `ffs_mount` downgrade).
    fn make_read_only(&self, s: &Sleepable) -> Result<(), Errno> {
        if !self.writable() {
            return Ok(());
        }
        self.sync_all(s, true)?;
        self.writable.store(false, Ordering::Release);
        Ok(())
    }

    /// The in-core inodes still alive, with their objects.
    fn cells(&self) -> Vec<(Arc<InodeCell>, Option<Arc<Object>>)> {
        self.inodes
            .lock()
            .values()
            .filter_map(|(c, o)| Some((c.upgrade()?, o.as_ref().and_then(Weak::upgrade))))
            .collect()
    }

    /// Writes every in-core file's dirty pages.
    fn sync_pages(&self, s: &Sleepable) {
        // All under way at once (FreeBSD's `bawrite` of each file's
        // buffers), then waited for.
        let objects: Vec<Arc<Object>> = self.cells().into_iter().filter_map(|(_, o)| o).collect();
        for o in &objects {
            wait_running(s);
            o.start_sync(0, u64::MAX);
        }
        self.wait_writes(s);
        // What could not start (a hole to allocate, a block still being
        // written) goes now.
        for o in &objects {
            if o.dirty_pages() != 0 {
                let _ = o.sync(0, u64::MAX);
            }
        }
    }

    /// Waits for every in-core file's writes under way.
    fn wait_writes(&self, s: &Sleepable) {
        for (c, _) in self.cells() {
            c.writing.wait_all(s);
        }
    }

    /// Writes every changed in-core inode.
    fn write_inodes(&self, s: &Sleepable, mode: Mode) -> Result<(), Errno> {
        for (c, _) in self.cells() {
            let mut st = c.lock(s);
            if st.dirty {
                self.ufs
                    .write_inode(s, c.ino, &st.ip, mode)
                    .map_err(errno)?;
                st.dirty = false;
            }
        }
        Ok(())
    }

    /// Writes everything: pages, inodes, buffers, and the summary (still
    /// marked dirty, unless `clean`). With soft updates, again and again,
    /// doing the work each round leaves, until nothing is waiting.
    fn sync_all(&self, s: &Sleepable, clean: bool) -> Result<(), Errno> {
        let Ok(sum) = self.sum() else {
            return Ok(());
        };
        let io = self.ufs.io();
        for _ in 0..64 {
            self.sync_pages(s);
            self.wait_writes(s);
            self.write_inodes(s, Mode::Delayed)?;
            if io.seen(io.cache.sync(s)).is_err() {
                if self.is_failing() {
                    break;
                }
                return Err(Errno::EIO);
            }
            let Some(deps) = &self.deps else {
                break;
            };
            self.process_work(s);
            if deps.with(s, |d| d.is_idle()) && !io.cache.any_dirty() {
                break;
            }
        }
        if let Some(deps) = &self.deps
            && !self.is_failing()
        {
            let left = deps.with(s, |d| d.len());
            if left != 0 {
                kprintln!("ufs: {}: {left} soft dependencies left:", io.disk.name());
                let held = deps.with(s, |d| d.describe());
                for line in held.lines().take(12) {
                    kprintln!("ufs:   {line}");
                }
            }
        }
        let mut sum = sum.lock(s);
        if sum.modified || clean {
            let r = self
                .ufs
                .write_summary(s, &mut sum, clean, wall_seconds())
                .map_err(errno);
            // With the disk gone, the summary goes with it.
            if !self.is_failing() {
                r?;
            }
        }
        Ok(())
    }

    /// Inode `ino` in core: the one a node or the work holds, or read now
    /// (and found by `load` meanwhile).
    fn cell(&self, s: &Sleepable, ino: u32) -> Result<Arc<InodeCell>, Errno> {
        loop {
            let before = self.changes.load(Ordering::Acquire);
            if let Some(c) = self.inodes.lock().get(&ino).and_then(|(c, _)| c.upgrade()) {
                return Ok(c);
            }
            let ip = self.ufs.inode(s, ino).map_err(errno)?;
            let cell = new_cell(ino, ip);
            let mut t = self.inodes.lock();
            if let Some(c) = t.get(&ino).and_then(|(c, _)| c.upgrade()) {
                drop(t);
                return Ok(c);
            }
            // A cell came and went meanwhile: what was read may be older
            // than what it wrote.
            if self.changes.load(Ordering::Acquire) != before {
                continue;
            }
            t.insert(ino, (Arc::downgrade(&cell), None));
            self.changes.fetch_add(1, Ordering::Release);
            return Ok(cell);
        }
    }

    /// Puts inode `ino`, just made as `ip`, in core unless it is (FreeBSD's
    /// `ffs_valloc` hands `ufs_makeinode` the vnode it read): its node's
    /// load then reads nothing.
    fn seed(&self, ino: u32, ip: ufs::Inode) -> Option<Arc<InodeCell>> {
        let cell = new_cell(ino, ip);
        let mut t = self.inodes.lock();
        if t.get(&ino).is_some_and(|(c, _)| c.strong_count() > 0) {
            return None;
        }
        t.insert(ino, (Arc::downgrade(&cell), None));
        self.changes.fetch_add(1, Ordering::Release);
        Some(cell)
    }
}

/// A new in-core inode `ino` holding `ip`, not yet in the table.
fn new_cell(ino: u32, ip: ufs::Inode) -> Arc<InodeCell> {
    let size = ip.size;
    Arc::new(InodeCell {
        stat: StatCopy::new(&ip),
        ino,
        state: Mutex::new(
            &INODE,
            InodeState {
                ip,
                dirty: false,
                nodes: 0,
                data_gen: 0,
                cluster: Cluster::default(),
                unwritten: Vec::new(),
            },
        ),
        size: AtomicU64::new(size),
        writing: Writing::new(),
    })
}

impl Shared {
    /// Does the soft-updates work that is due: frees, and link counts
    /// dropping (FreeBSD's `softdep_process_worklist`).
    fn process_work(&self, s: &Sleepable) {
        let Ok(sum) = self.sum() else {
            return;
        };
        let _working = self.working.lock(s);
        // Before taking: one who finds the work gone sees this (`flush_frees`).
        self.applying.store(true, Ordering::Release);
        loop {
            let work = self.ufs.take_work(s);
            if work.is_empty() {
                self.applying.store(false, Ordering::Release);
                return;
            }
            for w in work {
                let mut env = WorkEnv { fs: self, s, sum };
                if let Err(e) = self.ufs.apply(s, w, &mut env) {
                    kprintln!(
                        "ufs: {}: soft updates work: {e:?}",
                        self.ufs.io().disk.name()
                    );
                }
            }
        }
    }

    /// Out of space with soft updates: writes and does the work until what
    /// waited to be freed is (FreeBSD's `softdep_request_cleanup` from the
    /// top). File pages are written too: a block's free can wait for the
    /// data of a new block, which the page cache holds. Returns whether
    /// anything was waiting. The caller holds no inode lock.
    fn flush_frees(&self, s: &Sleepable) -> bool {
        let Some(deps) = &self.deps else {
            return false;
        };
        // Idle, and nobody doing taken work (which `process_work` below
        // would wait for): nothing will be freed.
        if deps.with(s, |d| d.is_idle()) && !self.applying.load(Ordering::Acquire) {
            return false;
        }
        // Called from the writing below: the outer call goes on.
        if self.flushing.is_mine() {
            return false;
        }
        let _flushing = self.flushing.lock(s);
        for _ in 0..8 {
            self.sync_pages(s);
            self.wait_writes(s);
            let _ = self.write_inodes(s, Mode::Delayed);
            let _ = self.ufs.io().cache.sync(s);
            self.process_work(s);
            if deps.with(s, |d| d.pending_free() == 0 && d.is_idle()) {
                break;
            }
        }
        let (left, pending) = deps.with(s, |d| (d.len(), d.pending_free()));
        if pending != 0 {
            kprintln!(
                "ufs: {}: out of space; {pending} bytes still to be freed, {left} dependencies",
                self.ufs.io().disk.name()
            );
        }
        true
    }

    /// Soft-updates items held.
    fn deps_held(&self, s: &Sleepable) -> usize {
        self.deps.as_ref().map_or(0, |d| d.with(s, |d| d.len()))
    }

    /// A writer past [`softdep_limit`] items waits for them to go below
    /// [`softdep_speedup`], writing (FreeBSD's `softdep_slowdown`). The
    /// caller holds no inode lock.
    fn throttle(&self, s: &Sleepable) {
        if self.deps_held(s) <= softdep_limit() {
            return;
        }
        for _ in 0..8 {
            let _ = self.write_inodes(s, Mode::Delayed);
            let _ = self.ufs.io().cache.sync(s);
            self.process_work(s);
            if self.deps_held(s) <= softdep_speedup() {
                break;
            }
        }
    }

    /// Bytes freed but not yet back in their groups (FreeBSD's
    /// `fs_pendingblocks`), which `statfs` counts free.
    fn pending_free(&self, s: &Sleepable) -> u64 {
        self.deps
            .as_ref()
            .map_or(0, |d| d.with(s, |d| d.pending_free()))
    }
}

/// The summary, locked around each allocation only (FreeBSD's
/// `UFS_LOCK`), for an operation that mostly allocates nothing.
struct SumLock<'a> {
    sum: &'a Mutex<ufs::Summary>,
    s: &'a Sleepable,
}

impl ufs::Sums for SumLock<'_> {
    fn with_sum<R>(&mut self, f: impl FnOnce(&mut ufs::Summary) -> R) -> R {
        f(&mut self.sum.lock(self.s))
    }
}

/// The work's way to in-core inodes and the summary.
struct WorkEnv<'a> {
    fs: &'a Shared,
    s: &'a Sleepable,
    sum: &'a Mutex<ufs::Summary>,
}

impl ufs::ops::WorkEnv<DiskIo> for WorkEnv<'_> {
    fn with_inode(
        &mut self,
        ino: u32,
        f: &mut dyn FnMut(&mut ufs::Inode, bool, &mut ufs::Summary) -> Result<(), ufs::Error>,
    ) -> Result<(), ufs::Error> {
        let cell = self.fs.cell(self.s, ino).map_err(|_| ufs::Error::Io)?;
        let mut st = cell.lock(self.s);
        let mut sum = self.sum.lock(self.s);
        let in_use = st.nodes > 0;
        let r = f(&mut st.ip, in_use, &mut sum);
        // The work writes the inode it changes.
        st.dirty = false;
        cell.publish(&st);
        r
    }

    fn with_summary(
        &mut self,
        f: &mut dyn FnMut(&mut ufs::Summary) -> Result<(), ufs::Error>,
    ) -> Result<(), ufs::Error> {
        f(&mut self.sum.lock(self.s))
    }
}

impl FileSystem for UfsFs {
    /// FreeBSD's `ufs_pathconf` (no ACLs or MAC labels).
    fn pathconf(&self, kind: Kind, name: u32) -> Result<i64, Errno> {
        use abi::fcntl::*;
        let sb = self.shared.ufs.superblock();
        let (bsize, fsize) = (i64::from(sb.bsize), i64::from(sb.fsize));
        match name {
            _PC_LINK_MAX => Ok(i64::from(ufs::ops::LINK_MAX)),
            _PC_NAME_MAX => Ok(255),
            _PC_PIPE_BUF if matches!(kind, Kind::Directory | Kind::Fifo) => Ok(512),
            _PC_PIPE_BUF => Err(Errno::EINVAL),
            _PC_CHOWN_RESTRICTED | _PC_NO_TRUNC | _PC_HAS_HIDDENSYSTEM => Ok(1),
            _PC_ACL_EXTENDED | _PC_ACL_NFS4 | _PC_PRIO_IO | _PC_SYNC_IO => Ok(0),
            _PC_ACL_PATH_MAX => Ok(3),
            _PC_MIN_HOLE_SIZE | _PC_REC_INCR_XFER_SIZE | _PC_REC_MIN_XFER_SIZE => Ok(bsize),
            _PC_ALLOC_SIZE_MIN => Ok(fsize),
            _PC_FILESIZEBITS => Ok(64),
            _PC_REC_MAX_XFER_SIZE => Ok(-1),
            _PC_REC_XFER_ALIGN => Ok(4096),
            _PC_SYMLINK_MAX => Ok(1024),
            _ => vfs::mount::std_pathconf(name),
        }
    }

    fn root(&self) -> u64 {
        u64::from(ufs::ROOT_INO)
    }

    fn load(&self, s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        let ino32 = u32::try_from(ino).map_err(|_| Errno::ENOENT)?;
        let cell = self.shared.cell(s, ino32)?;
        node_of(&self.shared, s, cell, None)
    }

    fn statfs(&self, s: &Sleepable) -> Result<FsStats, Errno> {
        let ufs = &self.shared.ufs;
        // The summary as last mounted read-write, if it was.
        let st = match self.shared.sum.get() {
            Some(sum) => {
                let totals = sum.lock(s).totals;
                ufs::Superblock {
                    totals,
                    ..ufs.superblock().clone()
                }
                .stats()
            }
            None => ufs.stats(),
        };
        // What soft updates will free soon counts free already, as on
        // FreeBSD.
        let pending = self.shared.pending_free(s) / u64::from(ufs.superblock().fsize);
        Ok(FsStats {
            bsize: st.fsize,
            iosize: st.iosize,
            blocks: st.blocks,
            bfree: st.bfree + pending,
            bavail: st.bavail.saturating_add_unsigned(pending),
            files: st.files,
            ffree: st.ffree,
        })
    }

    fn rename(&self, s: &Sleepable, r: Rename<'_>) -> Result<(), Errno> {
        let shared = Arc::clone(&self.shared);
        let sum = shared.sum()?;
        let t = now();
        let from_dir = ufs_node(r.from_dir.as_any_mut())?;
        let from = ufs_node(r.from.node_mut().as_any_mut())?;
        let to_dir = match r.to_dir {
            Some(d) => Some(ufs_node(d.as_any_mut())?),
            None => None,
        };
        let target = match r.to {
            Some((_, ops)) => Some(ufs_node(ops.node_mut().as_any_mut())?),
            None => None,
        };
        let to_dir_ino = to_dir.as_ref().map(|d| d.cell.ino);
        let is_dir;
        {
            // Inode locks: the vnodes are all locked, so any order holds.
            let mut fd = from_dir.cell.lock(s);
            let mut fi = from.cell.lock(s);
            is_dir = fi.ip.kind() == ufs::Kind::Directory;
            let mut td = to_dir.as_ref().map(|d| d.cell.lock(s));
            let mut tt = target.as_ref().map(|n| n.cell.lock(s));
            let mut sum = sum.lock(s);
            shared
                .ufs
                .rename(
                    s,
                    &mut sum,
                    from_dir.cell.ino,
                    &mut fd.ip,
                    r.from_name,
                    from.cell.ino,
                    &mut fi.ip,
                    to_dir_ino.zip(td.as_mut().map(|g| &mut g.ip)),
                    r.to_name,
                    target
                        .as_ref()
                        .map(|n| n.cell.ino)
                        .zip(tt.as_mut().map(|g| &mut g.ip)),
                    t,
                    Reserve::Allowed,
                )
                .map_err(errno)?;
        }
        if is_dir && let Some(ino) = to_dir_ino {
            from.parent = u64::from(ino);
        }
        shared.throttle(s);
        Ok(())
    }

    fn sync(&self, s: &Sleepable, _wait: bool) -> Result<(), Errno> {
        let r = self.shared.sync_all(s, false);
        if self.shared.is_failing() {
            return Ok(());
        }
        r
    }

    fn sync_aged(&self, s: &Sleepable, before: u64) {
        let shared = &self.shared;
        let Ok(sum) = shared.sum() else {
            return;
        };
        // Its disk went away with writes still to do: they would fail
        // with ENXIO, which unmounts it (as FreeBSD's syncer meets it).
        let io = shared.ufs.io();
        if io.disk.is_gone()
            && (io.cache.any_dirty()
                || io.gone_seen()
                || shared
                    .deps
                    .as_ref()
                    .is_some_and(|d| !d.with(s, |d| d.is_idle())))
        {
            shared.fsfail();
            return;
        }
        // File pages, inodes, and the summary every 30 seconds; metadata
        // as it ages, or all of it while soft updates hold too much
        // (FreeBSD's `softdep_speedup`).
        let now = crate::time::now_ns();
        let speedup = shared.deps_held(s) > softdep_speedup();
        let due = now.saturating_sub(shared.data_synced.load(Ordering::Relaxed)) >= 30_000_000_000;
        if due || speedup {
            if due {
                shared.data_synced.store(now, Ordering::Relaxed);
            }
            shared.sync_pages(s);
            let _ = shared.write_inodes(s, Mode::Delayed);
        }
        let before = if speedup { u64::MAX } else { before };
        let io = shared.ufs.io();
        let _ = io.seen(io.cache.write_aged(s, before));
        shared.process_work(s);
        if due {
            let mut sum = sum.lock(s);
            if sum.modified {
                let _ = shared.ufs.write_summary(s, &mut sum, false, wall_seconds());
            }
        }
    }

    fn unmount(&self, s: &Sleepable) -> Result<(), Errno> {
        let shared = &self.shared;
        let r = shared.sync_all(s, true);
        if !shared.is_failing() {
            r?;
        }
        // A read-only disk may refuse a flush (bhyve's does): a read-only
        // mount only lets its buffers go.
        shared.ufs.io().cache.invalidate();
        Ok(())
    }

    fn set_read_only(&self, s: &Sleepable, read_only: bool, force: bool) -> Result<(), Errno> {
        if read_only {
            self.shared.make_read_only(s)
        } else {
            self.shared.make_writable(s, force)
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn ufs_node(n: &mut dyn Any) -> Result<&mut UfsNode, Errno> {
    n.downcast_mut::<UfsNode>().ok_or(Errno::EXDEV)
}

/// One file's in-core node.
struct UfsNode {
    cell: Arc<InodeCell>,
    /// A directory's `..`.
    parent: u64,
    /// A regular file's pages.
    object: Option<Arc<Object>>,
    fs: Arc<Shared>,
}

/// A directory entry's type as a node kind (`None` for a whiteout).
fn kind_of(dtype: u8) -> Option<Kind> {
    use abi::dirent::{DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK};
    Some(match dtype {
        DT_REG => Kind::Regular,
        DT_DIR => Kind::Directory,
        DT_LNK => Kind::Symlink,
        DT_CHR => Kind::CharDevice,
        DT_BLK => Kind::BlockDevice,
        DT_FIFO => Kind::Fifo,
        DT_SOCK => Kind::Socket,
        _ => return None,
    })
}

/// Whether a file's maker may use the superuser's reserve.
fn reserve_for(uid: u32) -> Reserve {
    if uid == 0 {
        Reserve::Allowed
    } else {
        Reserve::Denied
    }
}

/// A sleepable context for node operations that are not given one.
fn sleepable() -> Sleepable {
    // SAFETY: node operations run in thread context (the VFS calls them
    // with vnode locks held, which sleep).
    unsafe { Sleepable::assume() }
}

impl Node for UfsNode {
    /// From the inode's [`StatCopy`], taking no lock.
    fn attr(&self) -> Attr {
        let [
            ids,
            mode_flags,
            links_gen,
            size,
            blocks,
            asec,
            msec,
            csec,
            bsec,
            am,
            cb,
            rdev,
        ] = self.cell.stat.load();
        let lo = |w: u64| u32::try_from(w & u64::from(u32::MAX)).unwrap_or(0);
        let hi = |w: u64| u32::try_from(w >> 32).unwrap_or(0);
        let at = |sec: u64, nsec: u32| Timespec {
            tv_sec: sec.cast_signed(),
            tv_nsec: i64::from(nsec),
        };
        Attr {
            mode: u16::try_from(lo(mode_flags) & 0o7777).unwrap_or(0),
            uid: lo(ids),
            gid: hi(ids),
            nlink: u64::from(lo(links_gen) & 0xffff),
            size,
            bytes: blocks * 512,
            atime: at(asec, lo(am)),
            mtime: at(msec, hi(am)),
            ctime: at(csec, lo(cb)),
            birthtime: at(bsec, hi(cb)),
            flags: hi(mode_flags),
            generation: u64::from(hi(links_gen)),
            rdev,
            blksize: self.fs.ufs.superblock().bsize,
            fileid: 0,
        }
    }

    fn setattr(&mut self, s: &Sleepable, a: &SetAttr) -> Result<(), Errno> {
        let shared = Arc::clone(&self.fs);
        let sum = shared.sum()?;
        let t = now();
        let mut st = self.cell.lock(s);
        st.data_gen = st.data_gen.wrapping_add(1);
        if let Some(size) = a.size {
            if i64::try_from(size).is_err() {
                return Err(Errno::EFBIG);
            }
            let old = st.ip.size;
            if let Some(o) = &self.object {
                // Pages past the new end go first (FreeBSD's
                // `vnode_pager_setsize`), then the blocks.
                vfs::pagecache::resize(o, old, size);
            }
            // No block goes under a write.
            self.cell.writing.wait_all(s);
            // A cut inside a hole needs the new last block: out of space,
            // soft updates may have some on the way back, fetched once
            // with no lock held.
            let mut attempt = 0;
            loop {
                let mut sum = sum.lock(s);
                match shared.ufs.truncate(
                    s,
                    &mut sum,
                    self.cell.ino,
                    &mut st.ip,
                    size,
                    Reserve::Allowed,
                    false,
                ) {
                    Ok(()) => break,
                    Err(ufs::Error::NoSpace) if attempt == 0 => attempt += 1,
                    Err(e) => return Err(errno(e)),
                }
                drop(sum);
                drop(st);
                let waited = shared.flush_frees(s);
                st = self.cell.lock(s);
                st.data_gen = st.data_gen.wrapping_add(1);
                if !waited {
                    return Err(Errno::ENOSPC);
                }
            }
            st.ip.mtime = t;
            self.cell.publish(&st);
        }
        let ip = &mut st.ip;
        if let Some(m) = a.mode {
            ip.mode = (ip.mode & 0o170_000) | (m & 0o7777);
        }
        if let Some(u) = a.uid {
            ip.uid = u;
        }
        if let Some(g) = a.gid {
            ip.gid = g;
        }
        if let Some(x) = a.atime {
            ip.atime = stamp(x);
        }
        if let Some(x) = a.mtime {
            ip.mtime = stamp(x);
        }
        if let Some(x) = a.birthtime {
            ip.birthtime = stamp(x);
        }
        if let Some(f) = a.flags {
            ip.flags = f;
        }
        ip.ctime = t;
        st.dirty = true;
        Ok(())
    }

    fn fsync(&mut self, s: &Sleepable) -> Result<(), Errno> {
        let shared = &self.fs;
        if !shared.writable() {
            return Ok(());
        }
        // The pages were written before this (by the generic layer); the
        // writes started earlier finish too.
        self.cell.writing.wait_all(s);
        let Some(deps) = &shared.deps else {
            {
                let mut st = self.cell.lock(s);
                shared
                    .ufs
                    .write_inode(s, self.cell.ino, &st.ip, Mode::Sync)
                    .map_err(errno)?;
                st.dirty = false;
            }
            // Without per-file buffer lists, all dirty metadata goes.
            let io = shared.ufs.io();
            return io.seen(io.cache.sync(s)).map_err(|_| Errno::EIO);
        };
        // With soft updates, only what this file needs (FreeBSD's
        // `softdep_sync`): its pointers' groups and blocks, its indirect
        // blocks, its inode, and the entries naming it, until nothing of
        // it waits. A directory's own blocks go too.
        let io = shared.ufs.io();
        let ino = self.cell.ino;
        let mut dir_blocks = Vec::new();
        {
            let mut st = self.cell.lock(s);
            shared
                .ufs
                .write_inode(s, ino, &st.ip, Mode::Delayed)
                .map_err(errno)?;
            st.dirty = false;
            if st.ip.kind() == ufs::Kind::Directory {
                let bsize = u64::from(shared.ufs.superblock().bsize);
                let fsize = u64::from(shared.ufs.superblock().fsize);
                for lbn in 0..st.ip.size.div_ceil(bsize) {
                    let addr = shared.ufs.bmap(s, &st.ip, lbn).map_err(errno)?;
                    if addr != 0 {
                        dir_blocks.push(addr * fsize);
                    }
                }
            }
        }
        for at in dir_blocks {
            io.cache.write_buffer(s, at).map_err(|_| Errno::EIO)?;
        }
        for _ in 0..16 {
            let need = deps.with(s, |d| d.needs(ino));
            if need.is_empty() {
                break;
            }
            for at in need {
                io.cache.write_buffer(s, at).map_err(|_| Errno::EIO)?;
            }
        }
        // The writes are done; as FreeBSD's UFS, no cache flush follows
        // (it sends `BIO_FLUSH` only for the journal, and only when
        // `debug.softdep.flushcache` asks).
        Ok(())
    }

    fn need_inactive(&self, _s: &Sleepable) -> bool {
        // Unlinked as the vnode sees it (`attr`'s count): with soft
        // updates the count on the disk drops later, and the vnode is
        // reclaimed now so that drop frees the inode.
        // From the stat copy: no lock.
        let w = self.cell.stat.load();
        let (links, mode) = (w[2], w[1] & 0xffff);
        let (effnlink, nlink) = (links & 0xffff, (links >> 16) & 0xffff);
        effnlink == 0 || (nlink == 0 && mode != 0)
    }

    fn inactive(&mut self, s: &Sleepable) {
        let shared = Arc::clone(&self.fs);
        let Ok(sum) = shared.sum() else {
            return;
        };
        let mut st = self.cell.lock(s);
        st.data_gen = st.data_gen.wrapping_add(1);
        if st.ip.nlink == 0 && st.ip.mode != 0 {
            if let Some(o) = &self.object {
                o.truncate(0);
            }
            // No block goes under a write.
            self.cell.writing.wait_all(s);
            let mut sum = sum.lock(s);
            let ino = self.cell.ino;
            if let Err(e) = shared
                .ufs
                .free_inode(s, &mut sum, ino, &mut st.ip, Reserve::Allowed)
            {
                kprintln!("ufs: freeing inode {ino}: {e:?}");
            }
            st.dirty = false;
            self.cell.publish(&st);
        }
    }

    fn reclaim(self: Box<Self>, s: &Sleepable) {
        let shared = &self.fs;
        // Its writes finish first: its blocks may be freed after.
        self.cell.writing.wait_all(s);
        if let Some(o) = &self.object {
            // Unlinked (soft updates drop the disk's count later) and
            // mapped by nobody: no one can read the data again, so it is
            // not written, only let go; freeing the inode later cancels
            // what waited for it (FreeBSD's dirty buffers of a removed
            // file go with its truncation).
            let unreachable = self.cell.lock(s).ip.effnlink == 0 && Arc::strong_count(o) == 1;
            if unreachable {
                o.truncate(0);
            } else if shared.writable() {
                let _ = o.sync(0, u64::MAX);
            }
            // Mappings may outlive the vnode: the object stays theirs.
            o.disown();
        }
        {
            let mut st = self.cell.lock(s);
            st.nodes = st.nodes.saturating_sub(1);
            if let Ok(sum) = shared.sum() {
                // Soft updates' work may have dropped the last link since
                // `inactive` looked.
                if st.nodes == 0 && st.ip.nlink == 0 && st.ip.mode != 0 {
                    let mut sum = sum.lock(s);
                    let ino = self.cell.ino;
                    if let Err(e) =
                        shared
                            .ufs
                            .free_inode(s, &mut sum, ino, &mut st.ip, Reserve::Allowed)
                    {
                        kprintln!("ufs: freeing inode {ino}: {e:?}");
                    }
                    st.dirty = false;
                }
                if st.dirty {
                    let _ = shared
                        .ufs
                        .write_inode(s, self.cell.ino, &st.ip, Mode::Delayed);
                    st.dirty = false;
                }
            }
        }
        let mut t = shared.inodes.lock();
        if t.get(&self.cell.ino)
            .is_some_and(|(w, _)| w.as_ptr() == Arc::as_ptr(&self.cell))
        {
            t.remove(&self.cell.ino);
            shared.changes.fetch_add(1, Ordering::Release);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Puts fragment `m` (grown or moved by `balloc_new`) in `object`'s pages:
/// each page of it held and dirty, so it is written whole to its new
/// place. A page in memory has the contents already; one that is not gets
/// the old fragments' bytes, read from where they were, and zeros past
/// them. The caller holds the inode and the summary.
fn carry_moved(
    s: &Sleepable,
    shared: &Shared,
    object: &Object,
    m: &ufs::Moved,
) -> Result<(), Errno> {
    let old_end = m.at + m.old_len;
    // The old contents, read once if a page needs them.
    let mut old: Option<Vec<u8>> = None;
    for page in m.at / PAGE..(m.at + m.len).div_ceil(PAGE) {
        if object.page_here(page).is_some() {
            object.mark_dirty(page);
            continue;
        }
        let start = page * PAGE;
        let pfn = object.page_for_overwrite(page).map_err(|_| Errno::ENOMEM)?;
        if start < old_end {
            if old.is_none() {
                let mut buf = alloc::vec![0u8; usize::try_from(m.old_len).map_err(|_| Errno::EIO)?];
                ufs::BlockIo::read_data(shared.ufs.io(), s, m.from, &mut buf)
                    .map_err(|_| Errno::EIO)?;
                old = Some(buf);
            }
            let bytes = old.as_deref().unwrap_or(&[]);
            // The fragment starts a block, so a page, and the page's part
            // of the old contents is its start.
            let from = usize::try_from(start - m.at).map_err(|_| Errno::EIO)?;
            let to = usize::try_from((start + PAGE).min(old_end) - m.at).map_err(|_| Errno::EIO)?;
            object.access(pfn, &mut |p| {
                p[..to - from].copy_from_slice(&bytes[from..to]);
            });
        }
        object.mark_dirty(page);
    }
    Ok(())
}

impl vfs::RegularNode for UfsNode {
    fn read(&self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let object = self.object.as_ref().ok_or(Errno::EINVAL)?;
        let size = self.cell.lock(s).ip.size;
        vfs::pagecache::read(object, size, offset, uio)
    }

    fn write(&mut self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let shared = Arc::clone(&self.fs);
        let sum = shared.sum()?;
        let object = self.object.clone().ok_or(Errno::EINVAL)?;
        let len = uio.resid() as u64;
        let end = offset.checked_add(len).ok_or(Errno::EFBIG)?;
        if i64::try_from(end).is_err() {
            return Err(Errno::EFBIG);
        }
        if len == 0 {
            return Ok(0);
        }
        let bsize = u64::from(shared.ufs.superblock().bsize);
        let ino = self.cell.ino;
        let mut old_size = 0;
        // How far blocks were given: out of space partway, the write is
        // short (FreeBSD's `ffs_write` stops at the block that failed).
        let mut reach = offset;
        // The file's newly allocated blocks, and fragments grown or moved:
        // cleared and carried over in the page cache, not on the disk.
        let mut new = ufs::NewSpace::default();
        // Give the range blocks, the size growing as FreeBSD's `ffs_write`
        // does, block by block. Out of space with soft updates waiting to
        // free some, it waits for that and tries again; still out, the
        // write is short.
        for attempt in 0..2 {
            let mut st = self.cell.lock(s);
            st.data_gen = st.data_gen.wrapping_add(1);
            old_size = st.ip.size;
            let mut sum = sum.lock(s);
            let mut at = offset;
            let mut full = false;
            while at < end {
                let within = at % bsize;
                let n = (bsize - within).min(end - at);
                let r = shared.ufs.balloc_new(
                    s,
                    &mut sum,
                    ino,
                    &mut st.ip,
                    at,
                    n,
                    Reserve::Allowed,
                    &mut new,
                );
                st.dirty = true;
                if let Err(e) = r {
                    // Out of space, the first time: wait for what soft
                    // updates will free, then go again (blocks given stay).
                    if e == ufs::Error::NoSpace && attempt == 0 {
                        full = true;
                        break;
                    }
                    if at == offset {
                        return Err(errno(e));
                    }
                    break;
                }
                if st.ip.size < at + n {
                    st.ip.size = at + n;
                }
                at += n;
            }
            // Fragments grown or moved: their pages hold what the old
            // fragments did, zeros past it, and are written to the new
            // place later (FreeBSD moves the buffer). Those not in memory
            // are read from the old place now, which nothing can take
            // while the summary is held.
            for m in &new.moved {
                carry_moved(s, &shared, &object, m)?;
            }
            // New blocks read as zeros from now on: until the copy below
            // is in, fills and read-ahead see them as holes, so none reads
            // the unwritten disk under them.
            st.unwritten.clone_from(&new.zeroed);
            self.cell.publish(&st);
            reach = at;
            if !full {
                break;
            }
            drop(sum);
            drop(st);
            if !shared.flush_frees(s) {
                // Nothing to wait for: short, or nothing at all.
                if reach == offset {
                    return Err(Errno::ENOSPC);
                }
                break;
            }
        }
        // Past the old end, what lies between reads as zeros.
        if offset > old_size {
            vfs::pagecache::resize(&object, old_size, offset);
        }
        // Pages past the old end start from zeros (new blocks' pages are
        // in already); so do pages the write covers whole.
        let fresh = |page: PageIndex| page * PAGE >= old_size;
        let mut capped = Capped {
            uio,
            left: usize::try_from(reach - offset).unwrap_or(usize::MAX),
        };
        let written = vfs::pagecache::write_fresh(&object, offset, &mut capped, &fresh);
        let done = written.as_ref().map_or(0, |&n| n as u64);
        let mut st = self.cell.lock(s);
        // Every page of the new blocks in and dirty (written whole later,
        // as FreeBSD writes a cleared buffer): what the copy filled, a
        // fill's zeros, or zeros now where a short copy stopped. Then the
        // disk under them may be read again.
        for (at, len) in core::mem::take(&mut st.unwritten) {
            for page in at / PAGE..(at + len).div_ceil(PAGE) {
                if object.page_here(page).is_some() || object.page_for_overwrite(page).is_ok() {
                    object.mark_dirty(page);
                }
            }
        }
        st.data_gen = st.data_gen.wrapping_add(1);
        let want = old_size.max(offset + done);
        if st.ip.size > want {
            // A short copy, or blocks given past what fit: give back what
            // lies past what was written.
            let mut sum = sum.lock(s);
            let _ =
                shared
                    .ufs
                    .truncate(s, &mut sum, ino, &mut st.ip, want, Reserve::Allowed, false);
        }
        let t = now();
        st.ip.mtime = t;
        st.ip.ctime = t;
        // Writing clears the set-id bits, as FreeBSD's `ffs_write` does.
        st.ip.mode &= !0o6000;
        st.dirty = true;
        self.cell.publish(&st);
        // The blocks this write filled go to the disk as FreeBSD's
        // `ffs_write` sends them (`cluster_write`): soon, without waiting,
        // in runs as long as the disk takes, so their data is out and
        // what soft updates hold for it goes too.
        let max =
            (u64::try_from(shared.ufs.io().disk.max_transfer()).unwrap_or(u64::MAX) / bsize).max(1);
        let sequential = offset == st.cluster.next;
        let mut runs = Vec::new();
        for lbn in offset / bsize..(offset + done) / bsize {
            let at_end = (lbn + 1) * bsize >= st.ip.size;
            st.cluster.filled(lbn, at_end, sequential, max, &mut runs);
        }
        st.cluster.next = offset + done;
        drop(st);
        for (first, blocks) in runs {
            wait_running(s);
            object.start_sync(first * bsize / PAGE, (first + blocks) * bsize / PAGE);
        }
        shared.throttle(s);
        written
    }

    fn object(&self) -> Option<Arc<Object>> {
        self.object.clone()
    }
}

/// A write's source, cut to the bytes that have blocks.
struct Capped<'a> {
    uio: &'a mut dyn Uio,
    left: usize,
}

impl Uio for Capped<'_> {
    fn resid(&self) -> usize {
        self.uio.resid().min(self.left)
    }

    fn copy_out(&mut self, data: &[u8]) -> Result<usize, Errno> {
        let n = data.len().min(self.left);
        let done = self.uio.copy_out(&data[..n])?;
        self.left -= done;
        Ok(done)
    }

    fn copy_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let n = buf.len().min(self.left);
        let done = self.uio.copy_in(&mut buf[..n])?;
        self.left -= done;
        Ok(done)
    }
}

impl SymlinkNode for UfsNode {
    fn target(&self, s: &Sleepable) -> Result<Vec<u8>, Errno> {
        let st = self.cell.lock(s);
        self.fs.ufs.readlink(s, &st.ip).map_err(errno)
    }
}

impl UfsNode {
    /// Indexes this directory if it is large (FreeBSD's dirhash): its
    /// lookups and inserts then touch one chunk.
    fn index(&self, s: &Sleepable, st: &mut InodeState) {
        if st.ip.dirhash.is_none() && st.ip.size >= ufs::dirhash::MIN_SIZE {
            let _ = self
                .fs
                .ufs
                .dirhash_build(s, &mut st.ip, ufs::dirhash::MIN_SIZE);
        }
    }
}

/// The node of in-core inode `cell` (`parent` is its `..`, for a
/// directory whose maker knows it; else looked up).
fn node_of(
    shared: &Arc<Shared>,
    s: &Sleepable,
    cell: Arc<InodeCell>,
    parent: Option<u64>,
) -> Result<NodeOps, Errno> {
    let ino32 = cell.ino;
    let (kind, parent) = {
        let mut st = cell.lock(s);
        let ip = &st.ip;
        if ip.mode == 0 {
            kprintln!(
                "ufs: {}: inode {ino32} is free",
                shared.ufs.io().disk.name()
            );
            return Err(Errno::EIO);
        }
        let kind = ip.kind();
        let parent = match parent {
            Some(p) => p,
            None if kind == ufs::Kind::Directory && ino32 != ufs::ROOT_INO => shared
                .ufs
                .lookup(s, ip, b"..")
                .map_err(errno)?
                .map_or(u64::from(ino32), u64::from),
            None => u64::from(ino32),
        };
        st.nodes += 1;
        (kind, parent)
    };
    let object = (kind == ufs::Kind::Regular).then(|| {
        let pager = Arc::new(FilePager {
            fs: Arc::clone(shared),
            cell: Arc::clone(&cell),
        });
        Object::with_pager(crate::mm::space::page_ops(), pager)
    });
    shared.inodes.lock().insert(
        ino32,
        (Arc::downgrade(&cell), object.as_ref().map(Arc::downgrade)),
    );
    shared.changes.fetch_add(1, Ordering::Release);
    let node = Box::new(UfsNode {
        cell,
        parent,
        object,
        fs: Arc::clone(shared),
    });
    Ok(match kind {
        ufs::Kind::Regular => NodeOps::Regular(node),
        ufs::Kind::Directory => NodeOps::Directory(node),
        ufs::Kind::Symlink => NodeOps::Symlink(node),
        ufs::Kind::CharDevice => NodeOps::Other(Kind::CharDevice, node),
        ufs::Kind::BlockDevice => NodeOps::Other(Kind::BlockDevice, node),
        ufs::Kind::Fifo => NodeOps::Other(Kind::Fifo, node),
        ufs::Kind::Socket => NodeOps::Other(Kind::Socket, node),
        ufs::Kind::Whiteout | ufs::Kind::None => {
            kprintln!("ufs: inode {ino32} has a bad mode");
            return Err(Errno::EIO);
        }
    })
}

impl DirectoryNode for UfsNode {
    fn lookup(&self, s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        let mut st = self.cell.lock(s);
        self.index(s, &mut st);
        self.fs
            .ufs
            .lookup(s, &st.ip, name)
            .map_err(errno)?
            .map(u64::from)
            .ok_or(Errno::ENOENT)
    }

    fn parent(&self) -> u64 {
        self.parent
    }

    fn create(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        node: &NewNode,
        c: &Create,
    ) -> Result<Made, Errno> {
        let shared = Arc::clone(&self.fs);
        let sum = shared.sum()?;
        let kind = match node {
            NewNode::Regular => NewKind::Regular,
            NewNode::Directory => NewKind::Directory,
            NewNode::Symlink(t) => NewKind::Symlink(t),
            NewNode::CharDevice(rdev) => NewKind::Device(*rdev),
            NewNode::BlockDevice(rdev) => NewKind::BlockDevice(*rdev),
            NewNode::Fifo => NewKind::Fifo,
            NewNode::Socket => NewKind::Socket,
        };
        let who = Maker {
            perm: c.mode & 0o7777,
            uid: c.uid,
            gid: c.gid,
            now: stamp(c.time),
            random,
            reserve: reserve_for(c.uid),
        };
        for attempt in 0..2 {
            let mut st = self.cell.lock(s);
            self.index(s, &mut st);
            // The summary only around allocations: the soft-updates work
            // freeing inodes meanwhile need not wait out the whole create.
            let mut sums = SumLock { sum, s };
            match shared
                .ufs
                .create(s, &mut sums, self.cell.ino, &mut st.ip, name, kind, &who)
            {
                Ok((ino, ip)) => {
                    // Its new times go out later (FreeBSD's `IN_CHANGE`).
                    st.dirty = true;
                    drop(st);
                    let ops = match shared.seed(ino, ip) {
                        Some(cell) => {
                            Some(node_of(&shared, s, cell, Some(u64::from(self.cell.ino)))?)
                        }
                        None => None,
                    };
                    shared.throttle(s);
                    return Ok(Made {
                        ino: u64::from(ino),
                        ops,
                    });
                }
                Err(ufs::Error::NoSpace) if attempt == 0 => {}
                Err(e) => return Err(errno(e)),
            }
            drop(st);
            // Out of space: soft updates may have some on the way back.
            if !shared.flush_frees(s) {
                break;
            }
        }
        Err(Errno::ENOSPC)
    }

    fn link(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        ino: u64,
        target: &mut dyn Node,
    ) -> Result<(), Errno> {
        let shared = Arc::clone(&self.fs);
        let sum = shared.sum()?;
        let target = ufs_node(target.as_any_mut())?;
        if u64::from(target.cell.ino) != ino {
            return Err(Errno::EINVAL);
        }
        // A new directory block may be needed: out of space, soft updates
        // may have some on the way back, fetched once with no lock held.
        let mut r;
        let mut attempt = 0;
        loop {
            r = {
                let mut st = self.cell.lock(s);
                let mut tt = target.cell.lock(s);
                let mut sum = sum.lock(s);
                shared.ufs.link(
                    s,
                    &mut sum,
                    self.cell.ino,
                    &mut st.ip,
                    name,
                    target.cell.ino,
                    &mut tt.ip,
                    now(),
                    Reserve::Allowed,
                )
            };
            if r != Err(ufs::Error::NoSpace) || attempt > 0 || !shared.flush_frees(s) {
                break;
            }
            attempt += 1;
        }
        shared.throttle(s);
        r.map_err(errno)
    }

    fn remove(&mut self, s: &Sleepable, name: &[u8], child: &mut dyn Node) -> Result<(), Errno> {
        let shared = Arc::clone(&self.fs);
        shared.sum()?;
        let child = ufs_node(child.as_any_mut())?;
        let r = {
            let mut st = self.cell.lock(s);
            let mut ct = child.cell.lock(s);
            let r = shared.ufs.unlink(
                s,
                self.cell.ino,
                &mut st.ip,
                name,
                child.cell.ino,
                &mut ct.ip,
                now(),
            );
            // The directory's new times go out later.
            st.dirty = true;
            r
        };
        shared.throttle(s);
        r.map_err(errno)
    }

    fn rmdir(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        child: &mut dyn DirectoryNode,
    ) -> Result<(), Errno> {
        let shared = Arc::clone(&self.fs);
        let sum = shared.sum()?;
        let child = ufs_node(child.as_any_mut())?;
        let r = {
            let mut st = self.cell.lock(s);
            let mut ct = child.cell.lock(s);
            let mut sum = sum.lock(s);
            shared.ufs.rmdir(
                s,
                &mut sum,
                self.cell.ino,
                &mut st.ip,
                name,
                child.cell.ino,
                &mut ct.ip,
                now(),
            )
        };
        shared.throttle(s);
        r.map_err(errno)
    }

    fn readdir(
        &self,
        s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        let ufs = &self.fs.ufs;
        let st = self.cell.lock(s);
        let mut failed = None;
        ufs.read_dir(s, &st.ip, from, &mut |e| {
            let kind = match kind_of(e.kind) {
                Some(k) => k,
                // A whiteout names nothing.
                None if e.kind == abi::dirent::DT_WHT => return true,
                // An entry without a type: ask its inode.
                None => match ufs.inode(s, e.ino) {
                    Ok(ip) => match ip.kind() {
                        ufs::Kind::Directory => Kind::Directory,
                        ufs::Kind::Symlink => Kind::Symlink,
                        ufs::Kind::CharDevice => Kind::CharDevice,
                        ufs::Kind::BlockDevice => Kind::BlockDevice,
                        ufs::Kind::Fifo => Kind::Fifo,
                        ufs::Kind::Socket => Kind::Socket,
                        _ => Kind::Regular,
                    },
                    Err(err) => {
                        failed = Some(err);
                        return false;
                    }
                },
            };
            f(DirEntry {
                name: e.name,
                ino: u64::from(e.ino),
                kind,
                next: e.next,
            })
        })
        .map_err(errno)?;
        failed.map_or(Ok(()), |e| Err(errno(e)))
    }
}

/// The most pages one fill reads: read-ahead grows to this.
const MAX_RUN: PageIndex = 256;
const PAGE: u64 = 4096;
const PAGE_LEN: usize = 4096;

/// A regular file's pages, read from and written to its blocks.
struct FilePager {
    fs: Arc<Shared>,
    cell: Arc<InodeCell>,
}

/// Where a run of a file's pages lies: each part's disk offset and length,
/// and where in the run it goes. The rest reads as zeros: holes, and what
/// lies past the end of the file (`eof` on).
struct Layout {
    parts: Vec<(u64, usize)>,
    places: Vec<usize>,
    eof: usize,
}

impl Layout {
    /// Puts each part's bytes (`data`, as read) into `buf`, zeros around.
    fn place(&self, buf: &mut [u8], data: &[Vec<u8>]) {
        let mut at = 0;
        for ((&(_, len), &out), bytes) in self.parts.iter().zip(&self.places).zip(data) {
            buf[at..out].fill(0);
            let n = bytes.len().min(len);
            buf[out..out + n].copy_from_slice(&bytes[..n]);
            buf[out + n..out + len].fill(0);
            at = out + len;
        }
        let len = buf.len();
        buf[at.min(len)..].fill(0);
        buf[self.eof.min(len)..].fill(0);
    }

    /// Zeroes what the parts, read into `pages`, leave: as [`Self::place`].
    fn zero_around(&self, pages: &FreshPages) {
        let mut at = 0;
        for (&(_, len), &out) in self.parts.iter().zip(&self.places) {
            pages.zero(at, out - at);
            at = out + len;
        }
        let len = pages.len();
        pages.zero(at.min(len), len - at.min(len));
        pages.zero(self.eof.min(len), len - self.eof.min(len));
    }
}

impl FilePager {
    /// Where the `len` bytes of pages from `index` on lie, for inode `ip`
    /// (locked by the caller).
    /// Where the bytes of pages from `index` (`len` bytes) are on the
    /// disk; holes, and the `unwritten` ranges of new blocks, read as
    /// zeros.
    fn layout(
        &self,
        s: &Sleepable,
        ip: &ufs::Inode,
        unwritten: &[(u64, u64)],
        index: PageIndex,
        len: usize,
    ) -> Result<Layout, PagerError> {
        let ufs = &self.fs.ufs;
        let sb = ufs.superblock();
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let size = ip.size;
        let start = index * PAGE;
        let end = start + u64::try_from(len).map_err(|_| PagerError)?;
        let mut parts = Vec::new();
        let mut places = Vec::new();
        let mut at = start;
        while at < end && at < size {
            let lbn = at / bsize;
            let within = at % bsize;
            let blocks = (end - at).div_ceil(bsize).max(1);
            let (addr, run) = ufs.bmap_run(s, ip, lbn, blocks).map_err(|_| PagerError)?;
            // The bytes the run holds from `at`: whole blocks, but only
            // the allocated fragments of the file's last one.
            let last = lbn + run - 1;
            let held = (run - 1) * bsize + u64::from(sb.block_size(size, last)) - within;
            let n = held.min(end - at);
            if addr != 0 {
                // The run's bytes from `at`, less any unwritten range.
                let disk = addr * fsize + within;
                let mut from = at;
                while from < at + n {
                    let cut = unwritten
                        .iter()
                        .filter(|&&(u, l)| u < at + n && u + l > from)
                        .min_by_key(|&&(u, _)| u);
                    let (stop, next) = match cut {
                        Some(&(u, l)) => (u.max(from), u + l),
                        None => (at + n, at + n),
                    };
                    if stop > from {
                        parts.push((
                            disk + (from - at),
                            usize::try_from(stop - from).map_err(|_| PagerError)?,
                        ));
                        places.push(usize::try_from(from - start).map_err(|_| PagerError)?);
                    }
                    from = next.max(stop);
                }
            }
            at += n;
        }
        let eof = usize::try_from(size.saturating_sub(start)).map_or(len, |e| e.min(len));
        Ok(Layout { parts, places, eof })
    }
}

/// Read-ahead of a file's pages in flight ([`Pager::start_run`]).
struct UfsAhead {
    fs: Arc<Shared>,
    cell: Arc<InodeCell>,
    /// The inode's `data_gen` when the reads went out.
    data_gen: u64,
    layout: Layout,
    reads: Option<storage::disk::PendingFills>,
}

impl PendingFill for UfsAhead {
    fn wait(&mut self, pages: &FreshPages) -> Result<(), PagerError> {
        let s = sleepable();
        let reads = self.reads.take().ok_or(PagerError)?;
        self.fs
            .ufs
            .io()
            .disk
            .finish_reads_into(&s, reads)
            .map_err(|_| PagerError)?;
        self.layout.zero_around(pages);
        Ok(())
    }

    fn commit(&self, insert: &mut dyn FnMut()) -> bool {
        let s = sleepable();
        let st = self.cell.lock(&s);
        if st.data_gen != self.data_gen {
            return false;
        }
        insert();
        true
    }
}

impl Pager for FilePager {
    fn size(&self) -> PageIndex {
        self.cell.size.load(Ordering::Acquire).div_ceil(PAGE)
    }

    fn max_run(&self) -> PageIndex {
        MAX_RUN
    }

    fn fill(&self, index: PageIndex, page: &mut [u8]) -> Result<(), PagerError> {
        self.fill_run(index, page)
    }

    fn fill_run(&self, index: PageIndex, buf: &mut [u8]) -> Result<(), PagerError> {
        // The VM calls pagers only where they may sleep (a fault fills with
        // no map lock held; the page cache in thread context).
        let s = sleepable();
        // The inode stays locked: a writer moving a fragment waits.
        let st = self.cell.lock(&s);
        let layout = self.layout(&s, &st.ip, &st.unwritten, index, buf.len())?;
        let data = if layout.parts.is_empty() {
            Vec::new()
        } else {
            self.fs
                .ufs
                .io()
                .disk
                .read_parts(&s, &layout.parts)
                .map_err(|_| PagerError)?
        };
        layout.place(buf, &data);
        Ok(())
    }

    fn start_run(&self, index: PageIndex, pages: &Arc<FreshPages>) -> Option<Box<dyn PendingFill>> {
        let s = sleepable();
        // Planned and sent under the inode lock, as a fill is; whatever
        // changes the blocks after bumps `data_gen`, and the data is
        // dropped.
        let st = self.cell.lock(&s);
        let layout = self
            .layout(&s, &st.ip, &st.unwritten, index, pages.len())
            .ok()?;
        let parts = layout
            .parts
            .iter()
            .zip(&layout.places)
            .map(|(&(at, len), &out)| (at, pages.slice(out, len)))
            .collect();
        let reads = self.fs.ufs.io().disk.start_reads_into(&s, parts);
        Some(Box::new(UfsAhead {
            fs: Arc::clone(&self.fs),
            cell: Arc::clone(&self.cell),
            data_gen: st.data_gen,
            layout,
            reads: Some(reads),
        }))
    }

    fn writes_back(&self) -> bool {
        self.fs.writable()
    }

    fn write_back(&self, index: PageIndex, page: &[u8]) -> Result<(), PagerError> {
        let s = sleepable();
        let shared = &self.fs;
        let ufs = &shared.ufs;
        let sb = ufs.superblock();
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let mut st = self.cell.lock(&s);
        st.data_gen = st.data_gen.wrapping_add(1);
        let at = index * PAGE;
        let lbn = at / bsize;
        let within = at % bsize;
        // After a write of this block still under way.
        self.cell.writing.wait(&s, lbn..=lbn);
        let mut addr = ufs.bmap(&s, &st.ip, lbn).map_err(|_| PagerError)?;
        // Past the end: written only into the last block's allocated
        // space (zeros, so its tail stays zero on the disk), never
        // allocated.
        if at >= st.ip.size && addr == 0 {
            return Ok(());
        }
        // A mapping wrote into a hole. Out of space, soft updates may have
        // some on the way back: once, with no lock held, it is fetched.
        let mut attempt = 0;
        while addr == 0 {
            let sum = shared.sum().map_err(|_| PagerError)?;
            let mut sum = sum.lock(&s);
            let n = PAGE.min(st.ip.size - at);
            let ino = self.cell.ino;
            match ufs.balloc(
                &s,
                &mut sum,
                ino,
                &mut st.ip,
                at,
                n,
                Reserve::Allowed,
                true,
                false,
            ) {
                Ok(a) => {
                    addr = a;
                    st.dirty = true;
                }
                Err(ufs::Error::NoSpace) if attempt == 0 => {
                    attempt += 1;
                    drop(sum);
                    drop(st);
                    // Pages too: frees can wait for new blocks' data, which
                    // goes from the page cache (this page is no longer
                    // marked dirty, so it is not written twice).
                    if !shared.flush_frees(&s) {
                        return Err(PagerError);
                    }
                    st = self.cell.lock(&s);
                    st.data_gen = st.data_gen.wrapping_add(1);
                    if at >= st.ip.size {
                        return Ok(());
                    }
                    addr = ufs.bmap(&s, &st.ip, lbn).map_err(|_| PagerError)?;
                }
                Err(_) => return Err(PagerError),
            }
        }
        let space = u64::from(sb.block_size(st.ip.size, lbn));
        let n = PAGE.min(space.saturating_sub(within));
        if n == 0 {
            return Ok(());
        }
        let len = usize::try_from(n).map_err(|_| PagerError)?;
        ufs.io()
            .write_data(&s, addr * fsize + within, &page[..len])
            .map_err(|_| PagerError)
    }

    fn write_back_run(&self, index: PageIndex, buf: &[u8]) -> Result<(), PagerError> {
        let s = sleepable();
        let ufs = &self.fs.ufs;
        let pages = buf.as_chunks::<PAGE_LEN>().0;
        // The inode stays locked while the run goes, as for one page: its
        // blocks cannot be freed and reused meanwhile.
        let mut st = self.cell.lock(&s);
        st.data_gen = st.data_gen.wrapping_add(1);
        let Some((extents, lbns)) = self.extents(&s, &st, index, pages.len())? else {
            // A hole a mapping wrote into: page by page, allocating.
            drop(st);
            for (i, page) in (index..).zip(pages) {
                self.write_back(i, page)?;
            }
            return Ok(());
        };
        // After a write of these blocks still under way.
        self.cell.writing.wait(&s, lbns);
        let parts: Vec<(u64, &[u8])> = extents
            .into_iter()
            .map(|(disk, from, len)| (disk, &buf[from..from + len]))
            .collect();
        ufs.io()
            .write_data_parts(&s, &parts)
            .map_err(|_| PagerError)
    }

    fn start_write_back_run(
        &self,
        index: PageIndex,
        buf: IoBuf,
        object: Weak<Object>,
    ) -> Result<(), IoBuf> {
        let s = sleepable();
        let pages = buf.len() / PAGE_LEN;
        // Locked until the write is claimed: the blocks stay meanwhile,
        // and whatever frees them later waits for the claim to go.
        let mut st = self.cell.lock(&s);
        st.data_gen = st.data_gen.wrapping_add(1);
        let Ok(Some((extents, lbns))) = self.extents(&s, &st, index, pages) else {
            return Err(buf);
        };
        if extents.is_empty() || !self.cell.writing.start(lbns.clone()) {
            return Err(buf);
        }
        // Each extent's part of the run (lent pages stay lent).
        let parts: Vec<(u64, IoBuf)> = extents
            .into_iter()
            .map(|(disk, from, len)| (disk, buf.slice(from, len)))
            .collect();
        let cell = Arc::clone(&self.cell);
        let end = index + PageIndex::try_from(pages).unwrap_or(0);
        let bytes: u64 = parts
            .iter()
            .map(|(_, d)| u64::try_from(d.len()).unwrap_or(0))
            .sum();
        RUNNING.fetch_add(bytes, Ordering::AcqRel);
        let started = self.fs.ufs.io().start_write_data(&s, parts, {
            let lbns = lbns.clone();
            move |ok| {
                if !ok {
                    // Written again later, by a sync.
                    if let Some(o) = object.upgrade() {
                        o.redirty(index, end);
                    }
                }
                cell.writing.finish(lbns);
                RUNNING.fetch_sub(bytes, Ordering::AcqRel);
                RUNNING_ROOM.wake_all();
            }
        });
        if let Err(parts) = started {
            RUNNING.fetch_sub(bytes, Ordering::AcqRel);
            self.cell.writing.finish(lbns);
            // Not started: the pages stay as they are.
            drop(parts);
            return Err(buf);
        }
        Ok(())
    }
}

impl FilePager {
    /// Where on the disk the run of `pages` pages from `index` goes: each
    /// extent's disk byte, its start in the run, and its length, with the
    /// file blocks the run covers; `None` if it covers a hole inside the
    /// file (which a write must allocate). The caller holds the inode.
    #[allow(clippy::type_complexity)]
    fn extents(
        &self,
        s: &Sleepable,
        st: &InodeState,
        index: PageIndex,
        pages: usize,
    ) -> Result<Option<(Vec<(u64, usize, usize)>, core::ops::RangeInclusive<u64>)>, PagerError>
    {
        let ufs = &self.fs.ufs;
        let sb = ufs.superblock();
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let last = index + PageIndex::try_from(pages.max(1) - 1).map_err(|_| PagerError)?;
        let lbns = index * PAGE / bsize..=last * PAGE / bsize;
        let mut extents: Vec<(u64, usize, usize)> = Vec::new();
        // The run of blocks last mapped: its first block, address, length.
        let mut mapped = (0, 0, 0);
        for (i, k) in (index..).zip(0..pages) {
            let at = i * PAGE;
            let lbn = at / bsize;
            let within = at % bsize;
            let (from, _, run) = mapped;
            if !(from..from + run).contains(&lbn) {
                let (first, run) = ufs
                    .bmap_run(s, &st.ip, lbn, lbns.end() - lbn + 1)
                    .map_err(|_| PagerError)?;
                mapped = (lbn, first, run);
            }
            let (from, first, _) = mapped;
            let addr = if first == 0 {
                0
            } else {
                first + (lbn - from) * u64::from(sb.frag)
            };
            if addr == 0 {
                if at >= st.ip.size {
                    continue;
                }
                return Ok(None);
            }
            let space = u64::from(sb.block_size(st.ip.size, lbn));
            let n =
                usize::try_from(PAGE.min(space.saturating_sub(within))).map_err(|_| PagerError)?;
            if n == 0 {
                continue;
            }
            let disk = addr * fsize + within;
            let from = k * PAGE_LEN;
            match extents.last_mut() {
                // Contiguous on the disk and in the run, after a whole page.
                Some(e)
                    if e.0 + u64::try_from(e.2).unwrap_or(u64::MAX) == disk
                        && e.1 + e.2 == from
                        && e.2 % PAGE_LEN == 0 =>
                {
                    e.2 += n;
                }
                _ => extents.push((disk, from, n)),
            }
        }
        Ok(Some((extents, lbns)))
    }
}
