// SPDX-License-Identifier: BSD-2-Clause
//! An intrusive FIFO of threads, threaded through each thread's link.
//!
//! A thread is on at most one list at a time (a run queue, a wait queue,
//! the zombie list), and the list's owner lock protects the links of the
//! threads on it. The list holds one strong reference per thread.

use alloc::sync::Arc;
use core::ptr;

use crate::Thread;

/// A thread's list membership. Only touched under the owning list's lock.
#[derive(Debug)]
pub(crate) struct Link {
    next: *const Thread,
    prev: *const Thread,
    linked: bool,
}

impl Link {
    pub(crate) const fn new() -> Self {
        Self {
            next: ptr::null(),
            prev: ptr::null(),
            linked: false,
        }
    }
}

/// A FIFO of threads.
#[derive(Debug)]
pub(crate) struct ThreadList {
    head: *const Thread,
    tail: *const Thread,
    len: usize,
}

// SAFETY: the raw pointers are strong references owned by the list; the
// list is only used under its owner's lock.
unsafe impl Send for ThreadList {}

impl ThreadList {
    pub(crate) const fn new() -> Self {
        Self {
            head: ptr::null(),
            tail: ptr::null(),
            len: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn push_back(&mut self, t: Arc<Thread>) {
        let p = Arc::into_raw(t);
        // SAFETY: `p` is live (we hold its reference) and, per the
        // one-list rule, not on any list; the caller holds our lock.
        unsafe {
            let link = &mut *(*p).link.get();
            assert!(!link.linked, "thread already on a list");
            link.linked = true;
            link.next = ptr::null();
            link.prev = self.tail;
            match self.tail.as_ref() {
                Some(tail) => (*tail.link.get()).next = p,
                None => self.head = p,
            }
        }
        self.tail = p;
        self.len += 1;
    }

    pub(crate) fn pop_front(&mut self) -> Option<Arc<Thread>> {
        let p = self.head;
        if p.is_null() {
            return None;
        }
        // SAFETY: `p` is on this list.
        Some(unsafe { self.unlink(p) })
    }

    /// Removes `t` if it is on this list.
    pub(crate) fn remove(&mut self, t: &Thread) -> Option<Arc<Thread>> {
        let mut p = self.head;
        while !p.is_null() {
            if ptr::eq(p, t) {
                // SAFETY: `p` is on this list.
                return Some(unsafe { self.unlink(p) });
            }
            // SAFETY: as above.
            p = unsafe { (*(*p).link.get()).next };
        }
        None
    }

    /// Iterates over the threads, front to back.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Thread> {
        let mut p = self.head;
        core::iter::from_fn(move || {
            // SAFETY: `p` is null or on this list, which we borrow.
            let t = unsafe { p.as_ref()? };
            // SAFETY: as above.
            p = unsafe { (*t.link.get()).next };
            Some(t)
        })
    }

    /// # Safety
    ///
    /// `p` is on this list.
    unsafe fn unlink(&mut self, p: *const Thread) -> Arc<Thread> {
        // SAFETY: per the contract; our lock is held.
        unsafe {
            let link = &mut *(*p).link.get();
            match link.prev.as_ref() {
                Some(prev) => (*prev.link.get()).next = link.next,
                None => self.head = link.next,
            }
            match link.next.as_ref() {
                Some(next) => (*next.link.get()).prev = link.prev,
                None => self.tail = link.prev,
            }
            *link = Link::new();
            self.len -= 1;
            Arc::from_raw(p)
        }
    }
}

impl Drop for ThreadList {
    fn drop(&mut self) {
        while self.pop_front().is_some() {}
    }
}
