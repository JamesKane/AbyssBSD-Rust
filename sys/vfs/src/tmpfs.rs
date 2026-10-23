// SPDX-License-Identifier: BSD-2-Clause
//! tmpfs (docs/VFS.md §10.2): a file system in memory.
//!
//! Every node lives in the file system's table while no vnode holds it,
//! and inside its vnode while one does (handed over by `load`, back by
//! `reclaim`). Directories map names to inodes; each entry gets a cookie
//! when it is made, so `getdirentries` resumes where it left off however
//! the directory changes meanwhile (0 is `.`, 1 is `..`, as FreeBSD's
//! tmpfs numbers them). A file's data is an anonymous VM object, read and
//! written through the page cache and mapped as it is (FreeBSD's tmpfs
//! uses a swap object): a page never written is a hole that reads as
//! zeros.
//!
//! Behavior reference: sys/fs/tmpfs.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::errno::Errno;
use intmap::IntMap;
use sched::Sleepable;
use sync::PreemptLock;

use crate::mount::{FileSystem, FsStats, Rename};
use crate::node::{
    Attr, Create, DirEntry, DirectoryNode, Kind, Made, NewNode, Node, NodeOps, RegularNode,
    SetAttr, SymlinkNode,
};
use crate::{Uio, now, pagecache};

/// The root directory's inode, as FreeBSD's tmpfs numbers it.
pub const ROOT_INO: u64 = 2;

/// The most links a node may have (FreeBSD's tmpfs: `TMPFS_LINK_MAX`).
const LINK_MAX: u64 = i32::MAX as u64;

/// What a node holds.
enum Data {
    /// The data's pages, and the size.
    Regular(Arc<vm::object::Object>, u64),
    Directory(Dir),
    Symlink(Vec<u8>),
    /// A FIFO, a socket, or a device node.
    Other,
}

struct Dir {
    parent: u64,
    /// A name's hash to the cookies of the entries with that hash (one,
    /// nearly always): a lookup compares numbers, then one name (FreeBSD's
    /// tmpfs keys its entries by a name's hash too). Hashed, so a lookup
    /// probes a slot or two rather than walking a tree.
    names: IntMap<u64, Cookies>,
    /// Cookie to the entry.
    entries: IntMap<u64, (Box<[u8]>, u64, Kind)>,
    /// The cookies in creation order, for `readdir` to resume from one.
    order: BTreeSet<u64>,
    next_cookie: u64,
}

/// The entries whose names share a hash.
enum Cookies {
    One(u64),
    More(Vec<u64>),
}

impl Cookies {
    fn as_slice(&self) -> &[u64] {
        match self {
            Self::One(c) => core::slice::from_ref(c),
            Self::More(v) => v,
        }
    }
}

impl Dir {
    fn new(parent: u64) -> Self {
        Self {
            parent,
            names: IntMap::new(),
            entries: IntMap::new(),
            order: BTreeSet::new(),
            next_cookie: 2,
        }
    }

    /// The cookie of entry `name`.
    fn cookie(&self, h: u64, name: &[u8]) -> Option<u64> {
        self.names
            .get(&h)?
            .as_slice()
            .iter()
            .copied()
            .find(|c| self.entries.get(c).is_some_and(|(n, ..)| **n == *name))
    }

    fn get(&self, name: &[u8]) -> Option<(u64, Kind)> {
        let c = self.cookie(hash(name), name)?;
        self.entries.get(&c).map(|(_, ino, k)| (*ino, *k))
    }

    fn insert(&mut self, name: &[u8], ino: u64, kind: Kind) {
        let c = self.next_cookie;
        self.next_cookie += 1;
        self.entries.insert(c, (name.into(), ino, kind));
        self.order.insert(c);
        let h = hash(name);
        match self.names.get_mut(&h) {
            None => {
                self.names.insert(h, Cookies::One(c));
            }
            Some(e) => {
                let more = match e {
                    Cookies::One(o) => alloc::vec![*o, c],
                    Cookies::More(v) => [v.as_slice(), &[c]].concat(),
                };
                *e = Cookies::More(more);
            }
        }
    }

    fn remove(&mut self, name: &[u8]) -> Option<u64> {
        let h = hash(name);
        let c = self.cookie(h, name)?;
        if let Some(e) = self.names.get_mut(&h) {
            let rest: Vec<u64> = e.as_slice().iter().copied().filter(|x| *x != c).collect();
            match rest.as_slice() {
                [] => {
                    self.names.remove(&h);
                }
                [one] => *e = Cookies::One(*one),
                _ => *e = Cookies::More(rest),
            }
        }
        self.order.remove(&c);
        self.entries.remove(&c).map(|(_, ino, _)| ino)
    }
}

/// A name's hash, for [`Dir`].
fn hash(name: &[u8]) -> u64 {
    crate::cache::hash_name(0, name)
}

/// One file.
struct TmpNode {
    ino: u64,
    kind: Kind,
    attr: Attr,
    data: Data,
    fs: Arc<Shared>,
    /// Freed by `inactive` (unlinked): `reclaim` drops it.
    freed: bool,
}

/// The state every node of one mount shares.
struct Shared {
    /// Where file pages come from.
    pages: &'static dyn vm::object::PageOps,
    /// Nodes no vnode holds.
    nodes: PreemptLock<BTreeMap<u64, Box<TmpNode>>>,
    next_ino: PreemptLock<u64>,
    /// Bytes and nodes in use, for `statfs`.
    usage: PreemptLock<(u64, u64)>,
}

/// A mounted tmpfs.
pub struct TmpFs {
    shared: Arc<Shared>,
}

impl core::fmt::Debug for TmpFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (bytes, nodes) = *self.shared.usage.lock();
        write!(f, "TmpFs({nodes} nodes, {bytes} bytes)")
    }
}

impl TmpFs {
    /// An empty tmpfs whose root has permissions `mode`, owned by `uid`
    /// and `gid`, whose files' pages come from `pages`.
    #[must_use]
    pub fn new(mode: u16, uid: u32, gid: u32, pages: &'static dyn vm::object::PageOps) -> Self {
        let shared = Arc::new(Shared {
            pages,
            nodes: PreemptLock::new(BTreeMap::new()),
            next_ino: PreemptLock::new(ROOT_INO + 1),
            usage: PreemptLock::new((0, 1)),
        });
        let t = now();
        let root = Box::new(TmpNode {
            ino: ROOT_INO,
            kind: Kind::Directory,
            attr: Attr {
                mode,
                uid,
                gid,
                nlink: 2,
                atime: t,
                mtime: t,
                ctime: t,
                birthtime: t,
                ..Attr::default()
            },
            data: Data::Directory(Dir::new(ROOT_INO)),
            fs: Arc::clone(&shared),
            freed: false,
        });
        shared.nodes.lock().insert(ROOT_INO, root);
        Self { shared }
    }
}

fn downcast(n: &mut dyn Node) -> Result<&mut TmpNode, Errno> {
    n.as_any_mut().downcast_mut::<TmpNode>().ok_or(Errno::EXDEV)
}

fn dir_mut(n: &mut TmpNode) -> Result<&mut Dir, Errno> {
    match &mut n.data {
        Data::Directory(d) => Ok(d),
        _ => Err(Errno::ENOTDIR),
    }
}

impl TmpNode {
    fn dir(&self) -> Result<&Dir, Errno> {
        match &self.data {
            Data::Directory(d) => Ok(d),
            _ => Err(Errno::ENOTDIR),
        }
    }

    /// The directory's contents changed.
    fn touch_dir(&mut self) {
        let t = now();
        self.attr.mtime = t;
        self.attr.ctime = t;
    }
}

impl Node for TmpNode {
    fn attr(&self) -> Attr {
        let mut a = self.attr;
        a.size = match &self.data {
            Data::Regular(_, size) => *size,
            Data::Symlink(v) => v.len() as u64,
            // FreeBSD's tmpfs: a fixed size per entry.
            Data::Directory(d) => (d.entries.len() as u64 + 2) * 32,
            Data::Other => 0,
        };
        a.bytes = a.size.next_multiple_of(4096);
        a
    }

    fn setattr(&mut self, _s: &Sleepable, a: &SetAttr) -> Result<(), Errno> {
        let t = now();
        if let Some(size) = a.size {
            let Data::Regular(object, cur) = &mut self.data else {
                return Err(Errno::EINVAL);
            };
            if i64::try_from(size).is_err() {
                return Err(Errno::EFBIG);
            }
            pagecache::resize(object, *cur, size);
            usage(&self.fs, size, *cur);
            *cur = size;
            self.attr.mtime = t;
        }
        if let Some(m) = a.mode {
            self.attr.mode = m & 0o7777;
        }
        if let Some(u) = a.uid {
            self.attr.uid = u;
        }
        if let Some(g) = a.gid {
            self.attr.gid = g;
        }
        if let Some(x) = a.atime {
            self.attr.atime = x;
        }
        if let Some(x) = a.mtime {
            self.attr.mtime = x;
        }
        if let Some(x) = a.birthtime {
            self.attr.birthtime = x;
        }
        if let Some(f) = a.flags {
            self.attr.flags = f;
        }
        self.attr.ctime = t;
        Ok(())
    }

    fn need_inactive(&self, _s: &Sleepable) -> bool {
        self.attr.nlink == 0 && !self.freed
    }

    fn inactive(&mut self, _s: &Sleepable) {
        if self.attr.nlink == 0 && !self.freed {
            let bytes = match &self.data {
                Data::Regular(_, size) => *size,
                Data::Symlink(v) => v.len() as u64,
                _ => 0,
            };
            usage(&self.fs, 0, bytes);
            self.fs.usage.lock().1 -= 1;
            self.data = Data::Other;
            self.freed = true;
        }
    }

    fn reclaim(self: Box<Self>, _s: &Sleepable) {
        if !self.freed {
            let fs = Arc::clone(&self.fs);
            fs.nodes.lock().insert(self.ino, self);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Moves the usage count from `old` bytes to `new`.
fn usage(fs: &Shared, new: u64, old: u64) {
    let mut u = fs.usage.lock();
    u.0 = u.0.saturating_sub(old) + new;
}

impl RegularNode for TmpNode {
    fn read(&self, _s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let Data::Regular(object, size) = &self.data else {
            return Err(Errno::EINVAL);
        };
        pagecache::read(object, *size, offset, uio)
    }

    fn write(&mut self, _s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let Data::Regular(object, size) = &mut self.data else {
            return Err(Errno::EINVAL);
        };
        let end = offset.checked_add(uio.resid() as u64).ok_or(Errno::EFBIG)?;
        if i64::try_from(end).is_err() {
            return Err(Errno::EFBIG);
        }
        // Past the end: what lies between reads as zeros (resize zeroes
        // the rest of the old last page).
        if offset > *size {
            pagecache::resize(object, *size, offset);
        }
        let n = pagecache::write(object, offset, uio)?;
        let new = (*size).max(offset + n as u64);
        usage(&self.fs, new, *size);
        *size = new;
        let t = now();
        self.attr.mtime = t;
        self.attr.ctime = t;
        Ok(n)
    }

    fn object(&self) -> Option<Arc<vm::object::Object>> {
        match &self.data {
            Data::Regular(o, _) => Some(Arc::clone(o)),
            _ => None,
        }
    }
}

impl SymlinkNode for TmpNode {
    fn target(&self, _s: &Sleepable) -> Result<Vec<u8>, Errno> {
        match &self.data {
            Data::Symlink(t) => Ok(t.clone()),
            _ => Err(Errno::EINVAL),
        }
    }
}

impl DirectoryNode for TmpNode {
    fn lookup(&self, _s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        self.dir()?
            .get(name)
            .map(|(ino, _)| ino)
            .ok_or(Errno::ENOENT)
    }

    fn parent(&self) -> u64 {
        self.dir().map_or(self.ino, |d| d.parent)
    }

    fn create(
        &mut self,
        _s: &Sleepable,
        name: &[u8],
        node: &NewNode,
        c: &Create,
    ) -> Result<Made, Errno> {
        let kind = node.kind();
        if kind == Kind::Directory && self.attr.nlink >= LINK_MAX {
            return Err(Errno::EMLINK);
        }
        let ino = {
            let mut n = self.fs.next_ino.lock();
            let ino = *n;
            *n += 1;
            ino
        };
        let (data, nlink, rdev) = match node {
            NewNode::Regular => {
                // The file holds its object, so no mapping is ever its
                // only one: unmapping must not free the file's pages
                // (FreeBSD clears `OBJ_ONEMAPPING` on tmpfs objects).
                let object = vm::object::Object::anonymous(self.fs.pages);
                object.set_shared();
                (Data::Regular(object, 0), 1, 0)
            }
            NewNode::Directory => (Data::Directory(Dir::new(self.ino)), 2, 0),
            NewNode::Symlink(t) => (Data::Symlink(t.clone()), 1, 0),
            NewNode::CharDevice(rdev) | NewNode::BlockDevice(rdev) => (Data::Other, 1, *rdev),
            NewNode::Fifo | NewNode::Socket => (Data::Other, 1, 0),
        };
        let bytes = match &data {
            Data::Symlink(t) => t.len() as u64,
            _ => 0,
        };
        let new = Box::new(TmpNode {
            ino,
            kind,
            attr: Attr {
                mode: c.mode & 0o7777,
                uid: c.uid,
                gid: c.gid,
                nlink,
                atime: c.time,
                mtime: c.time,
                ctime: c.time,
                birthtime: c.time,
                generation: 1,
                rdev,
                ..Attr::default()
            },
            data,
            fs: Arc::clone(&self.fs),
            freed: false,
        });
        {
            let mut u = self.fs.usage.lock();
            u.0 += bytes;
            u.1 += 1;
        }
        dir_mut(self)?.insert(name, ino, kind);
        if kind == Kind::Directory {
            self.attr.nlink += 1;
        }
        self.touch_dir();
        Ok(Made {
            ino,
            ops: Some(ops_of(new)),
        })
    }

    fn link(
        &mut self,
        _s: &Sleepable,
        name: &[u8],
        ino: u64,
        target: &mut dyn Node,
    ) -> Result<(), Errno> {
        let t = downcast(target)?;
        if t.attr.nlink >= LINK_MAX {
            return Err(Errno::EMLINK);
        }
        t.attr.nlink += 1;
        t.attr.ctime = now();
        let kind = t.kind;
        dir_mut(self)?.insert(name, ino, kind);
        self.touch_dir();
        Ok(())
    }

    fn remove(&mut self, _s: &Sleepable, name: &[u8], child: &mut dyn Node) -> Result<(), Errno> {
        let c = downcast(child)?;
        dir_mut(self)?.remove(name).ok_or(Errno::ENOENT)?;
        c.attr.nlink -= 1;
        c.attr.ctime = now();
        self.touch_dir();
        Ok(())
    }

    fn rmdir(
        &mut self,
        _s: &Sleepable,
        name: &[u8],
        child: &mut dyn DirectoryNode,
    ) -> Result<(), Errno> {
        let c = downcast(child)?;
        if !c.dir()?.entries.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        dir_mut(self)?.remove(name).ok_or(Errno::ENOENT)?;
        c.attr.nlink = 0;
        self.attr.nlink -= 1;
        self.touch_dir();
        Ok(())
    }

    fn readdir(
        &self,
        _s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        let d = self.dir()?;
        if from == 0
            && !f(DirEntry {
                name: b".",
                ino: self.ino,
                kind: Kind::Directory,
                next: 1,
            })
        {
            return Ok(());
        }
        if from <= 1
            && !f(DirEntry {
                name: b"..",
                ino: d.parent,
                kind: Kind::Directory,
                next: 2,
            })
        {
            return Ok(());
        }
        for &c in d.order.range(from.max(2)..) {
            let Some((name, ino, kind)) = d.entries.get(&c) else {
                continue;
            };
            if !f(DirEntry {
                name,
                ino: *ino,
                kind: *kind,
                next: c + 1,
            }) {
                break;
            }
        }
        Ok(())
    }
}

/// A node as its kind's operations.
fn ops_of(n: Box<TmpNode>) -> NodeOps {
    match n.kind {
        Kind::Regular => NodeOps::Regular(n),
        Kind::Directory => NodeOps::Directory(n),
        Kind::Symlink => NodeOps::Symlink(n),
        k => NodeOps::Other(k, n),
    }
}

impl FileSystem for TmpFs {
    fn root(&self) -> u64 {
        ROOT_INO
    }

    fn load(&self, _s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        let n = self.shared.nodes.lock().remove(&ino).ok_or(Errno::ENOENT)?;
        Ok(ops_of(n))
    }

    fn pathconf(&self, kind: Kind, name: u32) -> Result<i64, Errno> {
        use abi::fcntl::*;
        match name {
            _PC_LINK_MAX => Ok(LINK_MAX as i64),
            _PC_SYMLINK_MAX | _PC_PATH_MAX => Ok(PATH_MAX as i64),
            _PC_NAME_MAX => Ok(NAME_MAX as i64),
            _PC_PIPE_BUF if matches!(kind, Kind::Directory | Kind::Fifo) => Ok(512),
            _PC_PIPE_BUF => Err(Errno::EINVAL),
            _PC_CHOWN_RESTRICTED | _PC_NO_TRUNC | _PC_SYNC_IO | _PC_HAS_HIDDENSYSTEM => Ok(1),
            _PC_FILESIZEBITS => Ok(64),
            _PC_MIN_HOLE_SIZE => Ok(4096),
            _ => crate::mount::std_pathconf(name),
        }
    }

    fn statfs(&self, _s: &Sleepable) -> Result<FsStats, Errno> {
        let (bytes, nodes) = *self.shared.usage.lock();
        // No limit is set: report what is used and plenty free, as
        // FreeBSD's tmpfs does with no `size` (counted in pages).
        let used = bytes.div_ceil(4096);
        let total = used + (1 << 20);
        Ok(FsStats {
            bsize: 4096,
            iosize: 4096,
            blocks: total,
            bfree: total - used,
            bavail: i64::try_from(total - used).unwrap_or(i64::MAX),
            files: nodes + (1 << 20),
            ffree: 1 << 20,
        })
    }

    fn rename(&self, _s: &Sleepable, mut r: Rename<'_>) -> Result<(), Errno> {
        let t = now();
        let from_kind = r.from.kind();
        let from = downcast(r.from.node_mut())?;
        let from_dir = downcast(r.from_dir)?;
        let to_dir_ino;
        // The target's old entry goes first.
        if let Some((_, old)) = r.to {
            let old = downcast(old.node_mut())?;
            let td: &mut TmpNode = match r.to_dir.as_deref_mut() {
                Some(d) => downcast(d)?,
                None => &mut *from_dir,
            };
            dir_mut(td)?.remove(r.to_name).ok_or(Errno::ENOENT)?;
            if old.kind == Kind::Directory {
                if !old.dir()?.entries.is_empty() {
                    return Err(Errno::ENOTEMPTY);
                }
                old.attr.nlink = 0;
                td.attr.nlink -= 1;
            } else {
                old.attr.nlink -= 1;
            }
            old.attr.ctime = t;
        }
        dir_mut(from_dir)?
            .remove(r.from_name)
            .ok_or(Errno::ENOENT)?;
        from_dir.touch_dir();
        match r.to_dir {
            Some(d) => {
                let td = downcast(d)?;
                dir_mut(td)?.insert(r.to_name, r.from_ino, from_kind);
                td.touch_dir();
                to_dir_ino = td.ino;
                if from_kind == Kind::Directory {
                    td.attr.nlink += 1;
                    from_dir.attr.nlink -= 1;
                }
            }
            None => {
                dir_mut(from_dir)?.insert(r.to_name, r.from_ino, from_kind);
                to_dir_ino = from_dir.ino;
            }
        }
        if let Data::Directory(d) = &mut from.data {
            d.parent = to_dir_ino;
        }
        from.attr.ctime = t;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
