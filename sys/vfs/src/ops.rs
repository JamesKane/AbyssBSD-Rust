// SPDX-License-Identifier: BSD-2-Clause
//! The generic layer's operations (docs/VFS.md §2, §7, §13): what every
//! file system's `create`, `remove`, `link`, `rename`, and `setattr` share.
//! Permission checks, name rules, lock order, and name-cache upkeep are done
//! here once; file systems only change their nodes.
//!
//! Locks: a directory before its entries; a cross-directory rename holds
//! its mount's rename lock, takes one vnode lock, and only tries the rest,
//! backing off and retrying when one is busy, since its order can run
//! against a concurrent lookup's or removal's (FreeBSD's `vfs_relookup`
//! dance, in its simplest form).
//!
//! Behavior reference: sys/kern/vfs_syscalls.c, sys/kern/vfs_subr.c
//! (`vaccess`), sys/ufs/ufs/ufs_vnops.c (rename rules).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::dirent::{self, NAME_OFFSET};
use abi::errno::Errno;
use abi::stat::{
    APPEND, IMMUTABLE, NOUNLINK, S_ISGID, S_ISUID, S_ISVTX, SF_APPEND, SF_IMMUTABLE, SF_NOUNLINK,
    SF_SETTABLE, SF_SNAPSHOT,
};
use sched::Sleepable;
use sched::lock::WriteGuard;

use crate::cache::{self, Hit};
use crate::mount::Rename;
use crate::node::{Attr, Create, Kind, NewNode, NodeOps, SetAttr};
use crate::vnode::{State, Vnode};

/// Who is asking (FreeBSD's `struct ucred`, the parts the VFS uses).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cred {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups.
    pub groups: Vec<u32>,
}

impl Cred {
    /// The superuser.
    #[must_use]
    pub const fn root() -> Self {
        Self {
            uid: 0,
            gid: 0,
            groups: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }

    #[must_use]
    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.groups.contains(&gid)
    }
}

/// Access modes (FreeBSD's `VEXEC`, `VWRITE`, `VREAD`).
pub const VEXEC: u32 = 0o100;
pub const VWRITE: u32 = 0o200;
pub const VREAD: u32 = 0o400;

/// Whether `cred` may access a node with attributes `a` as `want` says
/// (FreeBSD's `vaccess`): `EACCES` if not, `EPERM` to write an immutable
/// node. The superuser may do anything else but execute a file with no
/// execute bit at all.
pub fn access(a: &Attr, kind: Kind, cred: &Cred, want: u32) -> Result<(), Errno> {
    // Nobody writes an immutable node (FreeBSD's `ufs_access`).
    if want & VWRITE != 0 && a.flags & (IMMUTABLE | SF_SNAPSHOT) != 0 {
        return Err(Errno::EPERM);
    }
    if cred.is_root() {
        if want & VEXEC != 0 && kind != Kind::Directory && a.mode & 0o111 == 0 {
            return Err(Errno::EACCES);
        }
        return Ok(());
    }
    let shift = if cred.uid == a.uid {
        0
    } else if cred.in_group(a.gid) {
        3
    } else {
        6
    };
    let granted = u32::from(a.mode << shift) & 0o700;
    if want & !granted & 0o700 == 0 {
        Ok(())
    } else {
        Err(Errno::EACCES)
    }
}

/// Whether `cred` owns the node or is the superuser (changing its mode,
/// times, flags): `EPERM` if not.
fn owner(a: &Attr, cred: &Cred) -> Result<(), Errno> {
    if cred.is_root() || cred.uid == a.uid {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

/// The node's attributes, locked.
fn attr_of(st: &State) -> Result<Attr, Errno> {
    Ok(st.live()?.node().attr())
}

fn read_only(dir: &Vnode) -> Result<(), Errno> {
    if dir.mount().read_only() {
        Err(Errno::EROFS)
    } else {
        Ok(())
    }
}

/// What `name` is in the directory locked as `st`: from the cache, or by
/// asking the file system.
fn entry(
    s: &Sleepable,
    dir: &Arc<Vnode>,
    st: &State,
    name: &[u8],
) -> Result<Option<Arc<Vnode>>, Errno> {
    match cache::lookup(dir, name) {
        Some(Hit::Positive(v)) => return Ok(Some(v)),
        Some(Hit::Negative) => return Ok(None),
        None => {}
    }
    let NodeOps::Directory(d) = st.live()? else {
        return Err(Errno::ENOTDIR);
    };
    match d.lookup(s, name) {
        Ok(ino) => Ok(Some(dir.mount().vget(s, ino)?)),
        Err(Errno::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `cred` may remove or rename `child` out of directory `dir` (write and
/// search permission, the sticky bit, and the no-unlink flags).
fn may_unlink(dir: &Attr, child: &Attr, cred: &Cred) -> Result<(), Errno> {
    access(dir, Kind::Directory, cred, VWRITE | VEXEC)?;
    if dir.mode & S_ISVTX != 0 && !cred.is_root() && cred.uid != dir.uid && cred.uid != child.uid {
        return Err(Errno::EPERM);
    }
    if child.flags & (IMMUTABLE | APPEND | NOUNLINK) != 0 || dir.flags & APPEND != 0 {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// Creates `name` in `dir` (`open(O_CREAT)`, `mkdir`, `symlink`, `mknod`,
/// `mkfifo`). An existing `name` is `EEXIST` if `exclusive`, or returned
/// as is (`open` without `O_EXCL`); the flag says whether it was made.
pub fn create(
    s: &Sleepable,
    dir: &Arc<Vnode>,
    name: &[u8],
    node: &NewNode,
    c: &Create,
    cred: &Cred,
    exclusive: bool,
) -> Result<(Arc<Vnode>, bool), Errno> {
    match name {
        b"" => return Err(Errno::EISDIR),
        b"." | b".." => {
            return Err(if exclusive {
                Errno::EEXIST
            } else {
                Errno::EISDIR
            });
        }
        _ => {}
    }
    let mut st = dir.write(s);
    if let Some(v) = entry(s, dir, &st, name)? {
        return if exclusive {
            Err(Errno::EEXIST)
        } else {
            Ok((v, false))
        };
    }
    read_only(dir)?;
    let da = attr_of(&st)?;
    access(&da, Kind::Directory, cred, VWRITE | VEXEC)?;
    if da.flags & IMMUTABLE != 0 {
        return Err(Errno::EPERM);
    }
    // A new node takes its directory's group (BSD semantics); a creator
    // outside that group cannot make it setgid (FreeBSD's
    // `ufs_makeinode`).
    let mut c = *c;
    c.gid = da.gid;
    if !cred.is_root() && !cred.in_group(da.gid) {
        c.mode &= !S_ISGID;
    }
    let NodeOps::Directory(d) = st.live_mut()? else {
        return Err(Errno::ENOTDIR);
    };
    let made = d.create(s, name, node, &c)?;
    let v = match made.ops {
        Some(ops) => dir.mount().adopt(s, made.ino, ops),
        None => dir.mount().vget(s, made.ino)?,
    };
    cache::enter(dir, name, Some(&v));
    Ok((v, true))
}

/// Removes `name` from `dir`: `unlink`, or `rmdir` with `dir_wanted`.
pub fn remove(
    s: &Sleepable,
    dir: &Arc<Vnode>,
    name: &[u8],
    cred: &Cred,
    dir_wanted: bool,
) -> Result<(), Errno> {
    match name {
        b"" => return Err(Errno::EISDIR),
        // `.` and `..` are directories, which `unlink` refuses first.
        b"." | b".." if !dir_wanted => return Err(Errno::EPERM),
        b"." | b".." => return Err(Errno::EINVAL),
        _ => {}
    }
    let child = {
        let mut st = dir.write(s);
        let child = entry(s, dir, &st, name)?.ok_or(Errno::ENOENT)?;
        match (dir_wanted, child.is_dir()) {
            (false, true) => return Err(Errno::EPERM),
            (true, false) => return Err(Errno::ENOTDIR),
            _ => {}
        }
        if child.mounted().is_some() {
            return Err(Errno::EBUSY);
        }
        read_only(dir)?;
        let da = attr_of(&st)?;
        let mut cst = child.write(s);
        may_unlink(&da, &attr_of(&cst)?, cred)?;
        let NodeOps::Directory(d) = st.live_mut()? else {
            return Err(Errno::ENOTDIR);
        };
        match cst.live_mut()? {
            NodeOps::Directory(c) if dir_wanted => d.rmdir(s, name, &mut **c)?,
            other => d.remove(s, name, other.node_mut())?,
        }
        cache::remove(dir, name);
        drop(cst);
        drop(st);
        if dir_wanted {
            cache::purge(&child);
        }
        child
    };
    child.release_if_unlinked(s);
    Ok(())
}

/// Links `target` into `dir` as `name` (`link`).
pub fn link(
    s: &Sleepable,
    target: &Arc<Vnode>,
    dir: &Arc<Vnode>,
    name: &[u8],
    cred: &Cred,
) -> Result<(), Errno> {
    match name {
        b"" | b"." | b".." => return Err(Errno::EEXIST),
        _ => {}
    }
    if target.mount().id != dir.mount().id {
        return Err(Errno::EXDEV);
    }
    if target.is_dir() {
        return Err(Errno::EPERM);
    }
    let mut st = dir.write(s);
    if entry(s, dir, &st, name)?.is_some() {
        return Err(Errno::EEXIST);
    }
    read_only(dir)?;
    access(&attr_of(&st)?, Kind::Directory, cred, VWRITE | VEXEC)?;
    let mut tst = target.write(s);
    if attr_of(&tst)?.flags & (IMMUTABLE | APPEND) != 0 {
        return Err(Errno::EPERM);
    }
    let NodeOps::Directory(d) = st.live_mut()? else {
        return Err(Errno::ENOTDIR);
    };
    d.link(s, name, target.ino(), tst.live_mut()?.node_mut())?;
    cache::enter(dir, name, Some(target));
    Ok(())
}

/// Write locks on up to four vnodes, in address order.
type Locked<'a> = [Option<(&'a Vnode, WriteGuard<'a, State>)>; 4];

/// Write locks on `vs` (duplicates once), taken one blocking and the rest
/// tried, retrying from scratch when one is busy.
fn lock_all<'a>(s: &Sleepable, vs: &[&'a Vnode; 4]) -> Locked<'a> {
    let mut uniq: [&Vnode; 4] = *vs;
    uniq.sort_by_key(|v| core::ptr::from_ref(*v).addr());
    let mut n = 1;
    for i in 1..uniq.len() {
        if !core::ptr::eq(uniq[i], uniq[n - 1]) {
            uniq[n] = uniq[i];
            n += 1;
        }
    }
    let uniq = &uniq[..n];
    loop {
        let mut held: Locked<'a> = Default::default();
        held[0] = Some((uniq[0], uniq[0].write(s)));
        let ok = uniq[1..]
            .iter()
            .zip(&mut held[1..])
            .all(|(v, slot)| match v.try_write() {
                Some(g) => {
                    *slot = Some((*v, g));
                    true
                }
                None => false,
            });
        if ok {
            return held;
        }
        drop(held);
        sched::yield_now();
    }
}

fn guard_of<'g, 'a>(held: &'g mut Locked<'a>, v: &Vnode) -> &'g mut WriteGuard<'a, State> {
    &mut held
        .iter_mut()
        .flatten()
        .find(|(x, _)| core::ptr::eq(*x, v))
        .expect("locked above")
        .1
}

/// Whether directory `v` holds nothing but `.` and `..`.
fn empty_dir(s: &Sleepable, st: &State) -> Result<bool, Errno> {
    let NodeOps::Directory(d) = st.live()? else {
        return Err(Errno::ENOTDIR);
    };
    let mut empty = true;
    d.readdir(s, 0, &mut |e| {
        if e.name == b"." || e.name == b".." {
            true
        } else {
            empty = false;
            false
        }
    })?;
    Ok(empty)
}

/// Whether directory `dir` is `ancestor` or below it, within one mount.
fn is_under(s: &Sleepable, dir: &Arc<Vnode>, ancestor: &Vnode) -> Result<bool, Errno> {
    let m = dir.mount();
    let mut ino = dir.ino();
    let root = m.fs().root();
    // Bounded by the depth a path can have.
    for _ in 0..abi::fcntl::PATH_MAX {
        if ino == ancestor.ino() {
            return Ok(true);
        }
        if ino == root {
            return Ok(false);
        }
        let v = m.vget(s, ino)?;
        let parent = match v.read(s).live()? {
            NodeOps::Directory(d) => d.parent(),
            _ => return Err(Errno::ENOTDIR),
        };
        ino = parent;
    }
    Err(Errno::ELOOP)
}

/// Checks a rename's ends against each other; `Ok(false)` when they are
/// one file under two names, which leaves nothing to do.
fn ends_differ(from: &Arc<Vnode>, to: Option<&Arc<Vnode>>) -> Result<bool, Errno> {
    if let Some(t) = to {
        if Arc::ptr_eq(t, from) {
            return Ok(false);
        }
        match (from.is_dir(), t.is_dir()) {
            (true, false) => return Err(Errno::ENOTDIR),
            (false, true) => return Err(Errno::EISDIR),
            _ => {}
        }
        if t.mounted().is_some() {
            return Err(Errno::EBUSY);
        }
    }
    if from.mounted().is_some() {
        return Err(Errno::EBUSY);
    }
    Ok(true)
}

/// Renames `fname` in `fdir` to `tname` in `tdir` (`rename`), with
/// FreeBSD's rules: a file over a file, a directory over an empty
/// directory, never a directory into itself.
pub fn rename(
    s: &Sleepable,
    fdir: &Arc<Vnode>,
    fname: &[u8],
    tdir: &Arc<Vnode>,
    tname: &[u8],
    cred: &Cred,
) -> Result<(), Errno> {
    for n in [fname, tname] {
        match n {
            b"" => return Err(Errno::EISDIR),
            b"." | b".." => return Err(Errno::EINVAL),
            _ => {}
        }
    }
    // A read-only side is EROFS before the mounts are compared, as
    // FreeBSD's lookups for a rename report it.
    read_only(fdir)?;
    read_only(tdir)?;
    if fdir.mount().id != tdir.mount().id {
        return Err(Errno::EXDEV);
    }
    let same_dir = Arc::ptr_eq(fdir, tdir);
    let _serial = (!same_dir).then(|| fdir.mount().rename_lock().lock(s));
    let from: Arc<Vnode>;
    let to: Option<Arc<Vnode>>;
    let mut held: Locked<'_> = Default::default();
    if same_dir {
        // One directory: locked exclusively, then the files in it (the
        // order `remove` takes), so what the names are found to be stays.
        let st = fdir.write(s);
        from = entry(s, fdir, &st, fname)?.ok_or(Errno::ENOENT)?;
        to = entry(s, fdir, &st, tname)?;
        if !ends_differ(&from, to.as_ref())? {
            return Ok(());
        }
        held[0] = Some((&**fdir, st));
        let mut files = [&*from, to.as_deref().unwrap_or(&from)];
        files.sort_by_key(|v| core::ptr::from_ref(*v).addr());
        held[1] = Some((files[0], files[0].write(s)));
        if !core::ptr::eq(files[0], files[1]) {
            held[2] = Some((files[1], files[1].write(s)));
        }
    } else {
        // Find both ends with the directories locked shared.
        let fst = fdir.read(s);
        from = entry(s, fdir, &fst, fname)?.ok_or(Errno::ENOENT)?;
        drop(fst);
        let tst = tdir.read(s);
        to = entry(s, tdir, &tst, tname)?;
        drop(tst);
        if !ends_differ(&from, to.as_ref())? {
            return Ok(());
        }
        // A directory may not move under itself (the rename lock keeps the
        // tree's shape still meanwhile).
        if from.is_dir() && is_under(s, tdir, &from)? {
            return Err(Errno::EINVAL);
        }
        // Without a target, `from` stands in for it: locked once.
        let want = [&**fdir, &**tdir, &*from, to.as_deref().unwrap_or(&from)];
        held = lock_all(s, &want);
        // Things may have moved before the locks: check the names again.
        {
            let fst = guard_of(&mut held, fdir);
            let again = entry(s, fdir, fst, fname)?;
            if !again.is_some_and(|v| Arc::ptr_eq(&v, &from)) {
                return Err(Errno::ENOENT);
            }
        }
        let tst = guard_of(&mut held, tdir);
        let again = entry(s, tdir, tst, tname)?;
        let same = match (&again, &to) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            // Lost a race with a creation or removal: start over.
            drop(held);
            drop(_serial);
            return rename(s, fdir, fname, tdir, tname, cred);
        }
    }
    let fda = attr_of(guard_of(&mut held, fdir))?;
    let tda = attr_of(guard_of(&mut held, tdir))?;
    let fa = attr_of(guard_of(&mut held, &from))?;
    may_unlink(&fda, &fa, cred)?;
    access(&tda, Kind::Directory, cred, VWRITE | VEXEC)?;
    if tda.flags & IMMUTABLE != 0 {
        return Err(Errno::EPERM);
    }
    if let Some(t) = &to {
        let ta = attr_of(guard_of(&mut held, t))?;
        may_unlink(&tda, &ta, cred)?;
        if t.is_dir() && !empty_dir(s, guard_of(&mut held, t))? {
            return Err(Errno::ENOTEMPTY);
        }
    }
    // Moving a directory to another parent rewrites its `..`.
    if from.is_dir() && !same_dir {
        access(&fa, Kind::Directory, cred, VWRITE)?;
    }
    // Take the guards apart for the file system.
    let mut fdir_g = None;
    let mut tdir_g = None;
    let mut from_g = None;
    let mut to_g = None;
    for (v, g) in held.iter_mut().flatten() {
        if core::ptr::eq(*v, &**fdir) {
            fdir_g = Some(g);
        } else if core::ptr::eq(*v, &**tdir) {
            tdir_g = Some(g);
        } else if core::ptr::eq(*v, &*from) {
            from_g = Some(g);
        } else {
            to_g = Some(g);
        }
    }
    let fdir_g = fdir_g.ok_or(Errno::EINVAL)?;
    let NodeOps::Directory(fd) = fdir_g.live_mut()? else {
        return Err(Errno::ENOTDIR);
    };
    let to_dir = match tdir_g {
        Some(g) => match g.live_mut()? {
            NodeOps::Directory(d) => Some(&mut **d),
            _ => return Err(Errno::ENOTDIR),
        },
        None => None,
    };
    let from_ops = from_g.ok_or(Errno::EINVAL)?.live_mut()?;
    let to_ops = match (&to, to_g) {
        (Some(t), Some(g)) => Some((t.ino(), g.live_mut()?)),
        _ => None,
    };
    fdir.mount().fs().rename(
        s,
        Rename {
            from_dir: &mut **fd,
            from_name: fname,
            from_ino: from.ino(),
            from: from_ops,
            to_dir,
            to_name: tname,
            to: to_ops,
        },
    )?;
    cache::remove(fdir, fname);
    cache::remove(tdir, tname);
    cache::enter(tdir, tname, Some(&from));
    drop(held);
    if let Some(t) = to {
        if t.is_dir() {
            cache::purge(&t);
        }
        t.release_if_unlinked(s);
    }
    Ok(())
}

/// Changes attributes (`chmod`, `chown`, `utimensat`, `truncate`,
/// `chflags`) with FreeBSD's permission rules. `times_now`: the times are
/// being set to the current time (`utimensat` with `UTIME_NOW` or a null
/// array), which write access permits as well as ownership.
pub fn setattr(
    s: &Sleepable,
    v: &Vnode,
    a: &SetAttr,
    cred: &Cred,
    times_now: bool,
) -> Result<(), Errno> {
    // Nothing to change (`chown` to -1 and -1): nothing is checked, and
    // the change time stays (FreeBSD's `ufs_setattr`).
    if *a == SetAttr::default() {
        return Ok(());
    }
    if v.mount().read_only() {
        return Err(Errno::EROFS);
    }
    let mut st = v.write(s);
    let cur = attr_of(&st)?;
    let mut a = *a;
    if let Some(flags) = a.flags {
        owner(&cur, cred)?;
        if cred.is_root() {
            // Not even the superuser toggles the snapshot flag.
            if (flags ^ cur.flags) & SF_SNAPSHOT != 0 {
                return Err(Errno::EPERM);
            }
        } else if cur.flags & (SF_NOUNLINK | SF_IMMUTABLE | SF_APPEND) != 0
            || (flags ^ cur.flags) & SF_SETTABLE != 0
        {
            // Only the superuser sets or clears system flags, or changes
            // flags while one is set.
            return Err(Errno::EPERM);
        }
    }
    // An immutable or append-only file keeps its other attributes
    // (FreeBSD's `ufs_setattr`, `tmpfs_chmod` and the rest).
    // A program running it keeps its size (FreeBSD's `vn_truncate`).
    if a.size.is_some() && v.is_text() {
        return Err(Errno::ETXTBSY);
    }
    let others = a.mode.is_some()
        || a.uid.is_some()
        || a.gid.is_some()
        || a.size.is_some()
        || a.atime.is_some()
        || a.mtime.is_some()
        || a.birthtime.is_some();
    if others && a.flags.unwrap_or(cur.flags) & (IMMUTABLE | APPEND) != 0 {
        return Err(Errno::EPERM);
    }
    if let Some(mode) = a.mode {
        owner(&cur, cred)?;
        // Without the superuser, the sticky bit is for directories only,
        // and setgid needs the group (FreeBSD's `ufs_chmod`).
        if !cred.is_root() {
            if v.kind() != Kind::Directory && mode & S_ISVTX != 0 {
                return Err(Errno::EFTYPE);
            }
            if mode & S_ISGID != 0 && !cred.in_group(cur.gid) {
                return Err(Errno::EPERM);
            }
        }
    }
    if a.uid.is_some() || a.gid.is_some() {
        let uid = a.uid.unwrap_or(cur.uid);
        let gid = a.gid.unwrap_or(cur.gid);
        // Only the superuser gives a file away; the owner may change its
        // group to one of their own.
        if !cred.is_root() && (uid != cur.uid || cred.uid != cur.uid || !cred.in_group(gid)) {
            return Err(Errno::EPERM);
        }
        // A non-superuser's change clears the set-id bits (FreeBSD's UFS).
        if !cred.is_root() && cur.mode & (S_ISUID | S_ISGID) != 0 {
            a.mode = Some(a.mode.unwrap_or(cur.mode) & !(S_ISUID | S_ISGID));
        }
    }
    if a.size.is_some() {
        match v.kind() {
            Kind::Directory => return Err(Errno::EISDIR),
            Kind::Regular => {}
            _ => return Err(Errno::EINVAL),
        }
        if cur.flags & APPEND != 0 {
            return Err(Errno::EPERM);
        }
        access(&cur, v.kind(), cred, VWRITE)?;
    }
    if a.atime.is_some() || a.mtime.is_some() || a.birthtime.is_some() {
        // The owner sets times; to the current time, so does a writer, and
        // a caller who is neither hears why it may not write (FreeBSD's
        // `vn_utimes_perm`).
        if owner(&cur, cred).is_err() {
            if times_now {
                access(&cur, v.kind(), cred, VWRITE)?;
            } else {
                return Err(Errno::EPERM);
            }
        }
        // A file is not born after it was last modified (FreeBSD's
        // `setutimes`).
        if let Some(m) = a.mtime
            && a.birthtime.is_none()
            && (m.tv_sec, m.tv_nsec) < (cur.birthtime.tv_sec, cur.birthtime.tv_nsec)
        {
            a.birthtime = Some(m);
        }
    }
    st.live_mut()?.node_mut().setattr(s, &a)
}

/// Packs a directory's entries from cookie `from` into `out` as FreeBSD's
/// `getdirentries` returns them, at most `room` bytes; returns the cookie
/// to resume at. `EINVAL` if not even one entry fits.
pub fn getdirentries(
    s: &Sleepable,
    dir: &Vnode,
    from: u64,
    room: usize,
    out: &mut Vec<u8>,
) -> Result<u64, Errno> {
    let mut next = from;
    let mut full = false;
    dir.readdir(s, from, &mut |e| {
        let len = dirent::reclen(e.name.len());
        if out.len() + len > room {
            full = true;
            return false;
        }
        let at = out.len();
        out.resize(at + len, 0);
        let r = &mut out[at..];
        r[0..8].copy_from_slice(&e.ino.to_ne_bytes());
        r[8..16].copy_from_slice(&e.next.to_ne_bytes());
        r[16..18].copy_from_slice(&u16::try_from(len).unwrap_or(u16::MAX).to_ne_bytes());
        r[18] = e.kind.dtype();
        r[20..22].copy_from_slice(&u16::try_from(e.name.len()).unwrap_or(0).to_ne_bytes());
        r[NAME_OFFSET..NAME_OFFSET + e.name.len()].copy_from_slice(e.name);
        next = e.next;
        true
    })?;
    if out.is_empty() && full {
        return Err(Errno::EINVAL);
    }
    Ok(next)
}
