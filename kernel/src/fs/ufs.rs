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
//! system unless forced, as FreeBSD does) and sets it again at unmount.
//! Changed inodes are written lazily: by `fsync`, `sync`, the syncer, or
//! when the vnode goes. Without soft updates (P5.7), ordered metadata is
//! written synchronously, as FreeBSD's UFS does without them.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::time::Timespec;
use bufcache::BufCache;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;
use ufs::ops::{Maker, NewKind};
use ufs::{Fill, Mode, Reserve, Timestamp};
use vfs::{
    Attr, Create, DirEntry, DirectoryNode, FileSystem, FsStats, Kind, NewNode, Node, NodeOps,
    Rename, SetAttr, SymlinkNode, Uio,
};
use vm::object::{Object, PageIndex, Pager, PagerError};

/// In-core inodes: under vnode locks, before the summary and buffers.
static INODE: LockClass = LockClass::new_dup_ok("ufs inode", 350);
/// The allocation summary (FreeBSD's `UFS_LOCK`).
static SUMMARY: LockClass = LockClass::new("ufs summary", 370);

/// The disk: metadata through the buffer cache, data straight to it.
struct DiskIo {
    cache: BufCache,
    disk: Arc<storage::Disk>,
}

impl DiskIo {
    /// The sector-aligned span covering `[offset, offset + len)`.
    fn span(&self, offset: u64, len: usize) -> (u64, usize) {
        let ss = u64::from(self.disk.sector_size());
        let start = offset - offset % ss;
        let end = (offset + len as u64).next_multiple_of(ss);
        (start, usize::try_from(end - start).unwrap_or(usize::MAX))
    }
}

impl ufs::BlockIo for DiskIo {
    type Cx = Sleepable;

    fn read(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), ufs::Error> {
        self.cache
            .read(s, offset, len, |b| f(b))
            .map_err(|_| ufs::Error::Io)
    }

    fn modify(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        fill: Fill,
        mode: Mode,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), ufs::Error> {
        let fill = match fill {
            Fill::Read => bufcache::Fill::Read,
            Fill::Zero => bufcache::Fill::Overwrite,
        };
        let mode = match mode {
            Mode::Sync => bufcache::WriteMode::Now,
            Mode::Delayed => bufcache::WriteMode::Delayed,
        };
        self.cache
            .write(s, offset, len, fill, mode, |b| f(b))
            .map_err(|_| ufs::Error::Io)
    }

    fn read_data(&self, s: &Sleepable, offset: u64, buf: &mut [u8]) -> Result<(), ufs::Error> {
        let (start, len) = self.span(offset, buf.len());
        let data = self.disk.read(s, start, len).map_err(|_| ufs::Error::Io)?;
        let from = usize::try_from(offset - start).unwrap_or(0);
        buf.copy_from_slice(&data[from..from + buf.len()]);
        Ok(())
    }

    fn write_data(&self, s: &Sleepable, offset: u64, data: &[u8]) -> Result<(), ufs::Error> {
        let (start, len) = self.span(offset, data.len());
        if start == offset && len == data.len() {
            return self.disk.write(s, offset, data).map_err(|_| ufs::Error::Io);
        }
        // Partial sectors: read, change, write.
        let mut buf = self.disk.read(s, start, len).map_err(|_| ufs::Error::Io)?;
        let from = usize::try_from(offset - start).unwrap_or(0);
        buf[from..from + data.len()].copy_from_slice(data);
        self.disk.write(s, start, &buf).map_err(|_| ufs::Error::Io)
    }

    fn forget(&self, s: &Sleepable, offset: u64, len: usize) {
        self.cache.discard(s, offset, len);
    }

    fn flush(&self, s: &Sleepable) -> Result<(), ufs::Error> {
        self.cache.sync(s).map_err(|_| ufs::Error::Io)
    }
}

/// An in-core inode, shared by its node and its file's pager.
struct InodeCell {
    ino: u32,
    state: Mutex<InodeState>,
    /// The size, readable with no lock (the VM asks a pager for it under
    /// the map lock); published whenever a regular file's size changes.
    size: AtomicU64,
}

impl InodeCell {
    fn publish(&self, st: &InodeState) {
        self.size.store(st.ip.size, Ordering::Release);
    }
}

struct InodeState {
    ip: ufs::Inode,
    /// Changed since last written.
    dirty: bool,
}

/// An in-core inode, and its object if it is a regular file.
type InCore = (Weak<InodeCell>, Option<Weak<Object>>);

/// What every node of one mount shares.
struct Shared {
    ufs: ufs::Ufs<DiskIo>,
    /// The allocation state; `None` on a read-only mount.
    sum: Option<Mutex<ufs::Summary>>,
    /// The in-core inodes, and the regular files' objects, for `sync`.
    inodes: SpinLock<BTreeMap<u32, InCore>>,
    /// When the syncer last wrote every file's dirty pages.
    data_synced: AtomicU64,
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
        let cache = crate::fs::block::cache(Arc::clone(&disk), bufcache::Limits::default());
        let io = DiskIo { cache, disk };
        let ufs = ufs::Ufs::open(s, io, sector).map_err(errno)?;
        let sum = if read_only {
            None
        } else {
            if !ufs.superblock().clean && !force {
                kprintln!(
                    "ufs: {} was not properly dismounted: read-write mount denied",
                    ufs.io().disk.name()
                );
                return Err(Errno::EPERM);
            }
            let mut sum = ufs.load_summary(s).map_err(errno)?;
            // Dirty until unmounted cleanly.
            ufs.write_summary(s, &mut sum, false, wall_seconds())
                .map_err(errno)?;
            Some(Mutex::new(&SUMMARY, sum))
        };
        Ok(Self {
            shared: Arc::new(Shared {
                ufs,
                sum,
                inodes: SpinLock::new(BTreeMap::new()),
                data_synced: AtomicU64::new(0),
            }),
        })
    }

    /// The block size, for the mount's I/O size.
    pub(crate) fn block_size(&self) -> u32 {
        self.shared.ufs.superblock().bsize
    }
}

impl Shared {
    fn sum(&self) -> Result<&Mutex<ufs::Summary>, Errno> {
        self.sum.as_ref().ok_or(Errno::EROFS)
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
    fn sync_pages(&self) {
        for (_, o) in self.cells() {
            if let Some(o) = o {
                let _ = o.sync(0, u64::MAX);
            }
        }
    }

    /// Writes every changed in-core inode.
    fn write_inodes(&self, s: &Sleepable, mode: Mode) -> Result<(), Errno> {
        for (c, _) in self.cells() {
            let mut st = c.state.lock(s);
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
    /// marked dirty, unless `clean`).
    fn sync_all(&self, s: &Sleepable, clean: bool) -> Result<(), Errno> {
        let Some(sum) = &self.sum else {
            return Ok(());
        };
        self.sync_pages();
        self.write_inodes(s, Mode::Delayed)?;
        self.ufs.io().cache.sync(s).map_err(|_| Errno::EIO)?;
        let mut sum = sum.lock(s);
        if sum.modified || clean {
            self.ufs
                .write_summary(s, &mut sum, clean, wall_seconds())
                .map_err(errno)?;
        }
        Ok(())
    }
}

impl FileSystem for UfsFs {
    fn root(&self) -> u64 {
        u64::from(ufs::ROOT_INO)
    }

    fn load(&self, s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        let ino32 = u32::try_from(ino).map_err(|_| Errno::ENOENT)?;
        let shared = &self.shared;
        let ip = shared.ufs.inode(s, ino32).map_err(errno)?;
        if ip.mode == 0 {
            kprintln!("ufs: {}: inode {ino} is free", shared.ufs.io().disk.name());
            return Err(Errno::EIO);
        }
        let kind = ip.kind();
        let mut parent = u64::from(ino32);
        if kind == ufs::Kind::Directory && ino32 != ufs::ROOT_INO {
            parent = shared
                .ufs
                .lookup(s, &ip, b"..")
                .map_err(errno)?
                .map_or(parent, u64::from);
        }
        let size = ip.size;
        let cell = Arc::new(InodeCell {
            ino: ino32,
            state: Mutex::new(&INODE, InodeState { ip, dirty: false }),
            size: AtomicU64::new(size),
        });
        let object = (kind == ufs::Kind::Regular).then(|| {
            let pager = Arc::new(FilePager {
                fs: Arc::clone(shared),
                cell: Arc::clone(&cell),
                writes_back: shared.sum.is_some(),
            });
            Object::with_pager(crate::mm::space::page_ops(), pager)
        });
        shared.inodes.lock().insert(
            ino32,
            (Arc::downgrade(&cell), object.as_ref().map(Arc::downgrade)),
        );
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
            ufs::Kind::CharDevice | ufs::Kind::BlockDevice => {
                NodeOps::Other(Kind::CharDevice, node)
            }
            ufs::Kind::Fifo => NodeOps::Other(Kind::Fifo, node),
            ufs::Kind::Socket => NodeOps::Other(Kind::Socket, node),
            ufs::Kind::Whiteout | ufs::Kind::None => {
                kprintln!("ufs: inode {ino} has a bad mode");
                return Err(Errno::EIO);
            }
        })
    }

    fn statfs(&self, s: &Sleepable) -> Result<FsStats, Errno> {
        let ufs = &self.shared.ufs;
        let st = match &self.shared.sum {
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
        Ok(FsStats {
            bsize: st.fsize,
            iosize: st.iosize,
            blocks: st.blocks,
            bfree: st.bfree,
            bavail: st.bavail,
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
            let mut fd = from_dir.cell.state.lock(s);
            let mut fi = from.cell.state.lock(s);
            is_dir = fi.ip.kind() == ufs::Kind::Directory;
            let mut td = to_dir.as_ref().map(|d| d.cell.state.lock(s));
            let mut tt = target.as_ref().map(|n| n.cell.state.lock(s));
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
        Ok(())
    }

    fn sync(&self, s: &Sleepable, _wait: bool) -> Result<(), Errno> {
        self.shared.sync_all(s, false)
    }

    fn sync_aged(&self, s: &Sleepable, before: u64) {
        let shared = &self.shared;
        let Some(sum) = &shared.sum else {
            return;
        };
        // File pages and inodes every 30 seconds; metadata as it ages.
        let now = crate::time::now_ns();
        if now.saturating_sub(shared.data_synced.load(Ordering::Relaxed)) >= 30_000_000_000 {
            shared.data_synced.store(now, Ordering::Relaxed);
            shared.sync_pages();
            let _ = shared.write_inodes(s, Mode::Delayed);
        }
        let _ = shared.ufs.io().cache.write_aged(s, before);
        let mut sum = sum.lock(s);
        if sum.modified {
            let _ = shared.ufs.write_summary(s, &mut sum, false, wall_seconds());
        }
    }

    fn unmount(&self, s: &Sleepable) -> Result<(), Errno> {
        let shared = &self.shared;
        shared.sync_all(s, true)?;
        // A read-only disk may refuse a flush (bhyve's does): a read-only
        // mount only lets its buffers go.
        shared.ufs.io().cache.invalidate();
        Ok(())
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

fn time(t: Timestamp) -> Timespec {
    Timespec {
        tv_sec: t.sec,
        tv_nsec: i64::from(t.nsec),
    }
}

/// A directory entry's type as a node kind (`None` for a whiteout).
fn kind_of(dtype: u8) -> Option<Kind> {
    use abi::dirent::{DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK};
    Some(match dtype {
        DT_REG => Kind::Regular,
        DT_DIR => Kind::Directory,
        DT_LNK => Kind::Symlink,
        DT_CHR | DT_BLK => Kind::CharDevice,
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
    fn attr(&self) -> Attr {
        let s = sleepable();
        let st = self.cell.state.lock(&s);
        let ip = &st.ip;
        Attr {
            mode: ip.perm(),
            uid: ip.uid,
            gid: ip.gid,
            nlink: u64::from(ip.nlink),
            size: ip.size,
            bytes: ip.blocks * 512,
            atime: time(ip.atime),
            mtime: time(ip.mtime),
            ctime: time(ip.ctime),
            birthtime: time(ip.birthtime),
            flags: ip.flags,
            generation: u64::from(ip.generation),
            rdev: match ip.kind() {
                ufs::Kind::CharDevice | ufs::Kind::BlockDevice => ip.rdev(),
                _ => 0,
            },
            blksize: self.fs.ufs.superblock().bsize,
        }
    }

    fn setattr(&mut self, s: &Sleepable, a: &SetAttr) -> Result<(), Errno> {
        let shared = Arc::clone(&self.fs);
        let sum = shared.sum()?;
        let t = now();
        let mut st = self.cell.state.lock(s);
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
            let mut sum = sum.lock(s);
            shared
                .ufs
                .truncate(
                    s,
                    &mut sum,
                    self.cell.ino,
                    &mut st.ip,
                    size,
                    Reserve::Allowed,
                    false,
                )
                .map_err(errno)?;
            drop(sum);
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
        if shared.sum.is_none() {
            return Ok(());
        }
        {
            let mut st = self.cell.state.lock(s);
            shared
                .ufs
                .write_inode(s, self.cell.ino, &st.ip, Mode::Sync)
                .map_err(errno)?;
            st.dirty = false;
        }
        // Without per-file buffer lists, all dirty metadata goes.
        shared.ufs.io().cache.sync(s).map_err(|_| Errno::EIO)
    }

    fn inactive(&mut self, s: &Sleepable) {
        let shared = Arc::clone(&self.fs);
        let Ok(sum) = shared.sum() else {
            return;
        };
        let mut st = self.cell.state.lock(s);
        if st.ip.nlink == 0 && st.ip.mode != 0 {
            if let Some(o) = &self.object {
                o.truncate(0);
            }
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
        if let Some(o) = &self.object {
            if shared.sum.is_some() {
                let _ = o.sync(0, u64::MAX);
            }
            // Mappings may outlive the vnode: the object stays theirs.
            o.disown();
        }
        {
            let mut st = self.cell.state.lock(s);
            if st.dirty && shared.sum.is_some() {
                let _ = shared
                    .ufs
                    .write_inode(s, self.cell.ino, &st.ip, Mode::Delayed);
                st.dirty = false;
            }
        }
        let mut t = shared.inodes.lock();
        if t.get(&self.cell.ino)
            .is_some_and(|(w, _)| w.as_ptr() == Arc::as_ptr(&self.cell))
        {
            t.remove(&self.cell.ino);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl vfs::RegularNode for UfsNode {
    fn read(&self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let object = self.object.as_ref().ok_or(Errno::EINVAL)?;
        let size = self.cell.state.lock(s).ip.size;
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
        let old_size;
        // Give the range blocks, the size growing as FreeBSD's `ffs_write`
        // does, block by block.
        {
            let mut st = self.cell.state.lock(s);
            old_size = st.ip.size;
            let mut sum = sum.lock(s);
            let mut at = offset;
            while at < end {
                let within = at % bsize;
                let n = (bsize - within).min(end - at);
                // A write covering a whole block needs no zeroing.
                let clear = !(within == 0 && n == bsize);
                let r = shared.ufs.balloc(
                    s,
                    &mut sum,
                    ino,
                    &mut st.ip,
                    at,
                    n,
                    Reserve::Allowed,
                    clear,
                    false,
                );
                st.dirty = true;
                if let Err(e) = r {
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
            self.cell.publish(&st);
        }
        // Past the old end, what lies between reads as zeros.
        if offset > old_size {
            vfs::pagecache::resize(&object, old_size, offset);
        }
        let written = vfs::pagecache::write(&object, offset, uio);
        let mut st = self.cell.state.lock(s);
        let done = written.as_ref().map_or(0, |&n| n as u64);
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
        written
    }

    fn object(&self) -> Option<Arc<Object>> {
        self.object.clone()
    }
}

impl SymlinkNode for UfsNode {
    fn target(&self, s: &Sleepable) -> Result<Vec<u8>, Errno> {
        let st = self.cell.state.lock(s);
        self.fs.ufs.readlink(s, &st.ip).map_err(errno)
    }
}

impl DirectoryNode for UfsNode {
    fn lookup(&self, s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        let st = self.cell.state.lock(s);
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
    ) -> Result<u64, Errno> {
        let shared = Arc::clone(&self.fs);
        let sum = shared.sum()?;
        let kind = match node {
            NewNode::Regular => NewKind::Regular,
            NewNode::Directory => NewKind::Directory,
            NewNode::Symlink(t) => NewKind::Symlink(t),
            NewNode::CharDevice(rdev) => NewKind::Device(*rdev),
            NewNode::Fifo => NewKind::Fifo,
            NewNode::Socket => NewKind::Socket,
        };
        let who = Maker {
            perm: c.mode & 0o7777,
            uid: c.uid,
            gid: c.gid,
            now: stamp(c.time),
            random: random(),
            reserve: reserve_for(c.uid),
        };
        let mut st = self.cell.state.lock(s);
        let mut sum = sum.lock(s);
        let (ino, _) = shared
            .ufs
            .create(s, &mut sum, self.cell.ino, &mut st.ip, name, kind, &who)
            .map_err(errno)?;
        Ok(u64::from(ino))
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
        let mut st = self.cell.state.lock(s);
        let mut tt = target.cell.state.lock(s);
        let mut sum = sum.lock(s);
        shared
            .ufs
            .link(
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
            .map_err(errno)
    }

    fn remove(&mut self, s: &Sleepable, name: &[u8], child: &mut dyn Node) -> Result<(), Errno> {
        let shared = Arc::clone(&self.fs);
        shared.sum()?;
        let child = ufs_node(child.as_any_mut())?;
        let mut st = self.cell.state.lock(s);
        let mut ct = child.cell.state.lock(s);
        shared
            .ufs
            .unlink(
                s,
                self.cell.ino,
                &mut st.ip,
                name,
                child.cell.ino,
                &mut ct.ip,
                now(),
            )
            .map_err(errno)
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
        let mut st = self.cell.state.lock(s);
        let mut ct = child.cell.state.lock(s);
        let mut sum = sum.lock(s);
        shared
            .ufs
            .rmdir(
                s,
                &mut sum,
                self.cell.ino,
                &mut st.ip,
                name,
                child.cell.ino,
                &mut ct.ip,
                now(),
            )
            .map_err(errno)
    }

    fn readdir(
        &self,
        s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        let ufs = &self.fs.ufs;
        let st = self.cell.state.lock(s);
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
                        ufs::Kind::CharDevice | ufs::Kind::BlockDevice => Kind::CharDevice,
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
const MAX_RUN: PageIndex = 64;
const PAGE: u64 = 4096;

/// A regular file's pages, read from and written to its blocks.
struct FilePager {
    fs: Arc<Shared>,
    cell: Arc<InodeCell>,
    writes_back: bool,
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
        let ufs = &self.fs.ufs;
        let sb = ufs.superblock();
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        // The inode stays locked: a writer moving a fragment waits.
        let st = self.cell.state.lock(&s);
        let ip = &st.ip;
        let size = ip.size;
        let start = index * PAGE;
        let end = start + buf.len() as u64;
        let mut at = start;
        while at < end {
            let out = usize::try_from(at - start).map_err(|_| PagerError)?;
            // At or past the end: zeros, whatever the disk holds there.
            if at >= size {
                buf[out..].fill(0);
                break;
            }
            let lbn = at / bsize;
            let within = at % bsize;
            let blocks = (end - at).div_ceil(bsize).max(1);
            let (addr, run) = ufs.bmap_run(&s, ip, lbn, blocks).map_err(|_| PagerError)?;
            // The bytes the run holds from `at`: whole blocks, but only
            // the allocated fragments of the file's last one.
            let last = lbn + run - 1;
            let held = (run - 1) * bsize + u64::from(sb.block_size(size, last)) - within;
            let n = held.min(end - at);
            let len = usize::try_from(n).map_err(|_| PagerError)?;
            if addr == 0 {
                buf[out..out + len].fill(0);
            } else {
                let data = ufs
                    .io()
                    .disk
                    .read(&s, addr * fsize + within, len)
                    .map_err(|_| PagerError)?;
                buf[out..out + len].copy_from_slice(&data);
            }
            at += n;
        }
        if let Ok(past) = usize::try_from(size.saturating_sub(start))
            && past < buf.len()
        {
            buf[past..].fill(0);
        }
        Ok(())
    }

    fn writes_back(&self) -> bool {
        self.writes_back
    }

    fn write_back(&self, index: PageIndex, page: &[u8]) -> Result<(), PagerError> {
        let s = sleepable();
        let shared = &self.fs;
        let ufs = &shared.ufs;
        let sb = ufs.superblock();
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let mut st = self.cell.state.lock(&s);
        let at = index * PAGE;
        // Past the end (truncated meanwhile): nothing to write.
        if at >= st.ip.size {
            return Ok(());
        }
        let lbn = at / bsize;
        let within = at % bsize;
        let mut addr = ufs.bmap(&s, &st.ip, lbn).map_err(|_| PagerError)?;
        if addr == 0 {
            // A mapping wrote into a hole.
            let sum = shared.sum().map_err(|_| PagerError)?;
            let mut sum = sum.lock(&s);
            let n = PAGE.min(st.ip.size - at);
            let ino = self.cell.ino;
            addr = ufs
                .balloc(
                    &s,
                    &mut sum,
                    ino,
                    &mut st.ip,
                    at,
                    n,
                    Reserve::Allowed,
                    true,
                    false,
                )
                .map_err(|_| PagerError)?;
            st.dirty = true;
        }
        let space = u64::from(sb.block_size(st.ip.size, lbn));
        let n = PAGE.min(space.saturating_sub(within));
        if n == 0 {
            return Ok(());
        }
        let len = usize::try_from(n).map_err(|_| PagerError)?;
        ufs.io()
            .disk
            .write(&s, addr * fsize + within, &page[..len])
            .map_err(|_| PagerError)
    }
}
