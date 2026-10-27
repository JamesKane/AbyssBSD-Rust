// SPDX-License-Identifier: BSD-2-Clause
//! [`Notify`]: an object's waiters, for `poll`/`select` and for kqueues.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering, fence};

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
    pollers: Vec<Weak<dyn Wake>>,
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
    inner: SpinLock<Inner>,
}

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
            inner: SpinLock::new(Inner {
                knotes: Vec::new(),
                pollers: Vec::new(),
            }),
        }
    }

    /// Registers a poller (`selrecord`): it is woken by the next
    /// notification. Look at the object only after this.
    pub fn record(&self, w: &Arc<dyn Wake>) {
        {
            let mut i = self.inner.lock();
            i.pollers.retain(|p| p.strong_count() > 0);
            let me = Arc::as_ptr(w);
            if !i.pollers.iter().any(|p| core::ptr::addr_eq(p.as_ptr(), me)) {
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

    /// The object's state changed: pollers wake (`selwakeup`) and knotes
    /// see `hint` (`KNOTE_LOCKED`).
    pub fn notify(&self, hint: u64) {
        self.deliver(hint, true);
    }

    /// Only the knotes see `hint`, the pollers stay asleep (`KNOTE`
    /// without `selwakeup`: the VFS's notes, process events).
    pub fn knote(&self, hint: u64) {
        self.deliver(hint, false);
    }

    fn deliver(&self, hint: u64, pollers: bool) {
        fence(Ordering::SeqCst);
        if !self.armed.load(Ordering::Relaxed) {
            return;
        }
        // Kqueues that newly have an event and are themselves watched:
        // told once this list is unlocked.
        let mut watched: Vec<Arc<Kqueue>> = Vec::new();
        {
            let mut i = self.inner.lock();
            if pollers {
                for p in i.pollers.drain(..) {
                    if let Some(p) = p.upgrade() {
                        p.wake();
                    }
                }
            }
            for kn in &i.knotes {
                if let Some(kq) = kn.deliver(hint) {
                    watched.push(kq);
                }
            }
            Self::rearm(&self.armed, &i);
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
            for p in i.pollers.drain(..) {
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
