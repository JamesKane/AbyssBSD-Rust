// SPDX-License-Identifier: BSD-2-Clause
//! Interrupt domains and dispatch (docs/DRIVER-MODEL.md §8.3).
//!
//! Interrupts reach a CPU through a hierarchy of **domains**: a device's
//! input (an I/O APIC pin, a GIC SPI, later an MSI-X vector) is
//! *connected* by its domain to a **root id**, the number the CPU's
//! interrupt controller reports (an x86 vector on one CPU, a GIC INTID).
//! The domains that program real controllers live with the kernel's
//! platform code; this crate holds what is common:
//!
//! - [`IrqDomain`], the interface every domain implements;
//! - the handler table, indexed by root id, which [`dispatch`] reads with
//!   no lock, lookup, or allocation: one load and one indirect call;
//! - **filters**, run in interrupt context ([`IrqCx`]), and **interrupt
//!   threads** for work that must sleep ([`Sleepable`]), FreeBSD's
//!   filter/ithread split;
//! - shared lines: level-triggered handlers on one input (PCI INTx) are
//!   chained; each filter says whether the interrupt was its device's, and
//!   the line stays masked until every thread it woke has run;
//! - synchronous removal: a handler is unlinked, then removal waits until
//!   no CPU is still walking its line, so a removed handler's data can be
//!   freed at once;
//! - storm detection: a line that keeps firing with nobody claiming it is
//!   masked and reported, as FreeBSD's `hw.intr_storm_threshold` does;
//! - per-CPU allocation of root ids for controllers that have per-CPU
//!   vectors (x86).
//!
//! Behavior reference: sys/kern/kern_intr.c, sys/x86/x86/intr_machdep.c,
//! sys/kern/subr_intr.c (INTRNG).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use hal::{CpuId, MAX_CPUS};
use sched::{IrqCx, Priority, Sleepable, Thread, WaitQueue};
use sync::{Once, SpinLock};

/// What a filter did, FreeBSD's `FILTER_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Not this device's interrupt.
    Stray,
    /// Handled entirely in the filter.
    Handled,
    /// Run the thread handler (the line stays masked until it has, if
    /// level-triggered).
    Schedule,
}

/// A filter: runs in interrupt context with the handler's argument.
pub type FilterFn = fn(usize, &IrqCx) -> Filter;
/// A thread handler: runs in the line's interrupt thread.
pub type ThreadFn = fn(usize, &Sleepable);

/// How a line signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Edge-triggered.
    Edge,
    /// Level-triggered.
    Level,
}

/// Which level is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    /// Active high (or rising edge).
    High,
    /// Active low (or falling edge).
    Low,
}

/// Why a handler could not be installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqError {
    /// The domain has no such input.
    NoSuchInput,
    /// No free root id on the target CPU.
    NoVector,
    /// The input has a handler it cannot share (only level-triggered
    /// handlers share), or the root id is taken by another input.
    Busy,
    /// A threaded-only domain was given a filter.
    NeedsThread,
    /// Neither a filter nor a thread handler.
    NoHandler,
    /// The interrupt thread could not be started.
    NoThread,
}

/// A source of interrupts: one interrupt controller's inputs (or, later,
/// a bus's message-signalled ones).
pub trait IrqDomain: Send + Sync {
    /// A name for reports.
    fn name(&self) -> &'static str;
    /// Programs input `hw` (masked) to arrive at `cpu`, returning the root
    /// id the CPU will report for it.
    fn connect(
        &self,
        hw: u32,
        trigger: Trigger,
        polarity: Polarity,
        cpu: CpuId,
    ) -> Result<u32, IrqError>;
    /// Undoes [`IrqDomain::connect`] (the input is masked first).
    fn disconnect(&self, hw: u32, cpu: CpuId, root: u32);
    /// Masks input `hw`.
    fn mask(&self, hw: u32);
    /// Unmasks input `hw`.
    fn unmask(&self, hw: u32);
    /// Whether handlers must run in a thread (for example GPIO behind a
    /// bus that needs I/O to acknowledge, or ACPI GED).
    fn threaded_only(&self) -> bool {
        false
    }
}

/// What runs for an interrupt.
#[derive(Clone, Copy, Debug)]
pub struct Handler {
    /// A name for reports (`vmstat -i`-style).
    pub name: &'static str,
    /// The filter, if any.
    pub filter: Option<FilterFn>,
    /// The thread handler, if any.
    pub thread: Option<ThreadFn>,
    /// Passed to both.
    pub arg: usize,
}

/// One connected input: its handlers, and what they share.
struct Line {
    domain: &'static dyn IrqDomain,
    hw: u32,
    trigger: Trigger,
    cpu: CpuId,
    root: u32,
    /// The first handler; each links to the next.
    entries: AtomicPtr<Entry>,
    /// Holds on the line's mask: threads woken for this (level-triggered)
    /// line that have not run yet, and installs in progress; the line is
    /// unmasked when the last is released.
    busy: AtomicU32,
    /// Its last handler is being removed: a thread finishing late must
    /// not unmask it (with no slot, a held level input would storm).
    dead: AtomicBool,
    strays: AtomicU64,
    /// Strays in the current storm window, and when it started (ticks).
    storm: SpinLock<(u64, u64)>,
    masked_by_storm: AtomicBool,
}

/// One handler on a line.
struct Entry {
    handler: Handler,
    line: *const Line,
    next: AtomicPtr<Entry>,
    count: AtomicU64,
    ithread: Option<Arc<IThread>>,
}

/// An interrupt thread's state.
struct IThread {
    pending: AtomicBool,
    exit: AtomicBool,
    q: WaitQueue,
    thread: Once<Arc<Thread>>,
    /// Its entry's address, once installed.
    entry: Once<usize>,
}

/// The platform's choices, set once by [`init`].
#[derive(Clone, Debug)]
pub struct Config {
    /// Root ids below this are per CPU (x86: 256 vectors; arm: 32 SGIs and
    /// PPIs); at or above it they are global (arm SPIs).
    pub per_cpu_ids: u32,
    /// Global root ids above `per_cpu_ids`, up to this (exclusive).
    pub global_ids: u32,
    /// Per-CPU ids a domain may allocate ([`alloc_vector`]): x86 device
    /// vectors.
    pub allocatable: core::ops::Range<u32>,
    /// The clock, for storm detection: counter ticks and their rate.
    pub now: fn() -> u64,
    /// Counter ticks per second.
    pub frequency: u64,
    /// Strays per second that mask a line (0 disables detection).
    pub storm_threshold: u64,
}

struct Table {
    config: Config,
    /// `per_cpu_ids` slots for each CPU, then the global slots.
    slots: Box<[AtomicPtr<Line>]>,
    /// Allocated per-CPU ids (bit per id), per CPU.
    used: SpinLock<Vec<Vec<u64>>>,
}

static TABLE: Once<Table> = Once::new();
/// The line each CPU is dispatching, for synchronous removal.
static RUNNING: [AtomicPtr<Line>; MAX_CPUS] = [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];

/// Connected lines by input, for sharing. Installs and removals hold it.
struct Lines(Vec<*mut Line>);
// SAFETY: the pointers are only followed under the lock, and lines are
// freed only after being unlinked from it.
unsafe impl Send for Lines {}
static LINES: SpinLock<Lines> = SpinLock::new(Lines(Vec::new()));
/// Interrupts with no handler.
static STRAYS: AtomicU64 = AtomicU64::new(0);

/// Sets up the handler table. Call once at boot.
///
/// # Panics
///
/// If called twice.
pub fn init(config: Config) {
    let per_cpu = config.per_cpu_ids as usize;
    let n = per_cpu * MAX_CPUS + config.global_ids.saturating_sub(config.per_cpu_ids) as usize;
    let slots: Box<[AtomicPtr<Line>]> = (0..n).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
    let words = (per_cpu).div_ceil(64);
    let used = SpinLock::new((0..MAX_CPUS).map(|_| alloc::vec![0u64; words]).collect());
    let ok = TABLE
        .set(Table {
            config,
            slots,
            used,
        })
        .is_ok();
    assert!(ok, "irq::init called twice");
}

fn table() -> &'static Table {
    TABLE.get().expect("irq::init not called")
}

impl Table {
    fn slot(&self, cpu: CpuId, root: u32) -> Option<&AtomicPtr<Line>> {
        let c = &self.config;
        let i = if root < c.per_cpu_ids {
            cpu.index() * c.per_cpu_ids as usize + root as usize
        } else if root < c.global_ids {
            MAX_CPUS * c.per_cpu_ids as usize + (root - c.per_cpu_ids) as usize
        } else {
            return None;
        };
        self.slots.get(i)
    }
}

/// Allocates a per-CPU root id on `cpu` from the configured range, for a
/// domain's [`IrqDomain::connect`].
pub fn alloc_vector(cpu: CpuId) -> Option<u32> {
    let t = table();
    let mut used = t.used.lock();
    let bits = used.get_mut(cpu.index())?;
    let id = t.config.allocatable.clone().find(|&v| {
        let (w, b) = ((v / 64) as usize, v % 64);
        bits.get(w).is_some_and(|word| word & (1 << b) == 0)
    })?;
    bits[(id / 64) as usize] |= 1 << (id % 64);
    Some(id)
}

/// Frees a root id from [`alloc_vector`].
pub fn free_vector(cpu: CpuId, id: u32) {
    let t = table();
    if let Some(bits) = t.used.lock().get_mut(cpu.index())
        && let Some(w) = bits.get_mut((id / 64) as usize)
    {
        *w &= !(1 << (id % 64));
    }
}

/// An installed handler; [`Installed::remove`] takes it away.
#[must_use = "dropping the handle leaks the handler; call remove"]
pub struct Installed {
    entry: *mut Entry,
}

// SAFETY: the handle only carries the entry's address; the entry is shared
// with dispatch under the protocol described on `remove`.
unsafe impl Send for Installed {}

impl fmt::Debug for Installed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: the entry, and its line, live until `remove`, which
        // consumes us.
        let (e, l) = unsafe { (&*self.entry, &*(*self.entry).line) };
        f.debug_struct("Installed")
            .field("name", &e.handler.name)
            .field("domain", &l.domain.name())
            .field("hw", &l.hw)
            .field("root", &l.root)
            .finish()
    }
}

fn same_domain(a: &'static dyn IrqDomain, b: &'static dyn IrqDomain) -> bool {
    core::ptr::addr_eq(a, b)
}

/// Connects input `hw` of `domain` to `cpu` and installs `handler` for it,
/// then unmasks the input. A level-triggered input that already has
/// level-triggered handlers gets this one too (a shared line), on the CPU
/// the first chose.
pub fn install(
    domain: &'static dyn IrqDomain,
    hw: u32,
    trigger: Trigger,
    polarity: Polarity,
    cpu: CpuId,
    handler: Handler,
) -> Result<Installed, IrqError> {
    if handler.filter.is_none() && handler.thread.is_none() {
        return Err(IrqError::NoHandler);
    }
    if domain.threaded_only() && handler.filter.is_some() {
        return Err(IrqError::NeedsThread);
    }
    // The thread first: spawning may sleep, and the lock below masks
    // interrupts. A refused install tells it to exit.
    let ithread = match handler.thread {
        Some(f) => Some(start_ithread(handler.name, f, handler.arg)?),
        None => None,
    };
    let refuse = |e: IrqError| {
        if let Some(it) = &ithread {
            it.exit.store(true, Ordering::Release);
            it.q.wake_all();
        }
        Err(e)
    };
    let mut lines = LINES.lock();
    // SAFETY: registered lines live until removed from the list, which
    // needs the lock we hold.
    let shared = lines
        .0
        .iter()
        .copied()
        .find(|&l| unsafe { same_domain((*l).domain, domain) && (*l).hw == hw });
    if let Some(l) = shared {
        // SAFETY: as above.
        let line = unsafe { &*l };
        if line.trigger != Trigger::Level || trigger != Trigger::Level {
            drop(lines);
            return refuse(IrqError::Busy);
        }
    }
    let line = match shared {
        Some(l) => l,
        None => {
            let root = match domain.connect(hw, trigger, polarity, cpu) {
                Ok(r) => r,
                Err(e) => {
                    drop(lines);
                    return refuse(e);
                }
            };
            let l = Box::into_raw(Box::new(Line {
                domain,
                hw,
                trigger,
                cpu,
                root,
                entries: AtomicPtr::new(ptr::null_mut()),
                busy: AtomicU32::new(0),
                dead: AtomicBool::new(false),
                strays: AtomicU64::new(0),
                storm: SpinLock::new((0, 0)),
                masked_by_storm: AtomicBool::new(false),
            }));
            let Some(slot) = table().slot(cpu, root) else {
                domain.disconnect(hw, cpu, root);
                // SAFETY: never published.
                drop(unsafe { Box::from_raw(l) });
                drop(lines);
                return refuse(IrqError::NoVector);
            };
            if slot
                .compare_exchange(ptr::null_mut(), l, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                domain.disconnect(hw, cpu, root);
                // SAFETY: never published.
                drop(unsafe { Box::from_raw(l) });
                drop(lines);
                return refuse(IrqError::Busy);
            }
            lines.0.push(l);
            l
        }
    };
    let entry = Box::into_raw(Box::new(Entry {
        handler,
        line,
        next: AtomicPtr::new(ptr::null_mut()),
        count: AtomicU64::new(0),
        ithread,
    }));
    // SAFETY: the entry is ours until published below.
    if let Some(it) = unsafe { &(*entry).ithread } {
        let _ = it.entry.set(entry.expose_provenance());
    }
    // Append. Release: the entry is fully built before dispatch can load
    // it.
    // SAFETY: the line is registered and we hold the lock.
    let l = unsafe { &*line };
    // Held while linking, released like a thread's: a shared line that a
    // thread holds masked stays masked until that thread is done.
    l.busy.fetch_add(1, Ordering::AcqRel);
    let mut link = &l.entries;
    loop {
        let next = link.load(Ordering::Acquire);
        if next.is_null() {
            break;
        }
        // SAFETY: chained entries live until unlinked under the lock.
        link = unsafe { &(*next).next };
    }
    link.store(entry, Ordering::Release);
    drop(lines);
    l.release();
    Ok(Installed { entry })
}

fn start_ithread(name: &'static str, f: ThreadFn, arg: usize) -> Result<Arc<IThread>, IrqError> {
    let it = Arc::new(IThread {
        pending: AtomicBool::new(false),
        exit: AtomicBool::new(false),
        q: WaitQueue::new(),
        thread: Once::new(),
        entry: Once::new(),
    });
    let me = Arc::clone(&it);
    let t = sched::Builder::new(name)
        .priority(Priority::INTERRUPT)
        .spawn(move |s| {
            loop {
                me.q.wait_until(s, || {
                    me.pending.load(Ordering::Acquire) || me.exit.load(Ordering::Acquire)
                });
                if me.exit.load(Ordering::Acquire) {
                    break;
                }
                me.pending.store(false, Ordering::Release);
                f(arg, s);
                me.done();
            }
        })
        .map_err(|_| IrqError::NoThread)?;
    let _ = it.thread.set(t);
    Ok(it)
}

impl IThread {
    /// The thread ran for its line: the last of a level line's woken
    /// threads unmasks it.
    fn done(&self) {
        let Some(&e) = self.entry.get() else { return };
        // SAFETY: the thread is joined before its entry and line are freed.
        let line = unsafe { &*(*core::ptr::with_exposed_provenance::<Entry>(e)).line };
        if line.trigger == Trigger::Level {
            line.release();
        }
    }
}

impl Line {
    /// Drops one hold on the line's mask; the last unmasks it.
    fn release(&self) {
        if self.busy.fetch_sub(1, Ordering::AcqRel) == 1 && !self.dead.load(Ordering::SeqCst) {
            self.domain.unmask(self.hw);
            // `remove` sets `dead` before it masks: if it masked between
            // our check and our unmask, put its mask back.
            if self.dead.load(Ordering::SeqCst) {
                self.domain.mask(self.hw);
            }
        }
    }
}

impl Installed {
    fn line(&self) -> &Line {
        // SAFETY: the entry and its line live until `remove`.
        unsafe { &*(*self.entry).line }
    }

    /// The root id the input arrives at.
    #[must_use]
    pub fn root(&self) -> u32 {
        self.line().root
    }

    /// Interrupts handled so far (filter or thread).
    #[must_use]
    pub fn count(&self) -> u64 {
        // SAFETY: as in `line`.
        unsafe { (*self.entry).count.load(Ordering::Relaxed) }
    }

    /// Whether storm detection masked the line.
    #[must_use]
    pub fn storm_masked(&self) -> bool {
        self.line().masked_by_storm.load(Ordering::Relaxed)
    }

    /// Unlinks the handler, waits until no CPU is still walking its line,
    /// stops its interrupt thread, and, for the line's last handler, masks
    /// and disconnects the input. When this returns nothing will call the
    /// handler again.
    pub fn remove(self, s: &Sleepable) {
        let e = self.entry;
        let mut lines = LINES.lock();
        // SAFETY: the entry and its line live until freed below; the lock
        // keeps the chain still.
        let (entry, lp) = unsafe { (&*e, (*e).line.cast_mut()) };
        // SAFETY: as above.
        let line = unsafe { &*lp };
        let mut link = &line.entries;
        loop {
            let cur = link.load(Ordering::Acquire);
            if cur.is_null() {
                break;
            }
            if cur == e {
                link.store(entry.next.load(Ordering::Acquire), Ordering::Release);
                break;
            }
            // SAFETY: chained entries live until unlinked under the lock.
            link = unsafe { &(*cur).next };
        }
        let last = line.entries.load(Ordering::Acquire).is_null();
        if last {
            line.dead.store(true, Ordering::SeqCst);
            line.domain.mask(line.hw);
            if let Some(slot) = table().slot(line.cpu, line.root) {
                slot.store(ptr::null_mut(), Ordering::Release);
            }
            lines.0.retain(|&l| l != lp);
        }
        drop(lines);
        // A CPU that loaded the line before the unlink marked it running
        // first; wait for every such CPU to finish walking it.
        core::sync::atomic::fence(Ordering::SeqCst);
        for (cpu, r) in RUNNING.iter().enumerate() {
            let mut spins = 0u64;
            while r.load(Ordering::Acquire) == lp {
                core::hint::spin_loop();
                spins += 1;
                if spins == 1 << 30 {
                    sched::lock::report(format_args!(
                        "irq: removing {} (hw {}): CPU {cpu} is still dispatching its line",
                        entry.handler.name, line.hw
                    ));
                }
            }
        }
        if let Some(it) = &entry.ithread {
            it.exit.store(true, Ordering::Release);
            it.q.wake_all();
            if let Some(t) = it.thread.get() {
                let mut yields = 0u64;
                while !t.has_exited() {
                    sched::yield_now();
                    yields += 1;
                    if yields == 1 << 20 {
                        sched::lock::report(format_args!(
                            "irq: removing {} (hw {}): its thread has not exited (state {:?}, \
                             pending {})",
                            entry.handler.name,
                            line.hw,
                            t.state(),
                            it.pending.load(Ordering::Relaxed)
                        ));
                    }
                }
                t.join(s);
            }
            // Woken but never run: it no longer holds the line masked.
            if it.pending.swap(false, Ordering::AcqRel) && !last {
                it.done();
            }
        }
        if last {
            line.domain.disconnect(line.hw, line.cpu, line.root);
        }
        // SAFETY: unlinked, no CPU is walking the line, and its thread is
        // gone: nothing else holds the entry (or, for the last, the line).
        unsafe {
            drop(Box::from_raw(e));
            if last {
                drop(Box::from_raw(lp));
            }
        }
    }
}

/// Runs the handlers for root id `root` on this CPU. Called by the
/// kernel's interrupt entry with interrupts masked, before it signals end
/// of interrupt to the controller. Returns false for an interrupt with no
/// handler.
pub fn dispatch(root: u32, cx: &IrqCx) -> bool {
    let cpu = hal::cpu_id();
    let Some(t) = TABLE.get() else {
        return false;
    };
    let Some(slot) = t.slot(cpu, root) else {
        return false;
    };
    let running = &RUNNING[cpu.index()];
    // Load, mark, fence, re-check. A remover unlinks, fences, then waits
    // for marks: either it sees our mark, or we see its unlink.
    let l = slot.load(Ordering::Acquire);
    if l.is_null() {
        STRAYS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    running.store(l, Ordering::Relaxed);
    core::sync::atomic::fence(Ordering::SeqCst);
    if slot.load(Ordering::Acquire) != l {
        running.store(ptr::null_mut(), Ordering::Release);
        return false;
    }
    // SAFETY: published by `install`, and `remove` cannot free the line or
    // an entry on it while this CPU's mark names the line.
    let line = unsafe { &*l };
    let mut claimed = false;
    let mut masked = false;
    let mut e = line.entries.load(Ordering::Acquire);
    while !e.is_null() {
        // SAFETY: as above.
        let entry = unsafe { &*e };
        let result = match entry.handler.filter {
            Some(f) => f(entry.handler.arg, cx),
            None => Filter::Schedule,
        };
        match result {
            Filter::Handled => {
                claimed = true;
                entry.count.fetch_add(1, Ordering::Relaxed);
            }
            Filter::Schedule => {
                claimed = true;
                entry.count.fetch_add(1, Ordering::Relaxed);
                if let Some(it) = &entry.ithread
                    && !it.pending.swap(true, Ordering::AcqRel)
                {
                    if line.trigger == Trigger::Level {
                        line.busy.fetch_add(1, Ordering::AcqRel);
                        if !masked {
                            line.domain.mask(line.hw);
                            masked = true;
                        }
                    }
                    it.q.wake_one();
                }
            }
            Filter::Stray => {}
        }
        e = entry.next.load(Ordering::Acquire);
    }
    if !claimed {
        storm_check(t, line);
    }
    running.store(ptr::null_mut(), Ordering::Release);
    true
}

/// Counts a stray and masks the line if strays exceed the threshold
/// within a second.
fn storm_check(t: &Table, l: &Line) {
    l.strays.fetch_add(1, Ordering::Relaxed);
    let c = &t.config;
    if c.storm_threshold == 0 {
        return;
    }
    let now = (c.now)();
    let mut w = l.storm.lock();
    if now.saturating_sub(w.1) > c.frequency {
        *w = (0, now);
    }
    w.0 += 1;
    if w.0 > c.storm_threshold && !l.masked_by_storm.swap(true, Ordering::Relaxed) {
        l.domain.mask(l.hw);
    }
}

/// Interrupts that arrived with no handler installed.
#[must_use]
pub fn strays() -> u64 {
    STRAYS.load(Ordering::Relaxed)
}
