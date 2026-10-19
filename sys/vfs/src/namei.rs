// SPDX-License-Identifier: BSD-2-Clause
//! Path lookup (FreeBSD's `namei`) over a [`Namespace`]: file systems
//! mounted on directories of others.
//!
//! A path is looked up a component at a time from the root (absolute) or
//! the starting directory (relative). Empty components and `.` stay put;
//! `..` goes up, but not above the process's root, and from a file
//! system's root to the directory it is mounted on. A directory something
//! is mounted on stands for that file system's root. Symbolic links are
//! followed, except as the last component when the caller says not to,
//! up to `MAXSYMLINKS` per lookup. A trailing slash requires a directory.
//!
//! [`Namespace::path_of`] goes the other way, for `__getcwd`: from a
//! directory up to the root, finding each one's name in its parent.
//!
//! Behavior reference: sys/kern/vfs_lookup.c, sys/kern/vfs_cache.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::{MAXSYMLINKS, NAME_MAX, PATH_MAX};

use crate::{Vnode, VnodeKind};

/// A mounted file system.
#[derive(Debug)]
pub struct Mount {
    /// The directory it is mounted on (`None` for the root file system).
    pub covered: Option<Arc<Vnode>>,
    pub root: Arc<Vnode>,
    /// Where (`f_mntonname`).
    pub on: Vec<u8>,
}

/// The mounted file systems.
#[derive(Debug)]
pub struct Namespace {
    mounts: Vec<Mount>,
}

/// How to look a path up.
#[derive(Debug)]
pub struct Lookup<'a> {
    /// Where a relative path starts (the working directory, or the
    /// directory an `*at` call names).
    pub start: &'a Arc<Vnode>,
    /// The process's root: absolute paths start here, and `..` stops.
    pub root: &'a Arc<Vnode>,
    /// Whether a symbolic link as the last component is followed.
    pub follow: bool,
    /// Whether the lookup must stay beneath `start` (`O_RESOLVE_BENEATH`):
    /// absolute paths, and `..` above `start`, fail with `ENOTCAPABLE`.
    pub beneath: bool,
}

/// What a lookup found.
#[derive(Debug)]
pub enum Found {
    Vnode(Arc<Vnode>),
    /// Everything up to the last component, which does not exist: where a
    /// file of that name would be created.
    Missing {
        dir: Arc<Vnode>,
        name: Vec<u8>,
    },
}

impl Namespace {
    /// A namespace whose root is file system root `root`.
    #[must_use]
    pub fn new(root: Arc<Vnode>) -> Self {
        Self {
            mounts: alloc::vec![Mount {
                covered: None,
                root,
                on: b"/".to_vec(),
            }],
        }
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &Arc<Vnode> {
        &self.mounts[0].root
    }

    /// Mounts the file system whose root is `root` on directory
    /// `covered`, which is at `on`.
    pub fn mount(&mut self, covered: Arc<Vnode>, root: Arc<Vnode>, on: &[u8]) -> Result<(), Errno> {
        if !covered.is_dir() || !root.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        if self
            .mounts
            .iter()
            .any(|m| m.covered.as_ref().is_some_and(|c| c.id() == covered.id()))
        {
            return Err(Errno::EBUSY);
        }
        self.mounts.push(Mount {
            covered: Some(covered),
            root,
            on: on.to_vec(),
        });
        Ok(())
    }

    /// The mount of `v`'s file system.
    #[must_use]
    pub fn mount_of(&self, v: &Vnode) -> Option<&Mount> {
        self.mounts.iter().find(|m| m.root.fs().id == v.fs().id)
    }

    /// The mounts, the root file system's first.
    #[must_use]
    pub fn mounts(&self) -> &[Mount] {
        &self.mounts
    }

    /// `v`, or the root of what is mounted on it.
    fn cover(&self, mut v: Arc<Vnode>) -> Arc<Vnode> {
        // Mounts stack at most once per mount.
        for _ in 0..self.mounts.len() {
            match self
                .mounts
                .iter()
                .find(|m| m.covered.as_ref().is_some_and(|c| c.id() == v.id()))
            {
                Some(m) => v = Arc::clone(&m.root),
                None => break,
            }
        }
        v
    }

    /// The directory a file system root `v` is mounted on, if it is one.
    fn covered(&self, v: &Vnode) -> Option<&Arc<Vnode>> {
        self.mounts
            .iter()
            .find(|m| m.root.id() == v.id())
            .and_then(|m| m.covered.as_ref())
    }

    /// `v/..`: `v` itself at `root` or the namespace's root.
    fn parent(&self, v: &Arc<Vnode>, root: &Arc<Vnode>) -> Arc<Vnode> {
        let mut v = Arc::clone(v);
        for _ in 0..=self.mounts.len() {
            if v.id() == root.id() {
                return v;
            }
            if let Some(p) = v.as_dir().and_then(|d| d.parent()) {
                return p;
            }
            match self.covered(&v) {
                Some(c) => v = Arc::clone(c),
                None => return v,
            }
        }
        v
    }

    /// The vnode at `path`.
    pub fn lookup(&self, l: &Lookup<'_>, path: &[u8]) -> Result<Arc<Vnode>, Errno> {
        match self.resolve(l, path)? {
            Found::Vnode(v) => Ok(v),
            Found::Missing { .. } => Err(Errno::ENOENT),
        }
    }

    /// Looks `path` up; a missing last component is not an error.
    pub fn resolve(&self, l: &Lookup<'_>, path: &[u8]) -> Result<Found, Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let mut rest = path.to_vec();
        let mut at = 0;
        let mut links = 0;
        // How many levels below `start` the lookup is, for `beneath`.
        let mut depth = 0usize;
        let mut cur = if rest[0] == b'/' {
            if l.beneath {
                return Err(Errno::ENOTCAPABLE);
            }
            Arc::clone(l.root)
        } else {
            Arc::clone(l.start)
        };
        loop {
            while rest.get(at) == Some(&b'/') {
                at += 1;
            }
            if at == rest.len() {
                return Ok(Found::Vnode(cur));
            }
            let end = rest[at..]
                .iter()
                .position(|&b| b == b'/')
                .map_or(rest.len(), |p| at + p);
            // Something follows: more components, or a trailing slash.
            let more = end < rest.len();
            let last = rest[end..].iter().all(|&b| b == b'/');
            let name = &rest[at..end];
            if name.len() > NAME_MAX {
                return Err(Errno::ENAMETOOLONG);
            }
            let dir = cur.as_dir().ok_or(Errno::ENOTDIR)?;
            let next = match name {
                b"." => Arc::clone(&cur),
                b".." => {
                    if l.beneath {
                        depth = depth.checked_sub(1).ok_or(Errno::ENOTCAPABLE)?;
                    }
                    self.parent(&cur, l.root)
                }
                _ => match dir.lookup(name) {
                    Ok(v) => {
                        depth += 1;
                        self.cover(v)
                    }
                    Err(Errno::ENOENT) if last => {
                        return Ok(Found::Missing {
                            dir: cur,
                            name: name.to_vec(),
                        });
                    }
                    Err(e) => return Err(e),
                },
            };
            if let VnodeKind::Symlink(target) = next.kind()
                && (more || l.follow)
            {
                links += 1;
                if links > MAXSYMLINKS {
                    return Err(Errno::ELOOP);
                }
                if target.is_empty() {
                    return Err(Errno::ENOENT);
                }
                // The link resolves from the directory holding it.
                depth = depth.saturating_sub(1);
                if target[0] == b'/' {
                    if l.beneath {
                        return Err(Errno::ENOTCAPABLE);
                    }
                    cur = Arc::clone(l.root);
                }
                let mut joined = Vec::new();
                joined
                    .try_reserve(target.len() + rest.len() - end)
                    .map_err(|_| Errno::ENOMEM)?;
                joined.extend_from_slice(target);
                joined.extend_from_slice(&rest[end..]);
                if joined.len() >= PATH_MAX {
                    return Err(Errno::ENAMETOOLONG);
                }
                rest = joined;
                at = 0;
                continue;
            }
            if more && !next.is_dir() {
                return Err(Errno::ENOTDIR);
            }
            cur = next;
            at = end;
        }
    }

    /// `v`'s path from `root` (`/` for `root` itself), as `__getcwd`
    /// reports it.
    pub fn path_of(&self, v: &Arc<Vnode>, root: &Arc<Vnode>) -> Result<Vec<u8>, Errno> {
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut total = 0;
        let mut v = Arc::clone(v);
        while v.id() != root.id() {
            let p = self.parent(&v, root);
            if p.id() == v.id() {
                break;
            }
            // A file system's root goes by the name of what it covers.
            let want = self.covered(&v).map_or(v.id(), |c| c.id());
            let mut name = None;
            p.as_dir().ok_or(Errno::ENOTDIR)?.entries(0, &mut |e| {
                if e.vnode.id() == want {
                    name = Some(e.name.to_vec());
                    false
                } else {
                    true
                }
            });
            let name = name.ok_or(Errno::ENOENT)?;
            total += name.len() + 1;
            if total >= PATH_MAX {
                return Err(Errno::ENAMETOOLONG);
            }
            names.push(name);
            v = p;
        }
        let mut path = Vec::with_capacity(total.max(1));
        for n in names.iter().rev() {
            path.push(b'/');
            path.extend_from_slice(n);
        }
        if path.is_empty() {
            path.push(b'/');
        }
        Ok(path)
    }
}
