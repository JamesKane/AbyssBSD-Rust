// SPDX-License-Identifier: BSD-2-Clause

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

use crate::Epoch;

fn pause() {
    thread::yield_now();
}

#[test]
fn a_grace_period_waits_for_sections() {
    static E: Epoch = Epoch::new("test");
    let s = E.enter();
    let done = Arc::new(AtomicBool::new(false));
    let d = Arc::clone(&done);
    let t = thread::spawn(move || {
        E.synchronize(&mut pause);
        d.store(true, Ordering::SeqCst);
    });
    for _ in 0..50 {
        thread::yield_now();
    }
    assert!(!done.load(Ordering::SeqCst), "returned inside a section");
    drop(s);
    t.join().unwrap();
    assert!(done.load(Ordering::SeqCst));
    // With no sections it returns at once; nested sections are fine.
    E.synchronize(&mut pause);
    let a = E.enter();
    let b = E.enter();
    drop((b, a));
    E.synchronize(&mut pause);
}

#[test]
fn a_late_reader_is_waited_for() {
    // A reader read the phase before a switch and counted itself after
    // that grace period ended: the next one must wait for it.
    static E: Epoch = Epoch::new("late");
    let stale = E.phase.load(Ordering::Relaxed);
    E.synchronize(&mut pause);
    let s = E.enter_in(stale);
    let done = Arc::new(AtomicBool::new(false));
    let d = Arc::clone(&done);
    let t = thread::spawn(move || {
        E.synchronize(&mut pause);
        d.store(true, Ordering::SeqCst);
    });
    for _ in 0..50 {
        thread::yield_now();
    }
    assert!(!done.load(Ordering::SeqCst));
    drop(s);
    t.join().unwrap();
}

#[test]
fn deferred_calls_run_in_order_after_their_sections() {
    static E: Epoch = Epoch::new("calls");
    let log = Arc::new(Mutex::new(Vec::new()));
    let s = E.enter();
    for i in 0..3 {
        let l = Arc::clone(&log);
        E.call(Box::new(move || l.lock().unwrap().push(i)));
    }
    // Nothing is due while the section that might see them is open.
    assert_eq!(E.run_due(), 0);
    let reclaimer = thread::spawn(|| E.reclaim(&mut pause));
    for _ in 0..50 {
        thread::yield_now();
    }
    assert!(log.lock().unwrap().is_empty());
    drop(s);
    assert_eq!(reclaimer.join().unwrap(), 3);
    assert_eq!(*log.lock().unwrap(), [0, 1, 2]);

    // A call made after a grace period waits for the next one.
    let l = Arc::clone(&log);
    E.call(Box::new(move || l.lock().unwrap().push(3)));
    assert_eq!(E.run_due(), 0);
    E.drain(&mut pause);
    assert_eq!(E.pending(), 0);
    assert_eq!(*log.lock().unwrap(), [0, 1, 2, 3]);
}

/// Readers follow a pointer the writer replaces, freeing the old object
/// through the epoch; a reader never sees a freed one.
#[test]
fn readers_never_see_a_freed_object() {
    struct Obj {
        live: AtomicBool,
        n: usize,
    }
    struct Old(*mut Obj);
    // SAFETY: handed to the one deferred call that frees it.
    unsafe impl Send for Old {}
    static E: Epoch = Epoch::new("stress");
    static CUR: AtomicPtr<Obj> = AtomicPtr::new(core::ptr::null_mut());
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let (writes, reads) = if cfg!(miri) { (20, 50) } else { (2000, 20000) };
    CUR.store(
        Box::into_raw(Box::new(Obj {
            live: AtomicBool::new(true),
            n: 0,
        })),
        Ordering::Release,
    );
    let readers: Vec<_> = (0..3)
        .map(|_| {
            thread::spawn(move || {
                let mut last = 0;
                for _ in 0..reads {
                    let _s = E.enter();
                    let p = CUR.load(Ordering::Acquire);
                    // SAFETY: published objects are freed only after
                    // every section that could load them has ended.
                    let o = unsafe { &*p };
                    assert!(o.live.load(Ordering::Relaxed), "freed object seen");
                    assert!(o.n >= last);
                    last = o.n;
                }
            })
        })
        .collect();
    for n in 1..=writes {
        let new = Box::into_raw(Box::new(Obj {
            live: AtomicBool::new(true),
            n,
        }));
        let old = Old(CUR.swap(new, Ordering::AcqRel));
        E.call(Box::new(move || {
            let old = old;
            // SAFETY: unlinked before the call; no section sees it now.
            let o = unsafe { Box::from_raw(old.0) };
            o.live.store(false, Ordering::Relaxed);
            FREED.fetch_add(1, Ordering::Relaxed);
        }));
        if n % 7 == 0 {
            E.reclaim(&mut pause);
        }
    }
    for r in readers {
        r.join().unwrap();
    }
    E.drain(&mut pause);
    assert_eq!(FREED.load(Ordering::Relaxed), writes);
    // SAFETY: the last object, no readers left.
    drop(unsafe { Box::from_raw(CUR.load(Ordering::Acquire)) });
}
