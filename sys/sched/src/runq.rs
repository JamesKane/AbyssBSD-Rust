// SPDX-License-Identifier: BSD-2-Clause
//! A per-CPU run queue: one FIFO per four priority levels and a bitmap of
//! the non-empty ones, so picking the best thread is a find-first-set.
//!
//! Behavior reference: sys/kern/kern_switch.c (runq).

use alloc::sync::Arc;

use crate::list::ThreadList;
use crate::{Priority, Thread};

const QUEUES: usize = 64;

/// Runnable threads waiting for one CPU.
#[derive(Debug)]
pub(crate) struct RunQueue {
    bits: u64,
    queues: [ThreadList; QUEUES],
    len: usize,
}

impl RunQueue {
    pub(crate) const fn new() -> Self {
        Self {
            bits: 0,
            queues: [const { ThreadList::new() }; QUEUES],
            len: 0,
        }
    }

    fn index(p: Priority) -> usize {
        usize::from(p.get() / 4)
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Queues `t` behind threads of equal priority.
    pub(crate) fn push(&mut self, t: Arc<Thread>, p: Priority) {
        let i = Self::index(p);
        self.queues[i].push_back(t);
        self.bits |= 1 << i;
        self.len += 1;
    }

    /// Takes the best thread.
    pub(crate) fn pop(&mut self) -> Option<Arc<Thread>> {
        if self.bits == 0 {
            return None;
        }
        let i = usize::try_from(self.bits.trailing_zeros()).unwrap_or(0);
        let t = self.queues[i].pop_front()?;
        if self.queues[i].is_empty() {
            self.bits &= !(1 << i);
        }
        self.len -= 1;
        Some(t)
    }

    /// Recomputes every queued thread's priority with `f` (`None`: leave
    /// it) and moves those whose queue changed. Order among threads that
    /// stay put is kept.
    pub(crate) fn reprioritize(&mut self, mut f: impl FnMut(&Thread) -> Option<Priority>) {
        let mut moved = ThreadList::new();
        let mut prios = alloc::vec::Vec::new();
        for i in 0..QUEUES {
            if self.bits & (1 << i) == 0 {
                continue;
            }
            let mut keep = ThreadList::new();
            while let Some(t) = self.queues[i].pop_front() {
                match f(&t) {
                    Some(p) if Self::index(p) != i => {
                        prios.push(p);
                        moved.push_back(t);
                    }
                    _ => keep.push_back(t),
                }
            }
            core::mem::swap(&mut self.queues[i], &mut keep);
            if self.queues[i].is_empty() {
                self.bits &= !(1 << i);
            }
        }
        self.len -= prios.len();
        for p in prios {
            if let Some(t) = moved.pop_front() {
                self.push(t, p);
            }
        }
    }

    /// Removes `t`, wherever it is queued.
    pub(crate) fn remove(&mut self, t: &Thread) -> Option<Arc<Thread>> {
        let mut bits = self.bits;
        while bits != 0 {
            let i = usize::try_from(bits.trailing_zeros()).unwrap_or(0);
            bits &= !(1 << i);
            if let Some(found) = self.queues[i].remove(t) {
                if self.queues[i].is_empty() {
                    self.bits &= !(1 << i);
                }
                self.len -= 1;
                return Some(found);
            }
        }
        None
    }

    /// Takes the best thread that `ok` accepts, for stealing.
    pub(crate) fn pop_if(&mut self, mut ok: impl FnMut(&Thread) -> bool) -> Option<Arc<Thread>> {
        let mut bits = self.bits;
        while bits != 0 {
            let i = usize::try_from(bits.trailing_zeros()).unwrap_or(0);
            bits &= !(1 << i);
            let found = self.queues[i]
                .iter()
                .find(|t| ok(t))
                .map(core::ptr::from_ref);
            if let Some(p) = found {
                // SAFETY: `p` points at a thread on queue `i`, which the
                // queue keeps alive.
                let t = self.queues[i].remove(unsafe { &*p })?;
                if self.queues[i].is_empty() {
                    self.bits &= !(1 << i);
                }
                self.len -= 1;
                return Some(t);
            }
        }
        None
    }
}
