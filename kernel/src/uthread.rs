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
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::kinfo::MAXCOMLEN;
use abi::thr::{RTP_PRIO_FIFO_BIT, RTP_PRIO_IDLE, RTP_PRIO_NORMAL, RTP_PRIO_REALTIME, Rtprio};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;

use crate::cred::Ucred;
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

/// A thread's CPUs: `mask`, and whether they are its own (an anonymous
/// set, `cpuset_setaffinity` at `CPU_LEVEL_WHICH`) rather than its
/// numbered set's, which it follows.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cpus {
    pub(crate) mask: u64,
    pub(crate) own: bool,
    /// A memory-domain policy of its own (`DOMAINSET_POLICY_*`), or 0 to
    /// follow its set's.
    pub(crate) domain: u8,
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
    /// They are its own, not its set's (see [`Cpus`]).
    own_cpus: AtomicBool,
    /// Its own memory-domain policy, 0 for its set's (see [`Cpus`]).
    domain: AtomicU8,
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
    /// Its credential (`td_ucred`): its own copy of the process's, read
    /// with no lock and no count by the thread itself ([`OwnCred`]).
    cred: OwnCred,
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
        cpus: Cpus,
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
                cpus: AtomicU64::new(cpus.mask),
                own_cpus: AtomicBool::new(cpus.own),
                domain: AtomicU8::new(cpus.domain),
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
                cred: OwnCred::new(process),
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

    /// The calling thread's credential (`td_ucred`), borrowed for no
    /// longer than the caller's `Sleepable`: `None` on a kernel thread.
    /// No lock is taken and no count changes.
    ///
    /// The borrow cannot outlive the kernel entry it is taken in. A
    /// `Sleepable` is not `Send`, `Sync`, `Clone` or `Copy`, and one only
    /// ever lives in a stack frame (no structure and no static holds one:
    /// every `Sleepable::assume()` is a local, or a function's result bound
    /// to one), so `'s` ends before the frame holding the token does. A
    /// user thread's kernel stack holds nothing from one kernel entry to
    /// the next: a system call, trap or interrupt from user mode starts on
    /// an empty stack and returns through every frame it made, and the
    /// frames a new thread makes before it first enters user mode
    /// (`proc`'s `enter`) and those under an exit (`sched::exit`) are
    /// never returned to. So the token, and with it the borrow, is of the
    /// current kernel entry, within which the credential stays allocated
    /// ([`OwnCred`]).
    #[inline]
    pub(crate) fn current_cred<'s>(_s: &'s Sleepable) -> Option<&'s Ucred> {
        // SAFETY: as in `with_current`: the owner, if any, is this
        // thread's `UserThread`, which it holds until it calls
        // `sched::set_owner(None)` on its way out (`Process::thread_exit`,
        // `Process::exit_current`), after which it runs nothing that holds
        // a credential; `'s` ends before that (above).
        let t: &'s Self = unsafe { sched::owner_at().cast::<Self>().as_ref() }?;
        // SAFETY: `t` is the calling thread's own, and `_s` says this is
        // thread context, not an interrupt handler. The credential read
        // here is freed only at the thread's next kernel entry
        // (`OwnCred::entered`) or when the thread is gone, both after `'s`
        // (above).
        Some(unsafe { &*t.cred.current() })
    }

    /// The calling thread's credential, to keep beyond the call: one count
    /// taken on the thread's own copy, with no lock. `None` on a kernel
    /// thread.
    #[inline]
    pub(crate) fn current_cred_arc() -> Option<Arc<Ucred>> {
        Self::with_current(|t| {
            // SAFETY: `with_current` gives the calling thread's own. The
            // pointer is used at once, to take a count: only this thread
            // replaces the credential, and it does not in between. The
            // pointer came from `Arc::into_raw`, whose count the state
            // keeps; the new `Arc` owns the one added here.
            unsafe {
                let c = t.cred.current();
                Arc::increment_strong_count(c);
                Arc::from_raw(c)
            }
        })
    }

    /// The calling thread's credential's jail (`td_ucred->cr_prison`):
    /// `None` on the host and on a kernel thread. No lock is taken, and
    /// the credential is not counted.
    #[inline]
    pub(crate) fn current_jail() -> Option<Arc<dyn security::Jail>> {
        Self::with_current(|t| {
            // SAFETY: `with_current` gives the calling thread's own. The
            // borrow ends with this statement, and only this thread
            // replaces or frees the credential, which it does not in
            // between.
            unsafe { (*t.cred.current()).jail.clone() }
        })
        .flatten()
    }

    /// Takes the process's credential if it changed since this thread last
    /// did (`thread_cow_update`), at a kernel entry: a system call
    /// (`syscallenter`), a trap from user mode (amd64's
    /// `trap_update_thread`), or the work before a return to user mode
    /// (`ast_prep`). The credentials the thread had before are freed.
    ///
    /// # Safety
    ///
    /// `self` is the calling thread's own (as `with_current` gives it),
    /// in thread context, and the thread has just come from user mode, or
    /// is on its way back with every frame of the entry's handler
    /// returned: nothing holds a borrow [`Self::current_cred`] gave. The
    /// callers are `syscall::entry`, `syscall::user_trap`,
    /// `mm::space::page_fault` (for a fault from user mode) and `signal`'s
    /// `user_return_work`.
    #[inline]
    pub(crate) unsafe fn cred_entered(&self) {
        // SAFETY: the caller's contract is `OwnCred::entered`'s.
        unsafe { self.cred.entered(&self.process) }
    }

    /// Takes the process's credential if it changed, inside a kernel
    /// entry: where FreeBSD renews `td_ucred` away from the boundary
    /// (`ptrace_syscallreq`). The credential the thread had stays
    /// allocated until its next kernel entry, since a caller further up
    /// the stack may hold a borrow of it.
    pub(crate) fn cred_renewed() {
        Self::with_current(|t| {
            // SAFETY: `with_current` gives the calling thread's own, in
            // thread context (only a stopped thread serving its debugger
            // calls this).
            unsafe { t.cred.renew(&t.process) }
        });
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
        // The running thread (a signal to itself): no reference to take.
        if Self::with_current(|c| core::ptr::eq(c, self)) == Some(true) {
            sched::interrupt_current(ast);
            return;
        }
        if let Some(t) = self.sched() {
            t.interrupt(ast);
        }
    }

    /// [`sched::Thread::poke`] on its scheduler thread, if it has one.
    pub(crate) fn poke(&self, ast: u32) {
        if let Some(t) = self.sched() {
            t.poke(ast);
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

    /// Its CPUs, and whether they are its own.
    pub(crate) fn affinity(&self) -> Cpus {
        Cpus {
            mask: self.cpus(),
            own: self.own_cpus.load(Ordering::Relaxed),
            domain: self.domain.load(Ordering::Relaxed),
        }
    }

    /// Its memory-domain policy: its own, or its set's.
    pub(crate) fn domain_policy(&self) -> u8 {
        crate::cpuset::thread_policy(self.domain.load(Ordering::Relaxed), self.process.cpuset())
    }

    /// Gives it a memory-domain policy of its own (`_cpuset_setthread`
    /// with a domain set); its set's own is none of its own.
    pub(crate) fn set_domain_policy(&self, policy: u8) {
        let own = if crate::cpuset::policy(self.process.cpuset()) == Ok(policy) {
            0
        } else {
            policy
        };
        self.domain.store(own, Ordering::Relaxed);
    }

    /// Gives it CPUs of its own (`cpuset_setthread`, an anonymous set).
    pub(crate) fn set_own_cpus(&self, mask: u64) {
        self.own_cpus.store(true, Ordering::Relaxed);
        self.set_cpus(mask);
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
            self.poke(crate::signal::AST_AFFINITY);
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
    #[inline]
    pub(crate) fn with_current<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        debug_assert!(sched::with_owner(|o| o.is_none_or(|o| o.is::<Self>())));
        // SAFETY: a thread's owner is only ever the `UserThread` its
        // start gave it (`proc`'s `enter`, the one caller of
        // `sched::set_owner` with a value), so a pointer that is not null
        // is to one; the thread holds it until it sets another, which
        // `f` does not. Asking the value what it is (`downcast_ref`)
        // would be three loads and an indirect call, and every system
        // call would pay for several.
        unsafe { sched::owner_at().cast::<Self>().as_ref() }.map(f)
    }
}

/// A thread's own copy of its process's credential (`td_ucred`), which
/// only the thread itself reads or replaces once it runs, so with no lock
/// and no count; its own copy, so that taking a reference to it touches
/// no line another thread's does, where the process's, shared across
/// `fork`, would move between CPUs on every count.
///
/// FreeBSD's rule holds here: the credential is replaced only by
/// `thread_cow_update`, which a thread calls on itself when it enters the
/// kernel, so code inside one kernel entry uses `td_ucred` without
/// counting. `proc_set_cred` leaves it alone: a thread that changes its
/// process's credential finishes the call on the old one. Another thread
/// that wants this one's credential reads the process's
/// (`Process::cred`, as FreeBSD reads `p_ucred` under the process lock),
/// never this.
struct OwnCred(UnsafeCell<CredState>);

struct CredState {
    /// The process's generation (`p_cowgen`) `cred` was copied at
    /// (`td_cowgen`).
    generation: u64,
    /// The credential, as `Arc::into_raw` gave it: this holds one count.
    cred: *const Ucred,
    /// Credentials replaced inside a kernel entry (`OwnCred::renew`), which
    /// a borrow may still be of; freed at the next entry.
    retired: Vec<Arc<Ucred>>,
}

// SAFETY: the state is the thread's own. Until the thread first runs,
// only its creator touches it (`OwnCred::new`); from then on only the
// thread itself (every method but `new` requires it), and whoever drops
// the `UserThread` last has it exclusively. The credentials it owns are
// `Arc<Ucred>`, which may be sent and shared (asserted below), so they
// may be created by one thread and dropped by another.
unsafe impl Send for OwnCred {}
// SAFETY: as above: no two threads use the state at once.
unsafe impl Sync for OwnCred {}

const _: () = {
    const fn sendable<T: Send + Sync>() {}
    sendable::<Arc<Ucred>>();
};

impl OwnCred {
    /// A new thread's copy of `p`'s credential (`thread_cow_get_proc`).
    fn new(p: &Process) -> Self {
        let (generation, cred) = own_cred(p);
        Self(UnsafeCell::new(CredState {
            generation,
            cred: Arc::into_raw(cred),
            retired: Vec::new(),
        }))
    }

    /// The credential. It stays allocated until the thread's next
    /// [`Self::entered`], or until the thread is dropped.
    ///
    /// # Safety
    ///
    /// The caller is the thread this belongs to. In an interrupt handler,
    /// the result may only be used before the handler returns.
    #[inline]
    unsafe fn current(&self) -> *const Ucred {
        // SAFETY: only the thread itself (the caller's contract) writes
        // the field, in `renew`, with interrupts masked and through a
        // reference that ends before they are unmasked: this read, by
        // that thread or by an interrupt handler above it, overlaps no
        // write and no exclusive reference.
        unsafe { (*self.0.get()).cred }
    }

    /// [`UserThread::cred_entered`]: the credential renewed if the
    /// process's changed, and the ones retired during the last entry
    /// freed.
    ///
    /// # Safety
    ///
    /// The caller is the thread this belongs to, in thread context, and
    /// nothing holds a pointer or borrow [`Self::current`] gave.
    #[inline]
    unsafe fn entered(&self, p: &Process) {
        // SAFETY: the thread's own state, read by the thread; nothing
        // else writes it.
        let (generation, idle) = unsafe {
            let st = &*self.0.get();
            (st.generation, st.retired.is_empty())
        };
        if generation != p.cred_gen() || !idle {
            // SAFETY: the caller's contract, passed on.
            unsafe { self.entered_changed(p) }
        }
    }

    /// [`Self::entered`] with something to do.
    ///
    /// # Safety
    ///
    /// As [`Self::entered`].
    #[cold]
    unsafe fn entered_changed(&self, p: &Process) {
        // SAFETY: the caller is the owning thread.
        unsafe { self.renew(p) };
        // SAFETY: the owning thread, in thread context: nothing else
        // reads `retired`, and no reference to the state is live.
        let retired = unsafe { core::mem::take(&mut (*self.0.get()).retired) };
        // No borrow is live (the caller's contract), so nothing is of
        // these.
        drop(retired);
    }

    /// Copies the process's credential if its generation is not this
    /// one's, keeping the old one allocated (retired).
    ///
    /// # Safety
    ///
    /// The caller is the thread this belongs to, in thread context.
    unsafe fn renew(&self, p: &Process) {
        // SAFETY: the thread's own state, read by the thread.
        if unsafe { (*self.0.get()).generation } == p.cred_gen() {
            return;
        }
        // Made, and room for the old one found, before interrupts are
        // masked: allocation may not happen with them masked.
        let (generation, new) = own_cred(p);
        // SAFETY: the owning thread, in thread context; the reference
        // ends with the statement.
        unsafe { (*self.0.get()).retired.reserve(1) };
        let saved = hal::irq::disable();
        // SAFETY: the owning thread, with interrupts masked: no interrupt
        // handler on this thread reads the state meanwhile
        // (`Self::current`), no other thread ever does, and the exclusive
        // reference ends before interrupts are restored. The old pointer
        // is the count `Arc::into_raw` gave, taken back exactly once. The
        // push does not allocate (reserved above).
        unsafe {
            let st = &mut *self.0.get();
            let old = core::mem::replace(&mut st.cred, Arc::into_raw(new));
            st.generation = generation;
            st.retired.push(Arc::from_raw(old));
        }
        hal::irq::restore(saved);
    }
}

impl Drop for OwnCred {
    fn drop(&mut self) {
        // SAFETY: the pointer holds the count `Arc::into_raw` gave, and
        // nothing uses it after this.
        drop(unsafe { Arc::from_raw(self.0.get_mut().cred) });
    }
}

/// A copy of `p`'s credential for one thread, with the generation it is
/// no newer than.
fn own_cred(p: &Process) -> (u64, Arc<Ucred>) {
    let (g, c) = p.cred_with_gen();
    (g, Arc::new(Ucred::clone(&c)))
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
