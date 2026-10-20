// SPDX-License-Identifier: BSD-2-Clause
//! The name cache (docs/VFS.md §6.1): (directory, name) to the vnode it
//! names, or to nothing (a negative entry), so a lookup that hits never
//! asks the file system.
//!
//! Only the generic layer adds and removes entries, under the directory's
//! exclusive lock for changes; file systems never see the cache. A
//! positive entry refers to its vnode weakly: it does not keep the vnode
//! in core (its mount's table does, until the reclaimer takes it), so the
//! cache never decides what memory vnodes use. Entries are purged when
//! their vnode is reclaimed, and one whose vnode is gone counts as a miss.
//! Each bucket keeps its entries oldest first; past the total limit an
//! insert evicts its bucket's oldest, and negative entries are held to a
//! fifth of the cache (FreeBSD's `ncnegfactor`).
//!
//! Behavior reference: sys/kern/vfs_cache.c.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use sync::SpinLock;

use crate::NodeId;
use crate::vnode::Vnode;

const BUCKETS: usize = 4096;

/// Entries in all, before inserts evict.
const MAX_ENTRIES: usize = 1 << 17;

/// Negative entries may be this share of the cache (FreeBSD's
/// `ncnegfactor`: a fifth), and never fewer than [`MIN_NEGATIVE`].
const NEGATIVE_FACTOR: usize = 5;
const MIN_NEGATIVE: usize = 1024;

struct Entry {
    dir: NodeId,
    name: Box<[u8]>,
    /// `None` for a negative entry.
    target: Option<Weak<Vnode>>,
}

static TABLE: [SpinLock<Vec<Entry>>; BUCKETS] = [const { SpinLock::new(Vec::new()) }; BUCKETS];
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static NEGATIVE: AtomicUsize = AtomicUsize::new(0);

/// What the cache knows about a name.
#[derive(Debug)]
pub enum Hit {
    Positive(Arc<Vnode>),
    /// The name is known not to exist.
    Negative,
}

fn bucket(dir: NodeId, name: &[u8]) -> &'static SpinLock<Vec<Entry>> {
    // FNV-1a over the directory and the name.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in dir
        .fs
        .to_le_bytes()
        .iter()
        .chain(dir.ino.to_le_bytes().iter())
        .chain(name)
    {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
    }
    &TABLE[usize::try_from(h % BUCKETS as u64).unwrap_or(0)]
}

fn forget(e: &Entry) {
    TOTAL.fetch_sub(1, Ordering::Relaxed);
    if e.target.is_none() {
        NEGATIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What `dir` holds as `name`, if the cache knows.
#[must_use]
pub fn lookup(dir: &Vnode, name: &[u8]) -> Option<Hit> {
    let id = dir.id();
    let b = bucket(id, name).lock();
    let e = b.iter().find(|e| e.dir == id && *e.name == *name)?;
    match &e.target {
        // Gone or going (reclaimed meanwhile): a miss.
        Some(v) => v.upgrade().filter(|v| !v.is_dying()).map(Hit::Positive),
        None => Some(Hit::Negative),
    }
}

/// Records that `dir` holds `target` as `name` (`None`: that it holds
/// nothing by that name), replacing what was known. A directory target
/// also records its name, for `getcwd`.
pub fn enter(dir: &Arc<Vnode>, name: &[u8], target: Option<&Arc<Vnode>>) {
    if let Some(t) = target
        && t.is_dir()
    {
        t.set_named(dir, name);
    }
    let id = dir.id();
    let mut evicted = Vec::new();
    {
        let mut b = bucket(id, name).lock();
        if let Some(i) = b.iter().position(|e| e.dir == id && *e.name == *name) {
            let old = b.remove(i);
            forget(&old);
            evicted.push(old);
        }
        // Over a limit: this bucket's oldest goes (a negative one first if
        // negatives are what is over).
        let total = TOTAL.load(Ordering::Relaxed);
        let negatives = NEGATIVE.load(Ordering::Relaxed);
        let neg_limit = (total / NEGATIVE_FACTOR).max(MIN_NEGATIVE);
        if target.is_none()
            && negatives >= neg_limit
            && let Some(i) = b.iter().position(|e| e.target.is_none())
        {
            let old = b.remove(i);
            forget(&old);
            evicted.push(old);
        } else if total >= MAX_ENTRIES && !b.is_empty() {
            let old = b.remove(0);
            forget(&old);
            evicted.push(old);
        }
        b.push(Entry {
            dir: id,
            name: name.into(),
            target: target.map(Arc::downgrade),
        });
        TOTAL.fetch_add(1, Ordering::Relaxed);
        if target.is_none() {
            NEGATIVE.fetch_add(1, Ordering::Relaxed);
        }
    }
    drop(evicted);
}

/// Forgets what `dir` holds as `name`.
pub fn remove(dir: &Vnode, name: &[u8]) {
    let id = dir.id();
    let old = {
        let mut b = bucket(id, name).lock();
        b.iter()
            .position(|e| e.dir == id && *e.name == *name)
            .map(|i| b.remove(i))
    };
    if let Some(e) = &old {
        forget(e);
    }
    drop(old);
}

/// Forgets every entry naming `v` or in directory `v` (it is being
/// reclaimed).
pub fn purge(v: &Vnode) {
    let id = v.id();
    purge_where(|e| {
        e.dir == id
            || e.target
                .as_ref()
                .is_some_and(|t| t.upgrade().is_none_or(|t| t.id() == id))
    });
    v.clear_named();
}

/// Forgets every entry of the file system with id `fs` (it is being
/// unmounted).
pub fn purge_fs(fs: u64) {
    purge_where(|e| {
        e.dir.fs == fs
            || e.target
                .as_ref()
                .is_some_and(|t| t.upgrade().is_none_or(|t| t.mount().id == fs))
    });
}

fn purge_where(doomed: impl Fn(&Entry) -> bool) {
    for bucket in &TABLE {
        let gone: Vec<Entry> = {
            let mut b = bucket.lock();
            let (gone, keep): (Vec<Entry>, Vec<Entry>) = core::mem::take(&mut *b)
                .into_iter()
                .partition(|e| doomed(e));
            *b = keep;
            gone
        };
        for e in &gone {
            forget(e);
        }
    }
}

/// Entries cached, and how many are negative.
#[must_use]
pub fn counts() -> (usize, usize) {
    (
        TOTAL.load(Ordering::Relaxed),
        NEGATIVE.load(Ordering::Relaxed),
    )
}
