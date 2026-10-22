// SPDX-License-Identifier: BSD-2-Clause
//! Path lookup (FreeBSD's `namei`) over a [`Namespace`]: file systems
//! mounted on directories of others (docs/VFS.md §6.3).
//!
//! A path is looked up a component at a time from the root (absolute) or
//! the starting directory (relative). Empty components and `.` stay put;
//! `..` goes up, but not above the process's root, and from a file
//! system's root to the directory it is mounted on. A directory something
//! is mounted on stands for that file system's root. Symbolic links are
//! followed, except as the last component when the caller says not to,
//! up to `MAXSYMLINKS` per lookup. A trailing slash requires a directory.
//! Searching a directory needs execute permission on it.
//!
//! Each component goes through the name cache first; a miss asks the file
//! system with the directory locked shared, and caches the answer (also a
//! negative one) before the lock goes, so no change can slip between.
//!
//! Operations that change a directory use [`Namespace::parent_of`]: the
//! directory and the last name, which they look up again under the
//! directory's exclusive lock.
//!
//! [`Namespace::path_of`] goes the other way, for `__getcwd`.
//!
//! Behavior reference: sys/kern/vfs_lookup.c, sys/kern/vfs_cache.c.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::{MAXSYMLINKS, NAME_MAX, PATH_MAX};
use sched::Sleepable;
use sync::SpinLock;

use crate::cache::{self, Hit};
use crate::mount::Mount;
use crate::node::{Kind, NodeOps};
use crate::ops::{self, Cred, VEXEC};
use crate::vnode::{Peek, Vnode};

/// The mounted file systems.
#[derive(Debug)]
pub struct Namespace {
    root: Arc<Vnode>,
    /// Every mount, the root file system's first.
    mounts: SpinLock<Vec<Arc<Mount>>>,
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
    /// Whether a missing last component is about to be created: that miss
    /// is not cached, since the entry made replaces it at once (FreeBSD
    /// caches no negative entry for `CREATE`).
    pub create: bool,
    pub cred: &'a Cred,
}

/// What a lookup found.
#[derive(Debug)]
pub enum Found {
    Vnode(Arc<Vnode>),
    /// Everything up to the last component, which does not exist: where a
    /// file of that name would be created.
    Missing {
        dir: Arc<Vnode>,
        name: LastName,
    },
}

/// Last components this long or shorter are held inline.
const INLINE_LAST: usize = 64;

/// A path's last component: a short one inline, so most lookups allocate
/// nothing for it.
#[derive(Clone)]
pub enum LastName {
    Inline { len: u8, bytes: [u8; INLINE_LAST] },
    Heap(Box<[u8]>),
}

impl LastName {
    #[must_use]
    pub fn new(name: &[u8]) -> Self {
        match u8::try_from(name.len()) {
            Ok(len) if name.len() <= INLINE_LAST => {
                let mut bytes = [0; INLINE_LAST];
                bytes[..name.len()].copy_from_slice(name);
                Self::Inline { len, bytes }
            }
            _ => Self::Heap(name.into()),
        }
    }
}

impl core::ops::Deref for LastName {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Inline { len, bytes } => &bytes[..usize::from(*len)],
            Self::Heap(b) => b,
        }
    }
}

impl core::fmt::Debug for LastName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&**self, f)
    }
}

/// The directory a path's last component is in, and that component.
#[derive(Debug)]
pub struct Parent {
    pub dir: Arc<Vnode>,
    /// The last component: a name, `.`, or `..`; empty for a path of
    /// slashes only (which operations that change a directory refuse with
    /// `EISDIR`, as FreeBSD's `namei` does).
    pub name: LastName,
    /// The path ended in a slash: the last component must be a directory.
    pub trailing_slash: bool,
}

/// One step of a walk: the next component and what follows it.
struct Component {
    start: usize,
    end: usize,
    /// More components follow (not just slashes).
    more: bool,
}

fn next_component(path: &[u8], mut at: usize) -> Option<Component> {
    while path.get(at) == Some(&b'/') {
        at += 1;
    }
    if at == path.len() {
        return None;
    }
    let end = path[at..]
        .iter()
        .position(|&b| b == b'/')
        .map_or(path.len(), |p| at + p);
    let more = path[end..].iter().any(|&b| b != b'/');
    Some(Component {
        start: at,
        end,
        more,
    })
}

impl Namespace {
    /// A namespace whose root is file system `root`'s.
    pub fn new(s: &Sleepable, root: Arc<Mount>) -> Result<Self, Errno> {
        root.set_on(b"/");
        Ok(Self {
            root: root.root(s)?,
            mounts: SpinLock::new(alloc::vec![root]),
        })
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &Arc<Vnode> {
        &self.root
    }

    /// The mounts, the root file system's first.
    #[must_use]
    pub fn mounts(&self) -> Vec<Arc<Mount>> {
        self.mounts.lock().clone()
    }

    /// Mounts `m` on directory `covered`, which is at `on`.
    pub fn mount(
        &self,
        s: &Sleepable,
        covered: Arc<Vnode>,
        m: Arc<Mount>,
        on: &[u8],
    ) -> Result<(), Errno> {
        if !covered.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let root = m.root(s)?;
        if !root.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let mut mounts = self.mounts.lock();
        if covered.mounted().is_some() {
            return Err(Errno::EBUSY);
        }
        m.set_on(on);
        m.set_covered(Some(Arc::clone(&covered)));
        covered.set_mounted(Some(Arc::clone(&m)));
        covered.set_mounted_root(Some(&root));
        mounts.push(m);
        Ok(())
    }

    /// Unmounts `m` (FreeBSD's `dounmount`): `EBUSY` while its files are in
    /// use or something is mounted on it, unless `force`, which dooms them.
    pub fn unmount(&self, s: &Sleepable, m: &Arc<Mount>, force: bool) -> Result<(), Errno> {
        let Some(covered) = m.covered() else {
            // The root file system stays.
            return Err(Errno::EINVAL);
        };
        let busy = self
            .mounts
            .lock()
            .iter()
            .any(|x| x.covered().is_some_and(|c| c.mount().id == m.id));
        if busy {
            return Err(Errno::EBUSY);
        }
        let vnodes = m.all_vnodes();
        if !force && vnodes.iter().any(|v| v.uses() > 0 && !v.is_mount_root()) {
            return Err(Errno::EBUSY);
        }
        m.fs().sync(s, true)?;
        // Lockless lookups no longer cross into it; those that may have
        // are done before its vnodes go.
        covered.set_mounted_root(None);
        cache::synchronize();
        cache::purge_fs(m.id);
        for v in &vnodes {
            v.reclaim(s);
        }
        m.fs().unmount(s)?;
        covered.set_mounted(None);
        m.set_covered(None);
        self.mounts.lock().retain(|x| x.id != m.id);
        Ok(())
    }

    /// Unmounts everything for shutdown (FreeBSD's `vfs_unmountall`):
    /// the mounts in reverse order, forcibly, then the root file system,
    /// written out and its vnodes reclaimed. Nothing may use the
    /// namespace after. Returns the mounts that failed, by where they
    /// were mounted.
    pub fn unmount_all(&self, s: &Sleepable) -> Vec<(Vec<u8>, Errno)> {
        let mut failed = Vec::new();
        let mounts = self.mounts();
        for m in mounts.iter().skip(1).rev() {
            if let Err(e) = self.unmount(s, m, true) {
                failed.push((m.on(), e));
            }
        }
        let root = &mounts[0];
        let done = root.fs().sync(s, true).and_then(|()| {
            cache::purge_fs(root.id);
            for v in root.all_vnodes() {
                v.reclaim(s);
            }
            root.fs().unmount(s)
        });
        if let Err(e) = done {
            failed.push((root.on(), e));
        }
        failed
    }

    /// `v`, or the root of what is mounted on it.
    fn cover(s: &Sleepable, mut v: Arc<Vnode>) -> Result<Arc<Vnode>, Errno> {
        // Mounts stack, finitely.
        for _ in 0..64 {
            match v.mounted() {
                Some(m) => v = m.root(s)?,
                None => return Ok(v),
            }
        }
        Err(Errno::ELOOP)
    }

    /// `v/..`: `v` itself at `root` or the namespace's root.
    fn parent(
        &self,
        s: &Sleepable,
        v: &Arc<Vnode>,
        root: &Arc<Vnode>,
    ) -> Result<Arc<Vnode>, Errno> {
        let mut v = Arc::clone(v);
        loop {
            if Arc::ptr_eq(&v, root) || Arc::ptr_eq(&v, &self.root) {
                return Ok(v);
            }
            if !v.is_mount_root() {
                break;
            }
            // From a file system's root, `..` is the covered directory's.
            match v.mount().covered() {
                Some(c) => v = c,
                None => return Ok(v),
            }
        }
        if let Some((p, _)) = v.named()
            && p.mount().id == v.mount().id
        {
            return Ok(p);
        }
        let ino = match v.read(s).live()? {
            NodeOps::Directory(d) => d.parent(),
            _ => return Err(Errno::ENOTDIR),
        };
        v.mount().vget(s, ino)
    }

    /// The vnode `dir` holds as `name` (not `.` or `..`), through the
    /// name cache.
    pub fn child(s: &Sleepable, dir: &Arc<Vnode>, name: &[u8]) -> Result<Arc<Vnode>, Errno> {
        Self::child_in(s, dir, name, true)
    }

    /// [`Self::child`], caching a miss only if `negative`.
    fn child_in(
        s: &Sleepable,
        dir: &Arc<Vnode>,
        name: &[u8],
        negative: bool,
    ) -> Result<Arc<Vnode>, Errno> {
        match cache::lookup(dir, name) {
            Some(Hit::Positive(v)) => {
                v.touch();
                return Ok(v);
            }
            Some(Hit::Negative) => return Err(Errno::ENOENT),
            None => {}
        }
        let st = dir.read(s);
        let NodeOps::Directory(d) = st.live()? else {
            return Err(Errno::ENOTDIR);
        };
        match d.lookup(s, name) {
            Ok(ino) => {
                let v = dir.mount().vget(s, ino)?;
                cache::enter(dir, name, Some(&v));
                Ok(v)
            }
            Err(Errno::ENOENT) => {
                if negative {
                    cache::enter(dir, name, None);
                }
                Err(Errno::ENOENT)
            }
            Err(e) => Err(e),
        }
    }

    /// The vnode at `path`.
    pub fn lookup(&self, s: &Sleepable, l: &Lookup<'_>, path: &[u8]) -> Result<Arc<Vnode>, Errno> {
        match self.walk(s, l, path, Want::Vnode)? {
            Walked::Found(Found::Vnode(v)) => Ok(v),
            _ => unreachable!("a missing vnode is ENOENT"),
        }
    }

    /// Looks `path` up; a missing last component is not an error.
    pub fn resolve(&self, s: &Sleepable, l: &Lookup<'_>, path: &[u8]) -> Result<Found, Errno> {
        self.walk(s, l, path, Want::Found).map(|w| match w {
            Walked::Found(f) => f,
            Walked::Parent(_) => unreachable!("walk stops at the parent only when asked"),
        })
    }

    /// The directory `path`'s last component is in, and that component
    /// (not followed if it is a symbolic link), for operations that create
    /// or remove it.
    pub fn parent_of(&self, s: &Sleepable, l: &Lookup<'_>, path: &[u8]) -> Result<Parent, Errno> {
        match self.walk(s, l, path, Want::Parent)? {
            Walked::Parent(p) => Ok(p),
            Walked::Found(_) => unreachable!("asked for the parent"),
        }
    }

    fn walk(
        &self,
        s: &Sleepable,
        l: &Lookup<'_>,
        path: &[u8],
        want: Want,
    ) -> Result<Walked, Errno> {
        let parent = want == Want::Parent;
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        if path.len() >= PATH_MAX {
            return Err(Errno::ENAMETOOLONG);
        }
        let resume = match self.walk_fast(l, path, want) {
            Fast::Done(w) => return w,
            Fast::Resume(dir, at) => Some((dir, at)),
            Fast::No => None,
        };
        // The path as given until a symbolic link rewrites it.
        let mut rest: Cow<'_, [u8]> = Cow::Borrowed(path);
        let mut links = 0;
        // How many levels below `start` the lookup is, for `beneath` (which
        // the lockless walk never serves, so it starts at 0 either way).
        let mut depth = 0usize;
        let (mut cur, mut at) = match resume {
            Some(r) => r,
            None if rest[0] == b'/' => {
                if l.beneath {
                    return Err(Errno::ENOTCAPABLE);
                }
                (Arc::clone(l.root), 0)
            }
            None => (Arc::clone(l.start), 0),
        };
        loop {
            let Some(c) = next_component(&rest, at) else {
                // Slashes only (or the end after a followed link).
                return Ok(if parent {
                    Walked::Parent(Parent {
                        dir: cur,
                        name: LastName::new(b""),
                        trailing_slash: true,
                    })
                } else {
                    Walked::Found(Found::Vnode(cur))
                });
            };
            let name = &rest[c.start..c.end];
            if name.len() > NAME_MAX {
                return Err(Errno::ENAMETOOLONG);
            }
            if !cur.is_dir() {
                return Err(Errno::ENOTDIR);
            }
            if l.cred.uid != 0 {
                ops::access(&cur.attr(s)?, cur.kind(), l.cred, VEXEC)?;
            }
            let trailing_slash = c.end < rest.len();
            if parent && !c.more {
                return Ok(Walked::Parent(Parent {
                    dir: cur,
                    name: LastName::new(name),
                    trailing_slash,
                }));
            }
            let next = match name {
                b"." => Arc::clone(&cur),
                b".." => {
                    if l.beneath {
                        depth = depth.checked_sub(1).ok_or(Errno::ENOTCAPABLE)?;
                    }
                    self.parent(s, &cur, l.root)?
                }
                _ => match Self::child_in(s, &cur, name, c.more || !l.create) {
                    Ok(v) => {
                        depth += 1;
                        Self::cover(s, v)?
                    }
                    Err(Errno::ENOENT) if !c.more && want == Want::Found => {
                        return Ok(Walked::Found(Found::Missing {
                            dir: cur,
                            name: LastName::new(name),
                        }));
                    }
                    Err(e) => return Err(e),
                },
            };
            if next.kind() == Kind::Symlink && (c.more || l.follow || trailing_slash) {
                links += 1;
                if links > MAXSYMLINKS {
                    return Err(Errno::ELOOP);
                }
                let target = next.readlink(s)?;
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
                    .try_reserve(target.len() + rest.len() - c.end)
                    .map_err(|_| Errno::ENOMEM)?;
                joined.extend_from_slice(&target);
                joined.extend_from_slice(&rest[c.end..]);
                if joined.len() >= PATH_MAX {
                    return Err(Errno::ENAMETOOLONG);
                }
                rest = Cow::Owned(joined);
                at = 0;
                continue;
            }
            if (c.more || trailing_slash) && !next.is_dir() {
                return Err(Errno::ENOTDIR);
            }
            cur = next;
            at = c.end;
        }
    }

    /// `v`'s path from `root` (`/` for `root` itself), as `__getcwd`
    /// reports it.
    pub fn path_of(
        &self,
        s: &Sleepable,
        v: &Arc<Vnode>,
        root: &Arc<Vnode>,
    ) -> Result<Vec<u8>, Errno> {
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut total = 0;
        let mut v = Arc::clone(v);
        // Bounded: a cycle (impossible, but cheap to rule out) ends it.
        for _ in 0..PATH_MAX {
            if Arc::ptr_eq(&v, root) || Arc::ptr_eq(&v, &self.root) {
                break;
            }
            // A file system's root goes by the name of what it covers.
            while v.is_mount_root() {
                match v.mount().covered() {
                    Some(c) => v = c,
                    None => break,
                }
            }
            if Arc::ptr_eq(&v, root) || Arc::ptr_eq(&v, &self.root) {
                break;
            }
            let p = self.parent(s, &v, root)?;
            if Arc::ptr_eq(&p, &v) {
                break;
            }
            let name = match v.named() {
                Some((np, name)) if Arc::ptr_eq(&np, &p) => name,
                _ => name_in(s, &p, v.ino())?,
            };
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

enum Walked {
    Found(Found),
    Parent(Parent),
}

/// What a walk is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// The vnode: a missing last component is `ENOENT`, with nothing made
    /// for it.
    Vnode,
    /// The vnode, or where it would be ([`Found::Missing`]).
    Found,
    /// The directory the last component is in ([`Parent`]).
    Parent,
}

/// Where a lockless walk is: a vnode it holds no reference to, whose
/// allocation stays for the reader's section.
#[derive(Clone, Copy)]
enum Here<'a> {
    /// One the caller holds (where the walk starts).
    Held(&'a Arc<Vnode>),
    /// A name-cache entry's target, weakly.
    Entry(&'a Weak<Vnode>),
    /// A mounted file system's root, published: its mount's table holds it,
    /// and unmounting waits for the section to end before it goes.
    Root(*const Vnode),
}

#[allow(unsafe_code)]
impl Here<'_> {
    fn ptr(self) -> *const Vnode {
        match self {
            Self::Held(a) => Arc::as_ptr(a),
            Self::Entry(w) => w.as_ptr(),
            Self::Root(p) => p,
        }
    }

    fn peek(self) -> Peek {
        // SAFETY: the allocation stays for the section (see the variants).
        unsafe { Peek::of(self.ptr()) }
    }

    /// A reference to it, unless it is gone. Not to be dropped inside the
    /// section (a last reference's drop may free).
    fn take(self) -> Option<Arc<Vnode>> {
        match self {
            Self::Held(a) => Some(Arc::clone(a)),
            Self::Entry(w) => w.upgrade(),
            // SAFETY: published, so its mount's table holds a reference
            // until unmounting has waited for this section to end.
            Self::Root(p) => unsafe {
                Arc::increment_strong_count(p);
                Some(Arc::from_raw(p))
            },
        }
    }
}

/// What a lockless walk ended on; names as ranges of the path, copied
/// out once the section is over.
enum FastEnd {
    Vnode(Arc<Vnode>),
    Missing(Arc<Vnode>, core::ops::Range<usize>),
    /// The last component is known missing, and nothing more is wanted.
    Absent,
    Parent(Arc<Vnode>, core::ops::Range<usize>, bool),
    /// The cache does not know the name at this offset in this directory:
    /// the locked walk goes on from there.
    Unknown(Arc<Vnode>, usize),
}

/// How far the lockless walk got.
enum Fast {
    Done(Result<Walked, Errno>),
    /// The locked walk resumes in this directory, at this offset.
    Resume(Arc<Vnode>, usize),
    /// The locked walk starts over.
    No,
}

impl Namespace {
    /// The lockless walk (docs/VFS.md §6.2, FreeBSD's `cache_fplookup`):
    /// the whole path in one epoch section, with no lock and no reference
    /// taken but on the vnode found. A name the cache does not know hands
    /// the locked walk the directory it is in ([`Fast::Resume`]); the locked
    /// walk starts over ([`Fast::No`]) for `..`, a symbolic link to follow,
    /// a vnode going away, `O_RESOLVE_BENEATH`, or a caller not root (whose
    /// search permission on each directory needs its attributes; until
    /// Phase 6 every process is root).
    fn walk_fast(&self, l: &Lookup<'_>, path: &[u8], want: Want) -> Fast {
        if l.cred.uid != 0 || l.beneath {
            return Fast::No;
        }
        let end = {
            let reader = cache::Reader::new();
            Self::walk_in(&reader, l, path, want)
        };
        // Out of the section: what goes now may allocate or free.
        match end {
            None => Fast::No,
            Some(FastEnd::Unknown(dir, at)) if !dir.is_dying() => Fast::Resume(dir, at),
            Some(FastEnd::Unknown(..)) => Fast::No,
            Some(FastEnd::Absent) => Fast::Done(Err(Errno::ENOENT)),
            Some(end) => Self::fast_done(end, path).map_or(Fast::No, |w| Fast::Done(Ok(w))),
        }
    }

    fn fast_done(end: FastEnd, path: &[u8]) -> Option<Walked> {
        let alive = |v: Arc<Vnode>| (!v.is_dying()).then_some(v);
        Some(match end {
            FastEnd::Vnode(v) => {
                let v = alive(v)?;
                v.touch();
                Walked::Found(Found::Vnode(v))
            }
            FastEnd::Missing(dir, name) => Walked::Found(Found::Missing {
                dir: alive(dir)?,
                name: LastName::new(&path[name]),
            }),
            FastEnd::Parent(dir, name, trailing_slash) => Walked::Parent(Parent {
                dir: alive(dir)?,
                name: LastName::new(&path[name]),
                trailing_slash,
            }),
            FastEnd::Unknown(..) | FastEnd::Absent => return None,
        })
    }

    fn walk_in(reader: &cache::Reader, l: &Lookup<'_>, path: &[u8], want: Want) -> Option<FastEnd> {
        let parent = want == Want::Parent;
        let mut cur = Here::Held(if path.first() == Some(&b'/') {
            l.root
        } else {
            l.start
        });
        let mut at = 0;
        loop {
            let Some(c) = next_component(path, at) else {
                // Slashes only, or a trailing slash after a directory.
                return if parent {
                    None
                } else {
                    cur.take().map(FastEnd::Vnode)
                };
            };
            let name = &path[c.start..c.end];
            let here = cur.peek();
            if name.len() > NAME_MAX || here.kind != Kind::Directory {
                return None;
            }
            let trailing_slash = c.end < path.len();
            if parent && !c.more {
                return Some(FastEnd::Parent(cur.take()?, c.start..c.end, trailing_slash));
            }
            match name {
                b"." => {
                    at = c.end;
                    continue;
                }
                b".." => return None,
                _ => {}
            }
            let Some(found) = reader.find(cur.ptr(), here.id, name) else {
                return Some(FastEnd::Unknown(cur.take()?, c.start));
            };
            let mut next = match found {
                cache::Peek::Negative if !c.more && want == Want::Vnode => {
                    return Some(FastEnd::Absent);
                }
                cache::Peek::Negative if !c.more => {
                    return Some(FastEnd::Missing(cur.take()?, c.start..c.end));
                }
                cache::Peek::Negative => return None,
                cache::Peek::Positive(w) if w.strong_count() > 0 => Here::Entry(w),
                cache::Peek::Positive(_) => return None,
            };
            let mut info = next.peek();
            // Mounts stack, finitely.
            for _ in 0..64 {
                if info.mounted_root.is_null() {
                    break;
                }
                next = Here::Root(info.mounted_root);
                info = next.peek();
            }
            if !info.mounted_root.is_null() || info.dying {
                return None;
            }
            if info.kind == Kind::Symlink && (c.more || l.follow || trailing_slash) {
                return None;
            }
            if (c.more || trailing_slash) && info.kind != Kind::Directory {
                return None;
            }
            cur = next;
            at = c.end;
        }
    }
}

/// The name of inode `ino` in directory `dir`, by reading it (FreeBSD's
/// `vn_vptocnp` fallback when the cache does not know).
fn name_in(s: &Sleepable, dir: &Vnode, ino: u64) -> Result<Vec<u8>, Errno> {
    let mut found = None;
    dir.readdir(s, 0, &mut |e| {
        if e.ino == ino && e.name != b"." && e.name != b".." {
            found = Some(e.name.to_vec());
            false
        } else {
            true
        }
    })?;
    found.ok_or(Errno::ENOENT)
}
