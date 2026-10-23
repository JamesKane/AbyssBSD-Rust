// SPDX-License-Identifier: BSD-2-Clause
//! File systems built once and then fixed: a [`Builder`] collects
//! directories, files, links, and devices by path, and [`Builder::finish`]
//! turns them into a read-only [`TreeFs`]. tarfs and the boot devfs are
//! both built this way.
//!
//! Inode numbers follow insertion order from 2 (the root), as UFS numbers
//! its root. A directory's link count is 2 plus its subdirectories; a
//! file's is the number of names it has. Directory entries come in name
//! order, after `.` and `..`.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::errno::Errno;
use sched::Sleepable;

use crate::mount::{FileSystem, FsStats, Rename};
use crate::node::{
    Attr, Create, Device, DeviceNode, DirEntry, DirectoryNode, Kind, Made, NewNode, Node, NodeOps,
    RegularNode, SetAttr, SymlinkNode,
};
use crate::{Meta, Uio};

/// The root's inode number.
pub const ROOT_INO: u64 = 2;

/// What a leaf of the tree holds.
#[derive(Clone)]
pub enum Leaf {
    /// A file whose contents sit in memory for the kernel's lifetime.
    Regular(&'static [u8]),
    Symlink(Arc<[u8]>),
    Device(Device),
}

impl core::fmt::Debug for Leaf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Regular(r) => write!(f, "Regular({} bytes)", r.len()),
            Self::Symlink(t) => write!(f, "Symlink({t:?})"),
            Self::Device(d) => write!(f, "Device({:#x})", d.rdev),
        }
    }
}

enum Proto {
    Dir {
        meta: Meta,
        children: BTreeMap<Box<[u8]>, usize>,
    },
    Leaf {
        meta: Meta,
        leaf: Leaf,
        links: u64,
    },
}

/// Collects a tree; node 0 is the root.
pub struct Builder {
    nodes: Vec<Proto>,
}

impl core::fmt::Debug for Builder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Builder({} nodes)", self.nodes.len())
    }
}

/// A path's components, without empty ones and `.`; `..` is refused.
fn components(path: &[u8]) -> impl Iterator<Item = Result<&[u8], Errno>> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(|c| {
            if c == b".." {
                Err(Errno::EINVAL)
            } else if c.len() > abi::fcntl::NAME_MAX {
                Err(Errno::ENAMETOOLONG)
            } else {
                Ok(c)
            }
        })
}

impl Builder {
    /// An empty tree whose root has `root`'s ownership and permissions.
    #[must_use]
    pub fn new(root: Meta) -> Self {
        Self {
            nodes: alloc::vec![Proto::Dir {
                meta: root,
                children: BTreeMap::new(),
            }],
        }
    }

    /// The inode number the next node added will have.
    #[must_use]
    pub fn next_ino(&self) -> u64 {
        ino(self.nodes.len())
    }

    /// The node at `path`, if there is one.
    #[must_use]
    pub fn find(&self, path: &[u8]) -> Option<usize> {
        let mut at = 0;
        for c in components(path) {
            let Proto::Dir { children, .. } = &self.nodes[at] else {
                return None;
            };
            at = *children.get(c.ok()?)?;
        }
        Some(at)
    }

    /// The child `name` of directory `dir`, made a directory with `meta`
    /// if it does not exist.
    fn subdir(&mut self, dir: usize, name: &[u8], meta: Meta) -> Result<usize, Errno> {
        let next = self.nodes.len();
        let Proto::Dir { children, .. } = &mut self.nodes[dir] else {
            return Err(Errno::ENOTDIR);
        };
        if let Some(&c) = children.get(name) {
            return match self.nodes[c] {
                Proto::Dir { .. } => Ok(c),
                Proto::Leaf { .. } => Err(Errno::ENOTDIR),
            };
        }
        children.insert(name.into(), next);
        self.nodes.push(Proto::Dir {
            meta,
            children: BTreeMap::new(),
        });
        Ok(next)
    }

    /// The directory holding `path`'s last component, and that component
    /// (`None` for the root); missing directories on the way are made
    /// with `meta`.
    fn parent<'p>(
        &mut self,
        path: &'p [u8],
        meta: Meta,
    ) -> Result<(usize, Option<&'p [u8]>), Errno> {
        let parts: Vec<&[u8]> = components(path).collect::<Result<_, _>>()?;
        let Some((last, dirs)) = parts.split_last() else {
            return Ok((0, None));
        };
        let mut at = 0;
        for d in dirs {
            at = self.subdir(at, d, meta)?;
        }
        Ok((at, Some(last)))
    }

    /// The directory at `path`, made if needed (its missing parents get
    /// `meta` too); an existing one takes `meta`.
    pub fn dir(&mut self, path: &[u8], meta: Meta) -> Result<usize, Errno> {
        let (parent, name) = self.parent(path, meta)?;
        let at = match name {
            Some(name) => self.subdir(parent, name, meta)?,
            None => 0,
        };
        if let Proto::Dir { meta: m, .. } = &mut self.nodes[at] {
            *m = meta;
        }
        Ok(at)
    }

    /// Adds `leaf` at `path` (missing directories are made with
    /// `dir_meta`), replacing a leaf already there, as a later archive
    /// entry replaces an earlier one.
    pub fn leaf(
        &mut self,
        path: &[u8],
        meta: Meta,
        leaf: Leaf,
        dir_meta: Meta,
    ) -> Result<usize, Errno> {
        let (parent, name) = self.parent(path, dir_meta)?;
        let name = name.ok_or(Errno::EEXIST)?;
        let next = self.nodes.len();
        let Proto::Dir { children, .. } = &self.nodes[parent] else {
            return Err(Errno::ENOTDIR);
        };
        if let Some(&c) = children.get(name)
            && matches!(self.nodes[c], Proto::Dir { .. })
        {
            return Err(Errno::EISDIR);
        }
        if let Proto::Dir { children, .. } = &mut self.nodes[parent] {
            children.insert(name.into(), next);
        }
        self.nodes.push(Proto::Leaf {
            meta,
            leaf,
            links: 1,
        });
        Ok(next)
    }

    /// Gives leaf `target` another name, `path`.
    pub fn link(&mut self, path: &[u8], target: usize, dir_meta: Meta) -> Result<(), Errno> {
        if !matches!(self.nodes.get(target), Some(Proto::Leaf { .. })) {
            return Err(Errno::EPERM);
        }
        let (parent, name) = self.parent(path, dir_meta)?;
        let name = name.ok_or(Errno::EEXIST)?;
        let Proto::Dir { children, .. } = &mut self.nodes[parent] else {
            return Err(Errno::ENOTDIR);
        };
        if children.insert(name.into(), target) == Some(target) {
            return Ok(());
        }
        if let Proto::Leaf { links, .. } = &mut self.nodes[target] {
            *links += 1;
        }
        Ok(())
    }

    /// The finished file system.
    #[must_use]
    pub fn finish(self) -> TreeFs {
        let mut parent = alloc::vec![0usize; self.nodes.len()];
        for (i, n) in self.nodes.iter().enumerate() {
            if let Proto::Dir { children, .. } = n {
                for &c in children.values() {
                    if matches!(self.nodes[c], Proto::Dir { .. }) {
                        parent[c] = i;
                    }
                }
            }
        }
        let nodes = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| match n {
                Proto::Dir { meta, children } => {
                    let entries: Vec<(Box<[u8]>, u64, Kind)> = children
                        .iter()
                        .map(|(name, &c)| (name.clone(), ino(c), kind_of(&self.nodes[c])))
                        .collect();
                    let subdirs = entries.iter().filter(|e| e.2 == Kind::Directory).count();
                    Fixed {
                        attr: attr(meta, 2 + subdirs as u64, 512, 0),
                        data: FixedData::Dir {
                            parent: ino(parent[i]),
                            entries,
                        },
                    }
                }
                Proto::Leaf { meta, leaf, links } => {
                    let (size, rdev) = match leaf {
                        Leaf::Regular(d) => (d.len() as u64, 0),
                        Leaf::Symlink(t) => (t.len() as u64, 0),
                        Leaf::Device(d) => (0, d.rdev),
                    };
                    Fixed {
                        attr: attr(meta, *links, size, rdev),
                        data: FixedData::Leaf(leaf.clone()),
                    }
                }
            })
            .collect();
        TreeFs {
            nodes: Arc::new(nodes),
        }
    }
}

fn ino(index: usize) -> u64 {
    ROOT_INO + index as u64
}

fn kind_of(p: &Proto) -> Kind {
    match p {
        Proto::Dir { .. } => Kind::Directory,
        Proto::Leaf { leaf, .. } => match leaf {
            Leaf::Regular(_) => Kind::Regular,
            Leaf::Symlink(_) => Kind::Symlink,
            Leaf::Device(_) => Kind::CharDevice,
        },
    }
}

fn attr(m: &Meta, nlink: u64, size: u64, rdev: u64) -> Attr {
    Attr {
        mode: m.perm & 0o7777,
        uid: m.uid,
        gid: m.gid,
        nlink,
        size,
        bytes: size.next_multiple_of(512),
        atime: m.mtime,
        mtime: m.mtime,
        ctime: m.mtime,
        birthtime: m.mtime,
        rdev,
        ..Attr::default()
    }
}

struct Fixed {
    attr: Attr,
    data: FixedData,
}

enum FixedData {
    Dir {
        parent: u64,
        /// By name.
        entries: Vec<(Box<[u8]>, u64, Kind)>,
    },
    Leaf(Leaf),
}

/// A read-only file system built by a [`Builder`].
pub struct TreeFs {
    nodes: Arc<Vec<Fixed>>,
}

impl core::fmt::Debug for TreeFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "TreeFs({} nodes)", self.nodes.len())
    }
}

/// One node of a [`TreeFs`], as its vnode holds it.
struct View {
    nodes: Arc<Vec<Fixed>>,
    index: usize,
}

impl View {
    fn fixed(&self) -> &Fixed {
        &self.nodes[self.index]
    }
}

impl Node for View {
    fn attr(&self) -> Attr {
        self.fixed().attr
    }

    fn setattr(&mut self, _s: &Sleepable, _a: &SetAttr) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn reclaim(self: Box<Self>, _s: &Sleepable) {}

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl RegularNode for View {
    fn read(&self, _s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let Some(data) = self.resident() else {
            return Err(Errno::EINVAL);
        };
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let Some(rest) = data.get(start..) else {
            return Ok(0);
        };
        uio.copy_out(&rest[..rest.len().min(uio.resid())])
    }

    fn write(&mut self, _s: &Sleepable, _offset: u64, _uio: &mut dyn Uio) -> Result<usize, Errno> {
        Err(Errno::EROFS)
    }

    fn resident(&self) -> Option<&'static [u8]> {
        match &self.fixed().data {
            FixedData::Leaf(Leaf::Regular(d)) => Some(d),
            _ => None,
        }
    }
}

impl SymlinkNode for View {
    fn target(&self, _s: &Sleepable) -> Result<Vec<u8>, Errno> {
        match &self.fixed().data {
            FixedData::Leaf(Leaf::Symlink(t)) => Ok(t.to_vec()),
            _ => Err(Errno::EINVAL),
        }
    }
}

impl DeviceNode for View {
    fn device(&self) -> Device {
        match &self.fixed().data {
            FixedData::Leaf(Leaf::Device(d)) => d.clone(),
            _ => unreachable!("only device views are device nodes"),
        }
    }
}

impl DirectoryNode for View {
    fn lookup(&self, _s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        let FixedData::Dir { entries, .. } = &self.fixed().data else {
            return Err(Errno::ENOTDIR);
        };
        entries
            .binary_search_by(|(n, _, _)| (**n).cmp(name))
            .map(|i| entries[i].1)
            .map_err(|_| Errno::ENOENT)
    }

    fn parent(&self) -> u64 {
        match &self.fixed().data {
            FixedData::Dir { parent, .. } => *parent,
            FixedData::Leaf(_) => ino(self.index),
        }
    }

    fn create(
        &mut self,
        _s: &Sleepable,
        _name: &[u8],
        _node: &NewNode,
        _c: &Create,
    ) -> Result<Made, Errno> {
        Err(Errno::EROFS)
    }

    fn link(
        &mut self,
        _s: &Sleepable,
        _name: &[u8],
        _ino: u64,
        _target: &mut dyn Node,
    ) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn remove(&mut self, _s: &Sleepable, _name: &[u8], _child: &mut dyn Node) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn rmdir(
        &mut self,
        _s: &Sleepable,
        _name: &[u8],
        _child: &mut dyn DirectoryNode,
    ) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn readdir(
        &self,
        _s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        let FixedData::Dir { parent, entries } = &self.fixed().data else {
            return Err(Errno::ENOTDIR);
        };
        let me = ino(self.index);
        if from == 0
            && !f(DirEntry {
                name: b".",
                ino: me,
                kind: Kind::Directory,
                next: 1,
            })
        {
            return Ok(());
        }
        if from <= 1
            && !f(DirEntry {
                name: b"..",
                ino: *parent,
                kind: Kind::Directory,
                next: 2,
            })
        {
            return Ok(());
        }
        let skip = usize::try_from(from.max(2) - 2).unwrap_or(usize::MAX);
        for (i, (name, ino, kind)) in entries.iter().enumerate().skip(skip) {
            if !f(DirEntry {
                name,
                ino: *ino,
                kind: *kind,
                next: i as u64 + 3,
            }) {
                break;
            }
        }
        Ok(())
    }
}

impl FileSystem for TreeFs {
    fn root(&self) -> u64 {
        ROOT_INO
    }

    fn load(&self, _s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        let index = ino
            .checked_sub(ROOT_INO)
            .and_then(|i| usize::try_from(i).ok())
            .filter(|&i| i < self.nodes.len())
            .ok_or(Errno::ENOENT)?;
        let view = Box::new(View {
            nodes: Arc::clone(&self.nodes),
            index,
        });
        Ok(match &self.nodes[index].data {
            FixedData::Dir { .. } => NodeOps::Directory(view),
            FixedData::Leaf(Leaf::Regular(_)) => NodeOps::Regular(view),
            FixedData::Leaf(Leaf::Symlink(_)) => NodeOps::Symlink(view),
            FixedData::Leaf(Leaf::Device(_)) => NodeOps::Device(view),
        })
    }

    fn pathconf(&self, kind: Kind, name: u32) -> Result<i64, Errno> {
        use abi::fcntl::*;
        match name {
            // `LINK_MAX`, as FreeBSD's devfs and tarfs report.
            _PC_LINK_MAX => Ok(32767),
            _PC_SYMLINK_MAX => Ok(PATH_MAX as i64),
            _PC_NAME_MAX => Ok(NAME_MAX as i64),
            _PC_PIPE_BUF if matches!(kind, Kind::Directory | Kind::Fifo) => Ok(512),
            _PC_CHOWN_RESTRICTED | _PC_NO_TRUNC => Ok(1),
            _ => crate::mount::std_pathconf(name),
        }
    }

    fn statfs(&self, _s: &Sleepable) -> Result<FsStats, Errno> {
        let bytes: u64 = self.nodes.iter().map(|n| n.attr.bytes).sum();
        Ok(FsStats {
            bsize: 512,
            iosize: 4096,
            blocks: bytes / 512,
            bfree: 0,
            bavail: 0,
            files: self.nodes.len() as u64,
            ffree: 0,
        })
    }

    fn rename(&self, _s: &Sleepable, _r: Rename<'_>) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
