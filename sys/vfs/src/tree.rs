// SPDX-License-Identifier: BSD-2-Clause
//! File systems built once and then fixed: a [`Builder`] collects
//! directories, files, links, and devices by path, and [`Builder::finish`]
//! turns them into vnodes. tarfs and devfs are both built this way.
//!
//! Inode numbers follow insertion order from 2 (the root), as UFS numbers
//! its root. A directory's link count is 2 plus its subdirectories; a
//! file's is the number of names it has.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::errno::Errno;
use sync::Once;

use crate::{DirEntry, Directory, FileSystem, Meta, Vnode, VnodeKind};

/// The root's inode number.
pub const ROOT_INO: u64 = 2;

/// What a leaf of the tree holds.
#[derive(Clone)]
pub enum Leaf {
    Regular(Arc<dyn crate::RegularFile>),
    Symlink(Arc<[u8]>),
    Device(crate::Device),
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
    fs: Arc<FileSystem>,
    nodes: Vec<Proto>,
}

impl core::fmt::Debug for Builder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Builder({} nodes)", self.nodes.len())
    }
}

impl core::fmt::Debug for Leaf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Regular(r) => write!(f, "Regular({} bytes)", r.size()),
            Self::Symlink(t) => write!(f, "Symlink({t:?})"),
            Self::Device(d) => write!(f, "Device({:#x})", d.rdev),
        }
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
    pub fn new(fs: Arc<FileSystem>, root: Meta) -> Self {
        Self {
            fs,
            nodes: alloc::vec![Proto::Dir {
                meta: root,
                children: BTreeMap::new(),
            }],
        }
    }

    /// The inode number the next node added will have.
    #[must_use]
    pub fn next_ino(&self) -> u64 {
        Self::ino(self.nodes.len())
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

    fn ino(index: usize) -> u64 {
        ROOT_INO + index as u64
    }

    /// The tree's root vnode.
    #[must_use]
    pub fn finish(self) -> Arc<Vnode> {
        // A child is always added after its directory, so building from
        // the last node back makes every child before its parent.
        let mut done: Vec<Option<Arc<Vnode>>> = alloc::vec![None; self.nodes.len()];
        let mut dirs: Vec<Option<Arc<StaticDir>>> = alloc::vec![None; self.nodes.len()];
        for (i, node) in self.nodes.iter().enumerate().rev() {
            let v = match node {
                Proto::Leaf { meta, leaf, links } => {
                    let kind = match leaf {
                        Leaf::Regular(f) => VnodeKind::Regular(Arc::clone(f)),
                        Leaf::Symlink(t) => VnodeKind::Symlink(Arc::clone(t)),
                        Leaf::Device(d) => VnodeKind::CharDevice(d.clone()),
                    };
                    Arc::new(Vnode::new(
                        Arc::clone(&self.fs),
                        Self::ino(i),
                        *meta,
                        *links,
                        kind,
                    ))
                }
                Proto::Dir { meta, children } => {
                    let entries: Vec<(Box<[u8]>, Arc<Vnode>)> = children
                        .iter()
                        .filter_map(|(n, &c)| Some((n.clone(), Arc::clone(done[c].as_ref()?))))
                        .collect();
                    let subdirs = entries.iter().filter(|(_, v)| v.is_dir()).count() as u64;
                    let dir = Arc::new(StaticDir {
                        parent: Once::new(),
                        entries,
                    });
                    dirs[i] = Some(Arc::clone(&dir));
                    let v = Arc::new(Vnode::new(
                        Arc::clone(&self.fs),
                        Self::ino(i),
                        *meta,
                        2 + subdirs,
                        VnodeKind::Directory(dir),
                    ));
                    for &c in children.values() {
                        if let Some(d) = &dirs[c] {
                            let _ = d.parent.set(Arc::downgrade(&v));
                        }
                    }
                    v
                }
            };
            done[i] = Some(v);
        }
        // Node 0 is the root, and was built last.
        done.swap_remove(0)
            .unwrap_or_else(|| unreachable!("the root is built"))
    }
}

/// A fixed directory: its entries sorted by name.
struct StaticDir {
    parent: Once<Weak<Vnode>>,
    entries: Vec<(Box<[u8]>, Arc<Vnode>)>,
}

impl Directory for StaticDir {
    fn lookup(&self, name: &[u8]) -> Result<Arc<Vnode>, Errno> {
        self.entries
            .binary_search_by(|(n, _)| (**n).cmp(name))
            .map(|i| Arc::clone(&self.entries[i].1))
            .map_err(|_| Errno::ENOENT)
    }

    fn parent(&self) -> Option<Arc<Vnode>> {
        self.parent.get()?.upgrade()
    }

    fn entries(&self, from: u64, f: &mut dyn FnMut(DirEntry<'_>) -> bool) {
        let from = usize::try_from(from).unwrap_or(usize::MAX);
        for (name, vnode) in self.entries.iter().skip(from) {
            if !f(DirEntry { name, vnode }) {
                break;
            }
        }
    }
}
