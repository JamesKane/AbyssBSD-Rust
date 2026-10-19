// SPDX-License-Identifier: BSD-2-Clause
//! Processes: an address space and the threads that run in it (FreeBSD's
//! `struct proc`).
//!
//! So far a process has one thread, created in user mode at a given entry
//! point, and an exit status its creator can wait for. `fork`, `exec`,
//! process IDs, and the process tree come with P4.3.

// The system calls' tests create processes; `exec` (P4.3) will too.
#![cfg_attr(not(feature = "ktest"), allow(dead_code))]

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use sched::{Sleepable, WaitQueue};

use crate::arch;
use crate::mm::space::{self, AddressSpace};

/// A process.
pub(crate) struct Process {
    space: Arc<AddressSpace>,
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
}

impl Process {
    /// A process running in `space`, with no threads yet.
    pub(crate) fn new(space: Arc<AddressSpace>) -> Arc<Self> {
        Arc::new(Self {
            space,
            status: AtomicI32::new(0),
            exited: AtomicBool::new(false),
            waiters: WaitQueue::new(),
            tls_base: AtomicU64::new(u64::MAX),
        })
    }

    pub(crate) fn space(&self) -> &Arc<AddressSpace> {
        &self.space
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

    /// Starts a thread in user mode at `entry` with stack pointer `sp`,
    /// bound to `cpu` if given.
    pub(crate) fn start(
        self: &Arc<Self>,
        entry: u64,
        sp: u64,
        cpu: Option<hal::CpuId>,
    ) -> Result<(), sched::SpawnError> {
        let start = Arc::new(Start {
            process: Arc::clone(self),
            entry,
            sp,
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

/// A new user thread's first code: joins its process and leaves for user
/// mode. Nothing on this stack outlives the jump.
fn enter(_s: &Sleepable, arg: usize) -> ! {
    // SAFETY: `start` made `arg` with `Arc::into_raw` and handed it over.
    let start = unsafe { Arc::from_raw(arg as *const Start) };
    let process = Arc::clone(&start.process);
    let frame = arch::user::user_frame(start.entry, start.sp, 0);
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
