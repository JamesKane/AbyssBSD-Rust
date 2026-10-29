// SPDX-License-Identifier: BSD-2-Clause
//! User threads: what each thread of a process keeps for itself (the part
//! of FreeBSD's `struct thread` past the scheduler's).
//!
//! A user thread is a [`sched::Thread`] whose owner is a [`UserThread`],
//! which belongs to its [`Process`]. The process holds its threads in
//! creation order (FreeBSD's `p_threads`); each thread holds its process.
//! A thread leaving drops out of the list, which breaks the cycle.
//!
//! Thread IDs come from one table for the whole system, as FreeBSD's
//! `tid_alloc` hands them out: from `NO_PID` (100000) up, the lowest free
//! one after the last given, wrapping at `kern.maxthread`, so a tid is
//! never a pid. The table also finds a thread by its tid (`tdfind`).
//!
//! Each thread has its signal state ([`ThreadSignals`]: the mask, the
//! signals sent to it, the alternate stack, the `sigfastblock` word),
//! under its own lock, taken after the process's signal lock when both
//! are held; its name (`thr_set_name`); its scheduling class and priority
//! (`rtprio_thread`); the CPUs it may run on (its `cpuset`, which the
//! process's is the union of); and on amd64 the TLS base `sysarch`
//! recorded.
//!
//! Behavior reference: sys/kern/kern_thread.c, sys/kern/kern_thr.c.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::kinfo::MAXCOMLEN;
use abi::thr::{RTP_PRIO_FIFO_BIT, RTP_PRIO_IDLE, RTP_PRIO_NORMAL, RTP_PRIO_REALTIME, Rtprio};
use sched::WaitQueue;
use sync::SpinLock;

use crate::proc::Process;
use crate::signal::ThreadSignals;

/// The first tid (FreeBSD's `NO_PID`): every pid is below it.
pub(crate) const NO_PID: i32 = 100_000;
/// Threads the system may have (`kern.maxthread`): FreeBSD's 64-bit
/// bound, `maxproc` times `max_threads_per_proc` being larger.
pub(crate) const MAX_THREAD: i32 = 1_000_000;
/// Threads a process may have (`kern.threads.max_threads_per_proc`).
pub(crate) const MAX_PER_PROC: usize = 1500;
/// Times a process was refused a thread for that (`max_threads_hits`).
pub(crate) static MAX_HITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Live threads by tid.
static TIDS: SpinLock<BTreeMap<i32, Weak<UserThread>>> = SpinLock::new(BTreeMap::new());
/// Where the next search for a free tid starts, as an offset from
/// `NO_PID` (FreeBSD's `trytid`).
static TRY_TID: AtomicI32 = AtomicI32::new(0);

/// The lowest free tid at or after the last one given plus one, wrapping
/// once; recorded for `t`. `EAGAIN` when every tid is taken.
fn alloc_tid(t: &Weak<UserThread>) -> Result<i32, Errno> {
    let mut tids = TIDS.lock();
    let free_from = |from: i32| {
        let mut want = from;
        for (&tid, _) in tids.range(NO_PID + from..NO_PID + MAX_THREAD) {
            if tid != NO_PID + want {
                break;
            }
            want += 1;
        }
        (want < MAX_THREAD).then_some(want)
    };
    let mut start = TRY_TID.load(Ordering::Relaxed);
    if start >= MAX_THREAD {
        start = 0;
    }
    let off = free_from(start)
        .or_else(|| free_from(0))
        .ok_or(Errno::EAGAIN)?;
    TRY_TID.store(off + 1, Ordering::Relaxed);
    let tid = NO_PID + off;
    tids.insert(tid, Weak::clone(t));
    Ok(tid)
}

/// The live thread with tid `tid` (FreeBSD's `tdfind`, any process).
pub(crate) fn find(tid: i32) -> Option<Arc<UserThread>> {
    TIDS.lock().get(&tid)?.upgrade()
}

/// The CPU a thread with set `mask` is bound to: its one CPU, if it has
/// one.
pub(crate) fn bound(mask: u64) -> Option<hal::CpuId> {
    (mask.count_ones() == 1).then(|| hal::CpuId(mask.trailing_zeros()))
}

/// A thread's name (`td_name`): up to `MAXCOMLEN` bytes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Name {
    bytes: [u8; MAXCOMLEN + 1],
}

impl Name {
    /// The first `MAXCOMLEN` bytes of `s`, up to its first NUL.
    pub(crate) fn new(s: &[u8]) -> Self {
        let s = s.split(|&b| b == 0).next().unwrap_or(&[]);
        let mut bytes = [0; MAXCOMLEN + 1];
        let n = s.len().min(MAXCOMLEN);
        bytes[..n].copy_from_slice(&s[..n]);
        Self { bytes }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        let n = self.bytes.iter().position(|&b| b == 0).unwrap_or(MAXCOMLEN);
        &self.bytes[..n]
    }
}

/// The time-sharing class at its base (FreeBSD's default).
pub(crate) const NORMAL: Rtprio = Rtprio {
    type_: RTP_PRIO_NORMAL,
    prio: 0,
};

/// The scheduler's base priority for class and priority `r` (FreeBSD's
/// `rtp_to_pri`, with 15.1's ranges, which the scheduler's are).
pub(crate) fn priority(r: Rtprio) -> sched::Priority {
    let base = match r.type_ & !RTP_PRIO_FIFO_BIT {
        RTP_PRIO_REALTIME => sched::Priority::REALTIME,
        RTP_PRIO_IDLE => sched::Priority::IDLE,
        _ => sched::Priority::TIMESHARE,
    };
    let top = match base {
        sched::Priority::TIMESHARE => sched::Priority::IDLE.get() - 1,
        _ => sched::Priority::IDLE_THREAD.get() - 1,
    };
    let prio = u8::try_from(r.prio).unwrap_or(u8::MAX);
    sched::Priority::new(base.get().saturating_add(prio).min(top))
}

/// A user thread.
pub(crate) struct UserThread {
    tid: i32,
    process: Arc<Process>,
    /// The scheduler's thread, once it runs; cleared as it leaves.
    sched: SpinLock<Option<Arc<sched::Thread>>>,
    pub(crate) sig: SpinLock<ThreadSignals>,
    name: SpinLock<Name>,
    /// Its class and priority, as `rtprio_thread` sets them.
    rtprio: SpinLock<Rtprio>,
    /// The CPUs it may run on, one bit each.
    cpus: AtomicU64,
    /// The TLS base `sysarch(AMD64_SET_TLSBASE)` recorded, `u64::MAX` for
    /// none.
    tls_base: AtomicU64,
    /// A `thr_wake` came while the thread was not in `thr_suspend`
    /// (FreeBSD's `TDF_THRWAKEUP`, or `TDP_WAKEUP` for its own).
    pub(crate) woken: AtomicBool,
    /// Where `thr_suspend` sleeps.
    pub(crate) suspend_wait: WaitQueue,
    /// Its umtx queue entry (FreeBSD's `td_umtxq`).
    umtx: Arc<crate::umtx::Entry>,
    /// It is making a ktrace record, and traces nothing meanwhile
    /// (`TDP_INKTRACE`).
    pub(crate) in_ktrace: AtomicBool,
    /// The system call it is in, or made last (`td_sa.code`).
    code: AtomicU32,
    /// Its debugging state (`td_dbgflags`, `td_xsig`, `td_frame`, ...).
    pub(crate) dbg: crate::ptrace::ThreadDebug,
    /// The child of a traced `vfork` it still waits for
    /// (`td_rfppwait_p`).
    pub(crate) vfork_child: SpinLock<Option<Arc<Process>>>,
}

impl UserThread {
    /// A new thread of `process` with signal state `sig`, named `name`, not
    /// yet in the process's list. `EAGAIN` when no tid is free.
    pub(crate) fn new(
        process: &Arc<Process>,
        sig: ThreadSignals,
        name: Name,
        tls_base: Option<u64>,
        rtprio: Rtprio,
        cpus: u64,
    ) -> Result<Arc<Self>, Errno> {
        let mut tid = Err(Errno::EAGAIN);
        let t = Arc::new_cyclic(|me| {
            tid = alloc_tid(me);
            let id = *tid.as_ref().unwrap_or(&0);
            Self {
                tid: id,
                process: Arc::clone(process),
                sched: SpinLock::new(None),
                sig: SpinLock::new(sig),
                name: SpinLock::new(name),
                rtprio: SpinLock::new(rtprio),
                cpus: AtomicU64::new(cpus),
                tls_base: AtomicU64::new(tls_base.unwrap_or(u64::MAX)),
                woken: AtomicBool::new(false),
                suspend_wait: WaitQueue::new(),
                umtx: Arc::new(crate::umtx::Entry::new(
                    crate::umtx::Parker::new(Weak::clone(me)),
                    id.cast_unsigned(),
                )),
                in_ktrace: AtomicBool::new(false),
                code: AtomicU32::new(0),
                dbg: crate::ptrace::ThreadDebug::default(),
                vfork_child: SpinLock::new(None),
            }
        });
        tid.map(|_| t)
    }

    pub(crate) const fn tid(&self) -> i32 {
        self.tid
    }

    /// The system call it is in, or made last (`td_sa.code`).
    pub(crate) fn syscall_code(&self) -> u32 {
        self.code.load(Ordering::Relaxed)
    }

    pub(crate) fn set_syscall_code(&self, code: u32) {
        self.code.store(code, Ordering::Relaxed);
    }

    pub(crate) fn process(&self) -> &Arc<Process> {
        &self.process
    }

    pub(crate) fn umtx(&self) -> &Arc<crate::umtx::Entry> {
        &self.umtx
    }

    /// The scheduler's thread, while it runs.
    pub(crate) fn sched(&self) -> Option<Arc<sched::Thread>> {
        self.sched.lock().clone()
    }

    /// Records the scheduler's thread (the thread itself, as it starts), or
    /// clears it (as it leaves).
    pub(crate) fn set_sched(&self, t: Option<Arc<sched::Thread>>) {
        *self.sched.lock() = t;
    }

    /// Interrupts the thread for `ast` work (see [`sched::Thread::interrupt`]).
    pub(crate) fn interrupt(&self, ast: u32) {
        if let Some(t) = self.sched() {
            t.interrupt(ast);
        }
    }

    pub(crate) fn name(&self) -> Name {
        *self.name.lock()
    }

    pub(crate) fn set_name(&self, n: Name) {
        *self.name.lock() = n;
    }

    /// The CPUs it may run on, one bit each.
    pub(crate) fn cpus(&self) -> u64 {
        self.cpus.load(Ordering::Relaxed)
    }

    /// The CPU it is bound to, if its set is one CPU (the scheduler binds
    /// to one CPU or to none).
    pub(crate) fn bound_cpu(&self) -> Option<hal::CpuId> {
        bound(self.cpus())
    }

    /// Restricts it to the CPUs in `mask` (`cpuset_setaffinity`): the
    /// caller's own thread moves at once, another at its next return to
    /// user mode.
    pub(crate) fn set_cpus(&self, mask: u64) {
        self.cpus.store(mask, Ordering::Relaxed);
        if Self::with_current(|me| core::ptr::eq(me, self)) == Some(true) {
            sched::move_to(bound(mask));
        } else {
            self.interrupt(crate::signal::AST_AFFINITY);
        }
    }

    pub(crate) fn rtprio(&self) -> Rtprio {
        *self.rtprio.lock()
    }

    /// Moves the thread to class and priority `r` (checked by the caller).
    pub(crate) fn set_rtprio(&self, r: Rtprio) {
        *self.rtprio.lock() = r;
        if let Some(t) = self.sched() {
            sched::set_thread_priority(&t, priority(r));
        }
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

    /// The current thread's, if it is a user thread.
    pub(crate) fn current() -> Option<Arc<Self>> {
        sched::owner()?.downcast::<Self>().ok()
    }

    /// Runs `f` on the current user thread, borrowed: `None` on a kernel
    /// thread.
    pub(crate) fn with_current<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        sched::with_owner(|o| o?.downcast_ref::<Self>().map(f))
    }
}

impl Drop for UserThread {
    fn drop(&mut self) {
        let mut tids = TIDS.lock();
        // A thread whose tid never came has no entry; one whose entry is
        // another's (impossible while this one lived) is left alone.
        if tids.get(&self.tid).is_some_and(|w| w.strong_count() == 0) {
            tids.remove(&self.tid);
        }
    }
}
