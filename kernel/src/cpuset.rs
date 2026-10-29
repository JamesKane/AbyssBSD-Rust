// SPDX-License-Identifier: BSD-2-Clause
//! Numbered CPU sets (`cpuset(2)`): set 0 holds every CPU, set 1 is the
//! one processes start in, and `cpuset` makes another, a child of the
//! caller's root set with its CPUs, which the caller's process joins. A
//! process is in one numbered set (inherited); each thread runs on its own
//! mask within it (`cpuset_setaffinity` at `CPU_LEVEL_WHICH`), which a
//! change to the set narrows (`cpuset_update`).
//!
//! Behavior reference: sys/kern/kern_cpuset.c (`sys_cpuset`,
//! `kern_cpuset_getid`, `kern_cpuset_setid`, `cpuset_modify`).

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::errno::Errno;
use sync::SpinLock;

use crate::proc::Process;

/// The set of every CPU.
pub(crate) const ROOT: i32 = 0;
/// The set processes start in (`cpuset_default`).
pub(crate) const DEFAULT: i32 = 1;

#[derive(Clone, Copy)]
struct Set {
    mask: u64,
    /// The set it was made in; [`ROOT`] for the two first.
    parent: i32,
}

/// The sets besides the first two, by id.
static SETS: SpinLock<BTreeMap<i32, Set>> = SpinLock::new(BTreeMap::new());
/// The next id `cpuset` hands out (`cpuset_unr` starts past the two).
static NEXT: AtomicI32 = AtomicI32::new(2);

/// Every CPU there is, one bit each.
fn all() -> u64 {
    let n = sched::online_cpus().min(64);
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

fn get(id: i32) -> Option<Set> {
    let first = Set {
        mask: all(),
        parent: ROOT,
    };
    match id {
        ROOT => Some(first),
        // The default set may have been narrowed.
        DEFAULT => Some(SETS.lock().get(&id).copied().unwrap_or(first)),
        _ => SETS.lock().get(&id).copied(),
    }
}

/// Set `id`'s CPUs; `ESRCH` for no such set.
pub(crate) fn mask(id: i32) -> Result<u64, Errno> {
    get(id).map(|s| s.mask).ok_or(Errno::ESRCH)
}

/// The root set above `id` (`cpuset_refroot`): the first two are roots.
pub(crate) fn root_of(id: i32) -> i32 {
    let mut id = id;
    for _ in 0..64 {
        if id == ROOT || id == DEFAULT {
            return id;
        }
        match get(id) {
            Some(s) => id = s.parent,
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
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    SETS.lock().insert(
        id,
        Set {
            mask: parent.mask,
            parent: root,
        },
    );
    join(p, id)?;
    Ok(id)
}

/// Process `p` moves into set `id` (`cpuset_setproc`): its threads run
/// within the set's CPUs, `EDEADLK` if one would have none.
pub(crate) fn join(p: &Process, id: i32) -> Result<(), Errno> {
    let m = mask(id)?;
    let mut ok = true;
    p.each_thread(|t| {
        ok &= t.cpus() & m != 0;
        ok
    });
    if !ok {
        return Err(Errno::EDEADLK);
    }
    p.set_cpuset(id);
    p.each_thread(|t| {
        t.set_cpus(t.cpus() & m);
        true
    });
    Ok(())
}

/// `cpuset_modify`: set `id` gets `new` (`PRIV_SCHED_CPUSET`), within its
/// parent's CPUs, and every thread in it is narrowed to it; `EDEADLK` if
/// any thread would have none, `EPERM` for the set of every CPU.
pub(crate) fn modify(me: &Process, id: i32, new: u64) -> Result<(), Errno> {
    me.priv_check(crate::cred::Priv::SchedCpuset)?;
    if id == ROOT {
        return Err(Errno::EPERM);
    }
    let set = get(id).ok_or(Errno::ESRCH)?;
    let parent = get(set.parent).ok_or(Errno::ESRCH)?;
    if new & !parent.mask != 0 || new == 0 {
        return Err(Errno::EDEADLK);
    }
    let members: alloc::vec::Vec<_> = crate::proc::all()
        .into_iter()
        .filter(|q| q.cpuset() == id)
        .collect();
    for q in &members {
        let mut ok = true;
        q.each_thread(|t| {
            ok &= t.cpus() & new != 0;
            ok
        });
        if !ok {
            return Err(Errno::EDEADLK);
        }
    }
    SETS.lock()
        .entry(id)
        .or_insert(Set {
            mask: new,
            parent: set.parent,
        })
        .mask = new;
    for q in &members {
        q.each_thread(|t| {
            t.set_cpus(t.cpus() & new);
            true
        });
    }
    Ok(())
}
