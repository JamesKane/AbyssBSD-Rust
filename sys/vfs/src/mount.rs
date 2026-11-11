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
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use abi::errno::Errno;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::{Once, PreemptLock};

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
    /// The type number `statfs` reports, when it is not the mount's own
    /// (nullfs reports the lower file system's, `nullfs_statfs`).
    pub type_num: Option<u32>,
    /// The `MNT_*` flags `statfs` shows of the mount's, when not all
    /// (fdescfs shows only `MNT_IGNORE`, `fdesc_statfs`).
    pub flags_mask: Option<u64>,
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

    /// Goes read-only (written out and marked clean) or read-write, for
    /// `MNT_UPDATE`; `force` lets an unclean file system go read-write.
    /// By default the change is refused.
    fn set_read_only(&self, _s: &Sleepable, _read_only: bool, _force: bool) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    /// Reads the file system's metadata from the disk again
    /// (`MNT_RELOAD`), on a read-only mount: a checker changed the disk
    /// under it. By default it is refused.
    fn reload(&self, _s: &Sleepable) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    /// Whether a new file's name goes in the name cache. FreeBSD's tmpfs
    /// leaves it out (a later lookup finds it in the directory's hash as
    /// fast); UFS puts it in.
    fn caches_created(&self) -> bool {
        true
    }

    /// Whether the name cache holds this file system's names. nullfs's
    /// lookups go to the layer below each time, whose cache answers
    /// (`null_lookup` enters nothing); fdescfs's names are each process's
    /// own descriptors.
    fn name_cache(&self) -> bool {
        true
    }

    /// [`crate::namei::Namespace::child`]'s lookup in `dir` without the
    /// directory's lock, for a file system whose names are not cached:
    /// `None` takes the lock and asks the node. fdescfs answers here from
    /// the caller's descriptor table, which is locked before vnodes
    /// (FreeBSD's `fdesc_lookup` unlocks its directory likewise).
    fn lookup_unlocked(
        &self,
        _s: &Sleepable,
        _dir: &Arc<Vnode>,
        _name: &[u8],
    ) -> Option<Result<Arc<Vnode>, Errno>> {
        None
    }

    /// `readdir` of `dir` without its lock (fdescfs's), or `None`.
    fn readdir_unlocked(
        &self,
        _s: &Sleepable,
        _dir: &Vnode,
        _from: u64,
        _f: &mut dyn FnMut(crate::node::DirEntry<'_>) -> bool,
    ) -> Option<Result<(), Errno>> {
        None
    }

    /// `readlink` of `v` without its lock (fdescfs's), or `None`.
    fn readlink_unlocked(&self, _s: &Sleepable, _v: &Vnode) -> Option<Result<Vec<u8>, Errno>> {
        None
    }

    /// `setattr` of `v` for `cred` in place of the generic layer's, or
    /// `None`: fdescfs passes it to the file the descriptor holds.
    fn setattr_unlocked(
        &self,
        _s: &Sleepable,
        _v: &Vnode,
        _a: &crate::node::SetAttr,
        _cred: &crate::ops::Cred,
    ) -> Option<Result<(), Errno>> {
        None
    }

    /// Whether `getdirentries` reports entries' types; cd9660 reports
    /// `DT_UNKNOWN` (`cd9660_readdir`).
    fn reports_dtype(&self) -> bool {
        true
    }

    /// Whether names can neither be made nor removed here (fdescfs's):
    /// removals and renames are `EROFS`, a new name is what its lookup
    /// says (`EEXIST`, or its error).
    fn fixed_names(&self) -> bool {
        false
    }

    /// Whether access is checked against its nodes' modes. fdescfs grants
    /// everything (`VOP_NULL`), leaving it to the descriptor an open
    /// shares.
    fn checks_access(&self) -> bool {
        true
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
    /// `MNT_*` flags when mounted ([`Mount::flags`] has them now).
    pub flags: u64,
    /// What it was mounted from (`f_mntfromname`).
    pub from: Vec<u8>,
    /// The preferred I/O size (`st_blksize`, `f_iosize`).
    pub io_size: u32,
}

/// Taken under vnode locks (a lookup's directory), and held while the
/// file system loads an inode, which takes the file system's own locks.
/// A layered file system's load gets the vnode below from its mount, so
/// two nest: always the upper mount's first, as nothing below calls up.
static VGET: LockClass = LockClass::new_dup_ok("vget", 342);
static RENAME: LockClass = LockClass::new("rename", 320);

/// A mounted file system.
pub struct Mount {
    /// `st_dev` of its files, and its `fsid`.
    pub id: u64,
    /// The type's number (`f_type`).
    pub type_num: u32,
    info: MountInfo,
    /// `MNT_*` flags now.
    flags: AtomicU64,
    /// Where it is mounted (`f_mntonname`).
    on: PreemptLock<Vec<u8>>,
    fs: Box<dyn FileSystem>,
    /// The in-core vnodes by inode.
    table: PreemptLock<intmap::IntMap<u64, Arc<Vnode>>>,
    /// Serializes loads, so an inode is loaded once (docs/VFS.md §4.1).
    loading: Mutex<()>,
    /// Serializes renames between directories (docs/VFS.md §5).
    rename: Mutex<()>,
    /// The directory this file system is mounted on (`None` for the root).
    covered: PreemptLock<Option<Arc<Vnode>>>,
    /// Its root vnode, once looked up, so crossing into the file system
    /// costs no table lookup (FreeBSD's `mnt_rootvnode`).
    root: Once<Weak<Vnode>>,
    /// An unmount is under way (`MNTK_UNMOUNT`).
    unmounting: AtomicBool,
    /// The mounts layered on this one (nullfs's, `mnt_uppers`): told of
    /// files that go, and keeping this one mounted.
    uppers: PreemptLock<Vec<(u64, Weak<Mount>)>>,
    /// Whether `uppers` has any, read without its lock on every removal.
    has_uppers: AtomicBool,
    /// [`FileSystem::name_cache`] and [`FileSystem::checks_access`], asked
    /// once: lookups and access checks read them on every call.
    names_cached: bool,
    access_checked: bool,
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
        let names_cached = fs.name_cache();
        let access_checked = fs.checks_access();
        Arc::new(Self {
            names_cached,
            access_checked,
            id: new_fsid(type_num),
            type_num,
            flags: AtomicU64::new(info.flags),
            info,
            on: PreemptLock::new(Vec::new()),
            fs,
            table: PreemptLock::new(intmap::IntMap::new()),
            loading: Mutex::new(&VGET, ()),
            rename: Mutex::new(&RENAME, ()),
            covered: PreemptLock::new(None),
            root: Once::new(),
            unmounting: AtomicBool::new(false),
            uppers: PreemptLock::new(Vec::new()),
            has_uppers: AtomicBool::new(false),
        })
    }

    /// Records `upper` as layered on this mount (`vfs_register_upper_from_vp`):
    /// until it goes, this one stays mounted. `ENOENT` while this one is
    /// being unmounted.
    pub fn add_upper(&self, upper: &Arc<Mount>) -> Result<(), Errno> {
        if self.unmounting.load(Ordering::Acquire) {
            return Err(Errno::ENOENT);
        }
        let mut u = self.uppers.lock();
        u.push((upper.id, Arc::downgrade(upper)));
        self.has_uppers.store(true, Ordering::Release);
        Ok(())
    }

    /// The mount whose id is `upper` is unmounted
    /// (`vfs_unregister_upper`).
    pub fn remove_upper(&self, upper: u64) {
        let mut u = self.uppers.lock();
        u.retain(|(id, _)| *id != upper);
        self.has_uppers.store(!u.is_empty(), Ordering::Release);
    }

    /// Whether its names go in the name cache ([`FileSystem::name_cache`]).
    #[must_use]
    pub fn names_cached(&self) -> bool {
        self.names_cached
    }

    /// Whether access to its nodes is checked
    /// ([`FileSystem::checks_access`]).
    #[must_use]
    pub fn access_checked(&self) -> bool {
        self.access_checked
    }

    /// Whether mounts are layered on this one.
    #[must_use]
    pub fn has_uppers(&self) -> bool {
        self.has_uppers.load(Ordering::Acquire)
    }

    /// Inode `ino` was removed or replaced: the mounts layered on this one
    /// let their idle vnodes of it go, top first, so the file is freed
    /// once nothing uses it (`vfs_notify_upper` with
    /// `VFS_NOTIFY_UPPER_UNLINK`).
    pub fn notify_unlinked(&self, s: &Sleepable, ino: u64) {
        if !self.has_uppers() {
            return;
        }
        let uppers: Vec<Arc<Mount>> = self
            .uppers
            .lock()
            .iter()
            .filter_map(|(_, w)| w.upgrade())
            .collect();
        for u in uppers {
            u.notify_unlinked(s, ino);
            u.reclaim_if_idle(s, ino);
        }
    }

    /// Reclaims the vnode of `ino` if nothing but the table refers to it
    /// and it is not in use; returns whether it went.
    pub fn reclaim_if_idle(&self, s: &Sleepable, ino: u64) -> bool {
        let v = {
            let mut t = self.table.lock();
            let Some(v) = t.get(&ino) else {
                return false;
            };
            if v.uses() != 0 || v.is_mount_root() {
                return false;
            }
            // Marked dying first, as `prune` claims: a name-cache hit
            // meanwhile misses, or its reference is counted here.
            v.set_dying(true);
            if Arc::strong_count(v) != 1 || v.uses() != 0 {
                v.set_dying(false);
                return false;
            }
            let v = Arc::clone(v);
            t.remove(&ino);
            v
        };
        v.reclaim(s);
        true
    }

    /// Marks an unmount under way: `false` if one already is.
    pub(crate) fn begin_unmount(&self) -> bool {
        !self.unmounting.swap(true, Ordering::AcqRel)
    }

    /// An unmount failed: another may try.
    pub(crate) fn end_unmount(&self) {
        self.unmounting.store(false, Ordering::Release);
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
    pub fn read_only(&self) -> bool {
        self.flags() & abi::mount::MNT_RDONLY != 0
    }

    /// Its `MNT_*` flags (`f_flags`).
    #[must_use]
    pub fn flags(&self) -> u64 {
        self.flags.load(Ordering::Acquire)
    }

    /// Changes the flags in `mask` to those in `flags` (`MNT_UPDATE`); a
    /// change of `MNT_RDONLY` goes through the file system first. A mount
    /// going read-only stops taking writes before it is written out.
    pub fn update(&self, s: &Sleepable, mask: u64, flags: u64, force: bool) -> Result<(), Errno> {
        use abi::mount::MNT_RDONLY;
        let old = self.flags();
        let new = (old & !mask) | (flags & mask);
        if (old ^ new) & MNT_RDONLY != 0 {
            if new & MNT_RDONLY != 0 {
                self.flags.fetch_or(MNT_RDONLY, Ordering::AcqRel);
                if let Err(e) = self.fs.set_read_only(s, true, force) {
                    self.flags.fetch_and(!MNT_RDONLY, Ordering::AcqRel);
                    return Err(e);
                }
            } else {
                self.fs.set_read_only(s, false, force)?;
            }
        }
        self.flags.store(new, Ordering::Release);
        Ok(())
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
        if let Some(v) = self.root.get().and_then(Weak::upgrade)
            && !v.is_dying()
        {
            return Ok(v);
        }
        let v = self.vget(s, self.fs.root())?;
        let _ = self.root.set(Arc::downgrade(&v));
        Ok(v)
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

    /// The vnode for `ino`, just made as `ops` (no load). Nothing else can
    /// reach the new node until its name is let go, so it is in no vnode
    /// yet.
    pub fn adopt(self: &Arc<Self>, s: &Sleepable, ino: u64, ops: NodeOps) -> Arc<Vnode> {
        let _loading = self.loading.lock(s);
        let v = Arc::new(Vnode::new(Arc::clone(self), ino, ops));
        let old = self.table.lock().insert(ino, Arc::clone(&v));
        debug_assert!(old.is_none(), "a new node's inode {ino} has a vnode");
        drop(old);
        v
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

    /// [`Self::reclaim_if_idle`] for `v`, which the caller holds a
    /// reference to (its last use going): reclaimed if nothing else does.
    pub(crate) fn reclaim_if_idle_held(&self, s: &Sleepable, v: &Arc<Vnode>) {
        let claimed = {
            let mut t = self.table.lock();
            let ours = t.get(&v.ino()).is_some_and(|x| Arc::ptr_eq(x, v));
            if !ours || v.uses() != 0 || v.is_mount_root() {
                return;
            }
            v.set_dying(true);
            if Arc::strong_count(v) == 2 && v.uses() == 0 {
                t.remove(&v.ino());
                true
            } else {
                v.set_dying(false);
                false
            }
        };
        if claimed {
            v.reclaim(s);
        }
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
        // What is used from now on is newer than all this pass may take.
        crate::vnode::next_generation();
        let mut idle: Vec<Arc<Vnode>> = self
            .table
            .lock()
            .values()
            .filter(|v| Arc::strong_count(v) == 1 && v.uses() == 0 && !v.is_mount_root())
            .cloned()
            .collect();
        // Each read once: lookups move `last_used` while this sorts, and a
        // key that changes mid-sort breaks the order the sort relies on.
        idle.sort_by_cached_key(|v| v.last_used());
        idle.truncate(max);
        let mut n = 0;
        for v in idle {
            // Claimed under the table lock, so the table hands it out no
            // more: with two references (the table's and ours) and no use,
            // nobody else has it. A name-cache hit meanwhile upgrades its
            // weak reference: marked dying first, the vnode is a miss for
            // it, or else its reference is counted here.
            let claimed = {
                let mut t = self.table.lock();
                v.set_dying(true);
                let ours = t.get(&v.ino()).is_some_and(|x| Arc::ptr_eq(x, &v));
                if ours && Arc::strong_count(&v) == 2 && v.uses() == 0 {
                    t.remove(&v.ino());
                    true
                } else {
                    v.set_dying(false);
                    false
                }
            };
            if claimed {
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
