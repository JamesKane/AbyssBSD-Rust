// SPDX-License-Identifier: BSD-2-Clause
//! `_umtx_op(obj, op, val, uaddr, uaddr2)`: reads the operation's
//! arguments from user memory (timeouts, robust-list heads, address
//! arrays) in the caller's ABI, and runs it in `sys/umtx`.
//!
//! `UMTX_OP__I386` and `UMTX_OP__32BIT` say the caller's structures are
//! i386's or another 32-bit ABI's; on amd64 the latter's `timespec` has a
//! 64-bit `tv_sec` and 32-bit `tv_nsec` (FreeBSD's `timespecx32`).
//!
//! Behavior reference: sys/kern/kern_umtx.c (`sys__umtx_op`, the
//! `__umtx_op_*` functions, `umtx_copyin_*`, `umtx_shm`).

use abi::errno::Errno;
use abi::time::{Timespec, clock};
use abi::umtx::*;
use sched::Sleepable;
use umtx::{CvTime, Mode, Robust, Timeout, Width};

use super::{SysResult, int};
use crate::uaccess::{self, copy_in, copy_out};
use crate::umtx::{Ctx, UMTX};
use crate::uthread::UserThread;

/// The ABI of the caller's structures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Abi {
    Native,
    /// `UMTX_OP__I386`: 32-bit `tv_sec` and `tv_nsec`, 32-bit pointers.
    I386,
    /// `UMTX_OP__32BIT`: 32-bit pointers; on amd64 a 64-bit `tv_sec`.
    X32,
}

impl Abi {
    /// `sizeof(struct timespec)`.
    const fn timespec(self) -> usize {
        match self {
            Abi::Native => 16,
            Abi::I386 => 8,
            Abi::X32 if cfg!(target_arch = "x86_64") => 16,
            Abi::X32 => 8,
        }
    }

    /// `sizeof(struct _umtx_time)`.
    const fn umtx_time_size(self) -> usize {
        self.timespec() + 8
    }

    const fn compat32(self) -> bool {
        !matches!(self, Abi::Native)
    }

    /// Decodes a `struct timespec`.
    fn decode_timespec(self, b: &[u8]) -> Timespec {
        let i32_at = |o: usize| {
            i64::from(i32::from_le_bytes(
                b[o..o + 4].try_into().unwrap_or_default(),
            ))
        };
        let i64_at = |o: usize| i64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        match self {
            Abi::Native => Timespec {
                tv_sec: i64_at(0),
                tv_nsec: i64_at(8),
            },
            Abi::X32 if cfg!(target_arch = "x86_64") => Timespec {
                tv_sec: i64_at(0),
                tv_nsec: i32_at(8),
            },
            _ => Timespec {
                tv_sec: i32_at(0),
                tv_nsec: i32_at(4),
            },
        }
    }

    /// Encodes a `struct timespec` into `b` (`timespec()` bytes).
    fn encode_timespec(self, t: &Timespec, b: &mut [u8]) {
        match self {
            Abi::Native => {
                b[0..8].copy_from_slice(&t.tv_sec.to_le_bytes());
                b[8..16].copy_from_slice(&t.tv_nsec.to_le_bytes());
            }
            Abi::X32 if cfg!(target_arch = "x86_64") => {
                b[0..8].copy_from_slice(&t.tv_sec.to_le_bytes());
                #[allow(clippy::cast_possible_truncation, reason = "below 10^9")]
                b[8..12].copy_from_slice(&(t.tv_nsec as i32).to_le_bytes());
                b[12..16].fill(0);
            }
            _ => {
                #[allow(clippy::cast_possible_truncation, reason = "the ABI's width")]
                b[0..4].copy_from_slice(&(t.tv_sec as i32).to_le_bytes());
                #[allow(clippy::cast_possible_truncation, reason = "below 10^9")]
                b[4..8].copy_from_slice(&(t.tv_nsec as i32).to_le_bytes());
            }
        }
    }

    /// `umtx_copyin_timeout`: a `struct timespec`, which must be a valid
    /// interval.
    fn timeout(self, at: u64) -> Result<Timespec, Errno> {
        let mut b = [0u8; 16];
        let b = &mut b[..self.timespec()];
        copy_in(b, at).map_err(|_| Errno::EFAULT)?;
        valid(self.decode_timespec(b))
    }

    /// `umtx_copyin_umtx_time`: a `struct _umtx_time`, or, if `size` is
    /// no bigger than a `timespec`, a `timespec` (a duration on the real
    /// time clock).
    fn umtx_time(self, at: u64, size: u64) -> Result<Timeout, Errno> {
        if size <= self.timespec() as u64 {
            return Ok(Timeout {
                clock: clock::REALTIME,
                absolute: false,
                time: self.timeout(at)?,
            });
        }
        let mut b = [0u8; 24];
        let b = &mut b[..self.umtx_time_size()];
        copy_in(b, at).map_err(|_| Errno::EFAULT)?;
        let n = self.timespec();
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or_default());
        let (flags, clock) = (u32_at(n), u32_at(n + 4));
        Ok(Timeout {
            clock,
            absolute: flags & UMTX_ABSTIME != 0,
            time: valid(self.decode_timespec(b))?,
        })
    }

    /// The optional timeout most operations take: `uaddr` its size,
    /// `uaddr2` where it is (none if null).
    fn optional(self, size: u64, at: u64) -> Result<Option<Timeout>, Errno> {
        if at == 0 {
            return Ok(None);
        }
        self.umtx_time(at, size).map(Some)
    }
}

/// `timespecvalid_interval`.
fn valid(t: Timespec) -> Result<Timespec, Errno> {
    if t.tv_sec < 0 || !(0..1_000_000_000).contains(&t.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    Ok(t)
}

fn ok(r: Result<(), Errno>) -> SysResult {
    r.map(|()| 0)
}

/// `_umtx_op`.
pub(super) fn umtx_op(
    s: &Sleepable,
    obj: u64,
    op: u64,
    val: u64,
    uaddr: u64,
    uaddr2: u64,
) -> SysResult {
    // `op` is an `int`.
    let op = int(op).cast_unsigned();
    let abi = if op & UMTX_OP__I386 != 0 {
        Abi::I386
    } else if op & UMTX_OP__32BIT != 0 {
        Abi::X32
    } else {
        Abi::Native
    };
    let op = op & !(UMTX_OP__I386 | UMTX_OP__32BIT);
    let t = UserThread::current().ok_or(Errno::EINVAL)?;
    let cx = Ctx { s, t: &t };
    let u = &UMTX;
    // What a sleep in each operation is called (kern_umtx.c's
    // `umtxq_sleep` messages; a mutex's is that of the normal protocol).
    let _w = sched::wait_message(match op {
        UMTX_OP_LOCK => "umtx",
        UMTX_OP_MUTEX_LOCK | UMTX_OP_MUTEX_WAIT => "umtxn",
        UMTX_OP_CV_WAIT => "ucond",
        UMTX_OP_RW_RDLOCK => "urdlck",
        UMTX_OP_RW_WRLOCK => "uwrlck",
        UMTX_OP_SEM_WAIT | UMTX_OP_SEM2_WAIT => "usem",
        _ => "uwait",
    });
    match op {
        UMTX_OP_LOCK => {
            let to = if uaddr2 == 0 {
                None
            } else {
                Some(Timeout {
                    clock: clock::REALTIME,
                    absolute: false,
                    time: abi.timeout(uaddr2)?,
                })
            };
            if abi.compat32() {
                return Err(Errno::EOPNOTSUPP);
            }
            ok(u.lock_umtx(&cx, obj, val, to.as_ref()))
        }
        UMTX_OP_UNLOCK => {
            if abi.compat32() {
                return Err(Errno::EOPNOTSUPP);
            }
            ok(u.unlock_umtx(&cx, obj, val))
        }
        UMTX_OP_WAIT | UMTX_OP_WAIT_UINT | UMTX_OP_WAIT_UINT_PRIVATE => {
            let to = abi.optional(uaddr, uaddr2)?;
            let width = if op == UMTX_OP_WAIT && !abi.compat32() {
                Width::Long
            } else {
                Width::Uint
            };
            let id = if width == Width::Uint {
                val & 0xffff_ffff
            } else {
                val
            };
            ok(u.wait(
                &cx,
                obj,
                id,
                to.as_ref(),
                width,
                op == UMTX_OP_WAIT_UINT_PRIVATE,
            ))
        }
        UMTX_OP_WAKE | UMTX_OP_WAKE_PRIVATE => {
            // At least one, as FreeBSD's count works.
            let n = usize::try_from(int(val)).unwrap_or(0).max(1);
            ok(u.wake(&cx, obj, n, op == UMTX_OP_WAKE_PRIVATE))
        }
        UMTX_OP_NWAKE_PRIVATE => {
            // `obj` is an array of `val` pointers, read in batches; a fault
            // ends it there.
            const BATCH: usize = 128;
            let width = if abi.compat32() { 4 } else { 8 };
            let mut left = usize::try_from(int(val)).unwrap_or(0);
            let mut at = obj;
            while left > 0 {
                let n = left.min(BATCH);
                let mut raw = [0u8; BATCH * 8];
                copy_in(&mut raw[..n * width], at).map_err(|_| Errno::EFAULT)?;
                let mut addrs = [0u64; BATCH];
                for (i, a) in addrs[..n].iter_mut().enumerate() {
                    let b = &raw[i * width..(i + 1) * width];
                    *a = if width == 4 {
                        u64::from(u32::from_le_bytes(b.try_into().unwrap_or_default()))
                    } else {
                        u64::from_le_bytes(b.try_into().unwrap_or_default())
                    };
                }
                u.wake_all_private(&cx, &addrs[..n]);
                left -= n;
                at = at.wrapping_add((n * width) as u64);
                sched::preempt_point();
            }
            Ok(0)
        }
        UMTX_OP_MUTEX_TRYLOCK => ok(u.lock_umutex(&cx, obj, None, Mode::Try)),
        UMTX_OP_MUTEX_LOCK | UMTX_OP_MUTEX_WAIT => {
            let to = abi.optional(uaddr, uaddr2)?;
            let mode = if op == UMTX_OP_MUTEX_LOCK {
                Mode::Lock
            } else {
                Mode::Wait
            };
            ok(u.lock_umutex(&cx, obj, to.as_ref(), mode))
        }
        UMTX_OP_MUTEX_UNLOCK => ok(u.unlock_umutex(&cx, obj, false)),
        UMTX_OP_MUTEX_WAKE => ok(u.wake_umutex(&cx, obj)),
        UMTX_OP_MUTEX_WAKE2 => ok(u.wake2_umutex(&cx, obj, int(val).cast_unsigned())),
        UMTX_OP_SET_CEILING => {
            let old = u.set_ceiling(&cx, obj, int(val).cast_unsigned())?;
            if uaddr != 0 {
                uaccess::store_u32(uaddr, old).map_err(|_| Errno::EFAULT)?;
            }
            Ok(0)
        }
        UMTX_OP_CV_WAIT => {
            let wflags = val;
            let time = if wflags & u64::from(CVWAIT_UMTX_TIME) != 0 {
                if uaddr2 == 0 {
                    return Err(Errno::EINVAL);
                }
                CvTime::Umtx(abi.umtx_time(uaddr2, abi.umtx_time_size() as u64)?)
            } else if uaddr2 == 0 {
                CvTime::Forever
            } else {
                CvTime::Timespec(abi.timeout(uaddr2)?)
            };
            ok(u.cv_wait(&cx, obj, uaddr, wflags, time))
        }
        UMTX_OP_CV_SIGNAL => ok(u.cv_signal(&cx, obj)),
        UMTX_OP_CV_BROADCAST => ok(u.cv_broadcast(&cx, obj)),
        UMTX_OP_RW_RDLOCK => {
            let to = abi.optional(uaddr, uaddr2)?;
            ok(u.rw_rdlock(&cx, obj, val, to.as_ref()))
        }
        UMTX_OP_RW_WRLOCK => {
            let to = abi.optional(uaddr, uaddr2)?;
            ok(u.rw_wrlock(&cx, obj, to.as_ref()))
        }
        UMTX_OP_RW_UNLOCK => ok(u.rw_unlock(&cx, obj)),
        UMTX_OP_SEM_WAIT => {
            let to = abi.optional(uaddr, uaddr2)?;
            ok(u.sem_wait(&cx, obj, to.as_ref()))
        }
        UMTX_OP_SEM_WAKE => ok(u.sem_wake(&cx, obj)),
        UMTX_OP_SEM2_WAIT => {
            let mut to = abi.optional(uaddr, uaddr2)?;
            let r = u.sem2_wait(&cx, obj, to.as_mut());
            // What is left of a relative timeout goes after it, if the
            // caller left room.
            if r == Err(Errno::EINTR)
                && let Some(t) = to
                && !t.absolute
                && uaddr >= (abi.umtx_time_size() + abi.timespec()) as u64
            {
                let mut b = [0u8; 16];
                let b = &mut b[..abi.timespec()];
                abi.encode_timespec(&t.time, b);
                copy_out(uaddr2.wrapping_add(abi.umtx_time_size() as u64), b)
                    .map_err(|_| Errno::EFAULT)?;
            }
            ok(r)
        }
        UMTX_OP_SEM2_WAKE => ok(u.sem2_wake(&cx, obj)),
        UMTX_OP_SHM => super::shm::umtx_shm(s, &t, uaddr, int(val).cast_unsigned()),
        UMTX_OP_ROBUST_LISTS => {
            let size = usize::try_from(val).map_err(|_| Errno::EINVAL)?;
            let width = if abi.compat32() { 4 } else { 8 };
            if size > 3 * width {
                return Err(Errno::EINVAL);
            }
            let mut b = [0u8; 24];
            copy_in(&mut b[..size], uaddr).map_err(|_| Errno::EFAULT)?;
            let field = |i: usize| {
                let b = &b[i * width..(i + 1) * width];
                if width == 4 {
                    u64::from(u32::from_le_bytes(b.try_into().unwrap_or_default()))
                } else {
                    u64::from_le_bytes(b.try_into().unwrap_or_default())
                }
            };
            ok(u.set_robust_lists(
                &cx,
                Robust {
                    list: field(0),
                    priv_list: field(1),
                    inact: field(2),
                    compat32: abi.compat32(),
                },
            ))
        }
        UMTX_OP_GET_MIN_TIMEOUT => {
            let v = t
                .process()
                .umtx_min_timeout
                .load(core::sync::atomic::Ordering::Relaxed);
            let r = if abi.compat32() {
                #[allow(clippy::cast_possible_truncation, reason = "an int, as FreeBSD's")]
                copy_out(uaddr, &(v as i32).to_le_bytes())
            } else {
                copy_out(uaddr, &v.to_le_bytes())
            };
            r.map(|()| 0).map_err(|_| Errno::EFAULT)
        }
        UMTX_OP_SET_MIN_TIMEOUT => {
            // `val` is a `long`.
            let v = u64::try_from(val.cast_signed()).map_err(|_| Errno::EINVAL)?;
            t.process()
                .umtx_min_timeout
                .store(v, core::sync::atomic::Ordering::Relaxed);
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}
