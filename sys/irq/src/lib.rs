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
//! - synchronous removal: a handler is unlinked, then removal waits until
//!   no CPU is still running it, so a removed handler's data can be freed
//!   at once;
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
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

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
    /// The input already has a handler (sharing is not supported yet).
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

/// One connected input with its handler.
struct Entry {
    handler: Handler,
    domain: &'static dyn IrqDomain,
    hw: u32,
    trigger: Trigger,
    cpu: CpuId,
    root: u32,
    count: AtomicU64,
    strays: AtomicU64,
    /// Strays in the current storm window, and when it started (ticks).
    storm: SpinLock<(u64, u64)>,
    masked_by_storm: AtomicBool,
    ithread: Option<Arc<IThread>>,
}

/// An interrupt thread's state.
struct IThread {
    pending: AtomicBool,
    exit: AtomicBool,
    q: WaitQueue,
    thread: Once<Arc<Thread>>,
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
    slots: Box<[AtomicPtr<Entry>]>,
    /// Allocated per-CPU ids (bit per id), per CPU.
    used: SpinLock<Vec<Vec<u64>>>,
}

static TABLE: Once<Table> = Once::new();
/// The entry each CPU is dispatching, for synchronous removal.
static RUNNING: [AtomicPtr<Entry>; MAX_CPUS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];
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
    let slots: Box<[AtomicPtr<Entry>]> = (0..n).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
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
    fn slot(&self, cpu: CpuId, root: u32) -> Option<&AtomicPtr<Entry>> {
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
        // SAFETY: the entry lives until `remove`, which consumes us.
        let e = unsafe { &*self.entry };
        f.debug_struct("Installed")
            .field("name", &e.handler.name)
            .field("domain", &e.domain.name())
            .field("hw", &e.hw)
            .field("root", &e.root)
            .finish()
    }
}

/// Connects input `hw` of `domain` to `cpu` and installs `handler` for it,
/// then unmasks the input.
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
    let ithread = if let Some(f) = handler.thread {
        Some(start_ithread(
            handler.name,
            f,
            handler.arg,
            domain,
            hw,
            trigger,
        )?)
    } else {
        None
    };
    let root = domain.connect(hw, trigger, polarity, cpu)?;
    let entry = Box::into_raw(Box::new(Entry {
        handler,
        domain,
        hw,
        trigger,
        cpu,
        root,
        count: AtomicU64::new(0),
        strays: AtomicU64::new(0),
        storm: SpinLock::new((0, 0)),
        masked_by_storm: AtomicBool::new(false),
        ithread,
    }));
    let slot = table().slot(cpu, root).ok_or(IrqError::NoVector)?;
    // Release: the entry is fully built before dispatch can load it.
    if slot
        .compare_exchange(ptr::null_mut(), entry, Ordering::Release, Ordering::Relaxed)
        .is_err()
    {
        domain.disconnect(hw, cpu, root);
        // SAFETY: never published.
        drop(unsafe { Box::from_raw(entry) });
        return Err(IrqError::Busy);
    }
    domain.unmask(hw);
    Ok(Installed { entry })
}

fn start_ithread(
    name: &'static str,
    f: ThreadFn,
    arg: usize,
    domain: &'static dyn IrqDomain,
    hw: u32,
    trigger: Trigger,
) -> Result<Arc<IThread>, IrqError> {
    let it = Arc::new(IThread {
        pending: AtomicBool::new(false),
        exit: AtomicBool::new(false),
        q: WaitQueue::new(),
        thread: Once::new(),
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
                if trigger == Trigger::Level {
                    domain.unmask(hw);
                }
            }
        })
        .map_err(|_| IrqError::NoThread)?;
    let _ = it.thread.set(t);
    Ok(it)
}

impl Installed {
    /// The root id the input arrives at.
    #[must_use]
    pub fn root(&self) -> u32 {
        // SAFETY: the entry lives until `remove`.
        unsafe { (*self.entry).root }
    }

    /// Interrupts handled so far (filter or thread).
    #[must_use]
    pub fn count(&self) -> u64 {
        // SAFETY: as above.
        unsafe { (*self.entry).count.load(Ordering::Relaxed) }
    }

    /// Whether storm detection masked the line.
    #[must_use]
    pub fn storm_masked(&self) -> bool {
        // SAFETY: as above.
        unsafe { (*self.entry).masked_by_storm.load(Ordering::Relaxed) }
    }

    /// Masks the input, unlinks the handler, waits until no CPU is still
    /// running it, stops its interrupt thread, and disconnects the input.
    /// When this returns nothing will call the handler again.
    pub fn remove(self, s: &Sleepable) {
        let e = self.entry;
        // SAFETY: the entry lives until we free it below.
        let entry = unsafe { &*e };
        entry.domain.mask(entry.hw);
        if let Some(slot) = table().slot(entry.cpu, entry.root) {
            slot.store(ptr::null_mut(), Ordering::Release);
        }
        // A CPU that loaded the entry before the store marked it running
        // first; wait for every such CPU to finish.
        core::sync::atomic::fence(Ordering::SeqCst);
        for r in &RUNNING {
            while r.load(Ordering::Acquire) == e {
                core::hint::spin_loop();
            }
        }
        if let Some(it) = &entry.ithread {
            it.exit.store(true, Ordering::Release);
            it.q.wake_all();
            if let Some(t) = it.thread.get() {
                t.join(s);
            }
        }
        entry.domain.disconnect(entry.hw, entry.cpu, entry.root);
        // SAFETY: unlinked, no CPU is running it, and its thread is gone:
        // nothing else holds the pointer.
        drop(unsafe { Box::from_raw(e) });
    }
}

/// Runs the handler for root id `root` on this CPU. Called by the
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
    // Load, mark, fence, re-check. A remover clears the slot, fences, then
    // waits for marks: either it sees our mark, or we see its clear.
    let e = slot.load(Ordering::Acquire);
    if e.is_null() {
        STRAYS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    running.store(e, Ordering::Relaxed);
    core::sync::atomic::fence(Ordering::SeqCst);
    // Re-check: the remover may have cleared the slot before seeing us.
    if slot.load(Ordering::Acquire) != e {
        running.store(ptr::null_mut(), Ordering::Release);
        return false;
    }
    // SAFETY: published by `install`, and `remove` cannot free it while
    // this CPU's RUNNING mark names it.
    let entry = unsafe { &*e };
    let result = match entry.handler.filter {
        Some(f) => f(entry.handler.arg, cx),
        None => Filter::Schedule,
    };
    match result {
        Filter::Handled => {
            entry.count.fetch_add(1, Ordering::Relaxed);
        }
        Filter::Schedule => {
            entry.count.fetch_add(1, Ordering::Relaxed);
            if let Some(it) = &entry.ithread {
                if entry.trigger == Trigger::Level {
                    entry.domain.mask(entry.hw);
                }
                it.pending.store(true, Ordering::Release);
                it.q.wake_one();
            }
        }
        Filter::Stray => storm_check(t, entry),
    }
    running.store(ptr::null_mut(), Ordering::Release);
    true
}

/// Counts a stray and masks the line if strays exceed the threshold
/// within a second.
fn storm_check(t: &Table, e: &Entry) {
    e.strays.fetch_add(1, Ordering::Relaxed);
    let c = &t.config;
    if c.storm_threshold == 0 {
        return;
    }
    let now = (c.now)();
    let mut w = e.storm.lock();
    if now.saturating_sub(w.1) > c.frequency {
        *w = (0, now);
    }
    w.0 += 1;
    if w.0 > c.storm_threshold && !e.masked_by_storm.swap(true, Ordering::Relaxed) {
        e.domain.mask(e.hw);
    }
}

/// Interrupts that arrived with no handler installed.
#[must_use]
pub fn strays() -> u64 {
    STRAYS.load(Ordering::Relaxed)
}
