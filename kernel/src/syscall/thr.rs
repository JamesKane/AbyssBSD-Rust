// SPDX-License-Identifier: BSD-2-Clause
//! Threads: `thr_self`, `thr_wake`, `rtprio_thread`, and `_umtx_op`'s
//! waits and wakes.
//!
//! A process has one thread so far; its thread ID is the scheduler's.
//! `_umtx_op` waiters sleep on one of 64 queues hashed by address, each
//! with a generation count a wake bumps. A waiter reads the generation,
//! then the user word, then sleeps until the generation moves: a wake
//! between the read and the sleep is not lost, and a spurious wake-up is
//! allowed (the caller rechecks), as with FreeBSD's.
//!
//! Behavior reference: sys/kern/kern_thr.c, sys/kern/kern_umtx.c.

use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::thr::*;
use sched::{Sleepable, WaitQueue};

use super::SysResult;
use crate::uaccess::{Plain, UserPtr};

// SAFETY: two `u16`s, no padding.
unsafe impl Plain for Rtprio {}

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
            // Timed waits come with the clock calls.
            if uaddr != 0 || uaddr2 != 0 {
                return Err(Errno::EINVAL);
            }
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
            if current == want {
                b.waiters
                    .wait_until(s, || b.generation.load(Ordering::Acquire) != generation);
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}
