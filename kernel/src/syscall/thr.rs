// SPDX-License-Identifier: BSD-2-Clause
//! Threads: `thr_self`, `thr_wake`, `rtprio_thread`, and `_umtx_op`'s
//! waits and wakes.
//!
//! A process has one thread so far; its thread ID is the scheduler's.
//! `_umtx_op` waiters sleep on one of 64 queues hashed by address, each
//! with a generation count a wake bumps. A waiter reads the generation,
//! then the user word, then sleeps until the generation moves: a wake
//! between the read and the sleep is not lost, and a spurious wake-up is
//! allowed (the caller rechecks), as with FreeBSD's. A wait may carry a
//! timeout, a duration or a time on any clock, and then fails with
//! `ETIMEDOUT` when it passes.
//!
//! Behavior reference: sys/kern/kern_thr.c, sys/kern/kern_umtx.c.

use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::thr::*;
use abi::time::{Timespec, clock};
use sched::{Sleepable, WaitQueue};

use super::SysResult;
use crate::uaccess::{Plain, UserPtr};

// SAFETY: two `u16`s, no padding.
unsafe impl Plain for Rtprio {}
// SAFETY: integers only, no padding.
unsafe impl Plain for UmtxTime {}

fn tid() -> u64 {
    sched::current().tid().0
}

/// `thr_self(id)`.
pub(super) fn thr_self(id: u64) -> SysResult {
    UserPtr::<u64>::new(id)
        .write(&tid())
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `thr_wake(id)`: only our own thread exists.
pub(super) fn thr_wake(id: u64) -> SysResult {
    if id == tid() {
        Ok(0)
    } else {
        Err(Errno::ESRCH)
    }
}

/// `rtprio_thread(function, lwpid, rtp)`: every thread is time-sharing.
pub(super) fn rtprio_thread(function: u64, lwpid: u64, rtp: u64) -> SysResult {
    if lwpid != 0 && lwpid != tid() {
        return Err(Errno::ESRCH);
    }
    let p = UserPtr::<Rtprio>::new(rtp);
    match u32::try_from(function).map_err(|_| Errno::EINVAL)? {
        RTP_LOOKUP => {
            p.write(&Rtprio {
                type_: RTP_PRIO_NORMAL,
                prio: 0,
            })
            .map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        RTP_SET => {
            let r = p.read().map_err(|_| Errno::EFAULT)?;
            // Real-time and idle classes need privilege we do not grant
            // yet.
            if r.type_ == RTP_PRIO_NORMAL {
                Ok(0)
            } else {
                Err(Errno::EPERM)
            }
        }
        _ => Err(Errno::EINVAL),
    }
}

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
            let b = bucket(obj);
            b.generation.fetch_add(1, Ordering::AcqRel);
            b.waiters.wake_all();
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
            match limit {
                None => b.waiters.wait_until(s, woken),
                Some(ns) => {
                    if !crate::time::wait_timeout(s, &b.waiters, woken, ns) {
                        return Err(Errno::ETIMEDOUT);
                    }
                }
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}
