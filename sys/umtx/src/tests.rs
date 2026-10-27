// SPDX-License-Identifier: BSD-2-Clause
//! Host tests: threads run libthr's user-space halves of each protocol
//! against the kernel halves here, with random yields between memory
//! accesses, and check the model's invariants (mutual exclusion, counts,
//! no lost wakeups, the priorities lent).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "simulated words and times"
)]

use std::boxed::Box;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::vec::Vec;

use abi::errno::Errno;
use abi::time::Timespec;
use abi::umtx::*;

use crate::{Deadline, Env, Fault, Key, Mode, Robust, Share, Thread, Timeout, Type, Umtx, Waiter};

/// Simulated user memory: 64-bit words, 32-bit accesses within them.
struct Memory {
    words: Vec<AtomicU64>,
}

const BASE: u64 = 0x1000;

impl Memory {
    fn new(words: usize) -> Self {
        Self {
            words: (0..words).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn slot(&self, addr: u64) -> Result<(&AtomicU64, u32), Fault> {
        let off = addr.checked_sub(BASE).ok_or(Fault)?;
        let w = self.words.get((off / 8) as usize).ok_or(Fault)?;
        Ok((w, ((off % 8) * 8) as u32))
    }

    fn load32(&self, addr: u64) -> Result<u32, Fault> {
        let (w, shift) = self.slot(addr)?;
        Ok((w.load(Ordering::SeqCst) >> shift) as u32)
    }

    fn cas32(&self, addr: u64, old: u32, new: u32) -> Result<u32, Fault> {
        let (w, shift) = self.slot(addr)?;
        let mask = 0xffff_ffffu64 << shift;
        let mut cur = w.load(Ordering::SeqCst);
        loop {
            let v = (cur >> shift) as u32;
            if v != old {
                return Ok(v);
            }
            let next = (cur & !mask) | (u64::from(new) << shift);
            match w.compare_exchange(cur, next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Ok(old),
                Err(c) => cur = c,
            }
        }
    }

    fn store32(&self, addr: u64, v: u32) -> Result<(), Fault> {
        loop {
            let cur = self.load32(addr)?;
            if self.cas32(addr, cur, v)? == cur {
                return Ok(());
            }
        }
    }
}

struct HostThread {
    lock: Mutex<()>,
    cv: Condvar,
    base: AtomicU8,
    lent: AtomicU8,
    /// Set to make the next sleep end as a signal would.
    signal: AtomicBool,
}

impl HostThread {
    fn new(pri: u8) -> Self {
        Self {
            lock: Mutex::new(()),
            cv: Condvar::new(),
            base: AtomicU8::new(pri),
            lent: AtomicU8::new(crate::PRI_MAX),
            signal: AtomicBool::new(false),
        }
    }
}

impl Thread for HostThread {
    fn unpark(&self) {
        let _g = self.lock.lock().unwrap();
        self.cv.notify_all();
    }

    fn user_pri(&self) -> u8 {
        self.base
            .load(Ordering::SeqCst)
            .min(self.lent.load(Ordering::SeqCst))
    }

    fn base_user_pri(&self) -> u8 {
        self.base.load(Ordering::SeqCst)
    }

    fn lend_user_pri(&self, pri: u8) {
        self.lent.store(pri, Ordering::SeqCst);
    }
}

type Registry = Mutex<HashMap<u32, Arc<Waiter<HostThread>>>>;

struct World {
    umtx: Box<Umtx<HostThread>>,
    mem: Memory,
    threads: Registry,
    start: Instant,
}

impl World {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            umtx: Box::new(Umtx::new()),
            mem: Memory::new(64),
            threads: Mutex::new(HashMap::new()),
            start: Instant::now(),
        })
    }

    fn thread(self: &Arc<Self>, tid: u32, pri: u8) -> HostEnv {
        let me = Arc::new(Waiter::new(HostThread::new(pri), tid));
        self.threads.lock().unwrap().insert(tid, me.clone());
        HostEnv {
            world: self.clone(),
            me,
            rng: AtomicU64::new(u64::from(tid).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
            shared: false,
        }
    }
}

struct HostEnv {
    world: Arc<World>,
    me: Arc<Waiter<HostThread>>,
    rng: AtomicU64,
    /// Keys by object, as for a process-shared mapping.
    shared: bool,
}

impl HostEnv {
    /// Sometimes lets another thread run: the random interleaving.
    fn jitter(&self) {
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        if x.is_multiple_of(4) {
            thread::yield_now();
        }
    }

    fn mem(&self) -> &Memory {
        &self.world.mem
    }

    fn u(&self) -> &Umtx<HostThread> {
        &self.world.umtx
    }

    fn ns(&self) -> u64 {
        self.world.start.elapsed().as_nanos() as u64
    }
}

fn ts(ns: u64) -> Timespec {
    Timespec {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_nsec: (ns % 1_000_000_000) as i64,
    }
}

fn ns_of(t: &Timespec) -> u64 {
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

impl Env for HostEnv {
    type T = HostThread;

    fn me(&self) -> &Arc<Waiter<HostThread>> {
        &self.me
    }

    fn load32(&self, addr: u64) -> Result<u32, Fault> {
        self.jitter();
        self.mem().load32(addr)
    }

    fn load64(&self, addr: u64) -> Result<u64, Fault> {
        self.jitter();
        let lo = self.mem().load32(addr)?;
        let hi = self.mem().load32(addr + 4)?;
        Ok(u64::from(hi) << 32 | u64::from(lo))
    }

    fn store32(&self, addr: u64, v: u32) -> Result<(), Fault> {
        self.jitter();
        self.mem().store32(addr, v)
    }

    fn cas32(&self, addr: u64, old: u32, new: u32) -> Result<u32, Fault> {
        self.jitter();
        self.mem().cas32(addr, old, new)
    }

    fn cas64(&self, _addr: u64, _old: u64, _new: u64) -> Result<u64, Fault> {
        unimplemented!("no 64-bit lock words in these tests")
    }

    fn key(&self, addr: u64, ty: Type, share: Share) -> Result<Key, Errno> {
        self.mem().slot(addr)?;
        Ok(if self.shared && share != Share::Thread {
            Key {
                ty,
                id: crate::Id::Shared {
                    object: 7,
                    offset: addr - BASE,
                },
                hold: None,
            }
        } else {
            Key::private(ty, 1, addr)
        })
    }

    fn check_susp(&self, _sleep: bool) -> Result<(), Errno> {
        Ok(())
    }

    fn sleep(&self, done: &dyn Fn() -> bool, deadline: Option<&Deadline>) -> Result<(), Errno> {
        let t = &self.me.thread;
        let mut g = t.lock.lock().unwrap();
        loop {
            if done() {
                return Ok(());
            }
            if t.signal.swap(false, Ordering::SeqCst) {
                return Err(Errno::EINTR);
            }
            let wait = match deadline {
                Some(d) => {
                    let (end, now) = (ns_of(&d.end), self.ns());
                    if now >= end {
                        return Err(Errno::ETIMEDOUT);
                    }
                    Duration::from_nanos(end - now)
                }
                // Wake now and then regardless: a lost wakeup still shows
                // as a hang, just not a silent one.
                None => Duration::from_millis(50),
            };
            g = t.cv.wait_timeout(g, wait).unwrap().0;
        }
    }

    fn park(&self, done: &dyn Fn() -> bool) {
        let t = &self.me.thread;
        let mut g = t.lock.lock().unwrap();
        while !done() {
            g = t.cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
        }
    }

    fn now(&self, _clock: u32) -> Result<Timespec, Errno> {
        Ok(ts(self.ns()))
    }

    fn find(&self, tid: u32, _shared: bool) -> Option<Arc<Waiter<HostThread>>> {
        self.world.threads.lock().unwrap().get(&tid).cloned()
    }

    fn may_rtprio(&self) -> bool {
        true
    }
}

// Word addresses.
const M: u64 = BASE;
const M_FLAGS: u64 = M + 4;
const CV: u64 = BASE + 64;
const COUNTER: u64 = BASE + 128;
const RW: u64 = BASE + 192;
const SEM: u64 = BASE + 256;
const M2: u64 = BASE + 320;
const WORD: u64 = BASE + 384;

/// libthr's `_thr_umutex_lock`: the fast path, else the kernel.
fn mutex_lock(e: &HostEnv, m: u64) -> Result<(), Errno> {
    let id = e.me.tid;
    let flags = e.mem().load32(m + 4)?;
    if flags & (UMUTEX_PRIO_INHERIT | UMUTEX_PRIO_PROTECT) == 0 {
        if e.cas32(m, UMUTEX_UNOWNED, id)? == UMUTEX_UNOWNED {
            return Ok(());
        }
    } else if flags & UMUTEX_PRIO_INHERIT != 0 && e.cas32(m, UMUTEX_UNOWNED, id)? == UMUTEX_UNOWNED
    {
        return Ok(());
    }
    loop {
        match e.u().lock_umutex(e, m, None, Mode::Lock) {
            Err(Errno::ERESTART) => continue,
            r => return r,
        }
    }
}

/// libthr's `_thr_umutex_unlock`.
fn mutex_unlock(e: &HostEnv, m: u64) -> Result<(), Errno> {
    let id = e.me.tid;
    let flags = e.mem().load32(m + 4)?;
    if flags & UMUTEX_PRIO_PROTECT == 0 && e.cas32(m, id, UMUTEX_UNOWNED)? == id {
        return Ok(());
    }
    e.u().unlock_umutex(e, m, false)
}

/// Runs `n` threads of `body`, each with its own tid.
fn run(world: &Arc<World>, n: u32, body: impl Fn(&HostEnv, u32) + Send + Sync + 'static) {
    let body = Arc::new(body);
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let e = world.thread(100_001 + i, 200);
            let body = body.clone();
            thread::spawn(move || body(&e, i))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

fn mutual_exclusion(flags: u32, shared: bool) {
    let w = World::new();
    w.mem.store32(M_FLAGS, flags).unwrap();
    let inside = Arc::new(AtomicU32::new(0));
    const N: u32 = 6;
    const ITERS: u32 = 400;
    let i2 = inside.clone();
    run(&w, N, move |e, _| {
        let e = HostEnv {
            shared,
            world: e.world.clone(),
            me: e.me.clone(),
            rng: AtomicU64::new(e.rng.load(Ordering::Relaxed)),
        };
        for _ in 0..ITERS {
            mutex_lock(&e, M).unwrap();
            assert_eq!(i2.fetch_add(1, Ordering::SeqCst), 0, "two owners");
            let c = e.mem().load32(COUNTER).unwrap();
            e.jitter();
            e.mem().store32(COUNTER, c + 1).unwrap();
            i2.fetch_sub(1, Ordering::SeqCst);
            mutex_unlock(&e, M).unwrap();
        }
    });
    assert_eq!(w.mem.load32(COUNTER).unwrap(), N * ITERS);
    // Every waiter gone, the mutex free.
    assert_eq!(w.mem.load32(M).unwrap() & !UMUTEX_CONTESTED, 0);
}

#[test]
fn normal_mutex() {
    mutual_exclusion(0, false);
}

#[test]
fn shared_mutex() {
    mutual_exclusion(USYNC_PROCESS_SHARED, true);
}

#[test]
fn pi_mutex() {
    mutual_exclusion(UMUTEX_PRIO_INHERIT, false);
}

#[test]
fn pp_mutex() {
    // A ceiling may not be more urgent than the caller's own priority, so
    // these threads run at the least urgent real-time one.
    let w = World::new();
    w.mem.store32(M_FLAGS, UMUTEX_PRIO_PROTECT).unwrap();
    w.mem.store32(M + 8, 10).unwrap();
    w.mem.store32(M + 12, u32::MAX).unwrap();
    // PP mutexes start contested: every lock goes to the kernel.
    w.mem.store32(M, UMUTEX_CONTESTED).unwrap();
    let inside = Arc::new(AtomicU32::new(0));
    let i2 = inside.clone();
    run(&w, 4, move |e, _| {
        e.me.thread
            .base
            .store(crate::PRI_MIN_REALTIME + 31, Ordering::SeqCst);
        for _ in 0..200 {
            mutex_lock(e, M).unwrap();
            // Raised to the ceiling: RTP_PRIO_MAX - 10 above the best.
            assert_eq!(e.me.thread.user_pri(), crate::PRI_MIN_REALTIME + 21);
            assert_eq!(i2.fetch_add(1, Ordering::SeqCst), 0, "two owners");
            i2.fetch_sub(1, Ordering::SeqCst);
            mutex_unlock(e, M).unwrap();
            assert_eq!(e.me.thread.lent.load(Ordering::SeqCst), crate::PRI_MAX);
        }
    });
    assert_eq!(w.mem.load32(M).unwrap(), UMUTEX_CONTESTED);
}

#[test]
fn trylock_and_timeout() {
    let w = World::new();
    let a = w.thread(100_001, 200);
    let b = w.thread(100_002, 200);
    mutex_lock(&a, M).unwrap();
    assert_eq!(
        w.umtx.lock_umutex(&b, M, None, Mode::Try),
        Err(Errno::EBUSY)
    );
    let t = Timeout {
        clock: abi::time::clock::MONOTONIC,
        absolute: false,
        time: ts(20_000_000),
    };
    let start = Instant::now();
    assert_eq!(
        w.umtx.lock_umutex(&b, M, Some(&t), Mode::Lock),
        Err(Errno::ETIMEDOUT)
    );
    assert!(start.elapsed() >= Duration::from_millis(20));
    // The timed-out waiter left the contested bit; the unlock clears it.
    assert_eq!(w.mem.load32(M).unwrap(), a.me.tid | UMUTEX_CONTESTED);
    mutex_unlock(&a, M).unwrap();
    assert_eq!(w.mem.load32(M).unwrap(), UMUTEX_UNOWNED);
    // Not the owner.
    assert_eq!(w.umtx.unlock_umutex(&b, M, false), Err(Errno::EPERM));
    // A signal ends a lock without a timeout with a restart.
    mutex_lock(&a, M).unwrap();
    b.me.thread.signal.store(true, Ordering::SeqCst);
    assert_eq!(
        w.umtx.lock_umutex(&b, M, None, Mode::Lock),
        Err(Errno::ERESTART)
    );
}

#[test]
fn condvar() {
    let w = World::new();
    const N: u32 = 4;
    const ITEMS: u32 = 300;
    // Producers add to COUNTER (the queue length), consumers take.
    let taken = Arc::new(AtomicU32::new(0));
    let t2 = taken.clone();
    run(&w, 2 * N, move |e, i| {
        for _ in 0..ITEMS {
            mutex_lock(e, M).unwrap();
            if i < N {
                let c = e.mem().load32(COUNTER).unwrap();
                e.mem().store32(COUNTER, c + 1).unwrap();
                // pthread_cond_signal: the kernel only with waiters.
                if e.mem().load32(CV).unwrap() != 0 {
                    e.u().cv_signal(e, CV).unwrap();
                }
            } else {
                while e.mem().load32(COUNTER).unwrap() == 0 {
                    match e.u().cv_wait(e, CV, M, 0, crate::CvTime::Forever) {
                        Ok(()) | Err(Errno::EINTR) => {}
                        Err(x) => panic!("cv_wait: {x:?}"),
                    }
                    mutex_lock(e, M).unwrap();
                }
                let c = e.mem().load32(COUNTER).unwrap();
                e.mem().store32(COUNTER, c - 1).unwrap();
                t2.fetch_add(1, Ordering::SeqCst);
            }
            mutex_unlock(e, M).unwrap();
        }
    });
    assert_eq!(taken.load(Ordering::SeqCst), N * ITEMS);
    assert_eq!(w.mem.load32(COUNTER).unwrap(), 0);
}

#[test]
fn rwlock() {
    let w = World::new();
    let readers = Arc::new(AtomicU32::new(0));
    let writers = Arc::new(AtomicU32::new(0));
    let (r2, w2) = (readers.clone(), writers.clone());
    run(&w, 6, move |e, i| {
        for _ in 0..300 {
            if i % 3 == 0 {
                // _thr_rwlock_wrlock.
                if e.cas32(RW, 0, URWLOCK_WRITE_OWNER).unwrap() != 0 {
                    e.u().rw_wrlock(e, RW, None).unwrap();
                }
                assert_eq!(w2.fetch_add(1, Ordering::SeqCst), 0);
                assert_eq!(r2.load(Ordering::SeqCst), 0);
                w2.fetch_sub(1, Ordering::SeqCst);
            } else {
                // _thr_rwlock_tryrdlock, writers preferred.
                let mut got = false;
                let mut s = e.load32(RW).unwrap();
                while s & (URWLOCK_WRITE_OWNER | URWLOCK_WRITE_WAITERS) == 0 {
                    let old = e.cas32(RW, s, s + 1).unwrap();
                    if old == s {
                        got = true;
                        break;
                    }
                    s = old;
                }
                if !got {
                    e.u().rw_rdlock(e, RW, 0, None).unwrap();
                }
                r2.fetch_add(1, Ordering::SeqCst);
                assert_eq!(w2.load(Ordering::SeqCst), 0);
                r2.fetch_sub(1, Ordering::SeqCst);
            }
            // _thr_rwlock_unlock.
            let mut s = e.load32(RW).unwrap();
            let fast = if s & URWLOCK_WRITE_OWNER != 0 {
                e.cas32(RW, URWLOCK_WRITE_OWNER, 0).unwrap() == URWLOCK_WRITE_OWNER
            } else {
                loop {
                    if s & (URWLOCK_WRITE_WAITERS | URWLOCK_READ_WAITERS) != 0
                        && s & URWLOCK_MAX_READERS == 1
                    {
                        break false;
                    }
                    let old = e.cas32(RW, s, s - 1).unwrap();
                    if old == s {
                        break true;
                    }
                    s = old;
                }
            };
            if !fast {
                e.u().rw_unlock(e, RW).unwrap();
            }
        }
    });
    assert_eq!(w.mem.load32(RW).unwrap(), 0);
    assert_eq!(w.mem.load32(RW + 8).unwrap(), 0);
    assert_eq!(w.mem.load32(RW + 12).unwrap(), 0);
}

#[test]
fn semaphore() {
    let w = World::new();
    const N: u32 = 3;
    const POSTS: u32 = 400;
    let got = Arc::new(AtomicU32::new(0));
    let g2 = got.clone();
    run(&w, 2 * N, move |e, i| {
        for _ in 0..POSTS {
            if i < N {
                // sem_post: add one; the kernel if waiters.
                let mut c = e.load32(SEM).unwrap();
                loop {
                    let old = e.cas32(SEM, c, c + 1).unwrap();
                    if old == c {
                        break;
                    }
                    c = old;
                }
                if c & USEM_HAS_WAITERS != 0 {
                    e.u().sem2_wake(e, SEM).unwrap();
                }
            } else {
                // sem_wait: take one, else wait for a post.
                loop {
                    let c = e.load32(SEM).unwrap();
                    if c & USEM_MAX_COUNT != 0 {
                        if e.cas32(SEM, c, c - 1).unwrap() == c {
                            break;
                        }
                        continue;
                    }
                    match e.u().sem2_wait(e, SEM, None) {
                        Ok(()) | Err(Errno::EINTR) => {}
                        Err(x) => panic!("sem2_wait: {x:?}"),
                    }
                }
                g2.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    assert_eq!(got.load(Ordering::SeqCst), N * POSTS);
    assert_eq!(w.mem.load32(SEM).unwrap() & USEM_MAX_COUNT, 0);
}

#[test]
fn wait_and_wake() {
    let w = World::new();
    run(&w, 2, |e, i| {
        for round in 0..500u32 {
            if i == 0 {
                // Wait for the word to become `round + 1`.
                loop {
                    let v = e.load32(WORD).unwrap();
                    if v > round {
                        break;
                    }
                    e.u()
                        .wait(e, WORD, u64::from(v), None, crate::Width::Uint, true)
                        .unwrap();
                }
            } else {
                e.store32(WORD, round + 1).unwrap();
                e.u().wake(e, WORD, 1, true).unwrap();
            }
        }
    });
}

#[test]
fn priority_inheritance_chain() {
    let w = World::new();
    w.mem.store32(M_FLAGS, UMUTEX_PRIO_INHERIT).unwrap();
    w.mem.store32(M2 + 4, UMUTEX_PRIO_INHERIT).unwrap();
    // A (time-sharing) holds M2; B (real-time 60) holds M and blocks on
    // M2; C (real-time 50) blocks on M. C's priority reaches A.
    let a = w.thread(100_001, 200);
    let b = Arc::new(w.thread(100_002, 60));
    let c = Arc::new(w.thread(100_003, 50));
    mutex_lock(&a, M2).unwrap();
    mutex_lock(&b, M).unwrap();
    let b2 = b.clone();
    let tb = thread::spawn(move || {
        mutex_lock(&b2, M2).unwrap();
        mutex_unlock(&b2, M2).unwrap();
        mutex_unlock(&b2, M).unwrap();
    });
    let until = |f: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out");
            thread::sleep(Duration::from_millis(1));
        }
    };
    until(&|| a.me.thread.user_pri() == 60);
    let c2 = c.clone();
    let tc = thread::spawn(move || {
        mutex_lock(&c2, M).unwrap();
        mutex_unlock(&c2, M).unwrap();
    });
    until(&|| a.me.thread.user_pri() == 50);
    assert_eq!(b.me.thread.user_pri(), 50);
    mutex_unlock(&a, M2).unwrap();
    assert_eq!(a.me.thread.user_pri(), 200);
    tb.join().unwrap();
    tc.join().unwrap();
    assert_eq!(b.me.thread.user_pri(), 60);
    assert_eq!(w.mem.load32(M).unwrap(), 0);
    assert_eq!(w.mem.load32(M2).unwrap(), 0);
}

#[test]
fn pi_deadlock_and_owner_dead() {
    let w = World::new();
    w.mem.store32(M_FLAGS, UMUTEX_PRIO_INHERIT).unwrap();
    let a = w.thread(100_001, 200);
    mutex_lock(&a, M).unwrap();
    assert_eq!(
        w.umtx.lock_umutex(&a, M, None, Mode::Lock),
        Err(Errno::EDEADLK)
    );
}

#[test]
fn robust_owner_dies() {
    let w = World::new();
    w.mem.store32(M_FLAGS, UMUTEX_ROBUST).unwrap();
    // The list head word, pointing at the mutex; its link ends the list.
    const HEAD: u64 = BASE + 448;
    w.mem.store32(HEAD, M as u32).unwrap();
    let a = w.thread(100_001, 200);
    let b = Arc::new(w.thread(100_002, 200));
    mutex_lock(&a, M).unwrap();
    w.umtx
        .set_robust_lists(
            &a,
            Robust {
                list: HEAD,
                ..Robust::default()
            },
        )
        .unwrap();
    let b2 = b.clone();
    let tb = thread::spawn(move || w_lock(&b2));
    thread::sleep(Duration::from_millis(20));
    // A exits holding it.
    w.umtx.thread_exit(&a);
    assert_eq!(tb.join().unwrap(), Err(Errno::EOWNERDEAD));
    assert_eq!(w.mem.load32(M).unwrap() & !UMUTEX_CONTESTED, b.me.tid);
    // Unlocked without being made consistent: not recoverable.
    w.mem
        .store32(M_FLAGS, UMUTEX_ROBUST | UMUTEX_NONCONSISTENT)
        .unwrap();
    w.umtx.unlock_umutex(&*b, M, false).unwrap();
    assert_eq!(w.mem.load32(M).unwrap(), UMUTEX_RB_NOTRECOV);
    assert_eq!(
        w.umtx.lock_umutex(&*b, M, None, Mode::Lock),
        Err(Errno::ENOTRECOVERABLE)
    );
}

fn w_lock(e: &HostEnv) -> Result<(), Errno> {
    mutex_lock(e, M)
}

#[test]
fn set_ceiling_returns_old() {
    let w = World::new();
    w.mem.store32(M_FLAGS, UMUTEX_PRIO_PROTECT).unwrap();
    w.mem.store32(M, UMUTEX_CONTESTED).unwrap();
    w.mem.store32(M + 8, 5).unwrap();
    let a = w.thread(100_001, 60);
    assert_eq!(w.umtx.set_ceiling(&a, M, 9), Ok(5));
    assert_eq!(w.mem.load32(M + 8).unwrap(), 9);
    assert_eq!(w.mem.load32(M).unwrap(), UMUTEX_CONTESTED);
    assert_eq!(w.umtx.set_ceiling(&a, M, 32), Err(Errno::EINVAL));
}
