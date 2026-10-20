// SPDX-License-Identifier: BSD-2-Clause
//! Mounted file systems (FreeBSD's `struct mount` and `vfsops`), and the
//! table of their in-core vnodes (docs/VFS.md §4.1, §11).
//!
//! Each mount keeps one vnode per inode in use: [`Mount::vget`] returns the
//! existing one or has the file system load the inode. The table holds a
//! strong reference to every vnode, so a vnode is only ever freed on
//! purpose: by [`Mount::prune`] (the vnode reclaimer) or at unmount, both
//! in thread context, where reclaiming may sleep.
//!
//! Behavior reference: sys/kern/vfs_mount.c, sys/kern/vfs_subr.c
//! (`vfs_getnewfsid`, `vget`, `vnlru`).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;

use crate::node::{DirectoryNode, NodeOps};
use crate::vnode::Vnode;

/// A file system's counts for `statfs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsStats {
    /// The fundamental block size (`f_bsize`).
    pub bsize: u64,
    /// The preferred I/O size (`f_iosize`).
    pub iosize: u64,
    pub blocks: u64,
    pub bfree: u64,
    /// Free blocks an unprivileged user may use (may be negative).
    pub bavail: i64,
    pub files: u64,
    pub ffree: i64,
}

/// The nodes a rename involves, locked exclusive (docs/VFS.md §5).
pub struct Rename<'a> {
    pub from_dir: &'a mut dyn DirectoryNode,
    pub from_name: &'a [u8],
    pub from_ino: u64,
    pub from: &'a mut NodeOps,
    /// The target directory, or `None` when it is `from_dir`.
    pub to_dir: Option<&'a mut dyn DirectoryNode>,
    pub to_name: &'a [u8],
    /// What `to_name` names now, replaced by the rename (the generic layer
    /// checked the kinds match and a directory is empty).
    pub to: Option<(u64, &'a mut NodeOps)>,
}

impl core::fmt::Debug for Rename<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rename")
            .field("from_ino", &self.from_ino)
            .field("to_ino", &self.to.as_ref().map(|t| t.0))
            .finish_non_exhaustive()
    }
}

/// A file system type's operations on one mounted instance.
pub trait FileSystem: Send + Sync + Any {
    /// The root directory's inode.
    fn root(&self) -> u64;

    /// Builds the in-core node for inode `ino` (a `vget` that missed). The
    /// node comes back in its [`reclaim`](crate::Node::reclaim).
    fn load(&self, s: &Sleepable, ino: u64) -> Result<NodeOps, Errno>;

    fn statfs(&self, s: &Sleepable) -> Result<FsStats, Errno>;

    /// Moves `r.from` from one name to another.
    fn rename(&self, s: &Sleepable, r: Rename<'_>) -> Result<(), Errno>;

    /// `pathconf`'s answer for `name` about a node of `kind` (FreeBSD's
    /// `VOP_PATHCONF`); by default, `vop_stdpathconf`'s.
    fn pathconf(&self, _kind: crate::Kind, name: u32) -> Result<i64, Errno> {
        std_pathconf(name)
    }

    /// Writes what is dirty; with `wait`, until it is on the disk.
    fn sync(&self, _s: &Sleepable, _wait: bool) -> Result<(), Errno> {
        Ok(())
    }

    /// The syncer's call, once a second: write what was dirtied at or
    /// before `before`, on the kernel's monotonic clock (nanoseconds since
    /// boot), which a file system's buffer cache is given.
    fn sync_aged(&self, _s: &Sleepable, _before: u64) {}

    /// The last step of an unmount, every vnode reclaimed.
    fn unmount(&self, _s: &Sleepable) -> Result<(), Errno> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any;
}

/// `pathconf` answers every file system shares (FreeBSD's
/// `vop_stdpathconf`): `EINVAL` for what a file system must answer itself.
pub fn std_pathconf(name: u32) -> Result<i64, Errno> {
    use abi::fcntl::*;
    match name {
        _PC_ASYNC_IO => Ok(POSIX_ASYNCHRONOUS_IO),
        _PC_PATH_MAX => Ok(PATH_MAX as i64),
        _PC_ACL_EXTENDED
        | _PC_ACL_NFS4
        | _PC_CAP_PRESENT
        | _PC_DEALLOC_PRESENT
        | _PC_INF_PRESENT
        | _PC_MAC_PRESENT
        | _PC_NAMEDATTR_ENABLED
        | _PC_HAS_NAMEDATTR
        | _PC_HAS_HIDDENSYSTEM
        | _PC_CLONE_BLKSIZE
        | _PC_CASE_INSENSITIVE => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

/// What a mount is called and how (`struct statfs`'s strings and flags).
#[derive(Clone, Debug)]
pub struct MountInfo {
    /// The type's name (`f_fstypename`).
    pub type_name: &'static str,
    /// `MNT_*` flags.
    pub flags: u64,
    /// What it was mounted from (`f_mntfromname`).
    pub from: Vec<u8>,
    /// The preferred I/O size (`st_blksize`, `f_iosize`).
    pub io_size: u32,
}

static VGET: LockClass = LockClass::new("vget", 360);
static RENAME: LockClass = LockClass::new("rename", 320);

/// A mounted file system.
pub struct Mount {
    /// `st_dev` of its files, and its `fsid`.
    pub id: u64,
    /// The type's number (`f_type`).
    pub type_num: u32,
    info: MountInfo,
    /// Where it is mounted (`f_mntonname`).
    on: SpinLock<Vec<u8>>,
    fs: Box<dyn FileSystem>,
    /// The in-core vnodes by inode.
    table: SpinLock<BTreeMap<u64, Arc<Vnode>>>,
    /// Serializes loads, so an inode is loaded once (docs/VFS.md §4.1).
    loading: Mutex<()>,
    /// Serializes renames between directories (docs/VFS.md §5).
    rename: Mutex<()>,
    /// The directory this file system is mounted on (`None` for the root).
    covered: SpinLock<Option<Arc<Vnode>>>,
}

impl core::fmt::Debug for Mount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Mount({:#x}, {})", self.id, self.info.type_name)
    }
}

/// Mounts so far, for file system ids.
static MOUNTS: AtomicU64 = AtomicU64::new(0);

/// A file system type's number (`vfc_typenum`): FreeBSD hashes the name
/// (32-bit FNV-1 from its own seed, low 8 bits, 0 made 1). FreeBSD then
/// moves a colliding type to the next free number; the types here do not
/// collide.
#[must_use]
pub const fn type_num(name: &str) -> u32 {
    let b = name.as_bytes();
    let mut h: u32 = 33_554_467; // FreeBSD's `FNV1_32_INIT`
    let mut i = 0;
    while i < b.len() {
        h = h.wrapping_mul(0x0100_0193) ^ b[i] as u32;
        i += 1;
    }
    match h & 0xff {
        0 => 1,
        n => n,
    }
}

/// A new file system id for type `type_num`, as FreeBSD's
/// `vfs_getnewfsid` makes them: `makedev(255, type << 24 | mount)`.
fn new_fsid(type_num: u32) -> u64 {
    let n = MOUNTS.fetch_add(1, Ordering::Relaxed);
    u64::from(type_num & 0xff) << 24 | (n & 0xff00) << 8 | 0xff00 | (n & 0xff)
}

impl Mount {
    /// A mount of `fs`, not yet attached anywhere.
    #[must_use]
    pub fn new(info: MountInfo, fs: Box<dyn FileSystem>) -> Arc<Self> {
        let type_num = type_num(info.type_name);
        Arc::new(Self {
            id: new_fsid(type_num),
            type_num,
            info,
            on: SpinLock::new(Vec::new()),
            fs,
            table: SpinLock::new(BTreeMap::new()),
            loading: Mutex::new(&VGET, ()),
            rename: Mutex::new(&RENAME, ()),
            covered: SpinLock::new(None),
        })
    }

    #[must_use]
    pub fn info(&self) -> &MountInfo {
        &self.info
    }

    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.info.type_name
    }

    #[must_use]
    pub const fn read_only(&self) -> bool {
        self.info.flags & abi::mount::MNT_RDONLY != 0
    }

    #[must_use]
    pub fn fs(&self) -> &dyn FileSystem {
        &*self.fs
    }

    /// Where it is mounted (`f_mntonname`).
    #[must_use]
    pub fn on(&self) -> Vec<u8> {
        self.on.lock().clone()
    }

    pub(crate) fn set_on(&self, path: &[u8]) {
        *self.on.lock() = path.to_vec();
    }

    /// The directory it is mounted on.
    #[must_use]
    pub fn covered(&self) -> Option<Arc<Vnode>> {
        self.covered.lock().clone()
    }

    pub(crate) fn set_covered(&self, v: Option<Arc<Vnode>>) {
        *self.covered.lock() = v;
    }

    pub(crate) fn rename_lock(&self) -> &Mutex<()> {
        &self.rename
    }

    /// The root directory's vnode.
    pub fn root(self: &Arc<Self>, s: &Sleepable) -> Result<Arc<Vnode>, Errno> {
        self.vget(s, self.fs.root())
    }

    /// The vnode for inode `ino`: the one in core, or a new one the file
    /// system loads.
    pub fn vget(self: &Arc<Self>, s: &Sleepable, ino: u64) -> Result<Arc<Vnode>, Errno> {
        if let Some(v) = self.table.lock().get(&ino) {
            v.touch();
            return Ok(Arc::clone(v));
        }
        let _loading = self.loading.lock(s);
        // Someone else's load may have finished meanwhile.
        if let Some(v) = self.table.lock().get(&ino) {
            return Ok(Arc::clone(v));
        }
        let ops = self.fs.load(s, ino)?;
        let v = Arc::new(Vnode::new(Arc::clone(self), ino, ops));
        self.table.lock().insert(ino, Arc::clone(&v));
        Ok(v)
    }

    /// The vnode for `ino` if it is in core.
    #[must_use]
    pub fn cached(&self, ino: u64) -> Option<Arc<Vnode>> {
        self.table.lock().get(&ino).cloned()
    }

    /// How many vnodes are in core.
    #[must_use]
    pub fn vnodes(&self) -> usize {
        self.table.lock().len()
    }

    /// Takes `v` out of the table (it was doomed or reclaimed).
    pub(crate) fn forget(&self, v: &Vnode) {
        let mut t = self.table.lock();
        if t.get(&v.ino()).is_some_and(|x| core::ptr::eq(&**x, v)) {
            t.remove(&v.ino());
        }
    }

    /// Reclaims up to `max` of the vnodes nothing but the table refers to,
    /// longest unused first (FreeBSD's `vnlru`); returns how many went.
    pub fn prune(&self, s: &Sleepable, max: usize) -> usize {
        let mut idle: Vec<Arc<Vnode>> = self
            .table
            .lock()
            .values()
            .filter(|v| Arc::strong_count(v) == 1 && v.uses() == 0 && !v.is_mount_root())
            .cloned()
            .collect();
        idle.sort_by_key(|v| v.last_used());
        idle.truncate(max);
        let mut n = 0;
        for v in idle {
            // Two references now: the table's and ours. Anyone else who
            // found it since keeps it.
            if Arc::strong_count(&v) == 2 && v.uses() == 0 {
                v.reclaim(s);
                n += 1;
            }
        }
        n
    }

    /// Every vnode in core, for unmount.
    pub(crate) fn all_vnodes(&self) -> Vec<Arc<Vnode>> {
        self.table.lock().values().cloned().collect()
    }

    pub fn statfs(&self, s: &Sleepable) -> Result<FsStats, Errno> {
        self.fs.statfs(s)
    }
}
