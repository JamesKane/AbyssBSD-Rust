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

use abi::event::{NOTE_ATTRIB, NOTE_DELETE, NOTE_EXTEND, NOTE_LINK, NOTE_RENAME, NOTE_WRITE};
use abi::inotify::{IN_ACCESS, IN_ATTRIB, IN_CREATE, IN_DELETE};
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

pub use security::Cred;
use security::Priv;

/// Access modes (FreeBSD's `VEXEC`, `VWRITE`, `VREAD`).
pub const VEXEC: u32 = 0o100;
pub const VWRITE: u32 = 0o200;
pub const VREAD: u32 = 0o400;

/// Whether `cred` may access a node with attributes `a` as `want` says
/// (FreeBSD's `vaccess`): `EACCES` if not, `EPERM` to write an immutable
/// node. What the mode does not grant, a privilege may: `PRIV_VFS_LOOKUP`
/// searching a directory, `PRIV_VFS_EXEC` executing a file with some
/// execute bit, `PRIV_VFS_READ` and `PRIV_VFS_WRITE`.
pub fn access(a: &Attr, kind: Kind, cred: &Cred, want: u32) -> Result<(), Errno> {
    // Nobody writes an immutable node (FreeBSD's `ufs_access`).
    if want & VWRITE != 0 && a.flags & (IMMUTABLE | SF_SNAPSHOT) != 0 {
        return Err(Errno::EPERM);
    }
    let shift = if cred.uid == a.uid {
        0
    } else if cred.in_group(a.gid) {
        3
    } else {
        6
    };
    let granted = u32::from(a.mode << shift) & 0o700;
    let missing = want & !granted & 0o700;
    if missing == 0 {
        return Ok(());
    }
    let held = |p| cred.priv_check(p).is_ok();
    let exec_ok = missing & VEXEC == 0
        || if kind == Kind::Directory {
            held(Priv::VfsLookup)
        } else {
            a.mode & 0o111 != 0 && held(Priv::VfsExec)
        };
    if exec_ok
        && (missing & VREAD == 0 || held(Priv::VfsRead))
        && (missing & VWRITE == 0 || held(Priv::VfsWrite))
    {
        Ok(())
    } else {
        Err(Errno::EACCES)
    }
}

/// Whether `cred` may administer the node, changing its mode, times, or
/// flags (`VADMIN`): its owner, or with `PRIV_VFS_ADMIN`; `EPERM` if not.
fn owner(a: &Attr, cred: &Cred) -> Result<(), Errno> {
    if cred.uid == a.uid {
        Ok(())
    } else {
        cred.priv_check(Priv::VfsAdmin)
    }
}

/// The node's attributes, locked.
fn attr_of(st: &State) -> Result<Attr, Errno> {
    Ok(st.live()?.node().attr())
}

/// `access` for `v`, locked as `st`, by its ACL where its mount keeps them
/// (`VOP_ACCESS`).
fn access_st(s: &Sleepable, v: &Vnode, st: &State, cred: &Cred, want: u32) -> Result<(), Errno> {
    let node = st.live()?.node();
    crate::acl::access_locked(s, v, node, &node.attr(), cred, want)
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

/// `cred`, with write and search permission on directory `dir`, may
/// remove or rename `child` out of it (the sticky bit, and the no-unlink
/// flags).
fn may_unlink(dir: &Attr, child: &Attr, cred: &Cred) -> Result<(), Errno> {
    if dir.mode & S_ISVTX != 0 && owner(dir, cred).is_err() && owner(child, cred).is_err() {
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
    access_st(s, dir, &st, cred, VWRITE | VEXEC)?;
    if da.flags & IMMUTABLE != 0 {
        return Err(Errno::EPERM);
    }
    // A new node takes its directory's group (BSD semantics); a creator
    // outside that group cannot make it setgid (FreeBSD's
    // `ufs_makeinode`).
    let mut c = *c;
    c.gid = da.gid;
    if c.mode & S_ISGID != 0 && !cred.in_group(da.gid) && cred.priv_check(Priv::VfsSetgid).is_err()
    {
        c.mode &= !S_ISGID;
    }
    // Under a default ACL, the new node takes no permission it withholds,
    // and inherits it (FreeBSD's `ufs_do_posix1e_acl_inheritance_*`).
    let dacl = crate::acl::inherited(s, dir, st.live()?.node())?;
    if let Some(d) = &dacl {
        c.mode = crate::acl::new_file_mode(c.mode, d);
    }
    let NodeOps::Directory(d) = st.live_mut()? else {
        return Err(Errno::ENOTDIR);
    };
    let made = d.create(s, name, node, &c)?;
    let v = match made.ops {
        Some(ops) => dir.mount().adopt(s, made.ino, ops),
        None => dir.mount().vget(s, made.ino)?,
    };
    if let Some(d) = &dacl {
        crate::acl::inherit(s, &v, c.mode, d)?;
    }
    if dir.mount().fs().caches_created() || v.is_dir() || crate::inotify::watched(dir) {
        cache::enter_made(dir, name, &v);
    } else {
        // A negative entry for the name goes.
        cache::remove(dir, name);
    }
    // `vop_create_post`, `vop_mkdir_post` (a new `..`), and the rest.
    dir.knote(if v.is_dir() {
        NOTE_WRITE | NOTE_LINK
    } else {
        NOTE_WRITE
    });
    crate::inotify::name_event(s, &v, dir, name, IN_CREATE);
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
        access_st(s, dir, &st, cred, VWRITE | VEXEC)?;
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
        // `vop_remove_post`, `vop_rmdir_post`.
        dir.knote(if dir_wanted {
            NOTE_WRITE | NOTE_LINK
        } else {
            NOTE_WRITE
        });
        child.knote(NOTE_DELETE);
        // A directory's removal is its deletion; a file's changes its link
        // count first.
        if !dir_wanted {
            crate::inotify::name_event(s, &child, dir, name, crate::inotify::ATTRIB_LINKCOUNT);
        }
        crate::inotify::name_event(s, &child, dir, name, IN_DELETE);
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
    access_st(s, dir, &st, cred, VWRITE | VEXEC)?;
    let mut tst = target.write(s);
    if attr_of(&tst)?.flags & (IMMUTABLE | APPEND) != 0 {
        return Err(Errno::EPERM);
    }
    let NodeOps::Directory(d) = st.live_mut()? else {
        return Err(Errno::ENOTDIR);
    };
    d.link(s, name, target.ino(), tst.live_mut()?.node_mut())?;
    cache::enter_made(dir, name, target);
    // `vop_link_post`.
    target.knote(NOTE_LINK);
    dir.knote(NOTE_WRITE);
    crate::inotify::name_event(s, target, dir, name, crate::inotify::ATTRIB_LINKCOUNT);
    crate::inotify::name_event(s, target, dir, name, IN_CREATE);
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
    access_st(s, fdir, guard_of(&mut held, fdir), cred, VWRITE | VEXEC)?;
    may_unlink(&fda, &fa, cred)?;
    access_st(s, tdir, guard_of(&mut held, tdir), cred, VWRITE | VEXEC)?;
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
        access_st(s, &from, guard_of(&mut held, &from), cred, VWRITE)?;
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
    // The new name is left for a lookup to enter, as FreeBSD does; a
    // directory's goes in now, for its name (`getcwd`).
    if from.is_dir() {
        cache::enter_made(tdir, tname, &from);
    }
    drop(held);
    rename_notes(fdir, tdir, &from, to.as_deref());
    // A file now in a watched directory answers to its new name there.
    if !from.is_dir() && crate::inotify::watched(tdir) {
        from.set_named(tdir, tname);
    }
    crate::inotify::move_event(s, &from, (fdir, fname), (tdir, tname), to.as_deref());
    if let Some(t) = to {
        if t.is_dir() {
            cache::purge(&t);
        }
        t.release_if_unlinked(s);
    }
    Ok(())
}

/// What a rename tells the vnodes' knotes (`vop_rename_post`).
fn rename_notes(fdir: &Vnode, tdir: &Vnode, from: &Vnode, to: Option<&Vnode>) {
    let mut hint = NOTE_WRITE;
    if core::ptr::eq(fdir, tdir) {
        if to.is_some_and(Vnode::is_dir) {
            hint |= NOTE_LINK;
        }
        fdir.knote(hint);
        tdir.knote(hint);
    } else {
        hint |= NOTE_EXTEND;
        if from.is_dir() {
            hint |= NOTE_LINK;
        }
        fdir.knote(hint);
        if from.is_dir() && to.is_some_and(Vnode::is_dir) {
            hint &= !NOTE_LINK;
        }
        tdir.knote(hint);
    }
    from.knote(NOTE_RENAME);
    if let Some(t) = to {
        t.knote(NOTE_DELETE);
    }
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
        // With `PRIV_VFS_SYSFLAGS` (root, or root in a jail with
        // `allow.chflags`), system flags change, but once one is set only
        // at securelevel 0 or below; without it, no system flag changes,
        // nor any flag while one is set (FreeBSD's `ufs_setattr`).
        if cred.priv_check(Priv::VfsSysflags).is_ok() {
            if cur.flags & (SF_NOUNLINK | SF_IMMUTABLE | SF_APPEND) != 0 {
                cred.securelevel_gt(0)?;
            }
            // Not even the superuser toggles the snapshot flag.
            if (flags ^ cur.flags) & SF_SNAPSHOT != 0 {
                return Err(Errno::EPERM);
            }
        } else if cur.flags & (SF_NOUNLINK | SF_IMMUTABLE | SF_APPEND) != 0
            || (flags ^ cur.flags) & SF_SETTABLE != 0
        {
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
        // The sticky bit on a file needs `PRIV_VFS_STICKYFILE`, setgid
        // outside the file's group `PRIV_VFS_SETGID`, and setuid on
        // another's file `PRIV_VFS_ADMIN` (FreeBSD's `ufs_chmod`).
        if v.kind() != Kind::Directory
            && mode & S_ISVTX != 0
            && cred.priv_check(Priv::VfsStickyfile).is_err()
        {
            return Err(Errno::EFTYPE);
        }
        if mode & S_ISGID != 0 && !cred.in_group(cur.gid) {
            cred.priv_check(Priv::VfsSetgid)?;
        }
        if mode & S_ISUID != 0 && cur.uid != cred.uid {
            cred.priv_check(Priv::VfsAdmin)?;
        }
    }
    if a.uid.is_some() || a.gid.is_some() {
        owner(&cur, cred)?;
        let uid = a.uid.unwrap_or(cur.uid);
        let gid = a.gid.unwrap_or(cur.gid);
        // Giving a file away, or to a group one is not in, needs
        // `PRIV_VFS_CHOWN` (FreeBSD's `ufs_chown`).
        if (uid != cur.uid && uid != cred.uid) || (gid != cur.gid && !cred.in_group(gid)) {
            cred.priv_check(Priv::VfsChown)?;
        }
        // The set-id bits go, unless `PRIV_VFS_RETAINSUGID` keeps them.
        if cur.mode & (S_ISUID | S_ISGID) != 0 && cred.priv_check(Priv::VfsRetainsugid).is_err() {
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
        access_st(s, v, &st, cred, VWRITE)?;
    }
    if a.atime.is_some() || a.mtime.is_some() || a.birthtime.is_some() {
        // The owner sets times; to the current time, so does a writer, and
        // a caller who is neither hears why it may not write (FreeBSD's
        // `vn_utimes_perm`).
        if owner(&cur, cred).is_err() {
            if times_now {
                access_st(s, v, &st, cred, VWRITE)?;
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
    st.live_mut()?.node_mut().setattr(s, &a)?;
    drop(st);
    // `vop_setattr_post`; a size change moves what `EVFILT_READ` sees.
    let mut hint = NOTE_ATTRIB;
    if let Some(size) = a.size {
        if size > cur.size {
            hint |= NOTE_EXTEND;
        }
        v.size_changed(size);
    }
    v.knote(hint);
    crate::inotify::event(v, IN_ATTRIB);
    Ok(())
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
    // A directory removed while open has nothing to read
    // (`kern_getdirentries`' `VV_UNLINKED`).
    if dir.attr(s)?.nlink == 0 {
        return Err(Errno::ENOENT);
    }
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
    // `vop_readdir_post`.
    crate::inotify::event(dir, IN_ACCESS);
    Ok(next)
}

/// Extended attribute namespaces (`EXTATTR_NAMESPACE_*`).
pub mod extattr {
    pub const USER: u8 = 1;
    pub const SYSTEM: u8 = 2;
    /// The longest name (`EXTATTR_MAXNAMELEN`).
    pub const MAXNAMELEN: usize = 255;
}

/// Whether `cred` may reach `v`'s extended attributes in `ns`, wanting
/// `want` (`VREAD` or `VWRITE`) (`extattr_check_cred`): the system
/// namespace takes `PRIV_VFS_EXTATTR_SYSTEM`, the user one the file's
/// own permission; any other is `EPERM`. Devices have none
/// (`EOPNOTSUPP`), and a name must be neither empty nor too long.
fn extattr_check(
    s: &Sleepable,
    v: &Vnode,
    cred: &Cred,
    ns: u8,
    name: Option<&[u8]>,
    want: u32,
) -> Result<(), Errno> {
    if matches!(v.kind(), Kind::CharDevice | Kind::BlockDevice) {
        return Err(Errno::EOPNOTSUPP);
    }
    match ns {
        extattr::SYSTEM => cred.priv_check(Priv::VfsExtattrSystem)?,
        extattr::USER => crate::acl::access(s, v, cred, want)?,
        _ => return Err(Errno::EPERM),
    }
    if let Some(n) = name
        && (n.is_empty() || n.len() > extattr::MAXNAMELEN)
    {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// `extattr_get_*`: the value of attribute `name`.
pub fn get_extattr(
    s: &Sleepable,
    v: &Vnode,
    cred: &Cred,
    ns: u8,
    name: &[u8],
) -> Result<Vec<u8>, Errno> {
    extattr_check(s, v, cred, ns, Some(name), VREAD)?;
    let st = v.read(s);
    st.live()?.node().get_extattr(s, ns, name)
}

/// `extattr_set_*`: attribute `name` becomes `value`.
pub fn set_extattr(
    s: &Sleepable,
    v: &Vnode,
    cred: &Cred,
    ns: u8,
    name: &[u8],
    value: &[u8],
) -> Result<(), Errno> {
    extattr_check(s, v, cred, ns, Some(name), VWRITE)?;
    let mut st = v.write(s);
    st.live_mut()?.node_mut().set_extattr(s, ns, name, value)?;
    drop(st);
    // `vop_setextattr_post`.
    crate::inotify::event(v, IN_ATTRIB);
    Ok(())
}

/// `extattr_delete_*`.
pub fn delete_extattr(
    s: &Sleepable,
    v: &Vnode,
    cred: &Cred,
    ns: u8,
    name: &[u8],
) -> Result<(), Errno> {
    extattr_check(s, v, cred, ns, Some(name), VWRITE)?;
    let mut st = v.write(s);
    st.live_mut()?.node_mut().delete_extattr(s, ns, name)?;
    drop(st);
    // `vop_deleteextattr_post`.
    crate::inotify::event(v, IN_ATTRIB);
    Ok(())
}

/// `extattr_list_*`: each name in `ns`, as the list `extattr_list_*`
/// returns: a length byte, then the name, no NUL.
pub fn list_extattr(s: &Sleepable, v: &Vnode, cred: &Cred, ns: u8) -> Result<Vec<u8>, Errno> {
    extattr_check(s, v, cred, ns, None, VREAD)?;
    let names = {
        let st = v.read(s);
        st.live()?.node().list_extattr(s, ns)?
    };
    let mut out = Vec::new();
    for n in names {
        out.push(u8::try_from(n.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(&n);
    }
    Ok(out)
}
