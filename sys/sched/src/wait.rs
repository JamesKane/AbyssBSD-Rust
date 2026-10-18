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

use sync::SpinLock;

use crate::list::ThreadList;
use crate::thread::State;

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
