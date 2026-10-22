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

/// A bucket per two entries at [`MAX_ENTRIES`] (FreeBSD's `ncsizefactor`),
/// so a full cache's lists stay short.
const BUCKET_BITS: u32 = 16;
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
    name: Name,
    /// `None` for a negative entry.
    target: Option<Weak<Vnode>>,
}

impl Entry {
    fn is(&self, h: u64, dir: *const Vnode, id: NodeId, name: &[u8]) -> bool {
        self.hash == h
            && self.dir == id
            && self.name.get() == name
            && ptr::eq(self.dir_vnode.as_ptr(), dir)
    }

    fn names(&self, v: &Vnode) -> bool {
        self.target.as_ref().is_some_and(|t| ptr::eq(t.as_ptr(), v))
    }
}

/// Names this long or shorter are kept in the entry itself.
const INLINE_NAME: usize = 32;

/// An entry's name: a short one inline, so an entry is one allocation.
enum Name {
    Inline { len: u8, bytes: [u8; INLINE_NAME] },
    Heap(Box<[u8]>),
}

impl Name {
    fn new(name: &[u8]) -> Self {
        match u8::try_from(name.len()) {
            Ok(len) if name.len() <= INLINE_NAME => {
                let mut bytes = [0; INLINE_NAME];
                bytes[..name.len()].copy_from_slice(name);
                Self::Inline { len, bytes }
            }
            _ => Self::Heap(name.into()),
        }
    }

    fn get(&self) -> &[u8] {
        match self {
            Self::Inline { len, bytes } => &bytes[..usize::from(*len)],
            Self::Heap(b) => b,
        }
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

    /// Puts `entry` at the head, returning its key; the caller holds the
    /// lock.
    fn push(&self, _locked: &SpinGuard<'_, ()>, entry: Entry) -> Key {
        let hash = entry.hash;
        let node = Box::into_raw(Box::new(Node {
            entry,
            next: AtomicPtr::new(self.head.load(Ordering::Relaxed)),
        }));
        // Release: a reader that sees the node sees it whole.
        self.head.store(node, Ordering::Release);
        Key {
            hash,
            node: node.addr(),
        }
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

    fn key(&self) -> Key {
        Key {
            hash: self.entry().hash,
            node: self.0.as_ptr().addr(),
        }
    }
}

/// Nodes unlinked and waiting for a grace period.
static RETIRED: SpinLock<Vec<Retired>> = SpinLock::new(Vec::new());

/// Frees what [`RETIRED`] holds once no reader can still see it, when a
/// batch has gathered: each batch after a grace period. Thread context,
/// holding no spin lock.
fn retire() {
    let batch = {
        let mut r = RETIRED.lock();
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
    /// The entries in this directory.
    src: Keys,
    /// The entries naming this vnode.
    dst: Keys,
}

/// A set of keys: most vnodes have one name or none, and so most sets one
/// key or none, kept without allocating.
enum Keys {
    None,
    One(Key),
    More(BTreeSet<Key>),
}

impl Keys {
    fn insert(&mut self, k: Key) {
        match self {
            Self::None => *self = Self::One(k),
            Self::One(o) if *o == k => {}
            Self::One(o) => {
                let o = *o;
                *self = Self::More(BTreeSet::from([o, k]));
            }
            Self::More(set) => {
                set.insert(k);
            }
        }
    }

    fn remove(&mut self, k: &Key) {
        match self {
            Self::One(o) if o == k => *self = Self::None,
            Self::More(set) => {
                set.remove(k);
            }
            _ => {}
        }
    }
}

impl IntoIterator for Keys {
    type Item = Key;
    type IntoIter = core::iter::Chain<
        core::option::IntoIter<Key>,
        core::iter::Flatten<core::option::IntoIter<BTreeSet<Key>>>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        let (one, more) = match self {
            Self::None => (None, None),
            Self::One(k) => (Some(k), None),
            Self::More(set) => (None, Some(set)),
        };
        one.into_iter().chain(more.into_iter().flatten())
    }
}

/// An entry, in its vnodes' [`Links`]: its hash, for its bucket, and its
/// node's address. An address may be reused once the node is freed, so a
/// purge also checks that the node it finds is the vnode's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    hash: u64,
    node: usize,
}

impl Links {
    pub(crate) const fn new() -> Self {
        Self {
            src: Keys::None,
            dst: Keys::None,
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
    hash_name(mix(mix(0, dir.fs), dir.ino), name)
}

fn mix(h: u64, w: u64) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    (h.rotate_left(5) ^ w).wrapping_mul(K)
}

/// [`hash`]'s name part, from `h`: also tmpfs's directory index.
pub(crate) fn hash_name(mut h: u64, name: &[u8]) -> u64 {
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

/// Vnodes let go, and retired nodes freed, once no bucket lock is held;
/// the usual one or two vnodes without allocating.
#[derive(Default)]
struct Held {
    vnodes: [Option<Arc<Vnode>>; 2],
    more: Vec<Arc<Vnode>>,
    retired: bool,
}

impl Held {
    fn keep(&mut self, v: Arc<Vnode>) {
        match self.vnodes.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => *slot = Some(v),
            None => self.more.push(v),
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.retired {
            retire();
        }
    }
}

/// Counts out a node just unlinked from its bucket, whose lock the caller
/// holds, and drops its keys from its directory (with `dir`) and its
/// target (with `target`).
fn forget(r: Retired, dir: bool, target: bool, held: &mut Held) {
    let e = r.entry();
    let key = r.key();
    TOTAL.fetch_sub(1, Ordering::Relaxed);
    if e.target.is_none() {
        NEGATIVE.fetch_sub(1, Ordering::Relaxed);
    }
    if dir && let Some(d) = e.dir_vnode.upgrade() {
        d.cache_links.lock().src.remove(&key);
        held.keep(d);
    }
    if target && let Some(t) = e.target.as_ref().and_then(Weak::upgrade) {
        t.cache_links.lock().dst.remove(&key);
        held.keep(t);
    }
    // Under the bucket lock (then this one): freed by `held` after.
    RETIRED.lock().push(r);
    held.retired = true;
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
            forget(old, true, true, &mut held);
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
        let key = b.push(
            &locked,
            Entry {
                hash: h,
                dir: id,
                dir_vnode: Arc::downgrade(dir),
                name: Name::new(name),
                target: target.map(Arc::downgrade),
            },
        );
        dir.cache_links.lock().src.insert(key);
        if let Some(t) = target {
            t.cache_links.lock().dst.insert(key);
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
    let Links { src, dst } = core::mem::replace(&mut *v.cache_links.lock(), Links::new());
    let mut held = Held::default();
    let at = |n: &Node, k: Key| ptr::from_ref(n).addr() == k.node;
    for k in src {
        let b = bucket(k.hash);
        let locked = b.lock.lock();
        if let Some(old) = b.unlink_first(&locked, |n| {
            at(n, k) && ptr::eq(n.entry.dir_vnode.as_ptr(), v)
        }) {
            forget(old, false, true, &mut held);
        }
    }
    for k in dst {
        let b = bucket(k.hash);
        let locked = b.lock.lock();
        if let Some(old) = b.unlink_first(&locked, |n| at(n, k) && n.entry.names(v)) {
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
