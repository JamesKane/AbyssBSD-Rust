// SPDX-License-Identifier: BSD-2-Clause
//! Kqueues and knotes.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::event::{
    EV_ADD, EV_CLEAR, EV_DELETE, EV_DISABLE, EV_DISPATCH, EV_DROP, EV_ENABLE, EV_FLAG1, EV_FLAG2,
    EV_FORCEONESHOT, EV_KEEPUDATA, EV_ONESHOT, EVFILT_PROC, Kevent,
};
use sync::SpinLock;

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
    /// or a registration asks. Called with the note locked and, from a
    /// notification, the object's [`Notify`]: it must not sleep, and must
    /// not touch that `Notify`.
    fn event(&self, kn: &Knote, note: &mut Note, hint: u64) -> bool;

    /// Whether the filter has `f_touch`: its [`Filter::register`] and
    /// [`Filter::process`] replace the generic handling, and `EV_CLEAR`
    /// is left to them.
    fn touches(&self) -> bool {
        false
    }

    /// A change to an existing knote (`f_touch(EVENT_REGISTER)`).
    fn register(&self, kn: &Knote, note: &mut Note, kev: &Kevent) {
        let _ = kn;
        note.sfflags = kev.fflags;
        note.sdata = kev.data;
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

/// One registered event (FreeBSD's `struct knote`).
pub struct Knote {
    /// First, so it is dropped first: a filter's own callbacks may use
    /// the knote until the filter has stopped them.
    filt: Box<dyn Filter>,
    kq: Weak<Kqueue>,
    pub ident: u64,
    pub filter: i16,
    fd: bool,
    /// `status` bits; changed only under the kqueue's lock.
    status: AtomicU16,
    /// The latest scan begun when it was last queued: scans begun since
    /// see it, earlier ones stop at it (FreeBSD's scan marker).
    pass: AtomicU64,
    note: SpinLock<Note>,
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

impl Knote {
    fn status(&self) -> u16 {
        self.status.load(Ordering::Relaxed)
    }

    /// Sets and clears status bits. The kqueue's lock is held.
    fn set(&self, on: u16, off: u16) {
        let s = self.status();
        self.status.store((s | on) & !off, Ordering::Relaxed);
    }

    /// The kqueue it belongs to, while it lives.
    pub fn kqueue(&self) -> Option<Arc<Kqueue>> {
        self.kq.upgrade()
    }

    /// The registered (ident, filter, flags, udata, ...), as it stands.
    pub fn note(&self) -> Note {
        *self.note.lock()
    }

    /// Runs `f` on the note and, if it says the event happened, activates
    /// the knote, as a callout or other source without a [`Notify`] does
    /// (FreeBSD's `KNOTE_ACTIVATE` from `filt_timerexpire`).
    pub fn trigger(&self, f: impl FnOnce(&mut Note) -> bool) {
        let watched = {
            let mut note = self.note.lock();
            if f(&mut note) { self.activate() } else { None }
        };
        if let Some(kq) = watched {
            kq.sel.notify(0);
        }
    }

    /// Takes the knote out of its queue and makes it inactive, as a timer
    /// re-added with new parameters does.
    pub fn deactivate(&self) {
        let Some(kq) = self.kq.upgrade() else {
            return;
        };
        let mut st = kq.st.lock();
        if self.status() & status::QUEUED != 0 {
            kq.unqueue(&mut st, self);
        }
        self.set(0, status::ACTIVE);
    }

    /// A notification: runs the filter, the note and the object's list
    /// locked; returns its kqueue if it newly has an event and is watched.
    pub(crate) fn deliver(&self, hint: u64) -> Option<Arc<Kqueue>> {
        let mut note = self.note.lock();
        if self.filt.event(self, &mut note, hint) {
            self.activate()
        } else {
            None
        }
    }

    /// Off its object's list (`knlist_clear`): `flags` join its own.
    pub(crate) fn detached(&self, flags: u16) {
        let mut note = self.note.lock();
        note.kev.flags |= flags;
        if let Some(kq) = self.kq.upgrade() {
            let _st = kq.st.lock();
            self.set(status::DETACHED, 0);
        }
    }

    /// `KNOTE_ACTIVATE`, the note locked: active, and queued unless
    /// disabled or already there. Returns the kqueue if it is watched and
    /// was just queued on.
    fn activate(&self) -> Option<Arc<Kqueue>> {
        let kq = self.kq.upgrade()?;
        let queued = {
            let mut st = kq.st.lock();
            kq.activate_locked(&mut st, self)
        };
        if queued {
            kq.wake.wake();
            if kq.sel.is_armed() {
                return Some(kq);
            }
        }
        None
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
}

/// A kqueue (FreeBSD's `struct kqueue`).
pub struct Kqueue {
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
        // Behind the marker of every scan under way.
        arc.pass.store(st.passes, Ordering::Relaxed);
        st.queue.push_back(arc);
        kn.set(status::QUEUED, 0);
        self.count.store(st.queue.len(), Ordering::Release);
    }

    fn unqueue(&self, st: &mut State, kn: &Knote) {
        if let Some(at) = st.queue.iter().position(|k| core::ptr::eq(&**k, kn)) {
            st.queue.remove(at);
            self.count.store(st.queue.len(), Ordering::Release);
        }
        kn.set(0, status::QUEUED);
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
                let watched = self.settle(&kn, &mut kn.note.lock());
                if watched {
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
        let mut note = kn.note.lock();
        let mut watched = false;
        if flags & EV_FORCEONESHOT != 0 {
            note.kev.flags |= EV_ONESHOT;
            let queued = self.activate_locked(&mut self.st.lock(), &kn);
            if queued {
                self.wake.wake();
                watched = self.sel.is_armed();
            }
        }
        {
            let _st = self.st.lock();
            if flags & EV_ENABLE != 0 {
                kn.set(0, status::DISABLED);
            } else if flags & EV_DISABLE != 0 {
                kn.set(status::DISABLED, 0);
            }
        }
        if flags & EV_KEEPUDATA == 0 {
            note.kev.udata = kev.udata;
        }
        if !fd && kn.filt.touches() {
            kn.filt.register(&kn, &mut note, kev);
        } else {
            note.sfflags = kev.fflags;
            note.sdata = kev.data;
        }
        watched |= self.settle(&kn, &mut note);
        drop(note);
        if watched {
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
            kq: Arc::downgrade(self),
            ident: kev.ident,
            filter: kev.filter,
            fd,
            status: AtomicU16::new(s),
            pass: AtomicU64::new(0),
            note: SpinLock::new(Note {
                kev: k,
                sfflags: kev.fflags,
                sdata: kev.data,
                hook: 0,
            }),
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
        let mut st = self.st.lock();
        if kn.status() & status::DROPPED != 0 {
            return false;
        }
        if kn.status() & status::QUEUED != 0 {
            self.unqueue(&mut st, kn);
        }
        kn.set(status::DROPPED, status::ACTIVE);
        if let Some(l) = list(&mut st, kn) {
            l.retain(|k| !core::ptr::eq(&**k, kn));
            if l.is_empty() && !kn.fd {
                st.other.remove(&kn.ident);
            }
        }
        true
    }

    /// The end of a registration (`done_ev_add`): the event is checked
    /// unless disabled, and the knote queued if active. Returns whether
    /// it was just queued and the kqueue is watched: the caller tells the
    /// watchers once the note is unlocked (a kqueue watching this one may
    /// be one this one watches).
    fn settle(&self, kn: &Knote, note: &mut Note) -> bool {
        let event = kn.status() & status::DISABLED == 0 && kn.filt.event(kn, note, 0);
        let queued = {
            let mut st = self.st.lock();
            let s = kn.status();
            if s & status::DROPPED != 0 {
                return false;
            }
            if event {
                kn.set(status::ACTIVE, 0);
            }
            let s = kn.status();
            if s & (status::ACTIVE | status::DISABLED | status::QUEUED) == status::ACTIVE {
                self.enqueue(&mut st, kn);
                true
            } else {
                false
            }
        };
        if queued {
            self.wake.wake();
        }
        queued && self.sel.is_armed()
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

    /// Starts a scan (one `kevent` call's collection).
    pub fn begin_scan(&self) -> u64 {
        let mut st = self.st.lock();
        st.passes += 1;
        st.passes
    }

    /// Collects up to `out.len()` events for scan `pass` (`kqueue_scan`'s
    /// loop): returns how many. Events queued after the scan began, and
    /// level-triggered ones it reported and put back, end it.
    pub fn collect(&self, pass: u64, out: &mut [Kevent]) -> usize {
        let mut n = 0;
        while n < out.len() {
            let kn = {
                let mut st = self.st.lock();
                let Some(kn) = st.queue.pop_front() else {
                    break;
                };
                if kn.pass.load(Ordering::Relaxed) >= pass {
                    st.queue.push_front(kn);
                    break;
                }
                self.count.store(st.queue.len(), Ordering::Release);
                if kn.status() & status::DISABLED != 0 {
                    kn.set(0, status::QUEUED);
                    continue;
                }
                kn
            };
            // Off the queue but still marked queued: a notification now
            // only makes it active.
            let flags = kn.note.lock().kev.flags;
            if flags & EV_DROP != 0 {
                self.dequeued(&kn);
                self.drop_knote(&kn);
                continue;
            }
            if flags & EV_ONESHOT != 0 {
                self.dequeued(&kn);
                let kev = kn.note.lock().kev;
                if self.forget(&kn) {
                    if kn.status() & status::DETACHED == 0 {
                        kn.filt.detach(&kn);
                    }
                    out[n] = kev;
                    n += 1;
                }
                continue;
            }
            let mut note = kn.note.lock();
            if !kn.filt.event(&kn, &mut note, 0) {
                let _st = self.st.lock();
                kn.set(0, status::QUEUED | status::ACTIVE);
                continue;
            }
            let touch = !kn.fd && kn.filt.touches();
            let kev = if touch {
                kn.filt.process(&kn, &mut note)
            } else {
                note.kev
            };
            let mut st = self.st.lock();
            if kn.status() & status::DROPPED != 0 {
                continue;
            }
            if note.kev.flags & (EV_CLEAR | EV_DISPATCH) != 0 {
                if !touch && note.kev.flags & EV_CLEAR != 0 {
                    note.kev.data = 0;
                    note.kev.fflags = 0;
                }
                if note.kev.flags & EV_DISPATCH != 0 {
                    kn.set(status::DISABLED, 0);
                }
                kn.set(0, status::QUEUED | status::ACTIVE);
            } else {
                kn.pass.store(st.passes, Ordering::Relaxed);
                st.queue.push_back(Arc::clone(&kn));
                self.count.store(st.queue.len(), Ordering::Release);
            }
            drop(st);
            drop(note);
            out[n] = kev;
            n += 1;
        }
        n
    }

    /// A knote `collect` took off the queue leaves it for good.
    fn dequeued(&self, kn: &Knote) {
        let _st = self.st.lock();
        kn.set(0, status::QUEUED);
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
            let note = *kn.note.lock();
            let copy = Arc::new(Knote {
                kq: Arc::downgrade(child),
                ident: kn.ident,
                filter: kn.filter,
                fd: kn.fd,
                status: AtomicU16::new(status::DETACHED | (kn.status() & status::DISABLED)),
                pass: AtomicU64::new(0),
                note: SpinLock::new(note),
                filt,
            });
            if child.insert(&copy).is_err() {
                continue;
            }
            if copy.filt.attach(&copy).is_err() {
                child.forget(&copy);
                continue;
            }
            {
                let _st = child.st.lock();
                copy.set(0, status::DETACHED);
            }
            if child.settle(&copy, &mut copy.note.lock()) {
                child.sel.notify(0);
            }
        }
    }

    /// Registered knotes, for diagnostics and tests.
    pub fn knotes(&self) -> usize {
        let st = self.st.lock();
        st.fds.iter().map(Vec::len).sum::<usize>() + st.other.values().map(Vec::len).sum::<usize>()
    }
}
