// SPDX-License-Identifier: BSD-2-Clause
//! inotify: watches on vnodes, and the queue of events an inotify
//! descriptor reads (FreeBSD 15's, which follows Linux's interface).
//!
//! A watch belongs to one descriptor ([`Inotify`]) and one vnode, which it
//! keeps in use (so a plain unmount is `EBUSY`). The vnode lists its
//! watches; the generic layer's operations report what they did through
//! [`event`], [`name_event`], and [`move_event`], each a single atomic load
//! when nothing watches. An event on a vnode reaches the watches on it,
//! and the watches on a watched directory that holds it: with the name in
//! hand for operations on names, and through the name the vnode was last
//! entered by otherwise (reads, writes, attributes), which is why a watch
//! on a directory marks its entries ([`PARENT`]) as FreeBSD's
//! `VIRF_INOTIFY_PARENT` does.
//!
//! The queue coalesces an event identical to the last one, and past
//! `vfs.inotify.max_queued_events` holds one overflow record instead. A
//! watch removed by its own event (`IN_ONESHOT`, `IN_DELETE_SELF`,
//! `IN_UNMOUNT`) cannot let its vnode go where the event came from, which
//! may hold that vnode's lock, so its use waits on the descriptor's list of
//! dead watches until the descriptor's next call ([`Inotify::reap`]).
//!
//! Behavior reference: sys/kern/vfs_inotify.c, sys/kern/vfs_cache.c
//! (`cache_vop_inotify`).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::inotify::{
    IN_ALL_EVENTS, IN_ATTRIB, IN_DELETE, IN_DELETE_SELF, IN_DIR_EVENTS, IN_IGNORED, IN_ISDIR,
    IN_MASK_ADD, IN_MASK_CREATE, IN_MOVE_SELF, IN_MOVED_FROM, IN_MOVED_TO, IN_ONESHOT,
    IN_Q_OVERFLOW, IN_UNMOUNT, InotifyEvent, name_size,
};
use sched::{Sleepable, WaitQueue};
use sync::PreemptLock;

use crate::namei::Namespace;
use crate::vnode::{Vnode, VnodeUse};

/// A vnode flag: watches are on it (`VIRF_INOTIFY`).
pub(crate) const WATCHED: u8 = 1;
/// A vnode flag: it may be in a watched directory
/// (`VIRF_INOTIFY_PARENT`).
pub(crate) const PARENT: u8 = 2;

/// Internal events: a rename's replaced target, and a change of link count
/// (`_IN_MOVE_DELETE`, `_IN_ATTRIB_LINKCOUNT`).
pub const MOVE_DELETE: u32 = 0x4000_0000;
pub const ATTRIB_LINKCOUNT: u32 = 0x8000_0000;

/// `vfs.inotify.max_queued_events`.
pub static MAX_QUEUED_EVENTS: AtomicI32 = AtomicI32::new(16384);
/// `vfs.inotify.max_user_instances`.
pub static MAX_USER_INSTANCES: AtomicI32 = AtomicI32::new(256);
/// `vfs.inotify.max_user_watches` (FreeBSD's is a third of
/// `kern.maxvnodes`; set by the kernel at boot).
pub static MAX_USER_WATCHES: AtomicI32 = AtomicI32::new(i32::MAX);
/// `vfs.inotify.max_watches` (half of `kern.maxvnodes` in FreeBSD).
pub static MAX_WATCHES: AtomicI32 = AtomicI32::new(i32::MAX);
/// `vfs.inotify.watches`: in use now.
pub static WATCHES: AtomicI32 = AtomicI32::new(0);
/// `vfs.inotify.coalesce`.
pub static COALESCE: AtomicI32 = AtomicI32::new(1);
/// `vfs.inotify.event_drops`.
pub static EVENT_DROPS: AtomicU64 = AtomicU64::new(0);

/// Pairs a rename's two halves (`inotify_rename_cookie`).
static RENAME_COOKIE: AtomicU32 = AtomicU32::new(0);
/// Descriptors' inode numbers, for `fstat`.
static INO: AtomicU64 = AtomicU64::new(1);

/// One queued event.
#[derive(Clone, PartialEq, Eq)]
struct Record {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
}

impl Record {
    fn new(wd: i32, mask: u32, cookie: u32, name: &[u8]) -> Self {
        Self {
            wd,
            mask,
            cookie,
            name: name.to_vec(),
        }
    }

    const fn overflow() -> Self {
        Self {
            wd: -1,
            mask: IN_Q_OVERFLOW,
            cookie: 0,
            name: Vec::new(),
        }
    }

    /// Its bytes as `read` returns them: the record, then the name padded
    /// with NULs to [`name_size`].
    fn len(&self) -> usize {
        size_of::<InotifyEvent>() + name_size(self.name.len())
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let room = name_size(self.name.len());
        let ev = InotifyEvent {
            wd: self.wd,
            mask: self.mask,
            cookie: self.cookie,
            len: u32::try_from(room).unwrap_or(u32::MAX),
        };
        for w in [ev.wd.cast_unsigned(), ev.mask, ev.cookie, ev.len] {
            out.extend_from_slice(&w.to_ne_bytes());
        }
        out.extend_from_slice(&self.name);
        out.resize(out.len() + (room - self.name.len()), 0);
    }
}

struct State {
    pending: VecDeque<Record>,
    /// The bytes `pending` reads as (`nbpending`).
    bytes: usize,
    watches: BTreeMap<i32, Arc<Watch>>,
    /// The next watch number to try: numbers are not reused for as long as
    /// possible.
    next: i32,
    /// Uses of vnodes whose watches went inside an event (see the module
    /// documentation).
    dead: Vec<VnodeUse>,
}

/// An inotify descriptor's state (`struct inotify_softc`).
pub struct Inotify {
    st: PreemptLock<State>,
    /// Readers waiting for an event.
    pub wait: WaitQueue,
    /// Pollers and knotes (`EVFILT_READ`).
    pub notify: event::Notify,
    ino: u64,
    /// Its creator's real ids, which `fstat` reports.
    uid: u32,
    gid: u32,
}

/// A watch: what one descriptor wants to hear of one vnode.
pub struct Watch {
    wd: i32,
    mask: AtomicU32,
    owner: Weak<Inotify>,
    vnode: VnodeUse,
}

impl core::fmt::Debug for Inotify {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Inotify({})", self.ino)
    }
}

impl core::fmt::Debug for Watch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Watch({} on {:?})", self.wd, self.vnode.vnode())
    }
}

/// What [`Inotify::stat`] reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    /// Bytes pending and records pending.
    pub bytes: u64,
    pub records: u64,
}

impl Inotify {
    #[must_use]
    pub fn new(uid: u32, gid: u32) -> Arc<Self> {
        Arc::new(Self {
            st: PreemptLock::new(State {
                pending: VecDeque::new(),
                bytes: 0,
                watches: BTreeMap::new(),
                // Watch numbers start at 1, as Linux's do.
                next: 1,
                dead: Vec::new(),
            }),
            wait: WaitQueue::new(),
            notify: event::Notify::new(),
            ino: INO.fetch_add(1, Ordering::Relaxed),
            uid,
            gid,
        })
    }

    /// The bytes a read would return now (`FIONREAD`, `kf_inotify`).
    pub fn pending_bytes(&self) -> usize {
        self.st.lock().bytes
    }

    /// The records pending.
    pub fn pending_records(&self) -> usize {
        self.st.lock().pending.len()
    }

    pub fn stat(&self) -> Stat {
        let st = self.st.lock();
        Stat {
            ino: self.ino,
            uid: self.uid,
            gid: self.gid,
            bytes: st.bytes as u64,
            records: st.pending.len() as u64,
        }
    }

    /// Takes as many whole records as fit in `room` bytes: `EAGAIN` with
    /// none pending (the caller waits on [`Inotify::wait`]), `EINVAL` if
    /// the first does not fit (`inotify_read`).
    pub fn read(&self, room: usize) -> Result<Vec<u8>, Errno> {
        self.reap();
        let mut st = self.st.lock();
        let first = st.pending.front().ok_or(Errno::EAGAIN)?;
        if first.len() > room {
            return Err(Errno::EINVAL);
        }
        let mut out = Vec::new();
        while let Some(r) = st.pending.front() {
            let len = r.len();
            if out.len() + len > room {
                break;
            }
            r.encode(&mut out);
            st.bytes -= len;
            st.pending.pop_front();
        }
        Ok(out)
    }

    /// Lets the vnodes of watches removed by events go.
    pub fn reap(&self) {
        let dead = core::mem::take(&mut self.st.lock().dead);
        drop(dead);
    }

    /// Queues `r` (`inotify_queue_record`): an identical last record
    /// takes it, and a full queue an overflow record instead. Returns
    /// whether anything was queued.
    fn queue(st: &mut State, mut r: Record) -> bool {
        let same = |st: &State, r: &Record| st.pending.back() == Some(r);
        if COALESCE.load(Ordering::Relaxed) != 0 && same(st, &r) {
            return false;
        }
        let max = usize::try_from(MAX_QUEUED_EVENTS.load(Ordering::Relaxed)).unwrap_or(0);
        if st.pending.len() >= max {
            EVENT_DROPS.fetch_add(1, Ordering::Relaxed);
            r = Record::overflow();
            if same(st, &r) {
                return false;
            }
        }
        st.bytes += r.len();
        st.pending.push_back(r);
        true
    }

    fn wakeup(&self) {
        self.wait.wake_all();
        self.notify.notify(0);
    }

    /// Adds a watch on `v` for `mask`, or changes this descriptor's watch
    /// on it (`vn_inotify_add_watch`); returns its number. The caller has
    /// checked `mask` and the caller's access.
    pub fn add_watch(
        self: &Arc<Self>,
        s: &Sleepable,
        v: &Arc<Vnode>,
        mask: u32,
    ) -> Result<i32, Errno> {
        self.reap();
        // A directory's entries are looked up now, so that events on them
        // find the watch through their names.
        if v.is_dir() {
            let mut names = Vec::new();
            v.readdir(s, 0, &mut |e| {
                if e.name != b"." && e.name != b".." {
                    names.push(e.name.to_vec());
                }
                true
            })?;
            for name in names {
                if let Ok(c) = Namespace::child(s, v, &name) {
                    c.set_inotify(PARENT);
                }
            }
        }
        let watches = v.inotify_watches();
        let mut list = watches.lock();
        if let Some(w) = list.iter().find(|w| ptr_eq(&w.owner, self)) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(Errno::EEXIST);
            }
            if mask & IN_MASK_ADD != 0 {
                w.mask.fetch_or(mask, Ordering::Relaxed);
            } else {
                w.mask.store(mask, Ordering::Relaxed);
            }
            return Ok(w.wd);
        }
        if WATCHES.fetch_add(1, Ordering::Relaxed) >= MAX_WATCHES.load(Ordering::Relaxed)
            || self.st.lock().watches.len()
                >= usize::try_from(MAX_USER_WATCHES.load(Ordering::Relaxed)).unwrap_or(0)
        {
            WATCHES.fetch_sub(1, Ordering::Relaxed);
            return Err(Errno::ENOSPC);
        }
        let mut st = self.st.lock();
        let wd = loop {
            let wd = st.next;
            st.next = st.next.checked_add(1).unwrap_or(1);
            if wd > 0 && !st.watches.contains_key(&wd) {
                break wd;
            }
        };
        let w = Arc::new(Watch {
            wd,
            mask: AtomicU32::new(mask),
            owner: Arc::downgrade(self),
            vnode: v.use_ref(),
        });
        st.watches.insert(wd, Arc::clone(&w));
        drop(st);
        list.push(w);
        drop(list);
        v.set_inotify(WATCHED);
        Ok(wd)
    }

    /// Removes watch `wd`, queueing `IN_IGNORED` for it
    /// (`kern_inotify_rm_watch`). Its pending events stay, as Linux's do.
    pub fn rm_watch(&self, wd: i32) -> Result<(), Errno> {
        self.reap();
        let w = {
            let mut st = self.st.lock();
            let w = st.watches.remove(&wd).ok_or(Errno::EINVAL)?;
            Self::queue(&mut st, Record::new(wd, IN_IGNORED, 0, &[]));
            w
        };
        self.wakeup();
        unlink(&w);
        Ok(())
    }

    /// The descriptor closed (`inotify_close`): every watch goes, and its
    /// events.
    pub fn close(&self) {
        let (watches, dead) = {
            let mut st = self.st.lock();
            st.pending.clear();
            st.bytes = 0;
            (
                core::mem::take(&mut st.watches),
                core::mem::take(&mut st.dead),
            )
        };
        for w in watches.into_values() {
            unlink(&w);
        }
        drop(dead);
    }

    /// Queues `mask` for watch `w` (`inotify_log_one`); an event that ends
    /// the watch removes it.
    fn log_one(&self, w: &Arc<Watch>, name: &[u8], mask: u32, cookie: u32) {
        let gone = {
            let mut st = self.st.lock();
            Self::queue(&mut st, Record::new(w.wd, mask, cookie, name));
            let ends = w.mask.load(Ordering::Relaxed) & IN_ONESHOT != 0
                || mask & (IN_DELETE_SELF | IN_UNMOUNT) != 0;
            // Only if still there: a close or an `rm_watch` may have taken
            // it meanwhile.
            if ends && st.watches.remove(&w.wd).is_some() {
                Self::queue(&mut st, Record::new(w.wd, IN_IGNORED, 0, &[]));
                true
            } else {
                false
            }
        };
        self.wakeup();
        if gone {
            let u = unlink_keep(w);
            self.st.lock().dead.extend(u);
        }
    }
}

fn ptr_eq(owner: &Weak<Inotify>, sc: &Arc<Inotify>) -> bool {
    core::ptr::eq(owner.as_ptr(), Arc::as_ptr(sc))
}

/// Takes `w` off its vnode's list; the vnode is no longer watched once its
/// last watch goes.
fn unlink(w: &Arc<Watch>) {
    drop(unlink_keep(w));
}

/// [`unlink`], handing back the watch's use of the vnode when this was the
/// list's reference to it, for the caller to drop where that is safe.
fn unlink_keep(w: &Arc<Watch>) -> Option<VnodeUse> {
    let v = w.vnode.vnode();
    let watches = v.inotify_watches();
    let mut list = watches.lock();
    let before = list.len();
    list.retain(|o| !Arc::ptr_eq(o, w));
    if list.is_empty() {
        v.clear_inotify(WATCHED);
    }
    let removed = list.len() != before;
    drop(list);
    WATCHES.fetch_sub(i32::from(removed), Ordering::Relaxed);
    removed.then(|| w.vnode.clone())
}

/// Queues `mask` (and `name`) for the watches on `v` that want it, every
/// watch for `IN_UNMOUNT` (`inotify_log`).
fn log(v: &Vnode, name: &[u8], mask: u32, cookie: u32) {
    let watches: Vec<Arc<Watch>> = v.inotify_watches().lock().clone();
    for w in watches {
        if (w.mask.load(Ordering::Relaxed) & mask != 0 || mask == IN_UNMOUNT)
            && let Some(sc) = w.owner.upgrade()
        {
            sc.log_one(&w, name, mask, cookie);
        }
    }
}

/// `mask` happened to `v` itself: a read, a write, an open or close, a
/// change of attributes (`INOTIFY`, then `cache_vop_inotify`, which finds
/// a watched directory holding it through its name).
pub fn event(v: &Vnode, mask: u32) {
    let flags = v.inotify_flags();
    if flags == 0 {
        return;
    }
    let isdir = if v.is_dir() { IN_ISDIR } else { 0 };
    if flags & WATCHED != 0 && (!v.is_dir() || mask & !IN_DIR_EVENTS != 0) {
        let own = if mask == ATTRIB_LINKCOUNT {
            IN_ATTRIB
        } else {
            mask
        };
        log(v, &[], own | isdir, 0);
    }
    if mask & IN_ALL_EVENTS == 0 || flags & PARENT == 0 {
        return;
    }
    match v.named() {
        Some((dir, name)) if dir.inotify_flags() & WATCHED != 0 => {
            log(&dir, &name, mask | isdir, 0);
        }
        // No watched directory holds it now: stop asking.
        _ => v.clear_inotify(PARENT),
    }
}

/// `mask` happened to `v` through its name `name` in `dir`: a create, a
/// remove, a link (`INOTIFY_NAME`, then `vn_inotify`). `s` reads the link
/// count a removal left.
pub fn name_event(s: &Sleepable, v: &Vnode, dir: &Vnode, name: &[u8], mask: u32) {
    if (v.inotify_flags() | dir.inotify_flags()) & WATCHED == 0 {
        return;
    }
    named(s, v, dir, name, mask, 0);
}

fn named(s: &Sleepable, v: &Vnode, dir: &Vnode, name: &[u8], mask: u32, cookie: u32) {
    let isdir = if v.is_dir() { IN_ISDIR } else { 0 };
    if v.inotify_flags() & WATCHED != 0 {
        let own = match mask {
            // Only the last link's removal deletes the file itself.
            MOVE_DELETE | IN_DELETE => {
                if !v.is_dir() && v.attr(s).is_ok_and(|a| a.nlink != 0) {
                    0
                } else {
                    IN_DELETE_SELF
                }
            }
            IN_MOVED_FROM => IN_MOVE_SELF,
            ATTRIB_LINKCOUNT => IN_ATTRIB,
            m => m,
        };
        if own & !IN_DIR_EVENTS != 0 {
            log(v, &[], own | isdir, 0);
        }
    }
    if mask & IN_ALL_EVENTS != 0 && dir.inotify_flags() & WATCHED != 0 {
        log(dir, name, mask | isdir, cookie);
    }
}

/// `v` was renamed from `fname` in `fdir` to `tname` in `tdir`, replacing
/// `replaced` (`INOTIFY_MOVE`): both halves share a cookie.
pub fn move_event(
    s: &Sleepable,
    v: &Vnode,
    (fdir, fname): (&Vnode, &[u8]),
    (tdir, tname): (&Vnode, &[u8]),
    replaced: Option<&Vnode>,
) {
    if (fdir.inotify_flags() | tdir.inotify_flags() | v.inotify_flags()) & WATCHED != 0 {
        let cookie = RENAME_COOKIE.fetch_add(1, Ordering::Relaxed);
        named(s, v, fdir, fname, IN_MOVED_FROM, cookie);
        named(s, v, tdir, tname, IN_MOVED_TO, cookie);
    }
    if let Some(t) = replaced {
        name_event(s, t, tdir, tname, MOVE_DELETE);
    }
}

/// `v` is going (a forced unmount, a revoke): its watches hear
/// `IN_UNMOUNT` and end (`vn_inotify_revoke`).
pub fn revoke(v: &Vnode) {
    if v.inotify_flags() & WATCHED != 0 {
        log(v, &[], IN_UNMOUNT, 0);
    }
}

/// Whether `dir` is watched: a file made in it is entered in the name
/// cache, so its events find the watch (`vn_open_cred`'s `MAKEENTRY`).
pub(crate) fn watched(dir: &Vnode) -> bool {
    dir.inotify_flags() & WATCHED != 0
}
