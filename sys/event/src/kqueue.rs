// SPDX-License-Identifier: BSD-2-Clause
//! Kqueues and knotes.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering, fence};

use abi::errno::Errno;
use abi::event::{
    EV_ADD, EV_CLEAR, EV_DELETE, EV_DISABLE, EV_DISPATCH, EV_DROP, EV_ENABLE, EV_FLAG1, EV_FLAG2,
    EV_FORCEONESHOT, EV_KEEPUDATA, EV_ONESHOT, EVFILT_PROC, Kevent,
};
use sync::{SpinGuard, SpinLock};

use crate::notify::{Notify, Wake};

/// A knote's `kn_status` bits, kept under its kqueue's lock.
mod status {
    /// The event happened and has not been reported (`KN_ACTIVE`).
    pub(super) const ACTIVE: u16 = 0x01;
    /// In the kqueue's queue (`KN_QUEUED`).
    pub(super) const QUEUED: u16 = 0x02;
    /// Not reported until enabled (`KN_DISABLED`).
    pub(super) const DISABLED: u16 = 0x08;
    /// Not on its object's list (`KN_DETACHED`).
    pub(super) const DETACHED: u16 = 0x10;
    /// Queued by a scan that reported it and will ask its filter again
    /// (a level-triggered event of a [`super::Filter::level`] filter): a
    /// notification changes nothing and takes no lock.
    pub(super) const RESCAN: u16 = 0x20;
    /// Gone from its kqueue: whoever still holds it ignores it.
    pub(super) const DROPPED: u16 = 0x8000;
}

/// What a knote reports (FreeBSD's `kn_kevent`), and what it was asked
/// for (`kn_sfflags`, `kn_sdata`). Its filter keeps `kev.data`,
/// `kev.fflags`, and `EV_EOF` in `kev.flags` up to date.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Note {
    pub kev: Kevent,
    pub sfflags: u32,
    pub sdata: i64,
    /// For the filter (`kn_hookid`): `EVFILT_USER`'s trigger.
    pub hook: u64,
}

/// The object-specific half of a knote (FreeBSD's `filterops` and the
/// object it reaches through `kn_hook` or `kn_fp`).
pub trait Filter: Send + Sync {
    /// Attaches the new knote to its object (`f_attach`), typically
    /// [`Notify::add`]. On error the knote is discarded.
    ///
    /// # Errors
    ///
    /// What `kevent` reports for this change.
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        let _ = kn;
        Ok(())
    }

    /// Detaches the knote from its object (`f_detach`).
    fn detach(&self, kn: &Knote) {
        let _ = kn;
    }

    /// Whether the event is active (`f_event`), updating `note` from the
    /// object's state. `hint` is the object's notification, 0 when a scan
    /// or a registration asks. Called with the knote's kqueue locked and,
    /// from a notification, the object's [`Notify`]: it must not sleep,
    /// must not touch that `Notify`, and must not ask the knote for its
    /// note or change its kqueue.
    fn event(&self, kn: &Knote, note: &mut Note, hint: u64) -> bool;

    /// Whether [`Filter::event`] answers from the object's state as it
    /// is, and from nothing a notification carries or counts (a pipe's
    /// bytes or room; not a signal's count or a vnode's `NOTE_` bits). A
    /// notification then passes over such a knote while it is active and
    /// queued: the scan that reports it asks the filter again.
    fn level(&self) -> bool {
        false
    }

    /// Whether the filter has `f_touch`: its [`Filter::register`] and
    /// [`Filter::process`] replace the generic handling, and `EV_CLEAR`
    /// is left to them.
    fn touches(&self) -> bool {
        false
    }

    /// A change to an existing knote (`f_touch(EVENT_REGISTER)`), under
    /// the same rules as [`Filter::event`]. Returns whether the knote
    /// starts over: off the queue and inactive, as a timer re-added with
    /// new parameters.
    fn register(&self, kn: &Knote, note: &mut Note, kev: &Kevent) -> bool {
        let _ = kn;
        note.sfflags = kev.fflags;
        note.sdata = kev.data;
        false
    }

    /// The event to report (`f_touch(EVENT_PROCESS)`).
    fn process(&self, kn: &Knote, note: &mut Note) -> Kevent {
        let _ = kn;
        note.kev
    }

    /// The filter for a copy of the knote in a forked kqueue (`f_copy`),
    /// or `None` if it is not copied.
    fn fork(&self, kn: &Knote) -> Option<Box<dyn Filter>> {
        let _ = kn;
        None
    }
}

/// How a filter names what it watches (`f_isfd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ident {
    /// A descriptor: the knote goes when it is closed.
    Fd,
    /// Anything else (a pid, a signal, a timer id, a user event).
    Other,
}

/// What the kernel supplies to register an event.
pub trait Source {
    /// How `kev.filter` names things.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a filter that does not exist; `EBADF` if `kev.ident`
    /// should be, and is not, an open descriptor.
    fn ident(&self, kev: &Kevent) -> Result<Ident, Errno>;

    /// The filter for a new knote for `kev` (the descriptor's
    /// `fo_kqfilter`, or the filter's own attach checks).
    ///
    /// # Errors
    ///
    /// What `kevent` reports for this change.
    fn filter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno>;
}

/// A knote's [`Note`], under its kqueue's lock ([`Knote::note_mut`]).
struct NoteCell(UnsafeCell<Note>);

// SAFETY: the note is reached only through `Knote::note_mut`, which
// takes the exclusive borrow of the held lock of the knote's own kqueue:
// one thread at a time.
unsafe impl Sync for NoteCell {}

/// One registered event (FreeBSD's `struct knote`).
pub struct Knote {
    /// First, so it is dropped first: a filter's own callbacks may use
    /// the knote until the filter has stopped them.
    filt: Box<dyn Filter>,
    /// Its kqueue, which holds it in turn until it is dropped from it
    /// (`knote_drop`, or the kqueue's close, which drops them all).
    kq: Arc<Kqueue>,
    pub ident: u64,
    pub filter: i16,
    fd: bool,
    /// `status` bits; changed only under the kqueue's lock.
    status: AtomicU16,
    /// The latest scan begun when it was last queued: scans begun since
    /// see it, earlier ones stop at it (FreeBSD's scan marker).
    pass: AtomicU64,
    /// [`Filter::level`], asked once.
    level: bool,
    /// What it reports, under the kqueue's lock as its status is: a
    /// notification runs the filter and queues the knote under that one
    /// lock (`knote` under `KQ_LOCK`), and a scan holds it from one
    /// knote to the next.
    note: NoteCell,
}

impl core::fmt::Debug for Knote {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Knote")
            .field("ident", &self.ident)
            .field("filter", &self.filter)
            .field("status", &self.status.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// A kqueue's lock, held.
struct Held<'a> {
    kq: &'a Kqueue,
    st: SpinGuard<'a, State>,
}

impl Knote {
    fn status(&self) -> u16 {
        self.status.load(Ordering::Relaxed)
    }

    /// Sets and clears status bits. The kqueue's lock is held.
    fn set(&self, on: u16, off: u16) {
        let s = self.status();
        self.status.store((s | on) & !off, Ordering::Relaxed);
    }

    /// The note, with the knote's kqueue locked.
    ///
    /// # Panics
    ///
    /// If `h` is another kqueue's lock.
    fn note_mut<'a>(&'a self, h: &'a mut Held<'_>) -> &'a mut Note {
        assert!(
            core::ptr::eq(Arc::as_ptr(&self.kq), h.kq),
            "a knote's note under another kqueue's lock"
        );
        // SAFETY: `h` is the held lock of this knote's kqueue (just
        // checked), borrowed exclusively for as long as the reference
        // lives, and every access to the note comes through here: no
        // other reference to it exists meanwhile.
        unsafe { &mut *self.note.0.get() }
    }

    /// The kqueue it belongs to.
    pub fn kqueue(&self) -> Option<Arc<Kqueue>> {
        Some(Arc::clone(&self.kq))
    }

    /// The registered (ident, filter, flags, udata, ...), as it stands.
    /// Not for a filter's [`Filter::event`], which is handed the note.
    pub fn note(&self) -> Note {
        let mut h = self.kq.hold();
        *self.note_mut(&mut h)
    }

    /// Runs `f` on the note and, if it says the event happened, activates
    /// the knote, as a callout or other source without a [`Notify`] does
    /// (FreeBSD's `KNOTE_ACTIVATE` from `filt_timerexpire`). `f` runs
    /// under the kqueue's lock, as [`Filter::event`] does.
    pub fn trigger(&self, f: impl FnOnce(&mut Note) -> bool) {
        let kq = &self.kq;
        let mut h = kq.hold();
        let queued = f(self.note_mut(&mut h)) && kq.activate_locked(&mut h.st, self);
        if kq.release(h, queued) {
            kq.sel.notify(0);
        }
    }

    /// Drops it from its kqueue (`knote_drop`), as [`crate::Notify::delete`]
    /// does for an object that is freed.
    pub fn delete(&self) {
        self.kq.drop_knote(self);
    }

    /// A notification, the object's list locked: runs the filter and
    /// queues the knote under its kqueue's lock; returns the kqueue if it
    /// newly has an event and is watched.
    pub(crate) fn deliver(&self, hint: u64) -> Option<Arc<Kqueue>> {
        // A scan will ask the filter again, and the filter answers from
        // the object alone: nothing to do. The scan that finds the event
        // gone clears this and then asks once more, so a notification
        // that came in between is not lost (`Kqueue::collect`).
        if self.status() & (status::RESCAN | status::DROPPED) == status::RESCAN {
            return None;
        }
        let kq = &self.kq;
        let mut h = kq.hold();
        if !self.filt.event(self, self.note_mut(&mut h), hint) {
            return None;
        }
        let queued = kq.activate_locked(&mut h.st, self);
        kq.release(h, queued).then(|| Arc::clone(kq))
    }

    /// Off its object's list (`knlist_clear`): `flags` join its own.
    pub(crate) fn detached(&self, flags: u16) {
        let mut h = self.kq.hold();
        self.note_mut(&mut h).kev.flags |= flags;
        self.set(status::DETACHED, 0);
    }
}

struct State {
    /// Active knotes to report, in order (`kq_head`).
    queue: VecDeque<Arc<Knote>>,
    /// Descriptor knotes by descriptor (`kq_knlist`).
    fds: Vec<Vec<Arc<Knote>>>,
    /// The rest by ident (`kq_knhash`).
    other: BTreeMap<u64, Vec<Arc<Knote>>>,
    /// Being closed (`KQ_CLOSING`): no new knotes.
    closing: bool,
    /// Scans so far, to number the next.
    passes: u64,
    /// A scan found the queue empty and is going to sleep (`KQ_SLEEP`):
    /// whoever queues a knote next wakes the scanners.
    sleeping: bool,
}

/// A kqueue (FreeBSD's `struct kqueue`). Its knotes hold it, and it
/// them: [`Kqueue::close`] lets both go.
pub struct Kqueue {
    /// `KQ_LOCK`: the queue, the index, and every knote's status and
    /// note.
    st: SpinLock<State>,
    /// Knotes queued (`kq_count`), for polling and `EVFILT_READ` on it.
    count: AtomicUsize,
    /// The threads scanning it, waiting for an event.
    wake: Box<dyn Wake>,
    /// Pollers of this kqueue, and knotes of kqueues watching it
    /// (`kq_sel`).
    pub sel: Notify,
}

impl core::fmt::Debug for Kqueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Kqueue")
            .field("count", &self.count.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// The knotes' ident lists, by kind.
fn list<'a>(st: &'a mut State, kn: &Knote) -> Option<&'a mut Vec<Arc<Knote>>> {
    if kn.fd {
        st.fds.get_mut(usize::try_from(kn.ident).ok()?)
    } else {
        st.other.get_mut(&kn.ident)
    }
}

impl Kqueue {
    /// An empty kqueue; `wake` wakes the threads waiting in its scans.
    pub fn new(wake: Box<dyn Wake>) -> Arc<Self> {
        Arc::new(Self {
            st: SpinLock::new(State {
                queue: VecDeque::new(),
                fds: Vec::new(),
                other: BTreeMap::new(),
                closing: false,
                passes: 0,
                sleeping: false,
            }),
            count: AtomicUsize::new(0),
            wake,
            sel: Notify::new(),
        })
    }

    /// Knotes queued (`kq_count`).
    pub fn pending(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    fn hold(&self) -> Held<'_> {
        Held {
            kq: self,
            st: self.st.lock(),
        }
    }

    /// Unlocks; `queued` says a knote was queued meanwhile. Wakes the
    /// scanners if one sleeps, and returns whether the kqueue's own
    /// watchers are to be told (by the caller, with nothing locked: a
    /// kqueue watching this one may be one this one watches).
    fn release(&self, mut h: Held<'_>, queued: bool) -> bool {
        let sleepers = queued && core::mem::take(&mut h.st.sleeping);
        drop(h);
        if sleepers {
            self.wake.wake();
        }
        queued && self.sel.is_armed()
    }

    /// `KNOTE_ACTIVATE`: active, and queued unless disabled or already
    /// there. Returns whether it was just queued.
    fn activate_locked(&self, st: &mut State, kn: &Knote) -> bool {
        let s = kn.status();
        if s & status::DROPPED != 0 {
            return false;
        }
        kn.set(status::ACTIVE, 0);
        if s & (status::QUEUED | status::DISABLED) == 0 {
            self.enqueue(st, kn);
            return true;
        }
        false
    }

    fn enqueue(&self, st: &mut State, kn: &Knote) {
        let Some(arc) = Self::find_arc(st, kn) else {
            return;
        };
        self.requeue(st, arc, 0);
    }

    /// Queues `kn`, behind the marker of every scan under way, with
    /// `also` set.
    fn requeue(&self, st: &mut State, kn: Arc<Knote>, also: u16) {
        kn.pass.store(st.passes, Ordering::Relaxed);
        kn.set(status::QUEUED | also, 0);
        st.queue.push_back(kn);
        self.count.store(st.queue.len(), Ordering::Release);
    }

    fn unqueue(&self, st: &mut State, kn: &Knote) {
        if let Some(at) = st.queue.iter().position(|k| core::ptr::eq(&**k, kn)) {
            st.queue.remove(at);
            self.count.store(st.queue.len(), Ordering::Release);
        }
        kn.set(0, status::QUEUED | status::RESCAN);
    }

    /// The `Arc` the kqueue's index holds for `kn`.
    fn find_arc(st: &mut State, kn: &Knote) -> Option<Arc<Knote>> {
        list(st, kn)?
            .iter()
            .find(|k| core::ptr::eq(&***k, kn))
            .cloned()
    }

    fn lookup(&self, kev: &Kevent, fd: bool) -> Option<Arc<Knote>> {
        let mut st = self.st.lock();
        let l = if fd {
            st.fds.get_mut(usize::try_from(kev.ident).ok()?)?
        } else {
            st.other.get_mut(&kev.ident)?
        };
        l.iter()
            .find(|k| k.filter == kev.filter && (fd || k.ident == kev.ident))
            .cloned()
    }

    /// Applies one change of a `kevent` changelist (`kqueue_register`).
    /// `kev.flags` has had `EV_SYSFLAGS` cleared.
    ///
    /// # Errors
    ///
    /// `EINVAL` for contradictory flags or an unknown filter, `EBADF` for
    /// a bad descriptor (or a kqueue being closed), `ENOENT` for a change
    /// to an event not registered, or the filter's error.
    pub fn register(self: &Arc<Self>, kev: &Kevent, src: &dyn Source) -> Result<(), Errno> {
        let flags = kev.flags;
        if flags & (EV_ENABLE | EV_DISABLE) == EV_ENABLE | EV_DISABLE {
            return Err(Errno::EINVAL);
        }
        let fd = src.ident(kev)? == Ident::Fd;
        if flags & EV_ADD != 0 && flags & EV_KEEPUDATA != 0 {
            return Err(Errno::EINVAL);
        }
        // An internal tracking note for a forked child is never merged.
        let existing = if kev.filter == EVFILT_PROC && flags & (EV_FLAG1 | EV_FLAG2) != 0 {
            None
        } else {
            self.lookup(kev, fd)
        };
        let kn = match existing {
            None => {
                if flags & EV_ADD == 0 {
                    return Err(Errno::ENOENT);
                }
                let kn = self.add(kev, fd, src)?;
                let mut h = self.hold();
                let queued = self.settle(&mut h, &kn);
                if self.release(h, queued) {
                    self.sel.notify(0);
                }
                return Ok(());
            }
            Some(kn) => kn,
        };
        if flags & EV_DELETE != 0 {
            self.drop_knote(&kn);
            return Ok(());
        }
        let mut h = self.hold();
        let mut queued = false;
        if flags & EV_FORCEONESHOT != 0 {
            kn.note_mut(&mut h).kev.flags |= EV_ONESHOT;
            queued = self.activate_locked(&mut h.st, &kn);
        }
        if flags & EV_ENABLE != 0 {
            kn.set(0, status::DISABLED);
        } else if flags & EV_DISABLE != 0 {
            kn.set(status::DISABLED, 0);
        }
        let restart = {
            let note = kn.note_mut(&mut h);
            if flags & EV_KEEPUDATA == 0 {
                note.kev.udata = kev.udata;
            }
            if !fd && kn.filt.touches() {
                kn.filt.register(&kn, note, kev)
            } else {
                note.sfflags = kev.fflags;
                note.sdata = kev.data;
                false
            }
        };
        if restart {
            if kn.status() & status::QUEUED != 0 {
                self.unqueue(&mut h.st, &kn);
            }
            kn.set(0, status::ACTIVE);
        }
        queued |= self.settle(&mut h, &kn);
        if self.release(h, queued) {
            self.sel.notify(0);
        }
        Ok(())
    }

    /// A new knote for `kev`, attached.
    fn add(
        self: &Arc<Self>,
        kev: &Kevent,
        fd: bool,
        src: &dyn Source,
    ) -> Result<Arc<Knote>, Errno> {
        let filt = src.filter(kev)?;
        let mut k = *kev;
        k.fflags = 0;
        k.data = 0;
        k.flags &= !(EV_ADD | EV_DELETE | EV_ENABLE | EV_DISABLE | EV_FORCEONESHOT);
        let mut s = status::DETACHED;
        if kev.flags & EV_DISABLE != 0 {
            s |= status::DISABLED;
        }
        let kn = Arc::new(Knote {
            kq: Arc::clone(self),
            ident: kev.ident,
            filter: kev.filter,
            fd,
            status: AtomicU16::new(s),
            pass: AtomicU64::new(0),
            level: filt.level(),
            note: NoteCell(UnsafeCell::new(Note {
                kev: k,
                sfflags: kev.fflags,
                sdata: kev.data,
                hook: 0,
            })),
            filt,
        });
        self.insert(&kn)?;
        if let Err(e) = kn.filt.attach(&kn) {
            self.forget(&kn);
            return Err(e);
        }
        let _st = self.st.lock();
        kn.set(0, status::DETACHED);
        Ok(kn)
    }

    /// Puts `kn` in the index (`knote_attach`).
    fn insert(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        let mut st = self.st.lock();
        if st.closing {
            return Err(Errno::EBADF);
        }
        if kn.fd {
            let i = usize::try_from(kn.ident).map_err(|_| Errno::EBADF)?;
            let len = st.fds.len();
            if len <= i {
                st.fds.try_reserve(i + 1 - len).map_err(|_| Errno::ENOMEM)?;
                st.fds.resize_with(i + 1, Vec::new);
            }
            st.fds[i].push(Arc::clone(kn));
        } else {
            st.other.entry(kn.ident).or_default().push(Arc::clone(kn));
        }
        Ok(())
    }

    /// Takes `kn` out of the index and the queue, marking it dropped.
    /// Returns whether it was there.
    fn forget(&self, kn: &Knote) -> bool {
        self.forget_locked(&mut self.st.lock(), kn)
    }

    fn forget_locked(&self, st: &mut State, kn: &Knote) -> bool {
        if kn.status() & status::DROPPED != 0 {
            return false;
        }
        if kn.status() & status::QUEUED != 0 {
            self.unqueue(st, kn);
        }
        kn.set(status::DROPPED, status::ACTIVE);
        if let Some(l) = list(st, kn) {
            l.retain(|k| !core::ptr::eq(&**k, kn));
            if l.is_empty() && !kn.fd {
                st.other.remove(&kn.ident);
            }
        }
        true
    }

    /// The end of a registration (`done_ev_add`): the event is checked
    /// unless disabled, and the knote queued if active. Returns whether
    /// it was just queued.
    fn settle(&self, h: &mut Held<'_>, kn: &Knote) -> bool {
        let s = kn.status();
        if s & status::DROPPED != 0 {
            return false;
        }
        if s & status::DISABLED == 0 && kn.filt.event(kn, kn.note_mut(h), 0) {
            kn.set(status::ACTIVE, 0);
        }
        let s = kn.status();
        if s & (status::ACTIVE | status::DISABLED | status::QUEUED) == status::ACTIVE {
            self.enqueue(&mut h.st, kn);
            true
        } else {
            false
        }
    }

    /// Deletes `kn` (`knote_drop`): out of the kqueue, detached from its
    /// object.
    fn drop_knote(&self, kn: &Knote) {
        if !self.forget(kn) {
            return;
        }
        if kn.status() & status::DETACHED == 0 {
            kn.filt.detach(kn);
        }
    }

    /// Whether the queue is empty, in which case the caller is a scan
    /// about to sleep for an event and the next knote queued wakes it
    /// (`kqueue_scan` setting `KQ_SLEEP`). The caller sleeps until
    /// [`Kqueue::pending`] says otherwise.
    pub fn sleeps(&self) -> bool {
        let mut st = self.st.lock();
        st.sleeping = st.queue.is_empty();
        st.sleeping
    }

    /// Collects up to `out.len()` events for one scan (one `kevent`
    /// call's collection: `kqueue_scan`'s loop): returns how many. `pass`
    /// is `None` for the scan's first call and names the scan after it.
    /// Events queued after the scan began, and level-triggered ones it
    /// reported and put back, end it. The kqueue stays locked from one
    /// knote to the next, but for a knote that leaves it.
    pub fn collect(&self, pass: &mut Option<u64>, out: &mut [Kevent]) -> usize {
        let mut n = 0;
        let mut h = self.hold();
        let pass = *pass.get_or_insert_with(|| {
            h.st.passes += 1;
            h.st.passes
        });
        while n < out.len() {
            let Some(kn) = h.st.queue.pop_front() else {
                break;
            };
            if kn.pass.load(Ordering::Relaxed) >= pass {
                h.st.queue.push_front(kn);
                break;
            }
            self.count.store(h.st.queue.len(), Ordering::Release);
            // Off the queue: a notification now finds it not queued, and
            // waits for this lock.
            kn.set(0, status::QUEUED | status::RESCAN);
            if kn.status() & status::DISABLED != 0 {
                continue;
            }
            let flags = kn.note_mut(&mut h).kev.flags;
            if flags & (EV_DROP | EV_ONESHOT) != 0 {
                if flags & EV_DROP == 0 {
                    // Notifications passed over a level filter's knote
                    // while a scan was to ask again: its data as it is
                    // now, if still an event.
                    let note = kn.note_mut(&mut h);
                    let before = *note;
                    if kn.level && !kn.filt.event(&kn, note, 0) {
                        *note = before;
                    }
                    let kev = note.kev;
                    if self.forget_locked(&mut h.st, &kn) {
                        out[n] = kev;
                        n += 1;
                    }
                } else {
                    self.forget_locked(&mut h.st, &kn);
                }
                // Its object's list is locked before a kqueue, never
                // after: detached with this one unlocked.
                drop(h);
                if kn.status() & status::DETACHED == 0 {
                    kn.filt.detach(&kn);
                }
                drop(kn);
                h = self.hold();
                continue;
            }
            let note = kn.note_mut(&mut h);
            if !kn.filt.event(&kn, note, 0) {
                kn.set(0, status::ACTIVE);
                if kn.level {
                    // A notification that saw `RESCAN` before it was
                    // cleared did nothing, trusting this scan to ask the
                    // filter: ask once more now that it is clear. Either
                    // the notifier's change is seen here, or it sees the
                    // bit clear and queues the knote itself.
                    fence(Ordering::SeqCst);
                    if kn.filt.event(&kn, kn.note_mut(&mut h), 0) {
                        kn.set(status::ACTIVE, 0);
                        self.requeue(&mut h.st, kn, 0);
                    }
                }
                continue;
            }
            let touch = !kn.fd && kn.filt.touches();
            let kev = if touch {
                kn.filt.process(&kn, note)
            } else {
                note.kev
            };
            if note.kev.flags & (EV_CLEAR | EV_DISPATCH) != 0 {
                if !touch && note.kev.flags & EV_CLEAR != 0 {
                    note.kev.data = 0;
                    note.kev.fflags = 0;
                }
                if note.kev.flags & EV_DISPATCH != 0 {
                    kn.set(status::DISABLED, 0);
                }
                kn.set(0, status::ACTIVE);
            } else {
                let again = if kn.level { status::RESCAN } else { 0 };
                self.requeue(&mut h.st, kn, again);
            }
            out[n] = kev;
            n += 1;
        }
        n
    }

    /// Descriptor `fd` closed (`knote_fdclose`): its knotes go.
    pub fn fd_closed(&self, fd: usize) {
        let knotes = match self.st.lock().fds.get(fd) {
            Some(l) => l.clone(),
            None => return,
        };
        for kn in knotes {
            self.drop_knote(&kn);
        }
    }

    /// The kqueue's descriptor closed for the last time (`kqueue_close`):
    /// every knote goes, and kqueues watching it see it end.
    pub fn close(&self) {
        let all: Vec<Arc<Knote>> = {
            let mut st = self.st.lock();
            st.closing = true;
            st.fds
                .iter()
                .flatten()
                .chain(st.other.values().flatten())
                .cloned()
                .collect()
        };
        for kn in all {
            self.drop_knote(&kn);
        }
        self.sel.clear();
    }

    /// Copies the knotes of this kqueue into `child`, a forked process's
    /// copy (`kqueue_fork_copy`): those whose filters copy, and, for
    /// descriptors, whose descriptor `open` says the child has.
    pub fn fork_into(&self, child: &Arc<Self>, open: &dyn Fn(u64) -> bool) {
        let all: Vec<Arc<Knote>> = {
            let st = self.st.lock();
            st.fds
                .iter()
                .flatten()
                .chain(st.other.values().flatten())
                .filter(|k| k.status() & (status::DETACHED | status::DROPPED) == 0)
                .cloned()
                .collect()
        };
        for kn in all {
            if kn.fd && !open(kn.ident) {
                continue;
            }
            let Some(filt) = kn.filt.fork(&kn) else {
                continue;
            };
            let note = kn.note();
            let copy = Arc::new(Knote {
                kq: Arc::clone(child),
                ident: kn.ident,
                filter: kn.filter,
                fd: kn.fd,
                status: AtomicU16::new(status::DETACHED | (kn.status() & status::DISABLED)),
                pass: AtomicU64::new(0),
                level: filt.level(),
                note: NoteCell(UnsafeCell::new(note)),
                filt,
            });
            if child.insert(&copy).is_err() {
                continue;
            }
            if copy.filt.attach(&copy).is_err() {
                child.forget(&copy);
                continue;
            }
            let mut h = child.hold();
            copy.set(0, status::DETACHED);
            let queued = child.settle(&mut h, &copy);
            if child.release(h, queued) {
                child.sel.notify(0);
            }
        }
    }

    /// Each registered knote's event and status, descriptors' first in
    /// descriptor order, then the others' (`kern_proc_kqueue_report`'s
    /// `kn_kevent` and `kn_status`, as `KNOTE_STATUS_*` bits).
    pub fn report(&self) -> Vec<(Kevent, u32)> {
        let mut h = self.hold();
        let all: Vec<Arc<Knote>> = {
            let st = &h.st;
            st.fds
                .iter()
                .flatten()
                .chain(st.other.values().flatten())
                .cloned()
                .collect()
        };
        all.iter()
            .map(|kn| {
                let s = kn.status.load(Ordering::Relaxed);
                let mut bits = 0;
                for (have, report) in [
                    (status::ACTIVE, 0x01),
                    (status::QUEUED, 0x02),
                    (status::DISABLED, 0x04),
                    (status::DETACHED, 0x08),
                ] {
                    if s & have != 0 {
                        bits |= report;
                    }
                }
                let mut kev = kn.note_mut(&mut h).kev;
                kev.ident = kn.ident;
                kev.filter = kn.filter;
                (kev, bits)
            })
            .collect()
    }

    /// Registered knotes, for diagnostics and tests.
    pub fn knotes(&self) -> usize {
        let st = self.st.lock();
        st.fds.iter().map(Vec::len).sum::<usize>() + st.other.values().map(Vec::len).sum::<usize>()
    }
}
