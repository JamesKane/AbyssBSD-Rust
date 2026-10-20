// SPDX-License-Identifier: BSD-2-Clause
//! msdosfs (P5.6, docs/VFS.md §10.4): FAT12/16/32 over `lib/fat`'s engine,
//! as FreeBSD's msdosfs presents it.
//!
//! A file is named by where its directory entry lies, and its number is
//! FreeBSD's file id for that place (`msdosfs_getattr`): a directory's
//! from its first cluster, a file's from its entry's position, and 1 for
//! the root. A rename moves a file's entry; its vnode keeps the number it
//! had (FreeBSD's changes), recorded so the number keeps finding it, and a
//! new file whose position's number is so taken gets another.
//!
//! Directory entries go through the disk's buffer cache, written as
//! FreeBSD's msdosfs writes them; file data goes through the page cache,
//! with a pager that maps clusters. Modes, owners, times, and flags are
//! FreeBSD's: the owner, group, and permission masks come from the mount,
//! the read-only attribute clears the write bits, and times are local
//! (here UTC) in FAT's resolution. Links, symbolic links, and device nodes
//! cannot be made, as on FreeBSD.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::stat::{UF_ARCHIVE, UF_HIDDEN, UF_READONLY, UF_SYSTEM};
use abi::time::Timespec;
use fat::fs::dir::{
    ATTR_ARCHIVE, ATTR_HIDDEN, ATTR_READONLY, ATTR_SYSTEM, Entry, Kind as FatKind, Loc, Name,
};
use fat::fs::time::{self, FatTime};
use fat::fs::{Alloc, Fat, Mode, ROOT};
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;
use vfs::{
    Attr, Create, DirEntry, DirectoryNode, FileSystem, FsStats, Kind, NewNode, Node, NodeOps,
    Rename, SetAttr, Uio,
};
use vm::object::{Object, PageIndex, Pager, PagerError};

use crate::fs::block::DiskIo;

/// In-core files: under vnode locks, before the allocation state.
static NODE: LockClass = LockClass::new_dup_ok("msdosfs node", 350);
/// The cluster allocation state (FreeBSD's `pm_fatlock`).
static ALLOC: LockClass = LockClass::new("msdosfs fat", 370);

/// What a mount's options say (FreeBSD's `msdosfs_args`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opts {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    /// Permission masks for files and directories.
    pub(crate) mask: u16,
    pub(crate) dirmask: u16,
    /// Short names only (FreeBSD's `shortnames`).
    pub(crate) shortname: bool,
}

impl Default for Opts {
    /// FreeBSD's kernel defaults: root, wheel, `0755`.
    fn default() -> Self {
        Self {
            uid: 0,
            gid: 0,
            mask: 0o755,
            dirmask: 0o755,
            shortname: false,
        }
    }
}

/// Where a removed in-core file's number points: nowhere.
const GONE: Loc = Loc {
    dirclust: u32::MAX,
    diroffset: 0,
};

/// Numbers of in-core files whose entries moved or went, and those given
/// out to avoid them.
#[derive(Default)]
struct Ids {
    by_loc: BTreeMap<Loc, u64>,
    by_id: BTreeMap<u64, Loc>,
    next: u64,
}

/// What every node of one mount shares.
struct Shared {
    fat: Fat<DiskIo>,
    alloc: Mutex<Alloc>,
    rw: bool,
    opts: Opts,
    ids: SpinLock<Ids>,
    /// In-core files and their objects, for `sync`.
    cells: SpinLock<BTreeMap<u64, InCore>>,
}

/// An in-core file, and its object if it is a regular file.
type InCore = (Weak<Cell>, Option<Weak<Object>>);

/// A mounted FAT file system.
pub(crate) struct MsdosFs {
    shared: Arc<Shared>,
}

impl core::fmt::Debug for MsdosFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MsdosFs({})", self.shared.fat.io().disk.name())
    }
}

fn errno(e: fat::fs::Error) -> Errno {
    use fat::fs::Error as E;
    match e {
        E::Io => Errno::EIO,
        E::NotFat => Errno::EINVAL,
        E::Corrupt(what) => {
            kprintln!("msdosfs: corrupt: {what}");
            Errno::EIO
        }
        E::ReadOnly => Errno::EROFS,
        E::NoSpace => Errno::ENOSPC,
        E::Exists => Errno::EEXIST,
        E::NotFound => Errno::ENOENT,
        E::NotEmpty => Errno::ENOTEMPTY,
        E::BadName => Errno::EINVAL,
        E::NameTooLong => Errno::ENAMETOOLONG,
        E::TooBig => Errno::EFBIG,
    }
}

fn now() -> (i64, u32) {
    let t = vfs::now();
    (t.tv_sec, u32::try_from(t.tv_nsec).unwrap_or(0))
}

fn timespec(t: (i64, u32)) -> Timespec {
    Timespec {
        tv_sec: t.0,
        tv_nsec: i64::from(t.1),
    }
}

/// A sleepable context for node operations that are not given one.
fn sleepable() -> Sleepable {
    // SAFETY: node operations and pagers run in thread context (the VFS
    // calls them with vnode locks held; the VM fills with no map lock).
    unsafe { Sleepable::assume() }
}

impl MsdosFs {
    /// Mounts the FAT file system on `disk` (FreeBSD's `mountmsdosfs`).
    pub(crate) fn mount(
        s: &Sleepable,
        disk: Arc<storage::Disk>,
        read_only: bool,
        opts: Opts,
    ) -> Result<Self, Errno> {
        let size = disk.bytes();
        let fat = Fat::open(s, DiskIo::new(disk), size).map_err(errno)?;
        let alloc = fat.load_alloc(s).map_err(errno)?;
        if !read_only {
            fat.set_clean(s, false).map_err(errno)?;
        }
        Ok(Self {
            shared: Arc::new(Shared {
                fat,
                alloc: Mutex::new(&ALLOC, alloc),
                rw: !read_only,
                opts,
                ids: SpinLock::new(Ids {
                    next: 1 << 62,
                    ..Ids::default()
                }),
                cells: SpinLock::new(BTreeMap::new()),
            }),
        })
    }

    /// The cluster size, for the mount's I/O size.
    pub(crate) fn cluster_size(&self) -> u32 {
        self.shared.fat.geometry().cluster
    }
}

impl Shared {
    fn rw(&self) -> Result<(), Errno> {
        if self.rw { Ok(()) } else { Err(Errno::EROFS) }
    }

    /// Entries per 512-byte block, as FreeBSD's file ids count them.
    fn dirsperblk(&self) -> u64 {
        u64::from(self.fat.geometry().bytes_per_sec) / 32
    }

    /// FreeBSD's file id of the directory starting at `start`.
    fn dir_id(&self, start: u32) -> u64 {
        if self.fat.is_root(start) {
            return 1;
        }
        let g = self.fat.geometry();
        (g.cluster_offset(start) / 512) * self.dirsperblk()
    }

    /// FreeBSD's file id of the file whose entry is at `loc`.
    fn file_id(&self, loc: Loc) -> u64 {
        let g = self.fat.geometry();
        let block = if loc.dirclust == ROOT {
            g.root_start
        } else {
            g.cluster_offset(loc.dirclust)
        } / 512;
        block * self.dirsperblk() + u64::from(loc.diroffset) / 32
    }

    /// The number of the entry `n` found: its directory's, or its own
    /// place's, unless an in-core file holds that number (moved away, or
    /// removed but in use): then what this place was given, or a new one.
    fn id_of(&self, n: &Name) -> u64 {
        let (key, natural) = if n.entry.kind() == FatKind::Directory {
            (
                Loc {
                    dirclust: n.entry.start,
                    diroffset: 0,
                },
                self.dir_id(n.entry.start),
            )
        } else {
            (n.loc, self.file_id(n.loc))
        };
        let mut ids = self.ids.lock();
        if let Some(&id) = ids.by_loc.get(&key) {
            return id;
        }
        match ids.by_id.get(&natural) {
            Some(other) if *other != key => {
                let id = ids.next;
                ids.next += 1;
                ids.by_loc.insert(key, id);
                ids.by_id.insert(id, key);
                id
            }
            _ => natural,
        }
    }

    /// In-core file `id` lost its entry (removed while in use): its number
    /// stays its own until it goes.
    fn vacate(&self, id: u64, loc: Loc) {
        let mut ids = self.ids.lock();
        if ids.by_loc.get(&loc) == Some(&id) {
            ids.by_loc.remove(&loc);
        }
        ids.by_id.insert(id, GONE);
    }

    /// Where file number `id` lies: recorded, or worked back from it.
    fn loc_of(&self, id: u64) -> Option<Loc> {
        if let Some(&l) = self.ids.lock().by_id.get(&id) {
            return Some(l);
        }
        let g = self.fat.geometry();
        let per = self.dirsperblk();
        let byte = (id / per) * 512 + (id % per) * 32;
        if byte >= g.data_start {
            let rel = byte - g.data_start;
            let cl = u64::from(g.cluster);
            Some(Loc {
                dirclust: u32::try_from(rel / cl).ok()? + fat::fs::CLUST_FIRST,
                diroffset: u32::try_from(rel % cl).ok()?,
            })
        } else if byte >= g.root_start && g.root_bytes > 0 {
            Some(Loc {
                dirclust: ROOT,
                diroffset: u32::try_from(byte - g.root_start).ok()?,
            })
        } else {
            None
        }
    }

    /// A file's entry moved from `old` to `new`, keeping number `id`.
    fn moved(&self, id: u64, old: Loc, new: Loc) {
        let mut ids = self.ids.lock();
        ids.by_loc.remove(&old);
        if self.file_id(new) == id && !ids.by_id.get(&id).is_some_and(|l| *l != old) {
            ids.by_id.remove(&id);
        } else {
            ids.by_loc.insert(new, id);
            ids.by_id.insert(id, new);
        }
    }

    fn live(&self) -> Vec<(Arc<Cell>, Option<Arc<Object>>)> {
        self.cells
            .lock()
            .values()
            .filter_map(|(c, o)| Some((c.upgrade()?, o.as_ref().and_then(Weak::upgrade))))
            .collect()
    }

    /// Writes every in-core file's pages and changed entry, the delayed
    /// metadata, and FSInfo.
    fn sync_all(&self, s: &Sleepable) -> Result<(), Errno> {
        if !self.rw {
            return Ok(());
        }
        for (c, o) in self.live() {
            if let Some(o) = o {
                let _ = o.sync(0, u64::MAX);
            }
            let mut st = c.state.lock(s);
            if st.dirty && !st.removed && st.loc != fat::fs::dir::ROOT_LOC {
                self.fat
                    .write_entry(s, st.loc, &st.entry, Mode::Delayed)
                    .map_err(errno)?;
                st.dirty = false;
            }
        }
        let mut a = self.alloc.lock(s);
        self.fat.write_fsinfo(s, &mut a).map_err(errno)?;
        drop(a);
        self.fat.io().cache.sync(s).map_err(|_| Errno::EIO)
    }
}

/// An in-core file: its entry and where it is.
struct Cell {
    id: u64,
    state: Mutex<NodeState>,
    /// The size, readable with no lock (the VM asks under the map lock).
    size: AtomicU64,
}

struct NodeState {
    entry: Entry,
    loc: Loc,
    /// Its entry is gone: free its clusters once nothing uses it.
    removed: bool,
    dirty: bool,
}

/// One file's node.
struct MsdosNode {
    cell: Arc<Cell>,
    kind: FatKind,
    /// A directory's `..`.
    parent: u64,
    object: Option<Arc<Object>>,
    fs: Arc<Shared>,
}

impl MsdosNode {
    /// A directory's first cluster (`ROOT` for the root).
    fn start(&self, s: &Sleepable) -> u32 {
        let st = self.cell.state.lock(s);
        if st.loc == fat::fs::dir::ROOT_LOC {
            ROOT
        } else {
            st.entry.start
        }
    }
}

fn msdos_node(n: &mut dyn Any) -> Result<&mut MsdosNode, Errno> {
    n.downcast_mut::<MsdosNode>().ok_or(Errno::EXDEV)
}

impl FileSystem for MsdosFs {
    fn root(&self) -> u64 {
        1
    }

    fn load(&self, s: &Sleepable, id: u64) -> Result<NodeOps, Errno> {
        let shared = &self.shared;
        let fat = &shared.fat;
        let (entry, loc, kind, parent) = if id == 1 {
            // The root has no entry (FreeBSD's `deget` makes one up).
            let start = if fat.geometry().root_cluster != 0 {
                fat.geometry().root_cluster
            } else {
                ROOT
            };
            let t = FatTime {
                date: 0x21,
                time: 0,
                hundredths: 0,
            };
            let e = Entry {
                name: [b' '; 11],
                attr: fat::fs::dir::ATTR_DIRECTORY,
                lcase: 0,
                ctime: t,
                mtime: t,
                adate: 0x21,
                start,
                size: 0,
            };
            (e, fat::fs::dir::ROOT_LOC, FatKind::Directory, 1)
        } else {
            let loc = shared.loc_of(id).ok_or(Errno::ENOENT)?;
            let e = fat.entry_at(s, loc).map_err(errno)?;
            if e.kind() == FatKind::Directory && loc.diroffset == 0 && &e.name == b".          " {
                // A directory, by its `.`; its parent from `..`.
                let dotdot = fat
                    .entry_at(
                        s,
                        Loc {
                            dirclust: loc.dirclust,
                            diroffset: 32,
                        },
                    )
                    .map_err(errno)?;
                (e, loc, FatKind::Directory, shared.dir_id(dotdot.start))
            } else {
                let k = e.kind();
                (e, loc, k, 0)
            }
        };
        let cell = Arc::new(Cell {
            id,
            size: AtomicU64::new(u64::from(entry.size)),
            state: Mutex::new(
                &NODE,
                NodeState {
                    entry,
                    loc,
                    removed: false,
                    dirty: false,
                },
            ),
        });
        let object = (kind == FatKind::File).then(|| {
            let pager = Arc::new(FilePager {
                fs: Arc::clone(shared),
                cell: Arc::clone(&cell),
            });
            Object::with_pager(crate::mm::space::page_ops(), pager)
        });
        shared.cells.lock().insert(
            id,
            (Arc::downgrade(&cell), object.as_ref().map(Arc::downgrade)),
        );
        let node = Box::new(MsdosNode {
            cell,
            kind,
            parent,
            object,
            fs: Arc::clone(shared),
        });
        Ok(match kind {
            FatKind::Directory => NodeOps::Directory(node),
            FatKind::File => NodeOps::Regular(node),
        })
    }

    fn statfs(&self, s: &Sleepable) -> Result<FsStats, Errno> {
        // FreeBSD's `msdosfs_statfs`.
        let g = self.shared.fat.geometry();
        let free = u64::from(self.shared.alloc.lock(s).free);
        let root_ents = g.root_bytes / 32;
        Ok(FsStats {
            bsize: u64::from(g.cluster),
            iosize: u64::from(g.cluster),
            blocks: u64::from(g.max_cluster) + 1,
            bfree: free,
            bavail: i64::try_from(free).unwrap_or(i64::MAX),
            files: root_ents,
            ffree: 0,
        })
    }

    fn rename(&self, s: &Sleepable, r: Rename<'_>) -> Result<(), Errno> {
        let shared = Arc::clone(&self.shared);
        shared.rw()?;
        let from_dir = msdos_node(r.from_dir.as_any_mut())?;
        let from_start = from_dir.start(s);
        let from = msdos_node(r.from.node_mut().as_any_mut())?;
        let to_start = match r.to_dir {
            Some(d) => Some((
                msdos_node(d.as_any_mut())?.start(s),
                d.as_any_mut()
                    .downcast_mut::<MsdosNode>()
                    .map(|n| n.cell.id),
            )),
            None => None,
        };
        let target = match r.to {
            Some((_, ops)) => Some(msdos_node(ops.node_mut().as_any_mut())?),
            None => None,
        };
        let fat = &shared.fat;
        let from_name = fat
            .lookup(s, from_start, r.from_name)
            .map_err(errno)?
            .ok_or(Errno::ENOENT)?;
        let (tstart, tid) = to_start.unwrap_or((from_start, Some(from_dir.cell.id)));
        let target_name = match target {
            Some(_) => fat.lookup(s, tstart, r.to_name).map_err(errno)?,
            None => None,
        };
        let mut a = shared.alloc.lock(s);
        let new = fat
            .rename(
                s,
                &mut a,
                from_start,
                &from_name,
                tstart,
                r.to_name,
                target_name.as_ref(),
            )
            .map_err(errno)?;
        drop(a);
        if let Some(t) = target {
            let mut st = t.cell.state.lock(s);
            st.removed = true;
            shared.vacate(t.cell.id, st.loc);
        }
        let mut st = from.cell.state.lock(s);
        if from.kind == FatKind::File {
            let old = st.loc;
            st.loc = new.loc;
            st.entry.name = new.entry.name;
            drop(st);
            shared.moved(from.cell.id, old, new.loc);
        } else if let Some(id) = tid {
            drop(st);
            from.parent = id;
        }
        Ok(())
    }

    fn sync(&self, s: &Sleepable, _wait: bool) -> Result<(), Errno> {
        self.shared.sync_all(s)
    }

    fn sync_aged(&self, s: &Sleepable, before: u64) {
        if self.shared.rw {
            let _ = self.shared.fat.io().cache.write_aged(s, before);
        }
    }

    fn unmount(&self, s: &Sleepable) -> Result<(), Errno> {
        let shared = &self.shared;
        if shared.rw {
            shared.sync_all(s)?;
            shared.fat.set_clean(s, true).map_err(errno)?;
            shared.fat.io().cache.sync(s).map_err(|_| Errno::EIO)?;
        }
        shared.fat.io().cache.invalidate();
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Node for MsdosNode {
    fn attr(&self) -> Attr {
        // FreeBSD's `msdosfs_getattr`.
        let s = sleepable();
        let shared = &self.fs;
        let g = shared.fat.geometry();
        let st = self.cell.state.lock(&s);
        let e = &st.entry;
        let is_dir = self.kind == FatKind::Directory;
        let mut mode = 0o777u16;
        if e.attr & ATTR_READONLY != 0 {
            mode &= !0o222;
        }
        mode &= if is_dir {
            shared.opts.dirmask
        } else {
            shared.opts.mask
        };
        let size = if is_dir {
            let start = if st.loc == fat::fs::dir::ROOT_LOC {
                ROOT
            } else {
                e.start
            };
            shared.fat.dir_size(&s, start).unwrap_or(0)
        } else {
            u64::from(e.size)
        };
        let cl = u64::from(g.cluster);
        let bytes = if e.start != ROOT
            && !(is_dir && st.loc == fat::fs::dir::ROOT_LOC && g.root_bytes > 0)
        {
            size.next_multiple_of(cl)
        } else {
            0
        };
        let mtime = timespec(time::from_fat(e.mtime));
        let (atime, birthtime) = if shared.opts.shortname {
            (
                mtime,
                Timespec {
                    tv_sec: -1,
                    tv_nsec: 0,
                },
            )
        } else {
            (
                timespec(time::from_fat(FatTime {
                    date: e.adate,
                    time: 0,
                    hundredths: 0,
                })),
                timespec(time::from_fat(e.ctime)),
            )
        };
        let mut flags = 0;
        for (attr, flag) in [
            (ATTR_ARCHIVE, UF_ARCHIVE),
            (ATTR_HIDDEN, UF_HIDDEN),
            (ATTR_READONLY, UF_READONLY),
            (ATTR_SYSTEM, UF_SYSTEM),
        ] {
            if e.attr & attr != 0 {
                flags |= flag;
            }
        }
        Attr {
            mode,
            uid: shared.opts.uid,
            gid: shared.opts.gid,
            nlink: 1,
            size,
            bytes,
            atime,
            mtime,
            ctime: mtime,
            birthtime,
            flags,
            generation: 0,
            rdev: 0,
            blksize: g.cluster,
            // FreeBSD numbers a FAT32 root by its first cluster (the root
            // vnode's key here stays 1).
            fileid: if st.loc == fat::fs::dir::ROOT_LOC && g.root_cluster != 0 {
                (g.cluster_offset(g.root_cluster) / 512) * shared.dirsperblk()
            } else {
                0
            },
        }
    }

    fn setattr(&mut self, s: &Sleepable, a: &SetAttr) -> Result<(), Errno> {
        // FreeBSD's `msdosfs_setattr`.
        let shared = Arc::clone(&self.fs);
        shared.rw()?;
        if let Some(f) = a.flags
            && f & !(UF_ARCHIVE | UF_HIDDEN | UF_READONLY | UF_SYSTEM) != 0
        {
            return Err(Errno::EOPNOTSUPP);
        }
        if a.uid.is_some_and(|u| u != shared.opts.uid)
            || a.gid.is_some_and(|g| g != shared.opts.gid)
        {
            return Err(Errno::EINVAL);
        }
        let mut st = self.cell.state.lock(s);
        if st.loc == fat::fs::dir::ROOT_LOC {
            // The root has no entry to change.
            return Ok(());
        }
        if let Some(size) = a.size {
            if self.kind == FatKind::Directory {
                return Err(Errno::EISDIR);
            }
            let old = u64::from(st.entry.size);
            if let Some(o) = &self.object {
                vfs::pagecache::resize(o, old, size);
            }
            let mut al = shared.alloc.lock(s);
            shared
                .fat
                .resize(s, &mut al, &mut st.entry, size, true)
                .map_err(errno)?;
            drop(al);
            self.cell.size.store(size, Ordering::Release);
            let t = time::to_fat(now().0, now().1);
            st.entry.mtime = FatTime { hundredths: 0, ..t };
            st.entry.attr |= ATTR_ARCHIVE;
        }
        if let Some(f) = a.flags {
            let mut attr = st.entry.attr & !(ATTR_ARCHIVE | ATTR_HIDDEN | ATTR_SYSTEM);
            if f & UF_ARCHIVE != 0 {
                attr |= ATTR_ARCHIVE;
            }
            if f & UF_HIDDEN != 0 {
                attr |= ATTR_HIDDEN;
            }
            if f & UF_SYSTEM != 0 {
                attr |= ATTR_SYSTEM;
            }
            if self.kind == FatKind::File {
                attr &= !ATTR_READONLY;
                if f & UF_READONLY != 0 {
                    attr |= ATTR_READONLY;
                }
            }
            st.entry.attr = attr;
        }
        if let Some(t) = a.atime
            && !shared.opts.shortname
        {
            st.entry.adate = time::to_fat(t.tv_sec, u32::try_from(t.tv_nsec).unwrap_or(0)).date;
        }
        if let Some(t) = a.mtime {
            let f = time::to_fat(t.tv_sec, u32::try_from(t.tv_nsec).unwrap_or(0));
            st.entry.mtime = FatTime { hundredths: 0, ..f };
            st.entry.attr |= ATTR_ARCHIVE;
        }
        if let Some(m) = a.mode
            && self.kind == FatKind::File
        {
            if m & 0o200 != 0 {
                st.entry.attr &= !ATTR_READONLY;
            } else {
                st.entry.attr |= ATTR_READONLY;
            }
            st.entry.attr |= ATTR_ARCHIVE;
        }
        st.dirty = true;
        Ok(())
    }

    fn fsync(&mut self, s: &Sleepable) -> Result<(), Errno> {
        let shared = &self.fs;
        if !shared.rw {
            return Ok(());
        }
        let mut st = self.cell.state.lock(s);
        if st.dirty && !st.removed && st.loc != fat::fs::dir::ROOT_LOC {
            shared
                .fat
                .write_entry(s, st.loc, &st.entry, Mode::Sync)
                .map_err(errno)?;
            st.dirty = false;
        }
        drop(st);
        shared.fat.io().cache.sync(s).map_err(|_| Errno::EIO)
    }

    fn inactive(&mut self, s: &Sleepable) {
        let shared = Arc::clone(&self.fs);
        if !shared.rw {
            return;
        }
        let mut st = self.cell.state.lock(s);
        if st.removed && self.kind == FatKind::File && st.entry.start != 0 {
            if let Some(o) = &self.object {
                o.truncate(0);
            }
            let mut al = shared.alloc.lock(s);
            let _ = shared.fat.resize(s, &mut al, &mut st.entry, 0, false);
            self.cell.size.store(0, Ordering::Release);
        }
    }

    fn reclaim(self: Box<Self>, s: &Sleepable) {
        let shared = &self.fs;
        if let Some(o) = &self.object {
            if shared.rw {
                let _ = o.sync(0, u64::MAX);
            }
            o.disown();
        }
        let loc;
        {
            let mut st = self.cell.state.lock(s);
            loc = st.loc;
            if st.dirty && !st.removed && shared.rw && st.loc != fat::fs::dir::ROOT_LOC {
                let _ = shared.fat.write_entry(s, st.loc, &st.entry, Mode::Delayed);
                st.dirty = false;
            }
        }
        let id = self.cell.id;
        {
            let mut ids = shared.ids.lock();
            match ids.by_id.get(&id) {
                Some(&l) if l == loc => {
                    ids.by_id.remove(&id);
                    ids.by_loc.remove(&loc);
                }
                Some(&l) if l == GONE => {
                    ids.by_id.remove(&id);
                }
                _ => {}
            }
        }
        let mut c = shared.cells.lock();
        if c.get(&id)
            .is_some_and(|(w, _)| w.as_ptr() == Arc::as_ptr(&self.cell))
        {
            c.remove(&id);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl vfs::RegularNode for MsdosNode {
    fn read(&self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let object = self.object.as_ref().ok_or(Errno::EINVAL)?;
        let size = u64::from(self.cell.state.lock(s).entry.size);
        vfs::pagecache::read(object, size, offset, uio)
    }

    fn write(&mut self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let shared = Arc::clone(&self.fs);
        shared.rw()?;
        let object = self.object.clone().ok_or(Errno::EINVAL)?;
        let len = uio.resid() as u64;
        if len == 0 {
            return Ok(0);
        }
        let end = offset.checked_add(len).ok_or(Errno::EFBIG)?;
        // A FAT file is under 4 GiB.
        if end > u64::from(u32::MAX) {
            return Err(Errno::EFBIG);
        }
        let old;
        {
            let mut st = self.cell.state.lock(s);
            old = u64::from(st.entry.size);
            if end > old {
                // New clusters read as zeros (FreeBSD's `deextend`).
                let mut al = shared.alloc.lock(s);
                shared
                    .fat
                    .resize(s, &mut al, &mut st.entry, end, true)
                    .map_err(errno)?;
                drop(al);
                self.cell.size.store(end, Ordering::Release);
                st.dirty = true;
            }
        }
        if offset > old {
            vfs::pagecache::resize(&object, old, offset);
        }
        let written = vfs::pagecache::write(&object, offset, uio);
        let mut st = self.cell.state.lock(s);
        let done = written.as_ref().map_or(0, |&n| n as u64);
        let want = old.max(offset + done);
        if u64::from(st.entry.size) > want {
            let mut al = shared.alloc.lock(s);
            let _ = shared.fat.resize(s, &mut al, &mut st.entry, want, false);
            drop(al);
            self.cell.size.store(want, Ordering::Release);
        }
        let t = now();
        let f = time::to_fat(t.0, t.1);
        st.entry.mtime = FatTime { hundredths: 0, ..f };
        st.entry.attr |= ATTR_ARCHIVE;
        st.dirty = true;
        written
    }

    fn object(&self) -> Option<Arc<Object>> {
        self.object.clone()
    }
}

impl DirectoryNode for MsdosNode {
    fn lookup(&self, s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        let start = self.start(s);
        let n = self
            .fs
            .fat
            .lookup(s, start, name)
            .map_err(errno)?
            .ok_or(Errno::ENOENT)?;
        Ok(self.fs.id_of(&n))
    }

    fn parent(&self) -> u64 {
        self.parent
    }

    fn create(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        node: &NewNode,
        _c: &Create,
    ) -> Result<u64, Errno> {
        let shared = Arc::clone(&self.fs);
        shared.rw()?;
        let kind = match node {
            NewNode::Regular => FatKind::File,
            NewNode::Directory => FatKind::Directory,
            // FreeBSD's msdosfs makes no symbolic links (`EOPNOTSUPP`), and
            // `mknod` of anything else fails (`EINVAL`).
            NewNode::Symlink(_) => return Err(Errno::EOPNOTSUPP),
            NewNode::CharDevice(_) | NewNode::Fifo | NewNode::Socket => return Err(Errno::EINVAL),
        };
        let start = self.start(s);
        let _st = self.cell.state.lock(s);
        let mut a = shared.alloc.lock(s);
        let n = shared
            .fat
            .create(s, &mut a, start, name, kind, now())
            .map_err(errno)?;
        drop(a);
        Ok(shared.id_of(&n))
    }

    fn link(&mut self, _: &Sleepable, _: &[u8], _: u64, _: &mut dyn Node) -> Result<(), Errno> {
        // FreeBSD's `msdosfs_link`.
        Err(Errno::EOPNOTSUPP)
    }

    fn remove(&mut self, s: &Sleepable, name: &[u8], child: &mut dyn Node) -> Result<(), Errno> {
        let shared = Arc::clone(&self.fs);
        shared.rw()?;
        let child = msdos_node(child.as_any_mut())?;
        let start = self.start(s);
        let _st = self.cell.state.lock(s);
        let n = shared
            .fat
            .lookup(s, start, name)
            .map_err(errno)?
            .ok_or(Errno::ENOENT)?;
        shared.fat.remove(s, start, &n).map_err(errno)?;
        let mut cs = child.cell.state.lock(s);
        cs.removed = true;
        shared.vacate(child.cell.id, cs.loc);
        Ok(())
    }

    fn rmdir(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        child: &mut dyn DirectoryNode,
    ) -> Result<(), Errno> {
        let shared = Arc::clone(&self.fs);
        shared.rw()?;
        let child = msdos_node(child.as_any_mut())?;
        let start = self.start(s);
        let _st = self.cell.state.lock(s);
        let n = shared
            .fat
            .lookup(s, start, name)
            .map_err(errno)?
            .ok_or(Errno::ENOENT)?;
        let mut a = shared.alloc.lock(s);
        shared.fat.rmdir(s, &mut a, start, &n).map_err(errno)?;
        drop(a);
        let mut cs = child.cell.state.lock(s);
        cs.removed = true;
        shared.vacate(child.cell.id, cs.loc);
        Ok(())
    }

    fn readdir(
        &self,
        s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        // FreeBSD's `msdosfs_readdir`: the root's `.` and `..` are made up
        // (number 1), and its entries' offsets come after them.
        let shared = &self.fs;
        let start = self.start(s);
        let root = shared.fat.is_root(start);
        let bias = if root { 64 } else { 0 };
        if root {
            for (cookie, name) in [(0u64, &b"."[..]), (32, b"..")] {
                if from <= cookie
                    && !f(DirEntry {
                        name,
                        ino: 1,
                        kind: Kind::Directory,
                        next: cookie + 32,
                    })
                {
                    return Ok(());
                }
            }
        }
        let real = from.saturating_sub(bias);
        let shortname = shared.opts.shortname;
        shared
            .fat
            .entries(s, start, real, &mut |n| {
                let ino = if n.entry.kind() == FatKind::Directory {
                    shared.dir_id(n.entry.start)
                } else {
                    shared.file_id(n.loc)
                };
                let short;
                let name: &[u8] = if shortname {
                    short = fat::fs::conv::dos2unixfn(
                        &n.entry.name,
                        fat::fs::conv::LCASE_BASE | fat::fs::conv::LCASE_EXT,
                    );
                    &short
                } else {
                    &n.name
                };
                let kind = if n.entry.kind() == FatKind::Directory {
                    Kind::Directory
                } else {
                    Kind::Regular
                };
                !f(DirEntry {
                    name,
                    ino,
                    kind,
                    next: n.diroff + 32 + bias,
                })
            })
            .map_err(errno)
    }
}

/// A file's pages, read from and written to its clusters.
struct FilePager {
    fs: Arc<Shared>,
    cell: Arc<Cell>,
}

const PAGE: u64 = 4096;

impl FilePager {
    /// Calls `f` with each piece of file bytes `[from, to)`: its disk
    /// offset, and where in the range it starts and how long it is.
    fn pieces(
        &self,
        s: &Sleepable,
        start: u32,
        from: u64,
        to: u64,
        f: &mut dyn FnMut(u64, usize, usize) -> Result<(), PagerError>,
    ) -> Result<(), PagerError> {
        let mut at = from;
        while at < to {
            let (disk, run) = self
                .fs
                .fat
                .bmap(s, start, at)
                .map_err(|_| PagerError)?
                .ok_or(PagerError)?;
            let n = run.min(to - at);
            f(
                disk,
                usize::try_from(at - from).map_err(|_| PagerError)?,
                usize::try_from(n).map_err(|_| PagerError)?,
            )?;
            at += n;
        }
        Ok(())
    }
}

impl Pager for FilePager {
    fn size(&self) -> PageIndex {
        self.cell.size.load(Ordering::Acquire).div_ceil(PAGE)
    }

    fn fill(&self, index: PageIndex, page: &mut [u8]) -> Result<(), PagerError> {
        let s = sleepable();
        let st = self.cell.state.lock(&s);
        let size = u64::from(st.entry.size);
        let start = st.entry.start;
        let from = index * PAGE;
        page.fill(0);
        if from >= size {
            return Ok(());
        }
        let to = (from + PAGE).min(size);
        let disk = &self.fs.fat.io().disk;
        self.pieces(&s, start, from, to, &mut |at, off, n| {
            // Whole sectors; the tail past the end stays zero.
            let ss = usize::try_from(disk.sector_size()).unwrap_or(512);
            let data = disk
                .read(&s, at, n.next_multiple_of(ss))
                .map_err(|_| PagerError)?;
            page[off..off + n].copy_from_slice(&data[..n]);
            Ok(())
        })
    }

    fn writes_back(&self) -> bool {
        self.fs.rw
    }

    fn write_back(&self, index: PageIndex, page: &[u8]) -> Result<(), PagerError> {
        let s = sleepable();
        let st = self.cell.state.lock(&s);
        let size = u64::from(st.entry.size);
        let from = index * PAGE;
        if from >= size || st.removed {
            return Ok(());
        }
        let start = st.entry.start;
        let cl = u64::from(self.fs.fat.geometry().cluster);
        // Up to the end of the file's last cluster (allocated whole).
        let to = (from + PAGE).min(size.next_multiple_of(cl));
        let disk = &self.fs.fat.io().disk;
        self.pieces(&s, start, from, to, &mut |at, off, n| {
            disk.write(&s, at, &page[off..off + n])
                .map_err(|_| PagerError)
        })
    }
}
