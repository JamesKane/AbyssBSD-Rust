// SPDX-License-Identifier: BSD-2-Clause
//! A sampling profiler (`abyss.prof`), for finding where the benchmarks'
//! time goes: each CPU's timer interrupts it every [`PERIOD_NS`], and the
//! address it interrupted is recorded (user mode as one bucket), with the
//! first return address up its frame chain (for a function that keeps no
//! frame, such as `memcpy`, its caller's caller). When a benchmark prints
//! its result (a console line starting `kbench `), the functions sampled
//! since the last result are reported, most first, the hottest with their
//! callers, and the count starts again, so each report covers one
//! benchmark and its set-up. Time spent waiting for the disk, which no
//! sample sees, is counted too: each synchronous request's wait, by its
//! waiter's call chain. Off unless asked for: nothing is armed then.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use hal::MAX_CPUS;
use sched::IrqCx;
use sync::{Once, SpinLock};

use crate::time::Timer;

/// How often each CPU is sampled.
const PERIOD_NS: u64 = 100_000;

/// Samples kept per CPU between reports (about 26 s of one busy CPU);
/// past that they are counted as dropped.
const PER_CPU: usize = 1 << 18;

/// Functions listed per report, how many of them with their callers, and
/// how many callers each.
const TOP: usize = 40;
const TOP_WITH_CALLERS: usize = 12;
const CALLERS: usize = 4;

/// What a user-mode sample records.
const USER: u64 = 1;

static ON: AtomicBool = AtomicBool::new(false);

/// The one CPU whose samples the reports' functions cover
/// (`abyss.prof.cpu=N`; the benchmarks bind to one), or `usize::MAX` for
/// all. Every CPU's busy count is reported either way.
static ONLY_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);

/// A report is being made: what it costs is no benchmark's.
static REPORTING: AtomicBool = AtomicBool::new(false);

/// The benchmark asked for no samples for now (a `kprof pause` line, until
/// `kprof resume`): what it runs meanwhile is set-up, not what it reports.
static PAUSED: AtomicBool = AtomicBool::new(false);

/// Whether what happens now is left out of the profile.
fn ignored() -> bool {
    REPORTING.load(Ordering::Relaxed) || PAUSED.load(Ordering::Relaxed)
}

/// The address the interrupt being handled on each CPU interrupted, and
/// the return address above it.
static PC: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static CALLER: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Each CPU's samples: address and return address.
static SAMPLES: [SpinLock<Vec<(u64, u64)>>; MAX_CPUS] =
    [const { SpinLock::new(Vec::new()) }; MAX_CPUS];
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Return addresses kept per disk wait, and wait chains listed per report.
const CHAIN: usize = 10;
const TOP_WAITS: usize = 10;

/// Disk waits by call chain: how many, and nanoseconds in all.
static WAITS: SpinLock<BTreeMap<[u64; CHAIN], (u64, u64)>> = SpinLock::new(BTreeMap::new());

static TIMERS: [Once<Pin<&'static Timer>>; MAX_CPUS] = [const { Once::new() }; MAX_CPUS];
/// The kernel's functions, sorted, made at the first report.
static FUNCTIONS: Once<Vec<(u64, u64, &'static [u8])>> = Once::new();
static ARMED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// A function's samples: how many in all, and how many from each caller.
type Counted = (u64, BTreeMap<&'static [u8], u64>);

/// Whether profiling is on.
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Sets this CPU up for sampling, if `abyss.prof` asks for it. Once per
/// CPU, at start-up, after its timer queue.
pub(crate) fn start_cpu() {
    if !crate::boot::flag("abyss.prof") {
        return;
    }
    if !ON.swap(true, Ordering::Relaxed) {
        if let Some(cpu) = crate::boot::value("abyss.prof.cpu").and_then(|v| v.parse().ok()) {
            ONLY_CPU.store(cpu, Ordering::Relaxed);
        }
        storage::disk::set_wait_hook(storage::disk::WaitHook {
            now: crate::time::now_ns,
            waited: io_waited,
        });
    }
    let cpu = hal::cpu_id().index();
    SAMPLES[cpu].lock().reserve_exact(PER_CPU);
    // Armed by the CPU's first interrupt: a started CPU's scheduler is not
    // up yet, and it takes no interrupt before it is.
    let t: Pin<&'static Timer> = Pin::static_ref(Box::leak(Box::new(Timer::new())));
    let _ = TIMERS[cpu].set(t);
}

/// Notes where an interrupt about to be handled interrupted.
pub(crate) fn interrupted(pc: u64, fp: u64, from_user: bool) {
    if on() {
        let cpu = hal::cpu_id().index();
        let (pc, caller) = if from_user {
            (USER, 0)
        } else {
            (pc, crate::arch::backtrace::walk(fp).next().unwrap_or(0))
        };
        PC[cpu].store(pc, Ordering::Relaxed);
        CALLER[cpu].store(caller, Ordering::Relaxed);
        if !ARMED[cpu].swap(true, Ordering::Relaxed)
            && let Some(t) = TIMERS[cpu].get()
        {
            t.reset(PERIOD_NS, sample, cpu);
        }
    }
}

fn sample(cpu: usize, _: &IrqCx) {
    let pc = PC[cpu].load(Ordering::Relaxed);
    let caller = CALLER[cpu].load(Ordering::Relaxed);
    if !ignored() {
        let mut v = SAMPLES[cpu].lock();
        if v.len() < v.capacity() {
            v.push((pc, caller));
        } else {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(t) = TIMERS[cpu].get() {
        t.reset(PERIOD_NS, sample, cpu);
    }
}

/// A synchronous disk request waited `ns`: counted by the waiter's chain.
fn io_waited(ns: u64) {
    if ignored() {
        return;
    }
    let mut chain = [0u64; CHAIN];
    for (slot, ret) in chain
        .iter_mut()
        .zip(crate::arch::backtrace::walk(crate::arch::backtrace::here()))
    {
        *slot = ret;
    }
    let mut w = WAITS.lock();
    let e = w.entry(chain).or_default();
    e.0 += 1;
    e.1 += ns;
}

/// Console output: a benchmark's result line ends its profile, and a
/// `kprof` line pauses or resumes sampling.
pub(crate) fn console_output(bytes: &[u8]) {
    if let Some(rest) = bytes.strip_prefix(b"kprof ") {
        if rest.starts_with(b"pause") {
            PAUSED.store(true, Ordering::Relaxed);
        } else if rest.starts_with(b"resume") {
            PAUSED.store(false, Ordering::Relaxed);
        }
    } else if let Some(rest) = bytes.strip_prefix(b"kbench ")
        && !rest.starts_with(b"begin ")
    {
        let end = rest
            .iter()
            .position(|&b| b == b' ' || b == b'\n')
            .unwrap_or(rest.len());
        report(&rest[..end]);
    }
}

/// Reports the samples taken since the last report, by function, and
/// starts again. Idle CPUs' samples (in the idle loop) are left out of the
/// shares, which are of the busy samples.
fn report(label: &[u8]) {
    REPORTING.store(true, Ordering::Relaxed);
    print_report(label);
    REPORTING.store(false, Ordering::Relaxed);
}

fn print_report(label: &[u8]) {
    let dropped = DROPPED.swap(0, Ordering::Relaxed);
    // By address first (cheap), each CPU's samples under its lock
    // (sampling there waits meanwhile), then by function.
    if FUNCTIONS.get().is_none() {
        let _ = FUNCTIONS.set(crate::symbols::functions());
    }
    let functions: &[(u64, u64, &[u8])] = FUNCTIONS.get().map_or(&[], Vec::as_slice);
    let function_of = |pc: u64| -> Option<&'static [u8]> {
        let i = functions.partition_point(|&(start, _, _)| start <= pc);
        let (start, size, name) = functions[i.checked_sub(1)?];
        (pc - start < size).then_some(name)
    };
    let only = ONLY_CPU.load(Ordering::Relaxed);
    let mut by_pc: BTreeMap<(u64, u64), u64> = BTreeMap::new();
    let mut total = 0u64;
    // Each CPU's samples not in the idle loop; whether a function is the
    // idle loop's, worked out once each.
    let mut busy_on = [0u64; MAX_CPUS];
    let mut idle_fn: BTreeMap<&'static [u8], bool> = BTreeMap::new();
    for (cpu, s) in SAMPLES.iter().enumerate() {
        let mut v = s.lock();
        for &sample in v.iter() {
            let idle = sample.0 != USER
                && function_of(sample.0)
                    .is_some_and(|f| *idle_fn.entry(f).or_insert_with(|| is_idle(f)));
            if !idle {
                busy_on[cpu] += 1;
            }
            if only == usize::MAX || only == cpu {
                total += 1;
                *by_pc.entry(sample).or_default() += 1;
            }
        }
        v.clear();
    }
    // Each function's samples, in all and by caller.
    let mut by_fn: BTreeMap<&'static [u8], Counted> = BTreeMap::new();
    let mut unknown = 0u64;
    let mut user = 0u64;
    for (&(pc, caller), &n) in &by_pc {
        if pc == USER {
            user += n;
            continue;
        }
        let Some(name) = function_of(pc) else {
            unknown += n;
            continue;
        };
        let f = by_fn.entry(name).or_default();
        f.0 += n;
        // A return address is just past its call.
        let from = function_of(caller.saturating_sub(1)).unwrap_or(b"?");
        *f.1.entry(from).or_default() += n;
    }
    let idle: u64 = by_fn
        .iter()
        .filter(|(name, _)| is_idle(name))
        .map(|(_, (n, _))| n)
        .sum();
    let busy = total - idle;
    let label = core::str::from_utf8(label).unwrap_or("?");
    kprintln!(
        "prof {label}: {total} samples, {busy} busy ({user} user, {unknown} unknown), {dropped} dropped"
    );
    let mut line = alloc::string::String::new();
    for (cpu, n) in busy_on.iter().enumerate().filter(|(_, n)| **n > 0) {
        let _ = core::fmt::write(&mut line, format_args!(" {cpu}:{n}"));
    }
    if only == usize::MAX {
        kprintln!("prof {label}: busy by CPU:{line}");
    } else {
        kprintln!("prof {label}: busy by CPU:{line} (functions: CPU {only} only)");
    }
    let share = |n: u64| {
        let tenths = n * 1000 / busy.max(1);
        (tenths / 10, tenths % 10)
    };
    let mut top: Vec<_> = by_fn
        .into_iter()
        .filter(|(name, _)| !is_idle(name))
        .collect();
    top.sort_by_key(|(_, (n, _))| core::cmp::Reverse(*n));
    for (i, (name, (n, callers))) in top.into_iter().take(TOP).enumerate() {
        let (whole, tenth) = share(n);
        kprintln!(
            "prof {label}: {whole:3}.{tenth}% {n:7} {}",
            demangle::Demangled(name)
        );
        if i < TOP_WITH_CALLERS {
            let mut callers: Vec<_> = callers.into_iter().collect();
            callers.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
            for (from, n) in callers.into_iter().take(CALLERS) {
                let (whole, tenth) = share(n);
                kprintln!(
                    "prof {label}:        {whole:3}.{tenth}% from {}",
                    demangle::Demangled(from)
                );
            }
        }
    }
    print_waits(label, &function_of);
}

/// Reports the disk waits since the last report, longest in all first,
/// each by its chain past the storage layer, and starts again.
fn print_waits(label: &str, function_of: &dyn Fn(u64) -> Option<&'static [u8]>) {
    let waits = core::mem::take(&mut *WAITS.lock());
    let total: u64 = waits.values().map(|&(_, ns)| ns).sum();
    let count: u64 = waits.values().map(|&(n, _)| n).sum();
    kprintln!(
        "prof {label}: disk waits: {count} requests, {}.{:03} ms",
        total / 1_000_000,
        total / 1000 % 1000
    );
    // Chains that differ only in addresses within a function merge.
    let mut by_names: BTreeMap<Vec<&'static [u8]>, (u64, u64)> = BTreeMap::new();
    for (chain, (n, ns)) in waits {
        let names: Vec<&'static [u8]> = chain
            .iter()
            .take_while(|&&r| r != 0)
            .map(|&r| function_of(r.saturating_sub(1)).unwrap_or(b"?"))
            .collect();
        let e = by_names.entry(names).or_default();
        e.0 += n;
        e.1 += ns;
    }
    let mut top: Vec<_> = by_names.into_iter().collect();
    top.sort_by_key(|(_, (_, ns))| core::cmp::Reverse(*ns));
    for (names, (n, ns)) in top.into_iter().take(TOP_WAITS) {
        let mut line = alloc::string::String::new();
        for name in names {
            let mut f = alloc::string::String::new();
            let _ = core::fmt::write(&mut f, format_args!("{}", demangle::Demangled(name)));
            // The storage layer and this module are every chain's start.
            if f.contains("storage::") || f.contains("prof::") || f.contains("DiskBlocks") {
                continue;
            }
            if !line.is_empty() {
                line.push_str(" < ");
            }
            line.push_str(&f);
        }
        kprintln!(
            "prof {label}:   {}.{:03} ms {n} requests: {line}",
            ns / 1_000_000,
            ns / 1000 % 1000
        );
    }
}

/// Whether `name` is where an idle CPU waits.
fn is_idle(name: &[u8]) -> bool {
    let mut s = alloc::string::String::new();
    let _ = core::fmt::write(&mut s, format_args!("{}", demangle::Demangled(name)));
    s.contains("idle_loop") || s.contains("run_idle") || s.contains("wait_for_interrupt")
}
