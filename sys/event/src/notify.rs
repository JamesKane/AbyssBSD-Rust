// SPDX-License-Identifier: BSD-2-Clause
//! [`Notify`]: an object's waiters, for `poll`/`select` and for kqueues.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering, fence};

use abi::event::{EV_EOF, EV_ONESHOT};
use sync::SpinLock;

use crate::kqueue::{Knote, Kqueue};

/// Something to wake: a thread in `poll` or `select`, or the threads
/// scanning a kqueue. Called with spin locks held, so it must not sleep.
///
/// Notifications come from thread context: they may allocate, and may
/// drop the last reference to a poller or a kqueue.
pub trait Wake: Send + Sync {
    fn wake(&self);
}

struct Inner {
    knotes: Vec<Arc<Knote>>,
    /// Pollers, until their call ends (a dead one is pruned).
    pollers: Pollers,
}

/// An object's pollers: the first in place, the others in a list. Nearly
/// every object polled has one poller, and `poll` over a thousand
/// descriptors registers with each: a list's buffer for each would be a
/// thousand more allocations to touch, wherever the heap put them.
#[derive(Default)]
struct Pollers {
    first: Option<Weak<dyn Wake>>,
    rest: Vec<Weak<dyn Wake>>,
}

impl Pollers {
    const fn new() -> Self {
        Self {
            first: None,
            rest: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.first.is_none() && self.rest.is_empty()
    }

    /// Drops the pollers whose calls have ended.
    fn prune(&mut self) {
        if self.first.as_ref().is_some_and(|p| p.strong_count() == 0) {
            self.first = None;
        }
        if !self.rest.is_empty() {
            self.rest.retain(|p| p.strong_count() > 0);
        }
    }

    fn contains(&self, w: *const dyn Wake) -> bool {
        self.first
            .iter()
            .chain(&self.rest)
            .any(|p| core::ptr::addr_eq(p.as_ptr(), w))
    }

    fn push(&mut self, w: Weak<dyn Wake>) {
        if self.first.is_none() {
            self.first = Some(w);
        } else {
            self.rest.push(w);
        }
    }

    /// Takes every poller off.
    fn drain(&mut self) -> impl Iterator<Item = Weak<dyn Wake>> + use<> {
        self.first
            .take()
            .into_iter()
            .chain(core::mem::take(&mut self.rest))
    }
}

/// An object's waiters (FreeBSD's `struct selinfo` and its `knlist`).
///
/// A waiter registers before it looks at the object, and the object
/// changes its state before it notifies, so a change is either seen or
/// woken for.
pub struct Notify {
    /// Whether anything is registered: lets [`Notify::notify`] skip the
    /// lock when nobody waits.
    armed: AtomicBool,
    /// Counts the object's changes, for a waiter that spins a moment
    /// before it registers ([`Notify::changes`]). Notifiers may race and
    /// lose a count; it still differs from what a spinner read.
    changes: AtomicU32,
    /// How the last spins went ([`Notify::may_spin`]).
    spin_missed: AtomicU8,
    inner: SpinLock<Inner>,
}

/// After this many spins in a row that saw no change, waiters stop
/// spinning, but for one in every [`SPIN_PROBE`] waits.
const SPIN_MISSES: u8 = 2;
const SPIN_PROBE: u8 = 64;

impl core::fmt::Debug for Notify {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Notify")
            .field("armed", &self.armed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

impl Notify {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            changes: AtomicU32::new(0),
            spin_missed: AtomicU8::new(0),
            inner: SpinLock::new(Inner {
                knotes: Vec::new(),
                pollers: Pollers::new(),
            }),
        }
    }

    /// Registers a poller (`selrecord`): it is woken by the next
    /// notification. Look at the object only after this.
    pub fn record(&self, w: &Arc<dyn Wake>) {
        {
            let mut i = self.inner.lock();
            i.pollers.prune();
            if !i.pollers.contains(Arc::as_ptr(w)) {
                i.pollers.push(Arc::downgrade(w));
            }
            self.armed.store(true, Ordering::Relaxed);
        }
        // Pairs with the fence in `notify`: either it sees `armed`, or we
        // see the state it changed before notifying.
        fence(Ordering::SeqCst);
    }

    /// Whether a poller or knote is registered.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Adds a knote (`knlist_add`), as its filter's attach does. Its
    /// filter's event is checked after this by the caller.
    pub fn add(&self, kn: &Arc<Knote>) {
        {
            let mut i = self.inner.lock();
            i.knotes.push(Arc::clone(kn));
            self.armed.store(true, Ordering::Relaxed);
        }
        fence(Ordering::SeqCst);
    }

    /// Removes a knote (`knlist_remove`), as its filter's detach does.
    pub fn remove(&self, kn: &Knote) {
        let mut i = self.inner.lock();
        if let Some(at) = i
            .knotes
            .iter()
            .position(|k| core::ptr::eq(Arc::as_ptr(k), kn))
        {
            i.knotes.swap_remove(at);
        }
        Self::rearm(&self.armed, &i);
    }

    fn rearm(armed: &AtomicBool, i: &Inner) {
        if i.knotes.is_empty() && i.pollers.is_empty() {
            armed.store(false, Ordering::Relaxed);
        }
    }

    /// Whether anything is registered, as [`Notify::notify`] sees it (the
    /// caller changed the state it is about first).
    #[must_use]
    pub fn armed(&self) -> bool {
        self.changed();
        fence(Ordering::SeqCst);
        self.armed.load(Ordering::Relaxed)
    }

    fn changed(&self) {
        let n = self.changes.load(Ordering::Relaxed);
        self.changes.store(n.wrapping_add(1), Ordering::Relaxed);
    }

    /// A number that differs after the object changes (its notifier
    /// called [`Notify::notify`] or [`Notify::armed`]).
    #[must_use]
    pub fn changes(&self) -> u32 {
        self.changes.load(Ordering::Relaxed)
    }

    /// Whether a waiter about to sleep should first spin a moment for a
    /// change: yes while spinning has been finding one (the other side
    /// is at work on another CPU, and waking a sleeper costs it more
    /// than the spin costs this side), and now and then to find out.
    #[must_use]
    pub fn may_spin(&self) -> bool {
        let m = self.spin_missed.load(Ordering::Relaxed);
        if m < SPIN_MISSES {
            return true;
        }
        let next = if m >= SPIN_MISSES + SPIN_PROBE {
            SPIN_MISSES - 1
        } else {
            m + 1
        };
        self.spin_missed.store(next, Ordering::Relaxed);
        false
    }

    /// Records whether a spin saw a change.
    pub fn spun(&self, hit: bool) {
        let m = self.spin_missed.load(Ordering::Relaxed);
        let next = if hit { 0 } else { (m + 1).min(SPIN_MISSES) };
        self.spin_missed.store(next, Ordering::Relaxed);
    }

    /// The object's state changed: pollers wake (`selwakeup`) and knotes
    /// see `hint` (`KNOTE_LOCKED`).
    pub fn notify(&self, hint: u64) {
        self.deliver(hint, true);
    }

    /// The descriptor is closing (`seldrain`): its pollers wake to find it
    /// gone (`POLLNVAL`); the knotes hear nothing.
    pub fn drain(&self) {
        fence(Ordering::SeqCst);
        if !self.armed.load(Ordering::Relaxed) {
            return;
        }
        let mut i = self.inner.lock();
        for p in i.pollers.drain() {
            if let Some(p) = p.upgrade() {
                p.wake();
            }
        }
        Self::rearm(&self.armed, &i);
    }

    /// Only the knotes see `hint`, the pollers stay asleep (`KNOTE`
    /// without `selwakeup`: the VFS's notes, process events).
    pub fn knote(&self, hint: u64) {
        self.deliver(hint, false);
    }

    fn deliver(&self, hint: u64, pollers: bool) {
        self.changed();
        fence(Ordering::SeqCst);
        if !self.armed.load(Ordering::Relaxed) {
            return;
        }
        // Kqueues that newly have an event and are themselves watched:
        // told once this list is unlocked.
        let mut watched: Vec<Arc<Kqueue>> = Vec::new();
        // Taken off under the lock, woken after it: a wakeup (and the
        // interrupt to another CPU it may send) does not hold up the
        // object's other waiters and notifiers.
        let mut woken = {
            let mut i = self.inner.lock();
            // Nothing moves when no poller waits (a kqueue's object).
            let woken = if pollers && !i.pollers.is_empty() {
                Some(core::mem::take(&mut i.pollers))
            } else {
                None
            };
            for kn in &i.knotes {
                if let Some(kq) = kn.deliver(hint) {
                    watched.push(kq);
                }
            }
            Self::rearm(&self.armed, &i);
            woken
        };
        for p in woken.iter_mut().flat_map(Pollers::drain) {
            if let Some(p) = p.upgrade() {
                p.wake();
            }
        }
        for kq in watched {
            kq.sel.notify(0);
        }
    }

    /// The object is going away (`knlist_clear`): its knotes are detached
    /// and marked `EV_EOF` and `EV_ONESHOT`, so they are reported once
    /// more, if active, and dropped; pollers wake.
    pub fn clear(&self) {
        let knotes = {
            let mut i = self.inner.lock();
            for p in i.pollers.drain() {
                if let Some(p) = p.upgrade() {
                    p.wake();
                }
            }
            self.armed.store(false, Ordering::Relaxed);
            core::mem::take(&mut i.knotes)
        };
        for kn in knotes {
            kn.detached(EV_EOF | EV_ONESHOT);
        }
    }

    /// The object is freed (`knlist_delete`): its knotes leave their
    /// kqueues at once, reported or not.
    pub fn delete(&self) {
        let knotes = {
            let mut i = self.inner.lock();
            let k = core::mem::take(&mut i.knotes);
            Self::rearm(&self.armed, &i);
            k
        };
        for kn in knotes {
            kn.delete();
        }
    }

    /// The knotes whose notes satisfy `f` (for `knote_fork`, which
    /// registers more for each).
    pub fn knotes_where(&self, f: impl Fn(&crate::Note) -> bool) -> Vec<Arc<Knote>> {
        self.inner
            .lock()
            .knotes
            .iter()
            .filter(|k| f(&k.note()))
            .cloned()
            .collect()
    }

    /// Knotes registered, for diagnostics and tests.
    pub fn knotes(&self) -> usize {
        self.inner.lock().knotes.len()
    }
}
