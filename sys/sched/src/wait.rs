// SPDX-License-Identifier: BSD-2-Clause
//! Wait queues: sleep until a condition holds.
//!
//! The waiter checks its condition with the queue locked and goes on the
//! queue in the same critical section; a waker changes the condition first
//! and then takes the queue lock to wake. So either the waiter sees the new
//! condition, or it is already queued when the waker looks: no wakeup is
//! lost.
//!
//! Behavior reference: sys/kern/subr_sleepqueue.c, sys/kern/kern_condvar.c.

use core::sync::atomic::Ordering;

use sync::SpinLock;

use crate::Thread;
use crate::list::ThreadList;
use crate::thread::State;

/// An interruptible sleep ended because the thread was interrupted
/// ([`Thread::interrupt`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupted;

/// Threads waiting for something.
#[derive(Debug)]
pub struct WaitQueue {
    waiters: SpinLock<ThreadList>,
}

impl WaitQueue {
    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            waiters: SpinLock::new(ThreadList::new()),
        }
    }

    /// Sleeps until `done` returns true. `done` runs with the queue locked
    /// (and interrupts masked), so it must be short and must not sleep.
    pub fn wait_until(&self, _s: &crate::Sleepable, mut done: impl FnMut() -> bool) {
        loop {
            // Mask interrupts before taking the queue lock, so they stay
            // masked from the check until the switch: a preemption between
            // queueing and switching would run the scheduler twice.
            let irq = hal::irq::disable();
            let mut q = self.waiters.lock();
            if done() {
                drop(q);
                hal::irq::restore(irq);
                return;
            }
            crate::lock::witness::check_sleep(hal::irq::were_enabled(&irq));
            let me = crate::current_arc();
            me.inner.lock().state = State::Sleeping;
            q.push_back(me);
            drop(q);
            crate::switch_away();
            hal::irq::restore(irq);
        }
    }

    /// Sleeps until `done` returns true, or until the thread is interrupted
    /// ([`Thread::interrupt`]; at once if it already is). `done` runs as in
    /// [`WaitQueue::wait_until`], and wins if both hold.
    pub fn wait_until_interruptible(
        &self,
        _s: &crate::Sleepable,
        mut done: impl FnMut() -> bool,
    ) -> Result<(), Interrupted> {
        let me = crate::current();
        loop {
            let irq = hal::irq::disable();
            let mut q = self.waiters.lock();
            if done() {
                drop(q);
                hal::irq::restore(irq);
                return Ok(());
            }
            if me.is_interrupted() {
                drop(q);
                hal::irq::restore(irq);
                return Err(Interrupted);
            }
            crate::lock::witness::check_sleep(hal::irq::were_enabled(&irq));
            let arc = crate::current_arc();
            arc.inner.lock().state = State::Sleeping;
            q.push_back(arc);
            me.wchan
                .store(core::ptr::from_ref(self).cast_mut(), Ordering::SeqCst);
            // An interrupt that read `wchan` before we published it set
            // the flag before it looked: see it now, and do not sleep.
            let raced = me.is_interrupted();
            if raced {
                let unqueued = q.remove(me);
                me.inner.lock().state = State::Running;
                drop(q);
                drop(unqueued);
            } else {
                drop(q);
                crate::switch_away();
            }
            // Whoever woke us unqueued us. An interrupt may still hold our
            // `wchan`: wait for it before the queue can go away.
            {
                let _abort = me.abort.lock();
                me.wchan.store(core::ptr::null_mut(), Ordering::SeqCst);
            }
            hal::irq::restore(irq);
        }
    }

    /// Unqueues `t` if it sleeps here (an interrupted sleeper).
    pub(crate) fn remove(&self, t: &Thread) -> Option<alloc::sync::Arc<Thread>> {
        self.waiters.lock().remove(t)
    }

    /// Wakes the longest waiter. Returns whether there was one.
    pub fn wake_one(&self) -> bool {
        let t = self.waiters.lock().pop_front();
        match t {
            Some(t) => {
                crate::make_runnable(t);
                true
            }
            None => false,
        }
    }

    /// Wakes every waiter. Returns how many there were.
    pub fn wake_all(&self) -> usize {
        let mut n = 0;
        while self.wake_one() {
            n += 1;
        }
        n
    }

    /// Number of waiters (racy; for diagnostics).
    pub fn waiters(&self) -> usize {
        self.waiters.lock().len()
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}
