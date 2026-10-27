// SPDX-License-Identifier: BSD-2-Clause
//! Callouts whose handlers run in thread context (FreeBSD's softclock
//! thread). A [`crate::time::Timer`] fires in interrupt context, where
//! the heap may not be used and sleeping locks may not be taken; the user
//! timers (`setitimer`, POSIX timers, `timerfd`, `EVFILT_TIMER`) post
//! signals, wake pollers, and may drop the last reference to something,
//! so their [`Callout`]s hand the expiry to one kernel thread that runs
//! the handlers in order.
//!
//! Behavior reference: sys/kern/kern_timeout.c (`softclock`).

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use sched::{IrqCx, Sleepable, WaitQueue};
use sync::SpinLock;

use crate::time::Timer;

/// A handler and its argument.
pub(crate) type SoftFn = fn(&Sleepable, usize);

/// The expiries waiting for the thread: a singly linked list through
/// [`Callout::next`], first to run at the head.
struct List {
    head: *const Callout,
    tail: *const Callout,
}

// SAFETY: the pointers are to pinned callouts, which leave the list before
// they are dropped.
unsafe impl Send for List {}

static LIST: SpinLock<List> = SpinLock::new(List {
    head: ptr::null(),
    tail: ptr::null(),
});
/// The callout whose handler runs now.
static RUNNING: AtomicPtr<Callout> = AtomicPtr::new(ptr::null_mut());
static WAKE: WaitQueue = WaitQueue::new();
/// The softclock thread, once started.
static THREAD: AtomicPtr<sched::Thread> = AtomicPtr::new(ptr::null_mut());

/// A timer whose handler runs on the softclock thread. Pinned (it is
/// boxed); dropping it stops it and waits for a running handler, unless
/// the drop comes from that handler.
pub(crate) struct Callout {
    timer: Timer,
    /// In [`LIST`], or about to be.
    queued: AtomicBool,
    /// Guarded by [`LIST`]'s lock.
    next: UnsafeCell<*const Callout>,
    /// Guarded by [`LIST`]'s lock.
    func: UnsafeCell<(SoftFn, usize)>,
}

// SAFETY: the cells are touched only under `LIST`'s lock.
unsafe impl Sync for Callout {}
// SAFETY: as above.
unsafe impl Send for Callout {}

/// The timer's interrupt half: queue the callout for the thread.
fn fire(arg: usize, _: &IrqCx) {
    // SAFETY: the callout outlives its timer's arming (dropping it drains
    // the timer first).
    let c = unsafe { &*ptr::with_exposed_provenance::<Callout>(arg) };
    if c.queued.swap(true, Ordering::AcqRel) {
        return;
    }
    {
        let mut l = LIST.lock();
        let me = ptr::from_ref(c);
        // SAFETY: under the list lock.
        unsafe { *c.next.get() = ptr::null() };
        if l.tail.is_null() {
            l.head = me;
        } else {
            // SAFETY: the tail is a live queued callout; under the lock.
            unsafe { *(*l.tail).next.get() = me };
        }
        l.tail = me;
    }
    WAKE.wake_one();
}

impl Callout {
    /// A disarmed callout.
    pub(crate) fn new() -> Pin<Box<Self>> {
        Box::pin(Self {
            timer: Timer::new(),
            queued: AtomicBool::new(false),
            next: UnsafeCell::new(ptr::null()),
            func: UnsafeCell::new((|_, _| {}, 0)),
        })
    }

    /// Arms it to run `func(arg)` on the softclock thread `ns`
    /// nanoseconds from now, replacing any arming not yet run
    /// (`callout_reset`).
    pub(crate) fn reset(self: Pin<&Self>, ns: u64, func: SoftFn, arg: usize) {
        self.unqueue();
        {
            let _l = LIST.lock();
            // SAFETY: under the list lock.
            unsafe { *self.func.get() = (func, arg) };
        }
        let me = ptr::from_ref(self.get_ref()).expose_provenance();
        // SAFETY: the timer is a field of a pinned callout.
        let timer = unsafe { self.map_unchecked(|c| &c.timer) };
        timer.reset(ns, fire, me);
    }

    /// [`Callout::reset`] to run at `deadline`, in nanoseconds of uptime.
    pub(crate) fn reset_at(self: Pin<&Self>, deadline: u64, func: SoftFn, arg: usize) {
        let ns = deadline.saturating_sub(crate::time::now_ns());
        self.reset(ns, func, arg);
    }

    /// Takes it off the thread's list if it is there; returns whether it
    /// was.
    fn unqueue(&self) -> bool {
        let mut l = LIST.lock();
        if !self.queued.load(Ordering::Acquire) {
            return false;
        }
        let me = ptr::from_ref(self);
        let mut prev: *const Callout = ptr::null();
        let mut at = l.head;
        while !at.is_null() {
            // SAFETY: a queued callout, under the lock.
            let next = unsafe { *(*at).next.get() };
            if ptr::eq(at, me) {
                if prev.is_null() {
                    l.head = next;
                } else {
                    // SAFETY: as above.
                    unsafe { *(*prev).next.get() = next };
                }
                if ptr::eq(l.tail, me) {
                    l.tail = prev;
                }
                self.queued.store(false, Ordering::Release);
                return true;
            }
            prev = at;
            at = next;
        }
        // Set by `fire`, which has not linked it yet: it will run.
        false
    }

    /// Disarms it (`callout_stop`): returns whether it was pending. A
    /// handler already running is not waited for.
    pub(crate) fn stop(&self) -> bool {
        let armed = self.timer.stop();
        self.unqueue() || armed
    }

    /// Disarms it and waits for its handler to finish (`callout_drain`),
    /// unless the handler is what calls.
    pub(crate) fn drain(&self) {
        let me = ptr::from_ref(self).cast_mut();
        let mine = on_softclock();
        loop {
            // After this the interrupt half has linked what it queued.
            self.timer.drain();
            self.unqueue();
            let busy = self.queued.load(Ordering::Acquire)
                || (!mine && ptr::eq(RUNNING.load(Ordering::Acquire), me));
            if !busy {
                return;
            }
            core::hint::spin_loop();
        }
    }
}

impl Drop for Callout {
    fn drop(&mut self) {
        self.drain();
    }
}

fn on_softclock() -> bool {
    ptr::eq(
        THREAD.load(Ordering::Acquire),
        ptr::from_ref(sched::current()).cast_mut(),
    )
}

/// The softclock thread.
fn run(s: &Sleepable) -> ! {
    THREAD.store(
        ptr::from_ref(sched::current()).cast_mut(),
        Ordering::Release,
    );
    loop {
        WAKE.wait_until(s, || !LIST.lock().head.is_null());
        let next = {
            let mut l = LIST.lock();
            let c = l.head;
            if c.is_null() {
                continue;
            }
            // SAFETY: a queued callout, under the lock.
            let (next, func) = unsafe { (*(*c).next.get(), *(*c).func.get()) };
            l.head = next;
            if next.is_null() {
                l.tail = ptr::null();
            }
            RUNNING.store(c.cast_mut(), Ordering::Release);
            // SAFETY: as above; it stays alive while `RUNNING` names it
            // (a drain waits).
            unsafe { (*c).queued.store(false, Ordering::Release) };
            func
        };
        (next.0)(s, next.1);
        RUNNING.store(ptr::null_mut(), Ordering::Release);
    }
}

/// Starts the softclock thread.
pub(crate) fn init() {
    sched::spawn("softclock", sched::Priority::KERNEL, |s| run(s)).expect("softclock thread");
}
