// SPDX-License-Identifier: BSD-2-Clause
//! Processes: an address space and the threads that run in it (FreeBSD's
//! `struct proc`).
//!
//! So far a process has one thread, created in user mode at a given entry
//! point, and an exit status its creator can wait for. It has a pid from
//! the process table, the signal state system calls set (delivered from
//! P4.4), and a randomized base for `mmap` placement. `fork` and the
//! process tree come next.

// The system calls' tests create processes; `exec` (P4.3) will too.
#![cfg_attr(not(feature = "ktest"), allow(dead_code))]

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use abi::signal::{SIG_MAXSIG, Sigaction, Sigset};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;

use crate::arch;
use crate::mm::space::{self, AddressSpace};

/// The highest pid (`PID_MAX`); allocation wraps to 2 past it.
const PID_MAX: i32 = 99_999;

/// Live processes by pid.
static PROCS: SpinLock<BTreeMap<i32, Weak<Process>>> = SpinLock::new(BTreeMap::new());
/// The pid handed out last.
static LAST_PID: AtomicI32 = AtomicI32::new(0);

/// The next free pid after the last one, recorded for `p`.
fn alloc_pid(p: &Weak<Process>) -> i32 {
    let mut procs = PROCS.lock();
    let mut pid = LAST_PID.load(Ordering::Relaxed);
    loop {
        pid = if pid >= PID_MAX { 2 } else { pid + 1 };
        if !procs.contains_key(&pid) {
            break;
        }
    }
    LAST_PID.store(pid, Ordering::Relaxed);
    procs.insert(pid, Weak::clone(p));
    pid
}

/// Signal dispositions and the blocked set (delivery comes with P4.4).
pub(crate) struct Signals {
    /// Indexed by signal number minus one.
    pub(crate) actions: [Sigaction; SIG_MAXSIG as usize],
    /// Blocked signals (per thread once processes have several).
    pub(crate) mask: Sigset,
    /// The `sigfastblock` word's address, 0 for none.
    pub(crate) fastblock: u64,
}

/// A process.
pub(crate) struct Process {
    pid: i32,
    space: Arc<AddressSpace>,
    /// Where `mmap` without an address starts looking (randomized).
    mmap_base: u64,
    pub(crate) signals: SpinLock<Signals>,
    /// The `exterrctl` buffer, 0 for none.
    pub(crate) exterr: AtomicU64,
    /// The exit status, valid once `exited` is set.
    status: AtomicI32,
    exited: AtomicBool,
    waiters: WaitQueue,
    /// The TLS base `sysarch` recorded (amd64), `u64::MAX` for none. Per
    /// thread once processes have several (P6).
    tls_base: AtomicU64,
}

/// How a process's first thread enters user mode.
struct Start {
    process: Arc<Process>,
    entry: u64,
    sp: u64,
    arg: u64,
}

impl Process {
    /// A process running in `space`, with no threads yet.
    pub(crate) fn new(space: Arc<AddressSpace>) -> Arc<Self> {
        let mut r = [0u8; 8];
        // Unseeded (early boot) leaves the base unrandomized.
        let _ = crate::random::fill(&mut r);
        // 64 GiB up, plus up to 8 TiB in 2 MiB steps.
        let mmap_base = (64 << 30) + (u64::from_le_bytes(r) % (1 << 22)) * (2 << 20);
        Arc::new_cyclic(|me| Self {
            pid: alloc_pid(me),
            space,
            mmap_base,
            signals: SpinLock::new(Signals {
                actions: [Sigaction::default(); SIG_MAXSIG as usize],
                mask: Sigset::default(),
                fastblock: 0,
            }),
            exterr: AtomicU64::new(0),
            status: AtomicI32::new(0),
            exited: AtomicBool::new(false),
            waiters: WaitQueue::new(),
            tls_base: AtomicU64::new(u64::MAX),
        })
    }

    pub(crate) fn space(&self) -> &Arc<AddressSpace> {
        &self.space
    }

    pub(crate) const fn pid(&self) -> i32 {
        self.pid
    }

    pub(crate) const fn mmap_base(&self) -> u64 {
        self.mmap_base
    }

    /// The TLS base `sysarch(AMD64_SET_TLSBASE)` recorded.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub(crate) fn tls_base(&self) -> Option<u64> {
        let v = self.tls_base.load(Ordering::Relaxed);
        (v != u64::MAX).then_some(v)
    }

    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub(crate) fn set_tls_base(&self, v: Option<u64>) {
        self.tls_base
            .store(v.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// The process the current thread runs for.
    pub(crate) fn current() -> Option<Arc<Self>> {
        sched::owner()?.downcast::<Self>().ok()
    }

    /// A new process running the memory-disk program at `path`, with
    /// arguments `argv` and environment `envp`; its thread is bound to
    /// `cpu` if given.
    pub(crate) fn spawn(
        s: &Sleepable,
        path: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
        cpu: Option<hal::CpuId>,
    ) -> Result<Arc<Self>, abi::errno::Errno> {
        use abi::errno::Errno;
        let space = AddressSpace::new().map_err(|_| Errno::ENOMEM)?;
        let image = crate::exec::load(s, &space, path, argv, envp)?;
        let p = Self::new(space);
        p.start(image.entry, image.sp, image.arg, cpu)
            .map_err(|_| Errno::EAGAIN)?;
        Ok(p)
    }

    /// Starts a thread in user mode at `entry` with stack pointer `sp` and
    /// first argument `arg`, bound to `cpu` if given.
    pub(crate) fn start(
        self: &Arc<Self>,
        entry: u64,
        sp: u64,
        arg: u64,
        cpu: Option<hal::CpuId>,
    ) -> Result<(), sched::SpawnError> {
        let start = Arc::new(Start {
            process: Arc::clone(self),
            entry,
            sp,
            arg,
        });
        let arg = Arc::into_raw(start) as usize;
        let mut b = sched::Builder::new("user")
            .priority(sched::Priority::TIMESHARE)
            .fpu();
        if let Some(cpu) = cpu {
            b = b.bind(cpu);
        }
        b.spawn_raw(enter, arg).map(drop).inspect_err(|_| {
            // SAFETY: the thread never started, so `arg` is still ours.
            drop(unsafe { Arc::from_raw(arg as *const Start) });
        })
    }

    /// Records the exit status and wakes waiters; the caller's thread then
    /// exits. The address space is emptied now (the CPUs it was loaded on
    /// keep only its tables until they load another).
    pub(crate) fn exit(&self, s: &Sleepable, status: i32) {
        self.space.clear(s);
        // This CPU lets go of the space too (others it ran on hold it
        // until they load another).
        sched::set_address_space(None);
        space::unload();
        self.status.store(status, Ordering::Relaxed);
        // Release: the status is visible to whoever sees `exited`.
        self.exited.store(true, Ordering::Release);
        self.waiters.wake_all();
    }

    /// Waits for the process to exit; returns its status.
    pub(crate) fn wait(&self, s: &Sleepable) -> i32 {
        self.waiters
            .wait_until(s, || self.exited.load(Ordering::Acquire));
        self.status.load(Ordering::Relaxed)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        PROCS.lock().remove(&self.pid);
    }
}

/// A new user thread's first code: joins its process and leaves for user
/// mode. Nothing on this stack outlives the jump.
fn enter(_s: &Sleepable, arg: usize) -> ! {
    // SAFETY: `start` made `arg` with `Arc::into_raw` and handed it over.
    let start = unsafe { Arc::from_raw(arg as *const Start) };
    let process = Arc::clone(&start.process);
    let frame = arch::user::user_frame(start.entry, start.sp, start.arg);
    drop(start);
    sched::set_address_space(Some(
        Arc::clone(process.space()) as Arc<dyn sched::AddressSpace>
    ));
    sched::set_owner(Some(process));
    let _ = hal::irq::disable();
    // SAFETY: the thread now runs in the process's space, its kernel stack
    // is set (`set_address_space` loads it), interrupts are masked, and
    // everything this function owned was moved or dropped.
    unsafe { arch::user::enter_user(&frame) }
}

/// Where a test process's stack ends (just below the top of the user
/// half, as FreeBSD's `USRSTACK` is).
pub(crate) const STACK_TOP: u64 = space::USER_END - (1 << 21);
