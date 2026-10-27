// SPDX-License-Identifier: BSD-2-Clause
//! Threads: `thr_new` and `thr_create`, `thr_exit`, `thr_self`,
//! `thr_suspend` and `thr_wake`, `thr_set_name`, `rtprio_thread`, and
//! `_umtx_op`'s waits and wakes.
//!
//! A new thread starts as a copy of its creator's registers at the system
//! call (FreeBSD's `cpu_copy_thread`), with the entry, argument, and stack
//! `thr_new` gives (`cpu_set_upcall`), or the whole context `thr_create`
//! gives; it has its creator's signal mask and scheduling class, and on
//! amd64 a copy of its FP state (arm64 starts it afresh, as FreeBSD's
//! `vfp_new_thread` does). The last thread's `thr_exit` returns: libthr
//! then calls `exit`.
//!
//! `_umtx_op` waiters sleep on one of 64 queues hashed by address, each
//! with a generation count a wake bumps. A waiter reads the generation,
//! then the user word, then sleeps until the generation moves: a wake
//! between the read and the sleep is not lost, and a spurious wake-up is
//! allowed (the caller rechecks), as with FreeBSD's. A wait may carry a
//! timeout, a duration or a time on any clock, and then fails with
//! `ETIMEDOUT` when it passes.
//!
//! Behavior reference: sys/kern/kern_thr.c, sys/kern/kern_thread.c,
//! sys/kern/kern_resource.c, sys/kern/kern_umtx.c,
//! sys/amd64/amd64/vm_machdep.c, sys/arm64/arm64/vm_machdep.c.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::thr::*;
use abi::time::{Timespec, clock};
use abi::ucontext::{Mcontext, Ucontext};
use sched::{Sleepable, WaitQueue};

use super::{SysResult, int, process, thread};
use crate::arch::trap::TrapFrame;
use crate::proc::{NewThread, Process};
use crate::uaccess::{Plain, UserPtr};
use crate::uthread::{MAX_PER_PROC, Name, UserThread};

// SAFETY: two `u16`s, no padding.
unsafe impl Plain for Rtprio {}
// SAFETY: integers only, no padding.
unsafe impl Plain for UmtxTime {}
// SAFETY: integers only, padding explicit (`abi-check` holds the C layout).
unsafe impl Plain for ThrParam {}

/// Stores a tid where libthr keeps it: a `long` (FreeBSD's
/// `suword_lwpid`).
fn store_tid(at: u64, tid: i32) -> Result<(), Errno> {
    UserPtr::<i64>::new(at)
        .write(&i64::from(tid))
        .map_err(|_| Errno::EFAULT)
}

/// `sched_yield()`: lets an equal or better thread run.
pub(super) fn sched_yield() -> SysResult {
    sched::yield_now();
    Ok(0)
}

/// `sched_get_priority_max(policy)` and `sched_get_priority_min`: the
/// POSIX range for `SCHED_FIFO` and `SCHED_RR` (FreeBSD's real-time
/// priorities, 0 to 31 inverted), and for `SCHED_OTHER` 0 up to 15.1's
/// time-sharing span.
pub(super) fn sched_priority_bound(policy: u64, max: bool) -> SysResult {
    const SCHED_FIFO: i32 = 1;
    const SCHED_OTHER: i32 = 2;
    const SCHED_RR: i32 = 3;
    match (int(policy), max) {
        (SCHED_FIFO | SCHED_RR, true) => Ok(u64::from(RTP_PRIO_MAX)),
        (SCHED_FIFO | SCHED_RR, false) | (SCHED_OTHER, false) => Ok(0),
        (SCHED_OTHER, true) => Ok(u64::from(RTP_PRIO_NORMAL_MAX)),
        _ => Err(Errno::EINVAL),
    }
}

/// `thr_self(id)`.
pub(super) fn thr_self(id: u64) -> SysResult {
    store_tid(id, thread()?.tid())?;
    Ok(0)
}

/// The class and priority `rtp` asks for a new thread, checked as
/// FreeBSD's `thread_create` does: only the superuser gets a real-time
/// class, a normal one is at its base, and no other class is allowed.
fn new_class(rtp: Rtprio, creator: &UserThread) -> Result<Rtprio, Errno> {
    match rtp.type_ {
        RTP_PRIO_REALTIME | RTP_PRIO_FIFO => {
            if !creator.process().is_root() {
                return Err(Errno::EPERM);
            }
            if rtp.prio > RTP_PRIO_MAX {
                return Err(Errno::EINVAL);
            }
            Ok(rtp)
        }
        RTP_PRIO_NORMAL => Ok(Rtprio {
            type_: RTP_PRIO_NORMAL,
            prio: 0,
        }),
        _ => Err(Errno::EINVAL),
    }
}

/// Starts a thread in the caller's process from the caller's registers,
/// which `init` adjusts once the thread has its tid (FreeBSD's
/// `thread_create`). `rtp` is the class asked for, if any.
fn create(
    frame: &TrapFrame,
    rtp: Option<Rtprio>,
    tls: [u64; 2],
    tls_base: Option<u64>,
    context: Option<crate::signal::Prepared>,
    init: impl FnOnce(&UserThread, &mut TrapFrame) -> Result<(), Errno>,
) -> SysResult {
    let me = thread()?;
    let rtprio = match rtp {
        Some(r) => new_class(r, &me)?,
        None => me.rtprio(),
    };
    let p = Arc::clone(me.process());
    if p.thread_count() >= MAX_PER_PROC {
        crate::uthread::MAX_HITS.fetch_add(1, Ordering::Relaxed);
        return Err(Errno::EPROCLIM);
    }
    let new = NewThread {
        tls,
        tls_base,
        sig: me.sig.lock().created(),
        // amd64 copies the creator's FP state; arm64 starts afresh.
        fork_fpu: cfg!(target_arch = "x86_64"),
        cpus: me.cpus(),
        rtprio,
        context,
    };
    let mut f = *frame;
    clear_trace(&mut f);
    p.start_with(f, new, init)?;
    Ok(0)
}

/// A new thread does not inherit a debugger's single step.
fn clear_trace(f: &mut TrapFrame) {
    #[cfg(target_arch = "x86_64")]
    {
        const PSL_T: u64 = 1 << 8;
        f.rflags &= !PSL_T;
    }
    #[cfg(target_arch = "aarch64")]
    let _ = f;
}

/// Points `f` at `entry(arg)` on the stack `[base, base + size)` (FreeBSD's
/// `cpu_set_upcall`): amd64 pushes a zero return address, to end stack
/// unwinding; arm64 clears the frame pointer and link register.
fn upcall(f: &mut TrapFrame, entry: u64, arg: u64, base: u64, size: u64) -> Result<(), Errno> {
    let top = base.wrapping_add(size) & !0xf;
    #[cfg(target_arch = "x86_64")]
    {
        f.rbp = 0;
        f.rsp = top.wrapping_sub(8);
        f.rip = entry;
        UserPtr::<u64>::new(f.rsp)
            .write(&0)
            .map_err(|_| Errno::EFAULT)?;
        f.rdi = arg;
    }
    #[cfg(target_arch = "aarch64")]
    {
        f.sp = top;
        f.elr = entry;
        f.x[0] = arg;
        f.x[29] = 0;
        f.x[30] = 0;
    }
    Ok(())
}

/// `thr_new(param, param_size)`: a new thread as `struct thr_param` says.
pub(super) fn thr_new(frame: &TrapFrame, param: u64, size: u64) -> SysResult {
    let size = usize::try_from(int(size)).map_err(|_| Errno::EINVAL)?;
    if size > size_of::<ThrParam>() {
        return Err(Errno::EINVAL);
    }
    let mut tp = ThrParam::default();
    // SAFETY: `ThrParam` is plain: any bytes are a valid value.
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::from_mut(&mut tp).cast::<u8>(),
            size_of::<ThrParam>(),
        )
    };
    crate::uaccess::copy_in(&mut bytes[..size], param).map_err(|_| Errno::EFAULT)?;
    if tp.flags & !(THR_SUSPENDED | THR_SYSTEM_SCOPE | THR_C_RUNTIME) != 0 {
        return Err(Errno::EINVAL);
    }
    let rtp = if tp.rtp == 0 {
        None
    } else {
        Some(
            UserPtr::<Rtprio>::new(tp.rtp)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    if tp.tls_base >= crate::mm::space::USER_END {
        return Err(Errno::EINVAL);
    }
    // amd64 records the TLS base for `AMD64_GET_TLSBASE` only for the C
    // runtime's threads (`PCB_TLSBASE`).
    let tls_base =
        (cfg!(target_arch = "x86_64") && tp.flags & THR_C_RUNTIME != 0).then_some(tp.tls_base);
    // The creator's GS base comes along (its PCB is copied).
    let tls = [tp.tls_base, hal::user::tls2()];
    create(frame, rtp, tls, tls_base, None, |t, f| {
        // Two places: a detached thread may be gone, and its memory
        // freed, before its creator looks at its own copy.
        if tp.child_tid != 0 {
            store_tid(tp.child_tid, t.tid())?;
        }
        if tp.parent_tid != 0 {
            store_tid(tp.parent_tid, t.tid())?;
        }
        upcall(f, tp.start_func, tp.arg, tp.stack_base, tp.stack_size)
    })
}

/// `thr_create(ctx, id, flags)`: a new thread running the context `ctx`.
pub(super) fn thr_create(frame: &TrapFrame, ctx: u64, id: u64, _flags: u64) -> SysResult {
    let uc = UserPtr::<Ucontext<Mcontext>>::new(ctx)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let mut f = *frame;
    let ready = crate::signal::prepare_context(&mut f, &uc.uc_mcontext)?;
    let tls = [
        ready.tls().unwrap_or_else(hal::user::tls),
        ready.tls2().unwrap_or_else(hal::user::tls2),
    ];
    create(&f, None, tls, None, Some(ready), |t, _| {
        if id != 0 {
            store_tid(id, t.tid())?;
        }
        Ok(())
    })
}

/// `thr_exit(state)`: stores 1 at `state` and wakes its waiters (libthr's
/// join), then the thread leaves; the process's last thread returns
/// instead, and libthr exits.
pub(super) fn thr_exit(state: u64) -> SysResult {
    if state != 0 {
        let _ = UserPtr::<i64>::new(state).write(&1);
        wake(state);
    }
    let left = UserThread::with_current(|t| t.process().remove_thread(t, true));
    if left != Some(true) {
        return Ok(0);
    }
    // Nothing on this stack holds a reference.
    match UserThread::current() {
        Some(t) => Process::thread_exit(t),
        None => sched::exit(),
    }
}

/// `thr_suspend(timeout)`: sleeps until `thr_wake` names this thread (or
/// already did), the timeout passes (`ETIMEDOUT`), or a signal comes.
pub(super) fn thr_suspend(s: &Sleepable, timeout: u64) -> SysResult {
    let limit = if timeout == 0 {
        None
    } else {
        let ts = UserPtr::<Timespec>::new(timeout)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
            return Err(Errno::EINVAL);
        }
        Some(
            ts.tv_sec
                .cast_unsigned()
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec.cast_unsigned()),
        )
    };
    let t = thread()?;
    if t.woken.swap(false, Ordering::AcqRel) {
        return Ok(0);
    }
    let r = if limit == Some(0) {
        Ok(crate::signal::Woke::TimedOut)
    } else {
        crate::signal::sleep(
            s,
            &t.suspend_wait,
            || t.woken.load(Ordering::Acquire),
            limit,
        )
    };
    if t.woken.swap(false, Ordering::AcqRel) {
        return Ok(0);
    }
    match r {
        Ok(_) => Err(Errno::ETIMEDOUT),
        // A timed sleep is not restarted.
        Err(Errno::ERESTART) if limit.is_some() => Err(Errno::EINTR),
        Err(e) => Err(e),
    }
}

/// `thr_wake(id)`: ends thread `id`'s `thr_suspend`, or its next one.
pub(super) fn thr_wake(id: u64) -> SysResult {
    let me = thread()?;
    let tid = i32::try_from(id.cast_signed()).map_err(|_| Errno::ESRCH)?;
    let t = if tid == me.tid() {
        me
    } else {
        me.process().find_thread(tid).ok_or(Errno::ESRCH)?
    };
    t.woken.store(true, Ordering::Release);
    t.suspend_wait.wake_all();
    Ok(0)
}

/// `thr_set_name(id, name)`: names thread `id` of this process; a name
/// too long is cut to `MAXCOMLEN` bytes, and none clears it.
pub(super) fn thr_set_name(id: u64, name: u64) -> SysResult {
    let mut buf = [0u8; abi::kinfo::MAXCOMLEN + 1];
    if name != 0 {
        match crate::uaccess::copy_in_str(name, &mut buf) {
            Ok(_) => {}
            Err(crate::uaccess::StrError::TooLong) => {
                buf[abi::kinfo::MAXCOMLEN] = 0;
            }
            Err(crate::uaccess::StrError::Fault) => return Err(Errno::EFAULT),
        }
    }
    let tid = i32::try_from(id.cast_signed()).map_err(|_| Errno::ESRCH)?;
    let t = process()?.find_thread(tid).ok_or(Errno::ESRCH)?;
    t.set_name(Name::new(&buf));
    Ok(0)
}

/// `rtprio_thread(function, lwpid, rtp)`: thread `lwpid`'s class and
/// priority (0 for the caller's own), in any process.
pub(super) fn rtprio_thread(function: u64, lwpid: u64, rtp: u64) -> SysResult {
    let function = u32::try_from(function).map_err(|_| Errno::EINVAL)?;
    let p = UserPtr::<Rtprio>::new(rtp);
    // Read before looking the thread up, as FreeBSD does.
    let wanted = (function == RTP_SET).then(|| p.read());
    let me = thread()?;
    let lwpid = int(lwpid);
    let t = if lwpid == 0 || lwpid == me.tid() {
        me
    } else {
        crate::uthread::find(lwpid).ok_or(Errno::ESRCH)?
    };
    match function {
        RTP_LOOKUP => {
            p.write(&t.rtprio()).map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        RTP_SET => {
            let r = wanted
                .unwrap_or(Err(crate::uaccess::Fault))
                .map_err(|_| Errno::EFAULT)?;
            let base = r.type_ & !RTP_PRIO_FIFO_BIT;
            // Real-time and idle classes need privilege (everyone is root
            // until P6.7 brings credentials that are not).
            if (base == RTP_PRIO_REALTIME || base == RTP_PRIO_IDLE) && !t.process().is_root() {
                return Err(Errno::EPERM);
            }
            match base {
                RTP_PRIO_REALTIME | RTP_PRIO_IDLE if r.prio > RTP_PRIO_MAX => Err(Errno::EINVAL),
                RTP_PRIO_NORMAL if r.prio > RTP_PRIO_NORMAL_MAX => Err(Errno::EINVAL),
                RTP_PRIO_REALTIME | RTP_PRIO_IDLE | RTP_PRIO_NORMAL => {
                    t.set_rtprio(r);
                    Ok(0)
                }
                _ => Err(Errno::EINVAL),
            }
        }
        _ => Err(Errno::EINVAL),
    }
}

/// The highest time-sharing priority `rtprio` takes: 15.1's
/// `PRI_MAX_TIMESHARE - PRI_MIN_TIMESHARE`.
const RTP_PRIO_NORMAL_MAX: u16 = 103;

struct Bucket {
    generation: AtomicU64,
    waiters: WaitQueue,
}

static BUCKETS: [Bucket; 64] = [const {
    Bucket {
        generation: AtomicU64::new(0),
        waiters: WaitQueue::new(),
    }
}; 64];

fn bucket(addr: u64) -> &'static Bucket {
    &BUCKETS[((addr >> 2) % 64) as usize]
}

/// Wakes every waiter on `addr`.
fn wake(addr: u64) {
    let b = bucket(addr);
    b.generation.fetch_add(1, Ordering::AcqRel);
    b.waiters.wake_all();
}

/// How long a wait may last, in nanoseconds (`None`: no limit), from
/// `_umtx_op`'s `uaddr` (the size of what `uaddr2` points at) and `uaddr2`:
/// a `struct timespec` duration, or a `struct _umtx_time`.
fn wait_limit(size: u64, at: u64) -> Result<Option<u64>, Errno> {
    if at == 0 {
        return Ok(None);
    }
    let t = if size <= size_of::<Timespec>() as u64 {
        UmtxTime {
            timeout: UserPtr::<Timespec>::new(at)
                .read()
                .map_err(|_| Errno::EFAULT)?,
            flags: 0,
            clockid: clock::REALTIME,
        }
    } else {
        UserPtr::<UmtxTime>::new(at)
            .read()
            .map_err(|_| Errno::EFAULT)?
    };
    let Timespec { tv_sec, tv_nsec } = t.timeout;
    let (Ok(sec), Ok(nsec)) = (u64::try_from(tv_sec), u64::try_from(tv_nsec)) else {
        return Err(Errno::EINVAL);
    };
    if nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    let ns = sec.saturating_mul(1_000_000_000).saturating_add(nsec);
    let now = super::time::read(u64::from(t.clockid))?;
    Ok(Some(if t.flags & UMTX_ABSTIME != 0 {
        ns.saturating_sub(now)
    } else {
        ns
    }))
}

/// `_umtx_op(obj, op, val, uaddr, uaddr2)`: waits and wakes.
pub(super) fn umtx_op(
    s: &Sleepable,
    obj: u64,
    op: u64,
    val: u64,
    uaddr: u64,
    uaddr2: u64,
) -> SysResult {
    match u32::try_from(op).map_err(|_| Errno::EINVAL)? {
        UMTX_OP_WAKE | UMTX_OP_WAKE_PRIVATE => {
            wake(obj);
            Ok(0)
        }
        UMTX_OP_NWAKE_PRIVATE => {
            // `obj` is an array of `val` addresses, read in FreeBSD's
            // batches; a fault ends it there.
            const BATCH: usize = 128;
            let mut left = usize::try_from(int(val)).unwrap_or(0);
            let mut at = obj;
            while left > 0 {
                let n = left.min(BATCH);
                let mut addrs = [0u64; BATCH];
                for (i, a) in addrs[..n].iter_mut().enumerate() {
                    *a = UserPtr::<u64>::new(at.wrapping_add(8 * i as u64))
                        .read()
                        .map_err(|_| Errno::EFAULT)?;
                }
                for &a in &addrs[..n] {
                    wake(a);
                }
                left -= n;
                at = at.wrapping_add(8 * n as u64);
            }
            Ok(0)
        }
        op @ (UMTX_OP_WAIT | UMTX_OP_WAIT_UINT | UMTX_OP_WAIT_UINT_PRIVATE) => {
            let limit = wait_limit(uaddr, uaddr2)?;
            let b = bucket(obj);
            let generation = b.generation.load(Ordering::Acquire);
            let current = if op == UMTX_OP_WAIT {
                UserPtr::<u64>::new(obj).read()
            } else {
                UserPtr::<u32>::new(obj).read().map(u64::from)
            }
            .map_err(|_| Errno::EFAULT)?;
            let want = if op == UMTX_OP_WAIT {
                val
            } else {
                val & 0xffff_ffff
            };
            if current != want {
                return Ok(0);
            }
            let woken = || b.generation.load(Ordering::Acquire) != generation;
            // A signal ends the wait with EINTR, never a restart (as
            // FreeBSD's `do_wait`); libthr retries.
            match crate::signal::sleep(s, &b.waiters, woken, limit) {
                Ok(crate::signal::Woke::Done) => Ok(0),
                Ok(crate::signal::Woke::TimedOut) => Err(Errno::ETIMEDOUT),
                Err(_) => Err(Errno::EINTR),
            }
        }
        _ => Err(Errno::EINVAL),
    }
}
