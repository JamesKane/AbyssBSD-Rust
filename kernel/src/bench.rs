// SPDX-License-Identifier: BSD-2-Clause
//! In-kernel benchmarks (`abyss.bench=<suite>`).
//!
//! The AbyssBSD side of the comparison with FreeBSD's `kbench` module
//! (tests/perf/kbench). Both follow the workload specification in
//! tests/perf/README.md, and `cargo xtask perf` compares their output; a
//! change to a workload changes both.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use addr::VirtAddr;
use hal::CpuId;
use sched::lock::{LockClass, Mutex, RwLock, SxLock};
use sched::{Priority, WaitQueue};
use vm::Order;

use crate::time::{self, Callout};

/// The VM size the suites assume.
const CPUS: usize = 8;
const LAT_CALLOUT: usize = 1000;
const LAT_WAKEUP: usize = 10_000;
const MISS_OBJS: usize = 4096;
const MISS_PAGES: usize = 1024;
const RUN_NS: u64 = time::NANOS;

/// Runs `suite`; returns whether it was known and ran.
pub(crate) fn run(suite: &str) -> bool {
    if suite != "p2" {
        kprintln!("bench: unknown suite {suite:?}");
        return false;
    }
    let cpus = sched::online_cpus();
    if cpus < CPUS {
        kprintln!("bench: needs {CPUS} CPUs, have {cpus}");
        return false;
    }
    if !sched::lock::witness::ENABLED {
        kprintln!("kbench begin {suite} cpus={cpus}");
    } else {
        // Debug builds measure the witness too; say so in the log.
        kprintln!("kbench begin {suite} cpus={cpus} (debug build: witness on)");
    }
    let t = sched::Builder::new("bench")
        .priority(Priority::KERNEL)
        .bind(CpuId(0))
        .spawn(suite_p2)
        .expect("bench: spawn coordinator");
    t.join();
    kprintln!("kbench end {suite}");
    true
}

/// Shared state of one run: workers' start and stop flags and results.
struct Run {
    ready: AtomicUsize,
    go: AtomicBool,
    stop: AtomicBool,
    ops: [Slot; CPUS],
}

/// One worker's result, on its own cache line.
#[repr(align(64))]
struct Slot(AtomicU64);

impl Run {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicUsize::new(0),
            go: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            ops: [const { Slot(AtomicU64::new(0)) }; CPUS],
        })
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// Worker `i`'s CPU in the default placement.
fn default_cpu(i: usize) -> CpuId {
    CpuId(u32::try_from((i + 1) % CPUS).unwrap_or(0))
}

/// Starts one worker per entry of `cpus`, each running `body(run, index)`,
/// lets them run for `run_ns` (0: until they return), and returns the
/// counter ticks between go and stop.
fn launch(
    run: &Arc<Run>,
    cpus: &[CpuId],
    run_ns: u64,
    body: impl Fn(&Run, usize) + Send + Sync + Clone + 'static,
) -> u64 {
    let threads: Vec<_> = cpus
        .iter()
        .enumerate()
        .map(|(i, &cpu)| {
            let (run, body) = (Arc::clone(run), body.clone());
            sched::Builder::new("kbench")
                .priority(Priority::KERNEL)
                .bind(cpu)
                .spawn(move || {
                    run.ready.fetch_add(1, Ordering::AcqRel);
                    while !run.go.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                    body(&run, i);
                })
                .expect("bench: spawn worker")
        })
        .collect();
    while run.ready.load(Ordering::Acquire) < cpus.len() {
        time::sleep_ns(1_000_000);
    }
    let t0 = time::counter();
    run.go.store(true, Ordering::Release);
    let mut t1 = 0;
    if run_ns > 0 {
        time::sleep_ns(run_ns);
        run.stop.store(true, Ordering::Release);
        t1 = time::counter();
    }
    for t in threads {
        t.join();
    }
    if run_ns == 0 {
        t1 = time::counter();
    }
    t1 - t0
}

fn report_ops(name: &str, run: &Run, workers: usize, ticks: u64) {
    let ops: u64 = run.ops[..workers]
        .iter()
        .map(|s| s.0.load(Ordering::Relaxed))
        .sum();
    let milli = u128::from(time::ticks_to_ns(ticks)) * 1000 / u128::from(ops.max(1));
    kprintln!("kbench {name} {}.{:03} ns/op", milli / 1000, milli % 1000);
}

fn report_lat(name: &str, ns: &mut [u64]) {
    ns.sort_unstable();
    kprintln!("kbench {name}.p50 {} ns", ns[ns.len() / 2]);
    kprintln!("kbench {name}.p99 {} ns", ns[ns.len() * 99 / 100]);
}

/// A throughput benchmark: `workers` threads in the default placement, each
/// running `op` until stopped; `op` returns how many operations it did.
fn throughput(name: &str, workers: usize, op: impl Fn() -> u64 + Send + Sync + Clone + 'static) {
    throughput_with(name, workers, || (), move |()| op());
}

/// As [`throughput`], with per-worker state made by `init` before the start.
fn throughput_with<S: 'static>(
    name: &str,
    workers: usize,
    init: fn() -> S,
    op: impl Fn(&mut S) -> u64 + Send + Sync + Clone + 'static,
) {
    let run = Run::new();
    let cpus: Vec<CpuId> = (0..workers).map(default_cpu).collect();
    let ticks = launch(&run, &cpus, RUN_NS, move |run, i| {
        let mut state = init();
        let mut n = 0;
        while !run.stopped() {
            n += op(&mut state);
        }
        run.ops[i].0.store(n, Ordering::Relaxed);
    });
    report_ops(name, &run, workers, ticks);
}

static BENCH_MTX: LockClass = LockClass::new("bench mtx", 900);
static BENCH_RW: LockClass = LockClass::non_sleepable("bench rw", 900);
static BENCH_SX: LockClass = LockClass::new("bench sx", 900);

static MTX: Mutex<u64> = Mutex::new(&BENCH_MTX, 0);
static RW: RwLock<u64> = RwLock::new(&BENCH_RW, 0);
static SX: SxLock<u64> = SxLock::new(&BENCH_SX, 0);

fn layout() -> Layout {
    Layout::from_size_align(256, 8).expect("layout")
}

fn alloc_256() -> *mut u8 {
    // SAFETY: the layout has a nonzero size.
    let p = unsafe { alloc::alloc::alloc(layout()) };
    assert!(!p.is_null(), "bench: out of memory");
    black_box(p)
}

fn free_256(p: *mut u8) {
    // SAFETY: `p` came from `alloc_256` with the same layout.
    unsafe { alloc::alloc::dealloc(p, layout()) }
}

fn suite_p2() {
    ctxsw("ctxsw.same", CpuId(1), CpuId(1));
    ctxsw("ctxsw.cross", CpuId(1), CpuId(2));

    for t in [1, 2, 4, 8] {
        throughput(&alloc::format!("mtx.t{t}"), t, || {
            *MTX.lock() += 1;
            1
        });
        throughput(&alloc::format!("rw.rd.t{t}"), t, || {
            black_box(*RW.read());
            1
        });
        throughput(&alloc::format!("rw.wr.t{t}"), t, || {
            *RW.write() += 1;
            1
        });
        throughput(&alloc::format!("sx.rd.t{t}"), t, || {
            black_box(*SX.read());
            1
        });
        throughput(&alloc::format!("sx.wr.t{t}"), t, || {
            *SX.write() += 1;
            1
        });
    }

    for t in [1, 8] {
        throughput(&alloc::format!("malloc.hit.t{t}"), t, || {
            free_256(alloc_256());
            1
        });
    }
    for t in [1, 8] {
        throughput_with(
            &alloc::format!("malloc.miss.t{t}"),
            t,
            || alloc::vec![core::ptr::null_mut(); MISS_OBJS],
            |p| {
                for slot in p.iter_mut() {
                    *slot = alloc_256();
                }
                for &q in p.iter() {
                    free_256(q);
                }
                MISS_OBJS as u64
            },
        );
    }
    for t in [1, 8] {
        throughput(&alloc::format!("page.hit.t{t}"), t, || {
            let pa = crate::mm::alloc_pages(Order::ZERO).expect("bench: out of pages");
            crate::mm::free_pages(black_box(pa), Order::ZERO);
            1
        });
    }
    throughput_with(
        "page.miss.t1",
        1,
        || alloc::vec![None; MISS_PAGES],
        |p| {
            for slot in p.iter_mut() {
                *slot = crate::mm::alloc_pages(Order::ZERO);
            }
            for pa in p.iter_mut() {
                crate::mm::free_pages(pa.take().expect("bench: out of pages"), Order::ZERO);
            }
            MISS_PAGES as u64
        },
    );

    throughput("callout.armcancel", 1, || {
        black_box(Callout::after(RUN_NS, || {}).cancel());
        1
    });
    callout_latency();
    wakeup_latency();

    throughput("ipi.rtt", 1, || {
        crate::smp::ping(CpuId(2));
        1
    });
    tlb_shootdown();
}

/// Strict alternation between two workers on `a` and `b`.
fn ctxsw(name: &str, a: CpuId, b: CpuId) {
    struct Turn {
        q: WaitQueue,
        turn: AtomicUsize,
    }
    let shared = Arc::new(Turn {
        q: WaitQueue::new(),
        turn: AtomicUsize::new(0),
    });
    let run = Run::new();
    let ticks = launch(&run, &[a, b], RUN_NS, move |run, me| {
        let other = 1 - me;
        let mut n = 0;
        loop {
            shared
                .q
                .wait_until(|| shared.turn.load(Ordering::Acquire) == me || run.stopped());
            shared.turn.store(other, Ordering::Release);
            if run.stopped() {
                shared.q.wake_all();
                break;
            }
            n += 1;
            shared.q.wake_one();
        }
        run.ops[me].0.store(n, Ordering::Relaxed);
    });
    report_ops(name, &run, 2, ticks);
}

/// How late a 1 ms callout's handler starts, from a sleeping worker.
fn callout_latency() {
    struct Fire {
        q: WaitQueue,
        fired: AtomicBool,
        at: AtomicU64,
    }
    let samples = Arc::new(sync::SpinLock::new(Vec::with_capacity(LAT_CALLOUT)));
    let out = Arc::clone(&samples);
    let run = Run::new();
    launch(&run, &[default_cpu(0)], 0, move |_, _| {
        let one_ms = time::ns_to_ticks(1_000_000);
        let mut local = Vec::with_capacity(LAT_CALLOUT);
        for _ in 0..LAT_CALLOUT {
            let f = Arc::new(Fire {
                q: WaitQueue::new(),
                fired: AtomicBool::new(false),
                at: AtomicU64::new(0),
            });
            let f2 = Arc::clone(&f);
            let t0 = time::counter();
            Callout::after(1_000_000, move || {
                f2.at.store(time::counter(), Ordering::Relaxed);
                f2.fired.store(true, Ordering::Release);
                f2.q.wake_all();
            })
            .forget();
            f.q.wait_until(|| f.fired.load(Ordering::Acquire));
            let late = f.at.load(Ordering::Relaxed).saturating_sub(t0 + one_ms);
            local.push(time::ticks_to_ns(late));
        }
        *out.lock() = local;
    });
    report_lat("callout.latency", &mut samples.lock());
}

/// Cross-CPU wakeup latency of a sleeping thread on an idle CPU.
fn wakeup_latency() {
    struct Shared {
        q: WaitQueue,
        /// The iteration the sleeper is sleeping for, plus one.
        asleep: AtomicUsize,
        seq: AtomicUsize,
        stamp: AtomicU64,
        samples: sync::SpinLock<Vec<u64>>,
    }
    let shared = Arc::new(Shared {
        q: WaitQueue::new(),
        asleep: AtomicUsize::new(0),
        seq: AtomicUsize::new(0),
        stamp: AtomicU64::new(0),
        samples: sync::SpinLock::new(Vec::new()),
    });
    let s = Arc::clone(&shared);
    let run = Run::new();
    launch(&run, &[CpuId(1), CpuId(2)], 0, move |_, me| {
        if me == 0 {
            let mut local = Vec::with_capacity(LAT_WAKEUP);
            for i in 0..LAT_WAKEUP {
                s.q.wait_until(|| {
                    let woken = s.seq.load(Ordering::Acquire) != i;
                    if !woken {
                        // Announced under the queue lock: once the waker
                        // sees it, the sleeper is queued.
                        s.asleep.store(i + 1, Ordering::Release);
                    }
                    woken
                });
                let t1 = time::counter();
                local.push(time::ticks_to_ns(t1 - s.stamp.load(Ordering::Acquire)));
            }
            *s.samples.lock() = local;
        } else {
            let spin = time::ns_to_ticks(50_000);
            for i in 0..LAT_WAKEUP {
                while s.asleep.load(Ordering::Acquire) != i + 1 {
                    core::hint::spin_loop();
                }
                let t = time::counter();
                while time::counter() - t < spin {
                    core::hint::spin_loop();
                }
                s.stamp.store(time::counter(), Ordering::Release);
                s.seq.store(i + 1, Ordering::Release);
                s.q.wake_one();
            }
        }
    });
    report_lat("wakeup.latency", &mut shared.samples.lock());
}

/// One kernel page invalidated on every CPU.
fn tlb_shootdown() {
    let page = Layout::from_size_align(4096, 4096).expect("layout");
    // SAFETY: the layout has a nonzero size.
    let p = unsafe { alloc::alloc::alloc(page) };
    assert!(!p.is_null(), "bench: out of memory");
    let va = VirtAddr::new(p as u64);
    throughput("tlb.shootdown", 1, move || {
        crate::mm::flush_range(va, 4096);
        crate::smp::shootdown(va, 4096);
        1
    });
    // SAFETY: allocated above with the same layout.
    unsafe { alloc::alloc::dealloc(p, page) };
}
