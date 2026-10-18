// SPDX-License-Identifier: BSD-2-Clause
//! In-kernel benchmarks (`abyss.bench=<suite>`).
//!
//! The AbyssBSD side of the comparison with FreeBSD's `kbench` module
//! (tests/perf/kbench). Both follow the workload specification in
//! tests/perf/README.md, and `cargo xtask perf` compares their output; a
//! change to a workload changes both.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use addr::VirtAddr;
use hal::CpuId;
use sched::lock::{LockClass, Mutex, RwLock, SxLock};
use sched::{IrqCx, Priority, Sleepable, WaitQueue};
use vm::Order;

use crate::time::{self, Timer};

/// The VM size the suites assume.
const CPUS: usize = 8;
const LAT_CALLOUT: usize = 1000;
const LAT_WAKEUP: usize = 10_000;
const MISS_OBJS: usize = 4096;
const MISS_PAGES: usize = 1024;
const RUN_NS: u64 = time::NANOS;
/// Rounds of each throughput benchmark; the median is reported.
const ROUNDS: usize = 3;

/// Runs `suite`; returns whether it was known and ran.
pub(crate) fn run(s: &Sleepable, suite: &str) -> bool {
    let body: fn(&Sleepable) = match suite {
        "p2" => suite_p2,
        "diag" => suite_diag,
        _ => {
            kprintln!("bench: unknown suite {suite:?}");
            return false;
        }
    };
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
        .spawn(body)
        .expect("bench: spawn coordinator");
    t.join(s);
    kprintln!("kbench end {suite}");
    true
}

/// Shared state of one run: workers' start and stop flags and results.
struct Run {
    ready: AtomicUsize,
    go: AtomicBool,
    /// Workers sleep here until the start (as the FreeBSD side does).
    start: WaitQueue,
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
            start: WaitQueue::new(),
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
    s: &Sleepable,
    run: &Arc<Run>,
    cpus: &[CpuId],
    run_ns: u64,
    body: impl Fn(&Run, usize, &Sleepable) + Send + Sync + Clone + 'static,
) -> u64 {
    let threads: Vec<_> = cpus
        .iter()
        .enumerate()
        .map(|(i, &cpu)| {
            let (run, body) = (Arc::clone(run), body.clone());
            sched::Builder::new("kbench")
                .priority(Priority::KERNEL)
                .bind(cpu)
                .spawn(move |ws| {
                    run.ready.fetch_add(1, Ordering::AcqRel);
                    run.start.wait_until(ws, || run.go.load(Ordering::Acquire));
                    body(&run, i, ws);
                })
                .expect("bench: spawn worker")
        })
        .collect();
    while run.ready.load(Ordering::Acquire) < cpus.len() {
        time::sleep_ns(s, 1_000_000);
    }
    let t0 = time::counter();
    run.go.store(true, Ordering::Release);
    run.start.wake_all();
    let mut t1 = 0;
    if run_ns > 0 {
        time::sleep_ns(s, run_ns);
        run.stop.store(true, Ordering::Release);
        t1 = time::counter();
    }
    for t in threads {
        t.join(s);
    }
    if run_ns == 0 {
        t1 = time::counter();
    }
    t1 - t0
}

/// One round's result: thousandths of a nanosecond per operation.
fn milli_ns_per_op(run: &Run, workers: usize, ticks: u64) -> u128 {
    let ops: u64 = run.ops[..workers]
        .iter()
        .map(|s| s.0.load(Ordering::Relaxed))
        .sum();
    u128::from(time::ticks_to_ns(ticks)) * 1000 / u128::from(ops.max(1))
}

/// Runs `round` [`ROUNDS`] times and reports the median.
fn report_median(name: &str, mut round: impl FnMut() -> u128) {
    let mut r: [u128; ROUNDS] = core::array::from_fn(|_| round());
    r.sort_unstable();
    let milli = r[ROUNDS / 2];
    kprintln!("kbench {name} {}.{:03} ns/op", milli / 1000, milli % 1000);
}

fn report_lat(name: &str, ns: &mut [u64]) {
    ns.sort_unstable();
    kprintln!("kbench {name}.p50 {} ns", ns[ns.len() / 2]);
    kprintln!("kbench {name}.p99 {} ns", ns[ns.len() * 99 / 100]);
}

/// A throughput benchmark: `workers` threads in the default placement, each
/// running `op` until stopped; `op` returns how many operations it did.
fn throughput(
    s: &Sleepable,
    name: &str,
    workers: usize,
    op: impl Fn(&Sleepable) -> u64 + Send + Sync + Clone + 'static,
) {
    throughput_with(s, name, workers, || (), move |(), ws| op(ws));
}

/// As [`throughput`], with per-worker state made by `init` before the start.
fn throughput_with<S: 'static>(
    s: &Sleepable,
    name: &str,
    workers: usize,
    init: fn() -> S,
    op: impl Fn(&mut S, &Sleepable) -> u64 + Send + Sync + Clone + 'static,
) {
    let cpus: Vec<CpuId> = (0..workers).map(default_cpu).collect();
    report_median(name, || {
        let run = Run::new();
        let op = op.clone();
        let ticks = launch(s, &run, &cpus, RUN_NS, move |run, i, ws| {
            let mut state = init();
            let mut n = 0;
            while !run.stopped() {
                n += op(&mut state, ws);
            }
            run.ops[i].0.store(n, Ordering::Relaxed);
        });
        milli_ns_per_op(&run, workers, ticks)
    });
}

static BENCH_MTX: LockClass = LockClass::new("bench mtx", 900);
static BENCH_RW: LockClass = LockClass::non_sleepable("bench rw", 900);
static BENCH_SX: LockClass = LockClass::new("bench sx", 900);

/// Lock benchmarks run on this many lock sets, each in its own page, and
/// report the mean: a contended lock's cost depends on where its cache line
/// lives (its L3 slice), by up to 2x on the build server.
const PLACEMENTS: usize = 8;
const PLACEMENT_NS: u64 = 250_000_000;

/// One placement's locks (each guards its own counter).
#[repr(align(4096))]
struct LockSet {
    mtx: Mutex<u64>,
    rw: RwLock<u64>,
    sx: SxLock<u64>,
}

fn lock_sets() -> Vec<&'static LockSet> {
    (0..PLACEMENTS)
        .map(|_| {
            &*Box::leak(Box::new(LockSet {
                mtx: Mutex::new(&BENCH_MTX, 0),
                rw: RwLock::new(&BENCH_RW, 0),
                sx: SxLock::new(&BENCH_SX, 0),
            }))
        })
        .collect()
}

/// A lock benchmark: `workers` threads run `op` on each lock set in turn
/// for [`PLACEMENT_NS`]; reports the mean.
fn lock_throughput<F: Fn(&LockSet, &Sleepable) -> u64 + Copy + Send + Sync + 'static>(
    s: &Sleepable,
    name: &str,
    workers: usize,
    sets: &[&'static LockSet],
    op: F,
) {
    let cpus: Vec<CpuId> = (0..workers).map(default_cpu).collect();
    let mut sum = 0;
    for &set in sets {
        let run = Run::new();
        let ticks = launch(s, &run, &cpus, PLACEMENT_NS, move |run, i, ws| {
            let mut n = 0;
            while !run.stopped() {
                n += op(set, ws);
            }
            run.ops[i].0.store(n, Ordering::Relaxed);
        });
        sum += milli_ns_per_op(&run, workers, ticks);
    }
    let milli = sum / sets.len() as u128;
    kprintln!("kbench {name} {}.{:03} ns/op", milli / 1000, milli % 1000);
}

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

fn suite_p2(s: &Sleepable) {
    ctxsw(s, "ctxsw.same", CpuId(1), CpuId(1));
    ctxsw(s, "ctxsw.cross", CpuId(1), CpuId(2));

    let sets = lock_sets();
    for t in [1, 2, 4, 8] {
        lock_throughput(s, &alloc::format!("mtx.t{t}"), t, &sets, |l, ws| {
            *l.mtx.lock(ws) += 1;
            1
        });
        lock_throughput(s, &alloc::format!("rw.rd.t{t}"), t, &sets, |l, ws| {
            black_box(*l.rw.read(ws));
            1
        });
        lock_throughput(s, &alloc::format!("rw.wr.t{t}"), t, &sets, |l, ws| {
            *l.rw.write(ws) += 1;
            1
        });
        lock_throughput(s, &alloc::format!("sx.rd.t{t}"), t, &sets, |l, ws| {
            black_box(*l.sx.read(ws));
            1
        });
        lock_throughput(s, &alloc::format!("sx.wr.t{t}"), t, &sets, |l, ws| {
            *l.sx.write(ws) += 1;
            1
        });
    }

    for t in [1, 8] {
        throughput(s, &alloc::format!("malloc.hit.t{t}"), t, |_| {
            free_256(alloc_256());
            1
        });
    }
    for t in [1, 8] {
        throughput_with(
            s,
            &alloc::format!("malloc.miss.t{t}"),
            t,
            || alloc::vec![core::ptr::null_mut(); MISS_OBJS],
            |p, _| {
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
        throughput(s, &alloc::format!("page.hit.t{t}"), t, |_| {
            let pa = crate::mm::alloc_pages(Order::ZERO).expect("bench: out of pages");
            crate::mm::free_pages(black_box(pa), Order::ZERO);
            1
        });
    }
    throughput_with(
        s,
        "page.miss.t1",
        1,
        || alloc::vec![None; MISS_PAGES],
        |p, _| {
            for slot in p.iter_mut() {
                *slot = crate::mm::alloc_pages(Order::ZERO);
            }
            for pa in p.iter_mut() {
                crate::mm::free_pages(pa.take().expect("bench: out of pages"), Order::ZERO);
            }
            MISS_PAGES as u64
        },
    );

    throughput_with(
        s,
        "callout.armcancel",
        1,
        || Box::pin(Timer::new()),
        |t, _| {
            t.as_ref().reset(RUN_NS, |_, _| {}, 0);
            black_box(t.stop());
            1
        },
    );
    callout_latency(s);
    wakeup_latency(s);

    throughput(s, "ipi.rtt", 1, |_| {
        crate::smp::ping(CpuId(2));
        1
    });
    tlb_shootdown(s);
}

/// AbyssBSD-only diagnostics: the cost of the pieces fast paths are made
/// of, for finding where time goes. Not compared with FreeBSD.
fn suite_diag(s: &Sleepable) {
    throughput(s, "diag.irq_pair", 1, |_| {
        let irq = hal::irq::disable();
        hal::irq::restore(black_box(irq));
        1
    });
    throughput(s, "diag.cpu_id", 1, |_| {
        black_box(hal::cpu_id());
        1
    });
    throughput(s, "diag.current", 1, |_| {
        black_box(sched::current());
        1
    });
    throughput_with(
        s,
        "diag.alloc_then_free_1024",
        1,
        || alloc::vec![core::ptr::null_mut(); 1024],
        |p, _| {
            for slot in p.iter_mut() {
                *slot = alloc_256();
            }
            for &q in p.iter() {
                free_256(q);
            }
            1024
        },
    );
    #[cfg(target_arch = "x86_64")]
    {
        throughput(s, "diag.apic_read", 1, |_| {
            black_box(crate::arch::apic::probe_read());
            1
        });
        throughput(s, "diag.apic_write", 1, |_| {
            crate::arch::apic::probe_write();
            1
        });
    }
    throughput(s, "diag.ping_idle", 1, |_| {
        crate::smp::ping(CpuId(2));
        1
    });
    // The same with CPU 2 running a spinning thread: no halt to wake from.
    static SPIN: AtomicBool = AtomicBool::new(true);
    SPIN.store(true, Ordering::Relaxed);
    let spinner = sched::Builder::new("spinner")
        .priority(Priority::KERNEL)
        .bind(CpuId(2))
        .spawn(|_| {
            while SPIN.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        })
        .expect("bench: spinner");
    time::sleep_ns(s, 10_000_000);
    throughput(s, "diag.ping_busy", 1, |_| {
        crate::smp::ping(CpuId(2));
        1
    });
    SPIN.store(false, Ordering::Relaxed);
    spinner.join(s);
    // Contended reads on locks in eight different pages: does the cost
    // depend on where the lock lives (its L3 slice)?
    let locks: Vec<&'static RwLock<u64>> = (0..8)
        .map(|_| {
            let page = Layout::from_size_align(4096, 4096).expect("layout");
            // SAFETY: nonzero size; the page is leaked for the benchmark.
            let p = unsafe { alloc::alloc::alloc(page) }.cast::<RwLock<u64>>();
            assert!(!p.is_null());
            // SAFETY: fresh, page-aligned, large enough.
            unsafe {
                p.write(RwLock::new(&BENCH_RW, 0));
                &*p
            }
        })
        .collect();
    for (i, l) in locks.into_iter().enumerate() {
        throughput(s, &alloc::format!("diag.rw_rd_t2_page{i}"), 2, move |ws| {
            black_box(*l.read(ws));
            1
        });
    }
    throughput(s, "diag.counter", 1, |_| {
        black_box(time::counter());
        1
    });
}

/// Strict alternation between two workers on `a` and `b`.
fn ctxsw(s: &Sleepable, name: &str, a: CpuId, b: CpuId) {
    struct Turn {
        q: WaitQueue,
        turn: AtomicUsize,
    }
    report_median(name, || {
        let shared = Arc::new(Turn {
            q: WaitQueue::new(),
            turn: AtomicUsize::new(0),
        });
        let run = Run::new();
        let ticks = launch(s, &run, &[a, b], RUN_NS, move |run, me, ws| {
            let other = 1 - me;
            let mut n = 0;
            loop {
                shared.q.wait_until(ws, || {
                    shared.turn.load(Ordering::Acquire) == me || run.stopped()
                });
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
        milli_ns_per_op(&run, 2, ticks)
    });
}

/// How late a 1 ms callout's handler starts, from a sleeping worker.
fn callout_latency(s: &Sleepable) {
    struct Fire {
        q: WaitQueue,
        fired: AtomicBool,
        at: AtomicU64,
    }
    fn fire(arg: usize, _: &IrqCx) {
        // SAFETY: `arg` is the worker's `Fire`, which outlives its timer.
        let f = unsafe { &*core::ptr::with_exposed_provenance::<Fire>(arg) };
        f.at.store(time::counter(), Ordering::Relaxed);
        f.fired.store(true, Ordering::Release);
        f.q.wake_all();
    }
    let samples = Arc::new(sync::SpinLock::new(Vec::with_capacity(LAT_CALLOUT)));
    let out = Arc::clone(&samples);
    let run = Run::new();
    launch(s, &run, &[default_cpu(0)], 0, move |_, _, ws| {
        let one_ms = time::ns_to_ticks(1_000_000);
        let mut local = Vec::with_capacity(LAT_CALLOUT);
        let f = Fire {
            q: WaitQueue::new(),
            fired: AtomicBool::new(false),
            at: AtomicU64::new(0),
        };
        let timer = core::pin::pin!(Timer::new());
        for _ in 0..LAT_CALLOUT {
            f.fired.store(false, Ordering::Relaxed);
            let t0 = time::counter();
            timer
                .as_ref()
                .reset(1_000_000, fire, core::ptr::from_ref(&f).expose_provenance());
            f.q.wait_until(ws, || f.fired.load(Ordering::Acquire));
            let late = f.at.load(Ordering::Relaxed).saturating_sub(t0 + one_ms);
            local.push(time::ticks_to_ns(late));
        }
        *out.lock() = local;
    });
    report_lat("callout.latency", &mut samples.lock());
}

/// Cross-CPU wakeup latency of a sleeping thread on an idle CPU.
fn wakeup_latency(s: &Sleepable) {
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
    let sh = Arc::clone(&shared);
    let run = Run::new();
    launch(s, &run, &[CpuId(1), CpuId(2)], 0, move |_, me, ws| {
        if me == 0 {
            let mut local = Vec::with_capacity(LAT_WAKEUP);
            for i in 0..LAT_WAKEUP {
                sh.q.wait_until(ws, || {
                    let woken = sh.seq.load(Ordering::Acquire) != i;
                    if !woken {
                        // Announced under the queue lock: once the waker
                        // sees it, the sleeper is queued.
                        sh.asleep.store(i + 1, Ordering::Release);
                    }
                    woken
                });
                let t1 = time::counter();
                local.push(time::ticks_to_ns(t1 - sh.stamp.load(Ordering::Acquire)));
            }
            *sh.samples.lock() = local;
        } else {
            let spin = time::ns_to_ticks(50_000);
            for i in 0..LAT_WAKEUP {
                while sh.asleep.load(Ordering::Acquire) != i + 1 {
                    core::hint::spin_loop();
                }
                let t = time::counter();
                while time::counter() - t < spin {
                    core::hint::spin_loop();
                }
                sh.stamp.store(time::counter(), Ordering::Release);
                sh.seq.store(i + 1, Ordering::Release);
                sh.q.wake_one();
            }
        }
    });
    report_lat("wakeup.latency", &mut shared.samples.lock());
}

/// One kernel page invalidated on every CPU.
fn tlb_shootdown(s: &Sleepable) {
    let page = Layout::from_size_align(4096, 4096).expect("layout");
    // SAFETY: the layout has a nonzero size.
    let p = unsafe { alloc::alloc::alloc(page) };
    assert!(!p.is_null(), "bench: out of memory");
    let va = VirtAddr::new(p as u64);
    throughput(s, "tlb.shootdown", 1, move |_| {
        crate::mm::flush_range(va, 4096);
        crate::smp::shootdown(va, 4096);
        1
    });
    // SAFETY: allocated above with the same layout.
    unsafe { alloc::alloc::dealloc(p, page) };
}
