// SPDX-License-Identifier: BSD-2-Clause
//! Numbered CPU sets (`cpuset(2)`): set 0 holds every CPU, set 1 is the
//! one processes start in and prison0's root, each jail has a root set of
//! its own under its parent's (`cpuset_create_root`), and `cpuset` makes
//! another, a child of the caller's root set with its CPUs, which the
//! caller's process joins. A process is in one numbered set (inherited);
//! each thread runs on its own mask within it (`cpuset_setaffinity` at
//! `CPU_LEVEL_WHICH`), which a change to the set narrows (`cpuset_update`).
//! A thread whose mask is its set's has none of its own, and takes a new
//! set's whole (`cpuset_setproc_setthread_mask` decides the same way).
//!
//! Behavior reference: sys/kern/kern_cpuset.c (`sys_cpuset`,
//! `kern_cpuset_getid`, `kern_cpuset_setid`, `cpuset_modify`,
//! `cpuset_setproc`).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::errno::Errno;
use sync::SpinLock;

use crate::proc::Process;
use crate::uthread::Cpus;

/// The set of every CPU.
pub(crate) const ROOT: i32 = 0;
/// The set processes start in (`cpuset_default`), prison0's root.
pub(crate) const DEFAULT: i32 = 1;

#[derive(Clone, Copy)]
struct Set {
    mask: u64,
    /// The set it was made in; [`ROOT`] for the two first.
    parent: i32,
    /// A root set (`CPU_SET_ROOT`): the first two and each jail's.
    root: bool,
}

/// The sets besides the first two, by id, and set 1 once narrowed.
static SETS: SpinLock<BTreeMap<i32, Set>> = SpinLock::new(BTreeMap::new());
/// The next id a new set gets (`cpuset_unr` starts past the two).
static NEXT: AtomicI32 = AtomicI32::new(2);

/// Every CPU there is, one bit each.
fn all() -> u64 {
    let n = sched::online_cpus().min(64);
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

fn first() -> Set {
    Set {
        mask: all(),
        parent: ROOT,
        root: true,
    }
}

fn get(id: i32) -> Option<Set> {
    match id {
        ROOT => Some(first()),
        // The default set may have been narrowed.
        DEFAULT => Some(SETS.lock().get(&id).copied().unwrap_or_else(first)),
        _ => SETS.lock().get(&id).copied(),
    }
}

/// A new set under `parent` with `mask`.
fn insert(parent: i32, mask: u64, root: bool) -> i32 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    SETS.lock().insert(id, Set { mask, parent, root });
    id
}

/// Set `id`'s CPUs; `ESRCH` for no such set.
pub(crate) fn mask(id: i32) -> Result<u64, Errno> {
    get(id).map(|s| s.mask).ok_or(Errno::ESRCH)
}

/// The root set above `id`, or `id` itself if it is one
/// (`cpuset_refroot`).
pub(crate) fn root_of(id: i32) -> i32 {
    let mut id = id;
    for _ in 0..64 {
        match get(id) {
            Some(s) if !s.root => id = s.parent,
            Some(_) => return id,
            None => return DEFAULT,
        }
    }
    DEFAULT
}

/// `cpuset(&setid)`: a new set in the caller's root, with its CPUs, which
/// process `p` joins. Returns its id.
pub(crate) fn create(p: &Process) -> Result<i32, Errno> {
    let root = root_of(p.cpuset());
    let parent = get(root).ok_or(Errno::ESRCH)?;
    let id = insert(root, parent.mask, false);
    join(p, id)?;
    Ok(id)
}

/// A new jail's root set, under its parent's root `parent` and with its
/// CPUs (`cpuset_create_root`).
pub(crate) fn create_root(parent: i32) -> i32 {
    let m = mask(parent).unwrap_or_else(|_| all());
    insert(parent, m, true)
}

/// Process `p` moves into set `id` (`cpuset_setproc` with a set): its
/// threads take the set's CPUs, or keep their own within it, `EDEADLK` if
/// one would have none.
pub(crate) fn join(p: &Process, id: i32) -> Result<(), Errno> {
    let m = mask(id)?;
    let old = mask(p.cpuset()).unwrap_or_else(|_| all());
    let threads = p.threads();
    // CPUs of its own narrower than its old set's carry over
    // (`cpuset_setproc_setthread_mask`); otherwise the new set's whole.
    let new_mask = |c: Cpus| {
        if c.own && c.mask & all() != old {
            c.mask & m
        } else {
            m
        }
    };
    if threads.iter().any(|t| new_mask(t.affinity()) == 0) {
        return Err(Errno::EDEADLK);
    }
    let left = p.cpuset();
    p.set_cpuset(id);
    for t in threads {
        t.set_cpus(new_mask(t.affinity()));
    }
    release(left);
    Ok(())
}

/// Frees set `id` if nothing refers to it any more (`cpuset_rel`): not a
/// root set (a jail's holds one), and no process or set in it.
fn release(id: i32) {
    if get(id).is_none_or(|s| s.root) || id == DEFAULT {
        return;
    }
    if crate::proc::all().iter().any(|q| q.cpuset() == id) {
        return;
    }
    let mut sets = SETS.lock();
    if !sets.values().any(|s| s.parent == id) {
        sets.remove(&id);
    }
}

/// Process `p`, attaching to a jail whose root set is `jail` from a
/// prison whose root set is `from`, moves under it (`cpuset_setproc` with
/// `rebase`): a process in its prison's root set joins the jail's; one in
/// a set of its own gets a new set under the jail's with both's CPUs, or
/// the jail's root itself if they share none and it may widen its mask
/// (`PRIV_SCHED_CPUSET`), `EDEADLK` if not.
pub(crate) fn rebase(p: &Process, from: i32, jail: i32) -> Result<(), Errno> {
    let base = p.cpuset();
    let mut to = jail;
    let mut made = None;
    if root_of(base) != jail && base != from {
        let both = mask(base)? & mask(jail)?;
        if both != 0 {
            let id = insert(jail, both, false);
            made = Some(id);
            to = id;
        } else if p.priv_check(crate::cred::Priv::SchedCpuset).is_err() {
            return Err(Errno::EDEADLK);
        }
    }
    let r = join(p, to);
    if let (Err(_), Some(id)) = (r, made) {
        SETS.lock().remove(&id);
    }
    r
}

/// `cpuset_modify`: set `id` gets `new` (`PRIV_SCHED_CPUSET`), within its
/// root's CPUs (a root's parent's), and the sets under it and every thread
/// in them are narrowed to it (`cpuset_update`); `EDEADLK` if one would
/// have none, `EPERM` for the set of every CPU or the caller's own jail's
/// root, `EINVAL` for CPUs outside the root.
pub(crate) fn modify(me: &Process, id: i32, new: u64) -> Result<(), Errno> {
    me.priv_check(crate::cred::Priv::SchedCpuset)?;
    let mine = crate::prison::of(&me.cred());
    if id == ROOT || (!mine.is_prison0() && id == mine.cpuset()) {
        return Err(Errno::EPERM);
    }
    let set = get(id).ok_or(Errno::ESRCH)?;
    let root = if set.root { set.parent } else { root_of(id) };
    let within = mask(root)?;
    if new & !within != 0 {
        return Err(Errno::EINVAL);
    }
    if new == 0 {
        return Err(Errno::EDEADLK);
    }
    // The set and those under it, with their masks once narrowed
    // (`cpuset_testupdate`), parents before children.
    let mut tree: Vec<(i32, u64)> = alloc::vec![(id, new)];
    {
        let sets = SETS.lock();
        let mut i = 0;
        while i < tree.len() {
            let (parent, pmask) = tree[i];
            for (&cid, c) in sets.iter() {
                if c.parent == parent && cid != parent {
                    let m = c.mask & pmask;
                    if m == 0 {
                        return Err(Errno::EDEADLK);
                    }
                    tree.push((cid, m));
                }
            }
            i += 1;
        }
    }
    let narrowed = |set: i32| tree.iter().find(|&&(s, _)| s == set).map(|&(_, m)| m);
    let members: Vec<_> = crate::proc::all()
        .into_iter()
        .filter_map(|q| narrowed(q.cpuset()).map(|m| (q, m)))
        .collect();
    for (q, m) in &members {
        if q.threads().iter().any(|t| follow(t.affinity(), *m) == 0) {
            return Err(Errno::EDEADLK);
        }
    }
    {
        // Only `id` can be missing: set 1, never narrowed before.
        let mut sets = SETS.lock();
        for &(s, m) in &tree {
            sets.entry(s).or_insert(set).mask = m;
        }
    }
    for (q, m) in &members {
        for t in q.threads() {
            t.set_cpus(follow(t.affinity(), *m));
        }
    }
    Ok(())
}

/// A thread's CPUs once its set becomes `new`: the set's, unless it has
/// its own, which are narrowed to it (`cpuset_update` of its anonymous
/// set).
fn follow(c: Cpus, new: u64) -> u64 {
    if c.own { c.mask & new } else { new }
}
