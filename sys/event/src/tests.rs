// SPDX-License-Identifier: BSD-2-Clause
//! Host tests: FreeBSD's knote state machine, scenario by scenario, and
//! concurrent notification against scans.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::event::{
    EV_ADD, EV_CLEAR, EV_DELETE, EV_DISABLE, EV_DISPATCH, EV_ENABLE, EV_EOF, EV_KEEPUDATA,
    EV_ONESHOT, EVFILT_READ, EVFILT_USER, Kevent, NOTE_FFCOPY, NOTE_FFOR, NOTE_TRIGGER,
};

use crate::{Filter, Ident, Knote, Kqueue, KqueueFilter, Note, Notify, Source, UserFilter, Wake};

/// A pollable object: `level` bytes readable.
struct Obj {
    level: AtomicUsize,
    notify: Notify,
}

impl Obj {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            level: AtomicUsize::new(0),
            notify: Notify::new(),
        })
    }

    fn set(&self, n: usize) {
        self.level.store(n, Ordering::SeqCst);
        self.notify.notify(0);
    }
}

struct ReadFilter(Arc<Obj>);

impl Filter for ReadFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.0.notify.add(kn);
        Ok(())
    }
    fn detach(&self, kn: &Knote) {
        self.0.notify.remove(kn);
    }
    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        note.kev.data = self.0.level.load(Ordering::SeqCst) as i64;
        note.kev.data > 0
    }
    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self(Arc::clone(&self.0))))
    }
}

/// Descriptors `0..objs.len()` are the objects; ident `KQ` is a kqueue.
struct Table {
    objs: Vec<Arc<Obj>>,
    kq: Option<Arc<Kqueue>>,
}

const KQ: u64 = 100;

impl Source for Table {
    fn ident(&self, kev: &Kevent) -> Result<Ident, Errno> {
        match kev.filter {
            EVFILT_READ => {
                let ok = usize::try_from(kev.ident).is_ok_and(|i| i < self.objs.len())
                    || (kev.ident == KQ && self.kq.is_some());
                if ok { Ok(Ident::Fd) } else { Err(Errno::EBADF) }
            }
            EVFILT_USER => Ok(Ident::Other),
            _ => Err(Errno::EINVAL),
        }
    }
    fn filter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        match kev.filter {
            EVFILT_READ if kev.ident == KQ => Ok(Box::new(KqueueFilter(Arc::clone(
                self.kq.as_ref().unwrap(),
            )))),
            EVFILT_READ => Ok(Box::new(ReadFilter(Arc::clone(
                &self.objs[usize::try_from(kev.ident).unwrap()],
            )))),
            _ => Ok(Box::new(UserFilter)),
        }
    }
}

#[derive(Default)]
struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn kq() -> Arc<Kqueue> {
    Kqueue::new(Box::new(Flag::default()))
}

fn table(n: usize) -> Table {
    Table {
        objs: (0..n).map(|_| Obj::new()).collect(),
        kq: None,
    }
}

fn kev(ident: u64, filter: i16, flags: u16, fflags: u32, data: i64, udata: u64) -> Kevent {
    Kevent {
        ident,
        filter,
        flags,
        fflags,
        data,
        udata,
        ext: [0; 4],
    }
}

fn scan(kq: &Kqueue, max: usize) -> Vec<Kevent> {
    let pass = kq.begin_scan();
    let mut out = Vec::new();
    loop {
        let mut buf = [Kevent::default(); crate::KQ_NEVENTS];
        let room = (max - out.len()).min(buf.len());
        let n = kq.collect(pass, &mut buf[..room]);
        out.extend_from_slice(&buf[..n]);
        if n < room || out.len() == max {
            return out;
        }
    }
}

#[test]
fn level_triggered_until_drained() {
    let t = table(1);
    let q = kq();
    q.register(&kev(0, EVFILT_READ, EV_ADD, 0, 0, 7), &t)
        .unwrap();
    assert!(scan(&q, 8).is_empty());
    t.objs[0].set(5);
    for _ in 0..3 {
        let e = scan(&q, 8);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].ident, e[0].data, e[0].udata), (0, 5, 7));
    }
    t.objs[0].level.store(0, Ordering::SeqCst);
    assert!(scan(&q, 8).is_empty());
    assert_eq!(q.pending(), 0);
}

#[test]
fn clear_reports_once_per_notification() {
    let t = table(1);
    let q = kq();
    t.objs[0].set(1);
    q.register(&kev(0, EVFILT_READ, EV_ADD | EV_CLEAR, 0, 0, 0), &t)
        .unwrap();
    assert_eq!(scan(&q, 8).len(), 1);
    assert!(scan(&q, 8).is_empty());
    t.objs[0].set(2);
    assert_eq!(scan(&q, 8)[0].data, 2);
    assert!(scan(&q, 8).is_empty());
}

#[test]
fn oneshot_goes_after_its_report() {
    let t = table(1);
    let q = kq();
    t.objs[0].set(1);
    q.register(&kev(0, EVFILT_READ, EV_ADD | EV_ONESHOT, 0, 0, 0), &t)
        .unwrap();
    assert_eq!(q.knotes(), 1);
    assert_eq!(scan(&q, 8).len(), 1);
    assert_eq!(q.knotes(), 0);
    assert_eq!(t.objs[0].notify.knotes(), 0);
    t.objs[0].set(3);
    assert!(scan(&q, 8).is_empty());
}

#[test]
fn dispatch_disables_until_enabled() {
    let t = table(1);
    let q = kq();
    t.objs[0].set(1);
    q.register(&kev(0, EVFILT_READ, EV_ADD | EV_DISPATCH, 0, 0, 0), &t)
        .unwrap();
    assert_eq!(scan(&q, 8).len(), 1);
    t.objs[0].set(2);
    assert!(scan(&q, 8).is_empty());
    q.register(&kev(0, EVFILT_READ, EV_ENABLE, 0, 0, 0), &t)
        .unwrap();
    assert_eq!(scan(&q, 8).len(), 1);
}

#[test]
fn disabled_at_add_then_enabled() {
    let t = table(1);
    let q = kq();
    t.objs[0].set(1);
    q.register(&kev(0, EVFILT_READ, EV_ADD | EV_DISABLE, 0, 0, 0), &t)
        .unwrap();
    assert!(scan(&q, 8).is_empty());
    q.register(&kev(0, EVFILT_READ, EV_ENABLE, 0, 0, 0), &t)
        .unwrap();
    assert_eq!(scan(&q, 8).len(), 1);
}

#[test]
fn registration_errors() {
    let t = table(1);
    let q = kq();
    let e = |k| q.register(&k, &t).unwrap_err();
    assert_eq!(e(kev(0, EVFILT_READ, EV_DELETE, 0, 0, 0)), Errno::ENOENT);
    assert_eq!(
        e(kev(0, EVFILT_READ, EV_ADD | EV_KEEPUDATA, 0, 0, 0)),
        Errno::EINVAL
    );
    assert_eq!(
        e(kev(
            0,
            EVFILT_READ,
            EV_ADD | EV_ENABLE | EV_DISABLE,
            0,
            0,
            0
        )),
        Errno::EINVAL
    );
    assert_eq!(e(kev(9, EVFILT_READ, EV_ADD, 0, 0, 0)), Errno::EBADF);
    assert_eq!(e(kev(0, -50, EV_ADD, 0, 0, 0)), Errno::EINVAL);
}

#[test]
fn readd_updates_udata_unless_kept() {
    let t = table(1);
    let q = kq();
    t.objs[0].set(1);
    q.register(&kev(0, EVFILT_READ, EV_ADD, 0, 0, 1), &t)
        .unwrap();
    q.register(&kev(0, EVFILT_READ, EV_ADD, 0, 0, 2), &t)
        .unwrap();
    assert_eq!(scan(&q, 8)[0].udata, 2);
    q.register(&kev(0, EVFILT_READ, EV_ENABLE | EV_KEEPUDATA, 0, 0, 3), &t)
        .unwrap();
    assert_eq!(scan(&q, 8)[0].udata, 2);
    assert_eq!(q.knotes(), 1);
}

#[test]
fn delete_detaches() {
    let t = table(1);
    let q = kq();
    t.objs[0].set(1);
    q.register(&kev(0, EVFILT_READ, EV_ADD, 0, 0, 0), &t)
        .unwrap();
    q.register(&kev(0, EVFILT_READ, EV_DELETE, 0, 0, 0), &t)
        .unwrap();
    assert_eq!(q.knotes(), 0);
    assert_eq!(q.pending(), 0);
    assert_eq!(t.objs[0].notify.knotes(), 0);
    assert!(scan(&q, 8).is_empty());
}

#[test]
fn user_events() {
    let t = table(0);
    let q = kq();
    q.register(
        &kev(1, EVFILT_USER, EV_ADD | EV_CLEAR, NOTE_TRIGGER, 0, 0),
        &t,
    )
    .unwrap();
    // A trigger at add is ignored, as FreeBSD's.
    assert!(scan(&q, 8).is_empty());
    q.register(
        &kev(1, EVFILT_USER, 0, NOTE_TRIGGER | NOTE_FFCOPY | 0x11, 5, 0),
        &t,
    )
    .unwrap();
    let e = scan(&q, 8);
    assert_eq!((e.len(), e[0].fflags, e[0].data), (1, 0x11, 5));
    assert!(scan(&q, 8).is_empty());
    q.register(
        &kev(1, EVFILT_USER, 0, NOTE_TRIGGER | NOTE_FFOR | 0x100, 0, 0),
        &t,
    )
    .unwrap();
    assert_eq!(scan(&q, 8)[0].fflags, 0x111);
}

#[test]
fn a_scan_reports_each_level_event_once() {
    let t = table(3);
    let q = kq();
    for (i, o) in t.objs.iter().enumerate() {
        o.set(1);
        q.register(&kev(i as u64, EVFILT_READ, EV_ADD, 0, 0, 0), &t)
            .unwrap();
    }
    assert_eq!(scan(&q, 10).len(), 3);
    assert_eq!(scan(&q, 2).len(), 2);
    // The one left over comes first next time.
    assert_eq!(scan(&q, 1)[0].ident, 2);
}

#[test]
fn kqueue_in_kqueue() {
    let mut t = table(1);
    let inner = kq();
    let outer = kq();
    t.kq = Some(Arc::clone(&inner));
    outer
        .register(&kev(KQ, EVFILT_READ, EV_ADD, 0, 0, 0), &t)
        .unwrap();
    assert!(scan(&outer, 8).is_empty());
    inner
        .register(&kev(0, EVFILT_READ, EV_ADD, 0, 0, 0), &t)
        .unwrap();
    t.objs[0].set(1);
    let e = scan(&outer, 8);
    assert_eq!((e.len(), e[0].data), (1, 1));
    // Each watching the other terminates.
    let mut t2 = table(0);
    t2.kq = Some(Arc::clone(&outer));
    inner
        .register(&kev(KQ, EVFILT_READ, EV_ADD, 0, 0, 0), &t2)
        .unwrap();
    t.objs[0].set(2);
    inner.close();
    assert_eq!(inner.knotes(), 0);
}

#[test]
fn closing_a_descriptor_drops_its_knotes() {
    let t = table(2);
    let q = kq();
    for i in 0..2 {
        t.objs[i].set(1);
        q.register(&kev(i as u64, EVFILT_READ, EV_ADD, 0, 0, 0), &t)
            .unwrap();
    }
    q.fd_closed(0);
    assert_eq!(q.knotes(), 1);
    assert_eq!(t.objs[0].notify.knotes(), 0);
    let e = scan(&q, 8);
    assert_eq!((e.len(), e[0].ident), (1, 1));
}

#[test]
fn a_vanishing_object_reports_eof_once() {
    let t = table(1);
    let q = kq();
    q.register(&kev(0, EVFILT_READ, EV_ADD, 0, 0, 0), &t)
        .unwrap();
    t.objs[0].set(1);
    t.objs[0].notify.clear();
    let e = scan(&q, 8);
    assert_eq!(e.len(), 1);
    assert_ne!(e[0].flags & EV_EOF, 0);
    assert_eq!(q.knotes(), 0);
}

#[test]
fn pollers_wake_once_and_are_pruned() {
    let o = Obj::new();
    let w: Arc<dyn Wake> = Arc::new(Flag::default());
    o.notify.record(&w);
    o.notify.record(&w);
    assert!(o.notify.is_armed());
    o.set(1);
    let woke = Arc::clone(&w);
    drop(woke);
    assert!(!o.notify.is_armed());
    let dead: Arc<dyn Wake> = Arc::new(Flag::default());
    o.notify.record(&dead);
    drop(dead);
    o.notify.record(&w);
    o.set(2);
    assert!(!o.notify.is_armed());
}

#[test]
fn fork_copies_what_copies() {
    let t = table(1);
    let q = kq();
    t.objs[0].set(1);
    q.register(&kev(0, EVFILT_READ, EV_ADD, 0, 0, 4), &t)
        .unwrap();
    q.register(&kev(3, EVFILT_USER, EV_ADD, 0, 0, 0), &t)
        .unwrap();
    let child = kq();
    q.fork_into(&child, &|fd| fd == 0);
    assert_eq!(child.knotes(), 2);
    let e = scan(&child, 8);
    assert_eq!((e.len(), e[0].udata), (1, 4));
    let child2 = kq();
    q.fork_into(&child2, &|_| false);
    assert_eq!(child2.knotes(), 1);
}

#[test]
fn notifications_race_scans() {
    use std::thread;
    let t = Arc::new(table(8));
    let q = kq();
    for i in 0..8 {
        let f = if i % 2 == 0 {
            EV_ADD | EV_CLEAR
        } else {
            EV_ADD
        };
        q.register(&kev(i, EVFILT_READ, f, 0, 0, 0), &*t).unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let t = Arc::clone(&t);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let o = &t.objs[(n + w) % 8];
                    o.set(n % 3);
                    n += 1;
                }
            })
        })
        .collect();
    let reader = {
        let q = Arc::clone(&q);
        thread::spawn(move || {
            for _ in 0..20_000 {
                let e = scan(&q, 16);
                let mut seen = [0u8; 8];
                for k in &e {
                    seen[usize::try_from(k.ident).unwrap()] += 1;
                }
                assert!(seen.iter().all(|&c| c <= 1), "{e:?}");
            }
        })
    };
    reader.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }
    for o in &t.objs {
        o.level.store(0, Ordering::SeqCst);
    }
    // Level-triggered knotes fall out once their objects drain; cleared
    // ones may still hold one report.
    let _ = scan(&q, 16);
    assert!(scan(&q, 16).is_empty());
    assert_eq!(q.pending(), 0);
    for i in 0..8 {
        q.register(&kev(i, EVFILT_READ, EV_DELETE, 0, 0, 0), &*t)
            .unwrap();
    }
    assert_eq!(q.knotes(), 0);
    assert!(t.objs.iter().all(|o| o.notify.knotes() == 0));
}
