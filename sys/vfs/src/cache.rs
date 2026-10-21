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
//! Each vnode keeps the keys of its entries, those in it as a directory
//! and those naming it, so purging one costs only its own entries.
//!
//! Behavior reference: sys/kern/vfs_cache.c.

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use sync::SpinLock;

use crate::NodeId;
use crate::vnode::Vnode;

const BUCKET_BITS: u32 = 12;
const BUCKETS: usize = 1 << BUCKET_BITS;

/// Entries in all, before inserts evict.
const MAX_ENTRIES: usize = 1 << 17;

/// Negative entries may be this share of the cache (FreeBSD's
/// `ncnegfactor`: a fifth), and never fewer than [`MIN_NEGATIVE`].
const NEGATIVE_FACTOR: usize = 5;
const MIN_NEGATIVE: usize = 1024;

struct Entry {
    /// [`hash`] of the directory and the name.
    hash: u64,
    dir: NodeId,
    /// The directory itself: an entry matches only the vnode it was made
    /// for, never a later one for the same inode.
    dir_vnode: Weak<Vnode>,
    name: Box<[u8]>,
    /// `None` for a negative entry.
    target: Option<Weak<Vnode>>,
}

impl Entry {
    fn is(&self, h: u64, dir: &Vnode, id: NodeId, name: &[u8]) -> bool {
        self.hash == h
            && self.dir == id
            && *self.name == *name
            && core::ptr::eq(self.dir_vnode.as_ptr(), dir)
    }

    fn names(&self, v: &Vnode) -> bool {
        self.target
            .as_ref()
            .is_some_and(|t| core::ptr::eq(t.as_ptr(), v))
    }
}

/// A vnode's entries, by key (FreeBSD's `v_cache_src` and `v_cache_dst`),
/// so purging it touches only its own. Kept under the entries' bucket
/// locks (a bucket lock, then these).
pub(crate) struct Links {
    /// The names cached in this directory.
    src: BTreeSet<Box<[u8]>>,
    /// The entries naming this vnode: directory and name.
    dst: BTreeSet<(NodeId, Box<[u8]>)>,
}

impl Links {
    pub(crate) const fn new() -> Self {
        Self {
            src: BTreeSet::new(),
            dst: BTreeSet::new(),
        }
    }
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

/// The hash of a directory and a name, eight bytes at a time (FxHash's
/// mixing: rotate, xor, multiply). Names hold no NUL, so padding the last
/// word with zeros makes no two names alike.
fn hash(dir: NodeId, name: &[u8]) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mix = |h: u64, w: u64| (h.rotate_left(5) ^ w).wrapping_mul(K);
    let mut h = mix(mix(0, dir.fs), dir.ino);
    let (words, tail) = name.as_chunks::<8>();
    for w in words {
        h = mix(h, u64::from_le_bytes(*w));
    }
    if !tail.is_empty() {
        let mut w = [0u8; 8];
        w[..tail.len()].copy_from_slice(tail);
        h = mix(h, u64::from_le_bytes(w));
    }
    h
}

/// The bucket for hash `h`: its high bits, the well-mixed ones.
fn bucket(h: u64) -> &'static SpinLock<Vec<Entry>> {
    &TABLE[usize::try_from(h >> (u64::BITS - BUCKET_BITS)).unwrap_or(0)]
}

/// Vnodes and entries let go once no bucket lock is held.
#[derive(Default)]
struct Held {
    vnodes: Vec<Arc<Vnode>>,
    entries: Vec<Entry>,
}

/// Counts out an entry just taken from its bucket, whose lock the caller
/// holds, and drops its keys from its directory (with `dir`) and its
/// target (with `target`).
fn unlink(e: Entry, dir: bool, target: bool, held: &mut Held) {
    TOTAL.fetch_sub(1, Ordering::Relaxed);
    if e.target.is_none() {
        NEGATIVE.fetch_sub(1, Ordering::Relaxed);
    }
    if dir && let Some(d) = e.dir_vnode.upgrade() {
        d.cache_links.lock().src.remove(&e.name);
        held.vnodes.push(d);
    }
    if target && let Some(t) = e.target.as_ref().and_then(Weak::upgrade) {
        t.cache_links.lock().dst.remove(&(e.dir, e.name.clone()));
        held.vnodes.push(t);
    }
    held.entries.push(e);
}

/// What `dir` holds as `name`, if the cache knows.
#[must_use]
pub fn lookup(dir: &Vnode, name: &[u8]) -> Option<Hit> {
    let id = dir.id();
    let h = hash(id, name);
    let b = bucket(h).lock();
    let e = b.iter().find(|e| e.is(h, dir, id, name))?;
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
    let h = hash(id, name);
    let mut held = Held::default();
    {
        let mut b = bucket(h).lock();
        if let Some(i) = b.iter().position(|e| e.is(h, dir, id, name)) {
            // The directory keeps the name.
            unlink(b.remove(i), false, true, &mut held);
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
            unlink(b.remove(i), true, true, &mut held);
        } else if total >= MAX_ENTRIES && !b.is_empty() {
            unlink(b.remove(0), true, true, &mut held);
        }
        b.push(Entry {
            hash: h,
            dir: id,
            dir_vnode: Arc::downgrade(dir),
            name: name.into(),
            target: target.map(Arc::downgrade),
        });
        dir.cache_links.lock().src.insert(name.into());
        if let Some(t) = target {
            t.cache_links.lock().dst.insert((id, name.into()));
        }
        TOTAL.fetch_add(1, Ordering::Relaxed);
        if target.is_none() {
            NEGATIVE.fetch_add(1, Ordering::Relaxed);
        }
    }
    drop(held);
}

/// Forgets what `dir` holds as `name`.
pub fn remove(dir: &Vnode, name: &[u8]) {
    let id = dir.id();
    let h = hash(id, name);
    let mut held = Held::default();
    {
        let mut b = bucket(h).lock();
        if let Some(i) = b.iter().position(|e| e.is(h, dir, id, name)) {
            unlink(b.remove(i), true, true, &mut held);
        }
    }
    drop(held);
}

/// Forgets every entry naming `v` or in directory `v` (it is being
/// reclaimed): only those, found by its links.
pub fn purge(v: &Vnode) {
    let id = v.id();
    let Links { src, dst } = core::mem::replace(&mut *v.cache_links.lock(), Links::new());
    let mut held = Held::default();
    for name in src {
        let h = hash(id, &name);
        let mut b = bucket(h).lock();
        if let Some(i) = b.iter().position(|e| e.is(h, v, id, &name)) {
            unlink(b.remove(i), false, true, &mut held);
        }
    }
    for (dir, name) in dst {
        let h = hash(dir, &name);
        let mut b = bucket(h).lock();
        if let Some(i) = b
            .iter()
            .position(|e| e.hash == h && e.dir == dir && *e.name == *name && e.names(v))
        {
            unlink(b.remove(i), true, false, &mut held);
        }
    }
    drop(held);
    v.clear_named();
}

/// Forgets every entry of the file system with id `fs` (it is being
/// unmounted).
pub fn purge_fs(fs: u64) {
    for bucket in &TABLE {
        let mut held = Held::default();
        {
            let mut b = bucket.lock();
            let mut i = 0;
            while i < b.len() {
                let e = &b[i];
                let doomed = e.dir.fs == fs
                    || e.target
                        .as_ref()
                        .is_some_and(|t| t.upgrade().is_none_or(|t| t.mount().id == fs));
                if doomed {
                    unlink(b.remove(i), true, true, &mut held);
                } else {
                    i += 1;
                }
            }
        }
        drop(held);
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
