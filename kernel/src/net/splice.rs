// SPDX-License-Identifier: BSD-2-Clause
//! Socket splicing's kernel half: the worker thread that moves spliced
//! data ([`inet::splice::run`]), the timeouts a splice may have, and
//! `setsockopt(SO_SPLICE)`'s argument.
//!
//! One worker serves every splice. It runs at a time-sharing priority,
//! as FreeBSD's drops its workers to `PUSER` after their first batch: a
//! splice loop keeps it busy for as long as the loop lasts, and it must
//! not starve the threads that would end it.
//!
//! Behavior reference: sys/kern/uipc_socket.c (`splice_init`,
//! `splice_work_thread`, `so_splice`'s `taskqueue_enqueue_timeout_sbt`,
//! `so_splice_timeout`).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::pin::Pin;

use alloc::boxed::Box;
use inet::splice::Splice;
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;

use crate::softclock::Callout;

// SAFETY: `struct splice` is integers, its alignment pad an explicit
// field, and a `timeval` (two integers), with no padding.
unsafe impl crate::uaccess::Plain for abi::socket::Splice {}

static WORK: WaitQueue = WaitQueue::new();

fn wake_worker() {
    WORK.wake_one();
}

/// The worker (`splice_work_thread`).
fn worker(s: &Sleepable) {
    sched::set_thread_priority(sched::current(), sched::Priority::TIMESHARE);
    loop {
        WORK.wait_until(s, inet::splice::pending);
        inet::splice::run();
    }
}

/// Starts the worker.
pub(crate) fn init() {
    inet::splice::set_worker(wake_worker);
    sched::spawn("so_splice", sched::Priority::TIMESHARE, worker).expect("so_splice thread");
}

/// The splices with a timeout, and when each expires (uptime, ns).
static TIMEOUTS: SpinLock<Vec<(u64, Weak<Splice>)>> = SpinLock::new(Vec::new());
static CALLOUT: sync::Once<Pin<Box<Callout>>> = sync::Once::new();

fn callout() -> Pin<&'static Callout> {
    if CALLOUT.get().is_none() {
        let _ = CALLOUT.set(Callout::new());
    }
    CALLOUT
        .get()
        .expect("the splice callout, just set")
        .as_ref()
}

/// Unsplices `sp` `ns` nanoseconds from now unless it ends first.
pub(crate) fn arm(sp: &Arc<Splice>, ns: u64) {
    let c = callout();
    let deadline = crate::time::now_ns().saturating_add(ns);
    let mut t = TIMEOUTS.lock();
    t.push((deadline, Arc::downgrade(sp)));
    let first = t.iter().map(|e| e.0).min().unwrap_or(deadline);
    // Under the list's lock, so an expiry rearming meanwhile cannot put
    // it later than this.
    c.reset_at(first, expire, 0);
}

/// The callout (`so_splice_timeout` for each splice due).
fn expire(_: &Sleepable, _: usize) {
    let now = crate::time::now_ns();
    let mut due = Vec::new();
    {
        let mut t = TIMEOUTS.lock();
        t.retain(|(at, w)| {
            if *at <= now {
                due.push(w.clone());
                false
            } else {
                w.strong_count() > 0
            }
        });
        if let Some(first) = t.iter().map(|e| e.0).min() {
            callout().reset_at(first, expire, 0);
        }
    }
    for w in due {
        if let Some(sp) = w.upgrade() {
            sp.expire();
        }
    }
}
