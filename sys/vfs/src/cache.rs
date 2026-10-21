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
//! Each bucket keeps its entries newest first; past the total limit an
//! insert evicts its bucket's oldest, and negative entries are held to a
//! fifth of the cache (FreeBSD's `ncnegfactor`). Each vnode keeps the keys
//! of its entries, those in it as a directory and those naming it, so
//! purging one costs only its own entries.
//!
//! Lookups take no lock (docs/VFS.md §6.2): a bucket is a list that
//! readers walk inside a section of the cache's epoch, while writers
//! change it under the bucket's lock. An entry taken out is freed only
//! after every reader that might still see it has left its section, so a
//! reader may follow an entry, and the weak reference in it, for as long
//! as its section lasts ([`Reader`]); that is what lets a whole path be
//! looked up with no lock and one reference, on the vnode found.
//!
//! Behavior reference: sys/kern/vfs_cache.c.

#![allow(unsafe_code)]

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use sched::lock::{Epoch, EpochGuard};
use sync::{SpinGuard, SpinLock};

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

/// Entries taken out wait for a grace period in batches of this many.
const RETIRE_BATCH: usize = 256;

/// Readers' sections, and the grace periods freeing waits for.
static EPOCH: Epoch = Epoch::new();

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
    fn is(&self, h: u64, dir: *const Vnode, id: NodeId, name: &[u8]) -> bool {
        self.hash == h
            && self.dir == id
            && *self.name == *name
            && ptr::eq(self.dir_vnode.as_ptr(), dir)
    }

    fn names(&self, v: &Vnode) -> bool {
        self.target.as_ref().is_some_and(|t| ptr::eq(t.as_ptr(), v))
    }
}

/// An entry in a bucket's list.
struct Node {
    entry: Entry,
    next: AtomicPtr<Node>,
}

/// A bucket: a list, newest first, changed under `lock` and read under
/// it or in an epoch section.
struct Bucket {
    lock: SpinLock<()>,
    head: AtomicPtr<Node>,
}

impl Bucket {
    const fn new() -> Self {
        Self {
            lock: SpinLock::new(()),
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// The nodes, newest first.
    ///
    /// # Safety
    ///
    /// The caller holds the bucket's lock or is in an epoch section, for as
    /// long as it uses what it gets: no node it reaches is freed meanwhile.
    unsafe fn nodes(&self) -> impl Iterator<Item = &Node> {
        let mut p = self.head.load(Ordering::Acquire);
        core::iter::from_fn(move || {
            // SAFETY: a node reachable from the head is live until a grace
            // period after it is unlinked (or, under the lock, until this
            // holder unlinks it): the caller's contract.
            let n = unsafe { p.as_ref()? };
            p = n.next.load(Ordering::Acquire);
            Some(n)
        })
    }

    /// Unlinks the first node `f` picks; the caller holds the lock.
    fn unlink_first(
        &self,
        _locked: &SpinGuard<'_, ()>,
        f: impl Fn(&Node) -> bool,
    ) -> Option<Retired> {
        let mut link = &self.head;
        loop {
            let p = link.load(Ordering::Relaxed);
            // SAFETY: under the bucket's lock, so nothing unlinks or frees
            // the nodes meanwhile.
            let n = unsafe { p.as_ref()? };
            if f(n) {
                // A reader in the list sees the node or its successor; the
                // node stays until a grace period has passed.
                link.store(n.next.load(Ordering::Relaxed), Ordering::Release);
                return NonNull::new(p).map(Retired);
            }
            link = &n.next;
        }
    }

    /// Unlinks the last (oldest) node whose entry `f` picks; the caller
    /// holds the lock.
    fn unlink_oldest(
        &self,
        locked: &SpinGuard<'_, ()>,
        f: impl Fn(&Entry) -> bool,
    ) -> Option<Retired> {
        // SAFETY: under the lock.
        let last = unsafe { self.nodes() }
            .filter(|n| f(&n.entry))
            .last()
            .map(ptr::from_ref)?;
        self.unlink_first(locked, |n| ptr::eq(n, last))
    }

    /// Puts `entry` at the head; the caller holds the lock.
    fn push(&self, _locked: &SpinGuard<'_, ()>, entry: Entry) {
        let node = Box::new(Node {
            entry,
            next: AtomicPtr::new(self.head.load(Ordering::Relaxed)),
        });
        // Release: a reader that sees the node sees it whole.
        self.head.store(Box::into_raw(node), Ordering::Release);
    }
}

/// An unlinked node, freed after a grace period.
struct Retired(NonNull<Node>);

// SAFETY: a retired node belongs to its retirer alone, and holds only
// `Send` data.
unsafe impl Send for Retired {}

impl Retired {
    fn entry(&self) -> &Entry {
        // SAFETY: unlinked but not freed: only `free_after_grace` frees it.
        unsafe { &self.0.as_ref().entry }
    }
}

/// Nodes unlinked and waiting for a grace period.
static RETIRED: SpinLock<Vec<Retired>> = SpinLock::new(Vec::new());

/// Frees `nodes` once no reader can still see them: in batches, each after
/// a grace period. Thread context, holding no spin lock.
fn retire(nodes: Vec<Retired>) {
    if nodes.is_empty() {
        return;
    }
    let batch = {
        let mut r = RETIRED.lock();
        r.extend(nodes);
        if r.len() < RETIRE_BATCH {
            return;
        }
        core::mem::take(&mut *r)
    };
    free_after_grace(batch);
}

fn free_after_grace(batch: Vec<Retired>) {
    EPOCH.synchronize(core::hint::spin_loop);
    for r in batch {
        // SAFETY: unlinked, and every section that might have seen it has
        // ended; made by `Box::into_raw` in `push`.
        drop(unsafe { Box::from_raw(r.0.as_ptr()) });
    }
}

/// Waits until no lockless reader can still see what was withdrawn before
/// (a mount's root, before the root goes), and frees what waited for that.
/// Thread context, holding no spin lock.
pub fn synchronize() {
    let batch = core::mem::take(&mut *RETIRED.lock());
    free_after_grace(batch);
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

static TABLE: [Bucket; BUCKETS] = [const { Bucket::new() }; BUCKETS];
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
fn bucket(h: u64) -> &'static Bucket {
    &TABLE[usize::try_from(h >> (u64::BITS - BUCKET_BITS)).unwrap_or(0)]
}

/// Vnodes and nodes let go once no bucket lock is held.
#[derive(Default)]
struct Held {
    vnodes: Vec<Arc<Vnode>>,
    retired: Vec<Retired>,
}

impl Drop for Held {
    fn drop(&mut self) {
        retire(core::mem::take(&mut self.retired));
    }
}

/// Counts out a node just unlinked from its bucket, whose lock the caller
/// holds, and drops its keys from its directory (with `dir`) and its
/// target (with `target`).
fn forget(r: Retired, dir: bool, target: bool, held: &mut Held) {
    let e = r.entry();
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
    held.retired.push(r);
}

/// A section in which lookups take no lock and what they find stays: for
/// looking a whole path up ([`crate::namei`]). Preemption is off while it
/// lasts, so nothing in it may sleep.
pub(crate) struct Reader {
    _section: EpochGuard<'static>,
}

/// What a [`Reader`] found for a name.
pub(crate) enum Peek<'a> {
    /// The vnode, weakly: its allocation stays for the section.
    Positive(&'a Weak<Vnode>),
    Negative,
}

impl Reader {
    pub(crate) fn new() -> Self {
        Self {
            _section: EPOCH.enter(),
        }
    }

    /// What directory `dir`, whose identity is `id`, holds as `name`.
    pub(crate) fn find(&self, dir: *const Vnode, id: NodeId, name: &[u8]) -> Option<Peek<'_>> {
        let h = hash(id, name);
        // SAFETY: in an epoch section (`self`), which what is returned
        // borrows.
        let n = unsafe { bucket(h).nodes() }.find(|n| n.entry.is(h, dir, id, name))?;
        Some(match &n.entry.target {
            Some(w) => Peek::Positive(w),
            None => Peek::Negative,
        })
    }
}

/// What `dir` holds as `name`, if the cache knows.
#[must_use]
pub fn lookup(dir: &Vnode, name: &[u8]) -> Option<Hit> {
    let r = Reader::new();
    match r.find(dir, dir.id(), name)? {
        // Gone or going (reclaimed meanwhile): a miss.
        Peek::Positive(v) => v.upgrade().filter(|v| !v.is_dying()).map(Hit::Positive),
        Peek::Negative => Some(Hit::Negative),
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
    let dp = Arc::as_ptr(dir);
    let mut held = Held::default();
    {
        let b = bucket(h);
        let locked = b.lock.lock();
        if let Some(old) = b.unlink_first(&locked, |n| n.entry.is(h, dp, id, name)) {
            // The directory keeps the name.
            forget(old, false, true, &mut held);
        }
        // Over a limit: this bucket's oldest goes (a negative one first if
        // negatives are what is over).
        let total = TOTAL.load(Ordering::Relaxed);
        let negatives = NEGATIVE.load(Ordering::Relaxed);
        let neg_limit = (total / NEGATIVE_FACTOR).max(MIN_NEGATIVE);
        if target.is_none() && negatives >= neg_limit {
            if let Some(old) = b.unlink_oldest(&locked, |e| e.target.is_none()) {
                forget(old, true, true, &mut held);
            }
        } else if total >= MAX_ENTRIES
            && let Some(old) = b.unlink_oldest(&locked, |_| true)
        {
            forget(old, true, true, &mut held);
        }
        b.push(
            &locked,
            Entry {
                hash: h,
                dir: id,
                dir_vnode: Arc::downgrade(dir),
                name: name.into(),
                target: target.map(Arc::downgrade),
            },
        );
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
        let b = bucket(h);
        let locked = b.lock.lock();
        if let Some(old) = b.unlink_first(&locked, |n| n.entry.is(h, dir, id, name)) {
            forget(old, true, true, &mut held);
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
        let b = bucket(h);
        let locked = b.lock.lock();
        if let Some(old) = b.unlink_first(&locked, |n| n.entry.is(h, v, id, &name)) {
            forget(old, false, true, &mut held);
        }
    }
    for (dir, name) in dst {
        let h = hash(dir, &name);
        let b = bucket(h);
        let locked = b.lock.lock();
        if let Some(old) = b.unlink_first(&locked, |n| {
            let e = &n.entry;
            e.hash == h && e.dir == dir && *e.name == *name && e.names(v)
        }) {
            forget(old, true, false, &mut held);
        }
    }
    drop(held);
    v.clear_named();
}

/// Forgets every entry of the file system with id `fs` (it is being
/// unmounted).
pub fn purge_fs(fs: u64) {
    let doomed = |n: &Node| {
        let e = &n.entry;
        e.dir.fs == fs
            || e.target
                .as_ref()
                .is_some_and(|t| t.upgrade().is_none_or(|t| t.mount().id == fs))
    };
    for b in &TABLE {
        let mut held = Held::default();
        {
            let locked = b.lock.lock();
            while let Some(old) = b.unlink_first(&locked, doomed) {
                forget(old, true, true, &mut held);
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
