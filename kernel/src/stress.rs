// SPDX-License-Identifier: BSD-2-Clause
//! The Phase 2 stress test (`abyss.stress=SECONDS`): 64 kernel threads
//! doing lock ping-pong, ordered multi-lock contention, shared/exclusive
//! locking, allocator churn, and sleeps with timeouts, for the given time.
//! At the end every thread must have made progress, the witness must have
//! seen no violations, and heap and page accounting must balance.
//!
//! `cargo xtask test --tier stress` runs it on `-smp 8`.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sched::Priority;
use sched::lock::{LockClass, Mutex, SxLock, witness};

const THREADS: usize = 64;

static PING: LockClass = LockClass::new("stress ping", 100);
static LEVEL_A: LockClass = LockClass::new("stress level a", 110);
static LEVEL_B: LockClass = LockClass::new("stress level b", 120);
static SX: LockClass = LockClass::new("stress sx", 130);

/// A small xorshift generator; each thread has its own.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A ping-pong court: two players alternate through a mutex-protected
/// turn and a wait queue. `seen` mirrors the turn for the wait condition,
/// which runs under a spin lock and so cannot take the mutex.
struct Court {
    turn: Mutex<u64>,
    seen: AtomicU64,
    q: Arc<sched::WaitQueue>,
}

struct Shared {
    stop: AtomicBool,
    courts: Vec<Court>,
    level_a: Vec<Mutex<u64>>,
    level_b: Vec<Mutex<u64>>,
    table: SxLock<Vec<u64>>,
    wake_q: Arc<sched::WaitQueue>,
    sleep_errors: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
enum Role {
    PingPong { court: usize, me: u64 },
    Ordered,
    Table,
    Churn,
    Sleeper,
}

fn role(i: usize) -> Role {
    match i % 8 {
        0 | 1 => Role::PingPong {
            court: i / 8,
            me: u64::from(i % 2 == 1),
        },
        2 | 3 => Role::Ordered,
        4 => Role::Table,
        5 | 6 => Role::Churn,
        _ => Role::Sleeper,
    }
}

/// Runs the stress test for `seconds`; returns whether it passed.
pub(crate) fn run(seconds: u64) -> bool {
    kprintln!(
        "stress: {THREADS} threads for {seconds} s on {} CPUs",
        sched::online_cpus()
    );
    // A short warm-up first, so per-CPU caches and lazily allocated
    // structures exist before the baseline is taken.
    let warm = phase(3, false);
    if !warm.stalled.is_empty() {
        kprintln!(
            "stress: warm-up: threads {:?} had not run yet",
            warm.stalled
        );
    }
    settle();
    let heap0 = crate::mm::heap_stats();
    let free0 = crate::mm::phys_stats().free;
    let kva0 = crate::mm::kstack::kva_allocated();
    let witness0 = witness::violations();
    let Outcome {
        total,
        stalled,
        sleep_errors,
    } = phase(seconds, true);
    settle();

    let heap1 = crate::mm::heap_stats();
    let free1 = crate::mm::phys_stats().free;
    let kva1 = crate::mm::kstack::kva_allocated();
    let violations = witness::violations() - witness0;
    kprintln!(
        "stress: done: {total} iterations; heap objects {} -> {}, large pages {} -> {}, \
         free pages {free0} -> {free1}, KVA {kva0:#x} -> {kva1:#x}, witness violations {violations}, \
         short sleeps {sleep_errors}, stalled threads {stalled:?}",
        heap0.live_objects,
        heap1.live_objects,
        heap0.large_pages,
        heap1.large_pages
    );
    let mut ok = true;
    let mut fail = |what: &str| {
        kprintln!("stress: FAIL: {what}");
        ok = false;
    };
    if !stalled.is_empty() {
        fail("threads made no progress");
    }
    if violations != 0 {
        fail("witness violations");
    }
    if sleep_errors != 0 || warm.sleep_errors != 0 {
        fail("sleeps ended early");
    }
    if heap1.live_objects != heap0.live_objects || heap1.large_pages != heap0.large_pages {
        fail("heap accounting does not balance");
    }
    if kva1 != kva0 {
        fail("kernel stacks leaked");
    }
    // Slabs and page tables may have grown; everything else came back.
    let slab_growth = heap1.slab_pages.saturating_sub(heap0.slab_pages);
    if free0.saturating_sub(free1) > slab_growth + 64 {
        fail("pages leaked");
    }
    if ok {
        kprintln!("stress: PASS");
        let _ = ktest::report_pass(&mut crate::console::Writer);
    } else {
        kprintln!("{}", ktest::FAIL_MARKER);
    }
    ok
}

/// What one phase achieved.
struct Outcome {
    total: u64,
    stalled: Vec<usize>,
    sleep_errors: u64,
}

/// Runs the workload for `seconds`, reporting every ten if `report`.
fn phase(seconds: u64, report: bool) -> Outcome {
    let witness0 = witness::violations();

    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        courts: (0..THREADS / 8)
            .map(|_| Court {
                turn: Mutex::new(&PING, 0),
                seen: AtomicU64::new(0),
                q: Arc::new(sched::WaitQueue::new()),
            })
            .collect(),
        level_a: (0..4).map(|_| Mutex::new(&LEVEL_A, 0)).collect(),
        level_b: (0..4).map(|_| Mutex::new(&LEVEL_B, 0)).collect(),
        table: SxLock::new(&SX, alloc::vec![0; 64]),
        wake_q: Arc::new(sched::WaitQueue::new()),
        sleep_errors: AtomicU64::new(0),
    });
    let progress: Vec<Arc<AtomicU64>> = (0..THREADS).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let threads: Vec<_> = (0..THREADS)
        .map(|i| {
            let (shared, progress) = (Arc::clone(&shared), Arc::clone(&progress[i]));
            // Time-sharing, with four niceness levels.
            let prio =
                Priority::new(Priority::TIMESHARE.get() + u8::try_from(i % 4).unwrap_or(0) * 10);
            sched::spawn("stress", prio, move || worker(i, &shared, &progress))
                .expect("stress thread")
        })
        .collect();

    let start = crate::time::now_ns();
    let end = start + seconds * crate::time::NANOS;
    let mut next_report = start + 10 * crate::time::NANOS;
    while crate::time::now_ns() < end {
        crate::time::sleep_ns(100_000_000);
        // Nudge the timed waiters now and then.
        shared.wake_q.wake_one();
        if report && crate::time::now_ns() >= next_report {
            next_report += 10 * crate::time::NANOS;
            let total: u64 = progress.iter().map(|p| p.load(Ordering::Relaxed)).sum();
            let heap = crate::mm::heap_stats();
            kprintln!(
                "stress: {:>4} s  {total} iterations  heap {} objects {} large pages  {} MiB free  witness {}",
                (crate::time::now_ns() - start) / crate::time::NANOS,
                heap.live_objects,
                heap.large_pages,
                (crate::mm::phys_stats().free * 4096) >> 20,
                witness::violations() - witness0
            );
        }
    }
    shared.stop.store(true, Ordering::Release);
    for c in &shared.courts {
        c.q.wake_all();
    }
    shared.wake_q.wake_all();
    for t in &threads {
        t.join();
    }
    let stalled = progress
        .iter()
        .enumerate()
        .filter(|(_, p)| p.load(Ordering::Relaxed) == 0)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    Outcome {
        total: progress.iter().map(|p| p.load(Ordering::Relaxed)).sum(),
        stalled,
        sleep_errors: shared.sleep_errors.load(Ordering::Relaxed),
    }
}

/// Lets exited threads be reaped and per-CPU caches settle.
fn settle() {
    for _ in 0..50 {
        crate::time::sleep_ns(10_000_000);
    }
}

fn worker(i: usize, s: &Shared, progress: &AtomicU64) {
    let seed = u64::try_from(i).unwrap_or(0) + 1;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x1234_5679));
    let r = role(i);
    let mut live: Vec<(Box<[u8]>, u8)> = Vec::new();
    while !s.stop.load(Ordering::Acquire) {
        match r {
            Role::PingPong { court, me } => {
                let c = &s.courts[court];
                // Wait for our turn (or the end), then pass it.
                c.q.wait_until(|| {
                    s.stop.load(Ordering::Acquire) || c.seen.load(Ordering::Acquire) % 2 == me
                });
                if s.stop.load(Ordering::Acquire) {
                    break;
                }
                {
                    let mut t = c.turn.lock();
                    if *t % 2 == me {
                        *t += 1;
                        c.seen.store(*t, Ordering::Release);
                    }
                }
                c.q.wake_all();
            }
            Role::Ordered => {
                // Two locks, always level a before level b.
                let a = &s.level_a[usize::try_from(rng.below(4)).unwrap_or(0)];
                let b = &s.level_b[usize::try_from(rng.below(4)).unwrap_or(0)];
                let mut ga = a.lock();
                let mut gb = b.lock();
                *ga += 1;
                *gb += 1;
                if rng.below(16) == 0 {
                    sched::yield_now();
                }
            }
            Role::Table => {
                if rng.below(8) == 0 {
                    let mut t = s.table.write();
                    let v = t[0] + 1;
                    t.fill(v);
                } else {
                    let t = s.table.read();
                    let first = t[0];
                    assert!(t.iter().all(|&v| v == first), "torn table write");
                }
            }
            Role::Churn => {
                if live.len() < 64 && (live.is_empty() || rng.below(3) != 0) {
                    let size = match rng.below(16) {
                        0 => 4096 + usize::try_from(rng.below(60_000)).unwrap_or(0),
                        _ => 1 + usize::try_from(rng.below(2000)).unwrap_or(0),
                    };
                    let tag = u8::try_from(rng.below(256)).unwrap_or(0);
                    live.push((alloc::vec![tag; size].into_boxed_slice(), tag));
                } else {
                    let n = u64::try_from(live.len()).unwrap_or(1);
                    let k = usize::try_from(rng.below(n)).unwrap_or(0);
                    let (b, tag) = live.swap_remove(k);
                    assert!(b.iter().all(|&x| x == tag), "heap object overwritten");
                }
            }
            Role::Sleeper => {
                let ns = 1_000_000 + rng.below(20_000_000);
                let t0 = crate::time::now_ns();
                if rng.below(2) == 0 {
                    crate::time::sleep_ns(ns);
                    if crate::time::now_ns() - t0 < ns {
                        s.sleep_errors.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    let woken = crate::time::wait_timeout(&s.wake_q, || false, ns);
                    let _ = woken;
                }
            }
        }
        progress.fetch_add(1, Ordering::Relaxed);
    }
}
