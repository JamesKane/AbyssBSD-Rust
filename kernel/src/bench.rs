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

/// The userland suite: FreeBSD's static `ubench`, the same binary FreeBSD
/// runs, from the memory disk as `/usr/bench/ubench p4` (it prints the `kbench`
/// lines itself).
fn run_ubench(s: &Sleepable) -> bool {
    const PATH: &[u8] = b"/usr/bench/ubench";
    run_waiting(s, PATH, &[PATH, b"p4"])
}

/// The file systems' suite: as FreeBSD's `rc.local` does, copies `ubench`,
/// `tree.tar`, and `fsx` off the bench disk (`vtbd1`) into `/tmp`, then
/// runs `/tmp/ubench p5`, which formats and mounts its own file systems.
fn run_ubench_p5(s: &Sleepable) -> bool {
    const SH: &[u8] = b"/rescue/sh";
    const SCRIPT: &[u8] = b"mount -o ro /dev/vtbd1 /mnt && \
        cp /mnt/ubench /mnt/tree.tar /mnt/fsx /tmp/ && umount /mnt && exec /tmp/ubench p5";
    run_waiting(s, SH, &[SH, b"-c", SCRIPT])
}

/// The threads component of the `p6` suite: as `p5`, `ubench p6.thr` (the
/// threaded build) from the bench disk.
fn run_ubench_p6_thr(s: &Sleepable) -> bool {
    const SH: &[u8] = b"/rescue/sh";
    const SCRIPT: &[u8] = b"mount -o ro /dev/vtbd1 /mnt && \
        cp /mnt/ubench /tmp/ && umount /mnt && exec /tmp/ubench p6.thr";
    run_waiting(s, SH, &[SH, b"-c", SCRIPT])
}

/// The events component of the `p6` suite, as `p6.thr`.
fn run_ubench_p6_event(s: &Sleepable) -> bool {
    const SH: &[u8] = b"/rescue/sh";
    const SCRIPT: &[u8] = b"mount -o ro /dev/vtbd1 /mnt && \
        cp /mnt/ubench /tmp/ && umount /mnt && exec /tmp/ubench p6.event";
    run_waiting(s, SH, &[SH, b"-c", SCRIPT])
}

/// The UNIX-domain sockets component of the `p6` suite, as `p6.thr`.
fn run_ubench_p6_unix(s: &Sleepable) -> bool {
    const SH: &[u8] = b"/rescue/sh";
    const SCRIPT: &[u8] = b"mount -o ro /dev/vtbd1 /mnt && \
        cp /mnt/ubench /tmp/ && umount /mnt && exec /tmp/ubench p6.unix";
    run_waiting(s, SH, &[SH, b"-c", SCRIPT])
}

/// The pseudo-terminals component of the `p6` suite, as `p6.thr`.
fn run_ubench_p6_pty(s: &Sleepable) -> bool {
    const SH: &[u8] = b"/rescue/sh";
    const SCRIPT: &[u8] = b"mount -o ro /dev/vtbd1 /mnt && \
        cp /mnt/ubench /tmp/ && umount /mnt && exec /tmp/ubench p6.pty";
    run_waiting(s, SH, &[SH, b"-c", SCRIPT])
}

/// The `vt` suite: `ubench vt`, from the memory disk as `p4`'s.
fn run_ubench_vt(s: &Sleepable) -> bool {
    const PATH: &[u8] = b"/usr/bench/ubench";
    run_waiting(s, PATH, &[PATH, b"vt"])
}

/// Runs `path` and waits; whether it exited with 0.
/// Under the profiler, `UB_PROF` has `ubench` sample each storm phase
/// apart, for longer.
fn run_waiting(s: &Sleepable, path: &[u8], argv: &[&[u8]]) -> bool {
    let env: &[&[u8]] = if crate::prof::on() {
        &[b"PATH=/rescue", b"UB_PROF=1"]
    } else {
        &[b"PATH=/rescue"]
    };
    match crate::proc::Process::spawn(s, path, argv, env, None) {
        Ok(p) => p.wait(s) == crate::proc::Exit::Code(0),
        Err(e) => {
            kprintln!("bench: cannot run {}: {e:?}", path.escape_ascii());
            false
        }
    }
}

/// Runs `suite`; returns whether it was known and ran.
pub(crate) fn run(s: &Sleepable, suite: &str) -> bool {
    match suite {
        "p4" => return run_ubench(s),
        "p5" => return run_ubench_p5(s),
        "p6.thr" => return run_ubench_p6_thr(s),
        "p6.event" => return run_ubench_p6_event(s),
        "p6.unix" => return run_ubench_p6_unix(s),
        "p6.pty" => return run_ubench_p6_pty(s),
        "vt" => return run_ubench_vt(s),
        _ => {}
    }
    let body: fn(&Sleepable) = match suite {
        "p2" => suite_p2,
        "p3" => suite_p3,
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

/// Diagnostics for a latency run (not compared): the tail, and how often
/// samples over 3x the median come (periodic ones point at a timer).
fn report_tail(name: &str, ns: &[u64], starts: &[u64]) {
    let mut s = ns.to_vec();
    s.sort_unstable();
    let p50 = s[s.len() / 2];
    let outliers: Vec<u64> = ns
        .iter()
        .zip(starts)
        .filter(|&(&v, _)| v > 3 * p50)
        .map(|(_, &t)| t)
        .collect();
    let mut gaps: Vec<u64> = outliers.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort_unstable();
    kprintln!(
        "kbench: tail {name} p50 {p50} p90 {} p999 {} max {} over3x {} gap {} ns",
        s[s.len() * 9 / 10],
        s[s.len() * 999 / 1000],
        s[s.len() - 1],
        outliers.len(),
        gaps.get(gaps.len() / 2).copied().unwrap_or(0)
    );
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

/// Suite p3: block reads, as tests/perf/README.md specifies them.
const BLK_RANGE: u64 = 16 << 20;
const BLK_LAT: usize = 10_000;
const QD: usize = 32;
const SEQ: usize = 128 << 10;

/// The workload's offsets: xorshift64 from 1, 4 KiB-aligned.
struct Offsets {
    seed: u64,
    range: u64,
}

impl Offsets {
    fn new(disk: &storage::Disk) -> Self {
        Self {
            seed: 1,
            range: disk.bytes().min(BLK_RANGE),
        }
    }

    fn next(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed % (self.range / 4096) * 4096
    }
}

/// One synchronous read (QD1 and sequential).
fn read_sync(s: &Sleepable, disk: &storage::Disk, buf: Vec<u8>, at: u64) -> Vec<u8> {
    let done: Arc<(sync::SpinLock<Option<Vec<u8>>>, WaitQueue)> =
        Arc::new((sync::SpinLock::new(None), WaitQueue::new()));
    let back = Arc::clone(&done);
    let bio = storage::Bio::new(storage::BioOp::Read, at, buf.into(), move |buf, r| {
        assert!(r.is_ok(), "bench: read failed: {r:?}");
        *back.0.lock() = Some(buf.into_vec());
        back.1.wake_all();
    });
    disk.strategy(s, bio);
    let mut out = None;
    done.1.wait_until(s, || {
        out = done.0.lock().take();
        out.is_some()
    });
    out.expect("completion")
}

/// QD32: each completion issues the next read, until the stop.
struct Qd {
    disk: Arc<storage::Disk>,
    run: Arc<Run>,
    offsets: sync::SpinLock<Offsets>,
    completed: AtomicU64,
    inflight: AtomicUsize,
    /// Buffers of completed reads, for the worker to resubmit.
    done: sync::SpinLock<Vec<Vec<u8>>>,
    /// The worker sleeps here for completed reads, or the drain.
    wake: WaitQueue,
}

fn qd_issue(q: &Arc<Qd>, s: &Sleepable, buf: Vec<u8>) {
    let at = q.offsets.lock().next();
    let q2 = Arc::clone(q);
    // The completion hands the buffer to the worker, which resubmits: one
    // thread hop, as FreeBSD's `bio_done` runs in `g_up`, not in the
    // driver's interrupt thread (resubmitting there never lets it sleep).
    let bio = storage::Bio::new(storage::BioOp::Read, at, buf.into(), move |buf, r| {
        assert!(r.is_ok(), "bench: read failed: {r:?}");
        q2.completed.fetch_add(1, Ordering::Relaxed);
        if q2.run.stopped() {
            q2.inflight.fetch_sub(1, Ordering::AcqRel);
        } else {
            q2.done.lock().push(buf.into_vec());
        }
        q2.wake.wake_all();
    });
    q.disk.strategy(s, bio);
}

fn bench_disk(s: &Sleepable, dev: &str, name: &str) {
    let Some(disk) = storage::find(dev) else {
        kprintln!("kbench: no disk {dev}");
        return;
    };
    let cpus = [default_cpu(0)];

    // QD1 latency.
    let samples = Arc::new(sync::SpinLock::new((
        alloc::vec![0u64; BLK_LAT],
        alloc::vec![0u64; BLK_LAT],
    )));
    {
        let (d, samples) = (Arc::clone(&disk), Arc::clone(&samples));
        let run = Run::new();
        launch(s, &run, &cpus, 0, move |_, _, ws| {
            let mut offs = Offsets::new(&d);
            let mut buf = alloc::vec![0u8; 4096];
            let (mut ns, mut starts) = samples.lock().clone();
            let base = time::counter();
            for (slot, start) in ns.iter_mut().zip(&mut starts) {
                let at = offs.next();
                let t0 = time::counter();
                *start = time::ticks_to_ns(t0 - base);
                buf = read_sync(ws, &d, buf, at);
                *slot = time::ticks_to_ns(time::counter() - t0);
            }
            *samples.lock() = (ns, starts);
        });
    }
    let (mut ns, starts) = samples.lock().clone();
    report_tail(&alloc::format!("blk.{name}.qd1"), &ns, &starts);
    report_lat(&alloc::format!("blk.{name}.qd1"), &mut ns);

    // QD32 throughput.
    report_median(&alloc::format!("blk.{name}.qd32"), || {
        let run = Run::new();
        let q = Arc::new(Qd {
            disk: Arc::clone(&disk),
            run: Arc::clone(&run),
            offsets: sync::SpinLock::new(Offsets::new(&disk)),
            completed: AtomicU64::new(0),
            inflight: AtomicUsize::new(QD),
            done: sync::SpinLock::new(Vec::new()),
            wake: WaitQueue::new(),
        });
        let q2 = Arc::clone(&q);
        let ticks = launch(s, &run, &cpus, RUN_NS, move |run, i, ws| {
            for _ in 0..QD {
                qd_issue(&q2, ws, alloc::vec![0u8; 4096]);
            }
            loop {
                q2.wake.wait_until(ws, || {
                    !q2.done.lock().is_empty() || q2.inflight.load(Ordering::Acquire) == 0
                });
                // Taken in its own statement: no spinlock across submit.
                let bufs = core::mem::take(&mut *q2.done.lock());
                if run.stopped() {
                    // Completed after the stop, before being resubmitted.
                    q2.inflight.fetch_sub(bufs.len(), Ordering::AcqRel);
                } else {
                    for buf in bufs {
                        qd_issue(&q2, ws, buf);
                    }
                }
                if q2.inflight.load(Ordering::Acquire) == 0 {
                    break;
                }
            }
            run.ops[i]
                .0
                .store(q2.completed.load(Ordering::Relaxed), Ordering::Relaxed);
        });
        milli_ns_per_op(&run, 1, ticks)
    });

    // Sequential 128 KiB reads.
    let d = Arc::clone(&disk);
    report_median(&alloc::format!("blk.{name}.seq"), || {
        let run = Run::new();
        let d = Arc::clone(&d);
        let ticks = launch(s, &run, &cpus, RUN_NS, move |run, i, ws| {
            let range = d.bytes().min(BLK_RANGE);
            let (mut at, mut n, mut buf) = (0, 0, alloc::vec![0u8; SEQ]);
            while !run.stopped() {
                buf = read_sync(ws, &d, buf, at);
                at = (at + SEQ as u64) % range;
                n += 1;
            }
            run.ops[i].0.store(n, Ordering::Relaxed);
        });
        milli_ns_per_op(&run, 1, ticks)
    });
}

/// The nodes the AML benchmark evaluates: every device, plus the predefined
/// `\_SB` and `\_TZ` scopes, which ACPICA's device walk also visits.
fn aml_walk(interp: &aml::Interpreter) -> Vec<aml::NodeId> {
    let mut nodes = interp.devices();
    let root = interp.root();
    nodes.extend(["_SB", "_TZ"].iter().filter_map(|n| interp.child(root, n)));
    nodes
}

/// `_STA` and `_CRS` of every namespace device, per device.
fn bench_aml(s: &Sleepable) {
    let Some(svc) = acpi::service() else {
        kprintln!("kbench: no ACPI namespace");
        return;
    };
    let n = svc.run(s, |interp, _| aml_walk(interp).len());
    kprintln!("kbench: aml devices {n}");
    report_median("aml.sta_crs", || {
        let (ticks, n) = svc.run(s, |interp, host| {
            let t0 = time::counter();
            let devices = aml_walk(interp);
            for &d in &devices {
                let _ = black_box(interp.sta(d, host));
                let _ = black_box(interp.crs(d, host));
            }
            (time::counter() - t0, devices.len())
        });
        u128::from(time::ticks_to_ns(ticks)) * 1000 / (n.max(1) as u128)
    });
}

/// Interrupt latency (x86): an HPET comparator delivered as an MSI to
/// CPU 0, from the counter reaching it to the filter running, while the
/// CPU spins (so no vCPU wake-up is in the way). FreeBSD's kbench drives
/// its per-CPU HPET event timer the same way. AbyssBSD's `hpet` driver
/// claims nothing, so this programs a timer itself.
#[cfg(target_arch = "x86_64")]
mod intr {
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::{CpuId, Sleepable, time};

    const CAP: usize = 0x00;
    const PERIOD_FS: usize = 0x04;
    const CONFIG: usize = 0x10;
    const COUNTER: usize = 0xf0;
    const fn timer_config(n: usize) -> usize {
        0x100 + 0x20 * n
    }
    const fn comparator(n: usize) -> usize {
        0x108 + 0x20 * n
    }
    const fn fsb_route(n: usize) -> usize {
        0x110 + 0x20 * n
    }
    const ENABLE: u32 = 1;
    const LEGACY_ROUTE: u32 = 1 << 1;
    const INT_ENABLE: u32 = 1 << 2;
    const MODE_32: u32 = 1 << 8;
    const FSB_ENABLE: u32 = 1 << 14;
    const FSB_CAPABLE: u32 = 1 << 15;
    const SAMPLES: usize = 1000;
    /// How far ahead each comparator is set.
    const AHEAD_NS: u64 = 20_000;

    /// The mapped registers and the timer in use.
    static BASE: AtomicUsize = AtomicUsize::new(0);
    static TIMER: AtomicUsize = AtomicUsize::new(0);
    /// The TSC when the filter ran; 0 while waiting.
    static FIRED: AtomicU64 = AtomicU64::new(0);

    fn regs() -> regs::Mmio {
        // SAFETY: the HPET block mapped in `run`, for good.
        unsafe { regs::Mmio::new(BASE.load(Ordering::Relaxed), 1024) }
    }

    fn set_enabled(on: bool) {
        let n = TIMER.load(Ordering::Relaxed);
        let c = regs().read32(timer_config(n));
        regs().write32(
            timer_config(n),
            if on { c | INT_ENABLE } else { c & !INT_ENABLE },
        );
    }

    /// The timer's MSI: fixed delivery, edge, to `cpu`'s local APIC.
    struct Domain;

    impl ::irq::IrqDomain for Domain {
        fn name(&self) -> &'static str {
            "hpet"
        }
        fn connect(
            &self,
            _: u32,
            _: ::irq::Trigger,
            _: ::irq::Polarity,
            cpu: CpuId,
        ) -> Result<u32, ::irq::IrqError> {
            let dest = crate::intc::hw_id(cpu)
                .and_then(|id| u32::try_from(id).ok())
                .filter(|&id| id < 256)
                .ok_or(::irq::IrqError::NoSuchInput)?;
            let vector = ::irq::alloc_vector(cpu).ok_or(::irq::IrqError::NoVector)?;
            let n = TIMER.load(Ordering::Relaxed);
            regs().write32(fsb_route(n), vector);
            regs().write32(fsb_route(n) + 4, 0xfee0_0000 | (dest << 12));
            let c = regs().read32(timer_config(n));
            regs().write32(timer_config(n), (c & !INT_ENABLE) | FSB_ENABLE | MODE_32);
            Ok(vector)
        }
        fn disconnect(&self, _: u32, cpu: CpuId, root: u32) {
            let n = TIMER.load(Ordering::Relaxed);
            let c = regs().read32(timer_config(n));
            regs().write32(timer_config(n), c & !(INT_ENABLE | FSB_ENABLE));
            ::irq::free_vector(cpu, root);
        }
        fn mask(&self, _: u32) {
            set_enabled(false);
        }
        fn unmask(&self, _: u32) {
            set_enabled(true);
        }
    }

    static DOMAIN: Domain = Domain;

    fn filter(_: usize, _: &sched::IrqCx) -> ::irq::Filter {
        FIRED.store(time::counter(), Ordering::Release);
        ::irq::Filter::Handled
    }

    /// Latency samples in ns, or `None` without an HPET that can send MSIs.
    pub(super) fn run(s: &Sleepable) -> Option<alloc::vec::Vec<u64>> {
        let t = crate::acpi::find(*b"HPET")?;
        let h = acpi_tables::hpet::parse(&t).ok()?;
        let va = crate::mm::map_device(addr::PhysAddr::new(h.base.address), 1024).ok()?;
        BASE.store(usize::try_from(va.get()).ok()?, Ordering::Relaxed);
        let r = regs();
        let timers = ((r.read32(CAP) >> 8) & 0x1f) as usize + 1;
        let n = (0..timers)
            .rev()
            .find(|&n| r.read32(timer_config(n)) & FSB_CAPABLE != 0)?;
        TIMER.store(n, Ordering::Relaxed);
        let period_fs = u64::from(r.read32(PERIOD_FS));
        if period_fs == 0 {
            return None;
        }
        let ahead = u32::try_from(AHEAD_NS * 1_000_000 / period_fs).ok()?;
        // The main counter runs; interrupts go where each timer says.
        r.write32(CONFIG, (r.read32(CONFIG) & !LEGACY_ROUTE) | ENABLE);
        let installed = ::irq::install(
            &DOMAIN,
            0,
            ::irq::Trigger::Edge,
            ::irq::Polarity::High,
            CpuId::BOOT,
            ::irq::Handler {
                name: "hpet-bench",
                filter: Some(filter),
                thread: None,
                arg: 0,
            },
        )
        .ok()?;
        let ahead_ticks = time::ns_to_ticks(AHEAD_NS);
        let mut ns = alloc::vec::Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            FIRED.store(0, Ordering::Relaxed);
            let t0 = time::counter();
            let now = r.read32(COUNTER);
            r.write32(comparator(n), now.wrapping_add(ahead));
            let deadline = t0 + time::ns_to_ticks(10_000_000);
            let fired = loop {
                let f = FIRED.load(Ordering::Acquire);
                if f != 0 || time::counter() > deadline {
                    break f;
                }
                core::hint::spin_loop();
            };
            if fired == 0 {
                break;
            }
            ns.push(time::ticks_to_ns(fired.saturating_sub(t0 + ahead_ticks)));
        }
        installed.remove(s);
        (ns.len() == SAMPLES).then_some(ns)
    }
}

/// Interrupt latency, where there is an HPET to measure it with.
fn bench_intr(s: &Sleepable) {
    #[cfg(target_arch = "x86_64")]
    match intr::run(s) {
        Some(mut ns) => report_lat("intr.latency", &mut ns),
        None => kprintln!("kbench: no HPET timer with MSI delivery"),
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = s;
}

/// The `aml.load` payload: QEMU q35's DSDT moved under `\KBNS`
/// (tests/perf/kbench/ssdt-q35.dsl).
static AML_SSDT: &[u8] = include_bytes!("../../tests/perf/kbench/ssdt-q35.aml");
const AML_LOADS: usize = 16;

/// AML namespace load: the payload loaded as an SSDT into a fresh
/// interpreter holding the firmware's tables (loaded untimed), as FreeBSD
/// loads it into its live namespace. Median of the loads.
fn bench_aml_load(s: &Sleepable) {
    let Some(svc) = acpi::service() else {
        return;
    };
    let (blocks, config) = (svc.definition_blocks(), svc.config());
    let mut ns = svc.run(s, move |_, host| {
        let mut ns = Vec::with_capacity(AML_LOADS);
        for _ in 0..AML_LOADS {
            let mut interp = aml::Interpreter::new(config.clone());
            for b in &blocks {
                let _ = interp.load_table(b, host);
            }
            let t0 = time::counter();
            let r = interp.load_table(AML_SSDT, host);
            ns.push(time::ticks_to_ns(time::counter() - t0));
            assert!(
                r.is_ok_and(|r| r.warnings.is_empty()),
                "bench: aml.load failed"
            );
        }
        ns
    });
    ns.sort_unstable();
    kprintln!("kbench aml.load {} ns", ns[AML_LOADS / 2]);
}

/// DMA: map one page-aligned 4 KiB buffer for a read, and unmap it.
fn bench_dma(s: &Sleepable) {
    let tag = vm::dma::DmaTag::new(
        &crate::mm::dma::KERNEL_DMA,
        vm::dma::DmaConstraints::UNLIMITED,
        crate::mm::dma::top_of_ram(),
    );
    throughput_with(
        s,
        "dma.map",
        1,
        || alloc::vec![0u8; 4096],
        move |buf, _| {
            let Ok(mapped) = tag.map::<vm::dma::dir::FromDevice, _>(core::mem::take(buf)) else {
                panic!("bench: dma map failed");
            };
            *buf = black_box(mapped).unmap();
            1
        },
    );
}

/// Attach and detach: vtblk1's transport, detached and probed again, each
/// until the manager is idle, with the console muted. Median of the cycles.
fn bench_dev(s: &Sleepable) {
    const CYCLES: usize = 16;
    let m = bus::manager();
    let Some((transport, _)) = crate::devices::find_named(m, s, crate::devices::nexus(), "vtblk1")
    else {
        kprintln!("kbench: no vtblk1");
        return;
    };
    // Muted, as FreeBSD's is: an emulated UART is slower than the attach.
    crate::console::set_muted(true);
    let (mut attach, mut cycle) = ([0u64; CYCLES], [0u64; CYCLES]);
    for i in 0..CYCLES {
        let t0 = time::counter();
        m.detach(transport, bus::DetachReason::Requested, s)
            .expect("bench: detach");
        m.wait_idle(s);
        let t1 = time::counter();
        m.probe_and_attach(transport, s).expect("bench: attach");
        m.wait_idle(s);
        let t2 = time::counter();
        attach[i] = time::ticks_to_ns(t2 - t1);
        cycle[i] = time::ticks_to_ns(t2 - t0);
    }
    crate::console::set_muted(false);
    attach.sort_unstable();
    cycle.sort_unstable();
    kprintln!("kbench dev.vtblk.attach {} ns", attach[CYCLES / 2]);
    kprintln!("kbench dev.vtblk.cycle {} ns", cycle[CYCLES / 2]);
}

/// Gate cost: an enter and exit. FreeBSD's counterpart is the reference
/// devfs takes around each cdevsw call (`dev_refthread`).
static GATE: bus::Gate = bus::Gate::new();

fn suite_p3(s: &Sleepable) {
    bench_disk(s, "vtbd1", "vtbd");
    bench_disk(s, "nda0", "nda");
    bench_aml(s);
    bench_aml_load(s);
    bench_intr(s);
    bench_dma(s);
    for t in [1, 8] {
        throughput(s, &alloc::format!("gate.t{t}"), t, |_| {
            let Ok(guard) = GATE.enter() else {
                panic!("bench: gate revoked");
            };
            drop(black_box(guard));
            1
        });
    }
    // Last: it re-creates vtbd1.
    bench_dev(s);
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
