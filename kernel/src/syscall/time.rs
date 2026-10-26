// SPDX-License-Identifier: BSD-2-Clause
//! Clocks and sleeping: `clock_gettime`, `clock_getres`, `nanosleep`,
//! `gettimeofday`, and resource limits.

use abi::errno::Errno;
use abi::resource::{Rlimit, rlimit};
use abi::time::{Timespec, Timeval, clock};
use sched::Sleepable;

use super::{SysResult, int, process};
use crate::uaccess::{Plain, UserPtr};

// SAFETY: integers only, no padding.
unsafe impl Plain for Timespec {}
// SAFETY: as above.
unsafe impl Plain for Timeval {}
// SAFETY: as above.
unsafe impl Plain for Rlimit {}

const NANOS: u64 = 1_000_000_000;

fn timespec(ns: u64) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(ns / NANOS).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from(ns % NANOS).unwrap_or(0),
    }
}

/// A clock's reading in nanoseconds. The time of day and uptime come from
/// the shared page's timehands, as libc reads them without the system
/// call, so the two always agree.
pub(super) fn read(id: u64) -> Result<u64, Errno> {
    use crate::shared_page::{realtime, to_ns, uptime};
    let id = u32::try_from(id).map_err(|_| Errno::EINVAL)?;
    Ok(match id {
        clock::REALTIME | clock::REALTIME_PRECISE | clock::REALTIME_FAST | clock::TAI => {
            to_ns(realtime())
        }
        clock::MONOTONIC
        | clock::MONOTONIC_PRECISE
        | clock::MONOTONIC_FAST
        | clock::UPTIME
        | clock::UPTIME_PRECISE
        | clock::UPTIME_FAST => to_ns(uptime()),
        clock::SECOND => to_ns(realtime()) / NANOS * NANOS,
        clock::THREAD_CPUTIME_ID | clock::PROCESS_CPUTIME_ID | clock::VIRTUAL | clock::PROF => {
            crate::time::ticks_to_ns(sched::current().runtime())
        }
        _ => return Err(Errno::EINVAL),
    })
}

/// `clock_gettime(clock_id, tp)`.
pub(super) fn clock_gettime(id: u64, tp: u64) -> SysResult {
    let ts = timespec(read(id)?);
    UserPtr::<Timespec>::new(tp)
        .write(&ts)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `clock_getres(clock_id, tp)`.
pub(super) fn clock_getres(id: u64, tp: u64) -> SysResult {
    read(id)?;
    if tp != 0 {
        let res = 1_000_000_000 / crate::time::frequency().clamp(1, NANOS);
        UserPtr::<Timespec>::new(tp)
            .write(&timespec(res.max(1)))
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `gettimeofday(tp, tzp)`.
pub(super) fn gettimeofday(tp: u64, tzp: u64) -> SysResult {
    if tp != 0 {
        let us = crate::shared_page::to_us(crate::shared_page::realtime());
        let tv = Timeval {
            tv_sec: i64::try_from(us / 1_000_000).unwrap_or(i64::MAX),
            tv_usec: i64::try_from(us % 1_000_000).unwrap_or(0),
        };
        UserPtr::<Timeval>::new(tp)
            .write(&tv)
            .map_err(|_| Errno::EFAULT)?;
    }
    if tzp != 0 {
        // struct timezone: UTC.
        UserPtr::<[i32; 2]>::new(tzp)
            .write(&[0, 0])
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `nanosleep(rqtp, rmtp)`.
pub(super) fn nanosleep(s: &Sleepable, rqtp: u64, rmtp: u64) -> SysResult {
    let t = UserPtr::<Timespec>::new(rqtp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if t.tv_sec < 0 || !(0..1_000_000_000).contains(&t.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    let ns = u64::try_from(t.tv_sec)
        .unwrap_or(0)
        .saturating_mul(NANOS)
        .saturating_add(u64::try_from(t.tv_nsec).unwrap_or(0));
    let start = crate::time::now_ns();
    let q = sched::WaitQueue::new();
    match crate::signal::sleep(s, &q, || false, Some(ns)) {
        Ok(_) => Ok(0),
        // Interrupted: the time left, and EINTR even for SA_RESTART (a
        // restart would sleep the whole time again).
        Err(_) => {
            if rmtp != 0 {
                let left = ns.saturating_sub(crate::time::now_ns().saturating_sub(start));
                UserPtr::<Timespec>::new(rmtp)
                    .write(&timespec(left))
                    .map_err(|_| Errno::EFAULT)?;
            }
            Err(Errno::EINTR)
        }
    }
}

fn limit_index(which: u64) -> Result<usize, Errno> {
    usize::try_from(int(which))
        .ok()
        .filter(|&i| i < rlimit::NLIMITS)
        .ok_or(Errno::EINVAL)
}

/// `getrlimit(which, rlp)`.
pub(super) fn getrlimit(which: u64, rlp: u64) -> SysResult {
    let l = process()?.limits.lock()[limit_index(which)?];
    UserPtr::<Rlimit>::new(rlp)
        .write(&l)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `setrlimit(which, rlp)`: root may raise as well as lower.
pub(super) fn setrlimit(s: &Sleepable, which: u64, rlp: u64) -> SysResult {
    let i = limit_index(which)?;
    let l = UserPtr::<Rlimit>::new(rlp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if l.rlim_cur > l.rlim_max {
        return Err(Errno::EINVAL);
    }
    let mut l = l;
    let p = process()?;
    if i == rlimit::STACK as usize {
        // No more than exec reserves; the stack's growth follows the new
        // limit at once.
        l.rlim_cur = l.rlim_cur.min(crate::exec::MAXSSIZ);
        l.rlim_max = l.rlim_max.min(crate::exec::MAXSSIZ);
        p.space().set_stack_limit(s, l.rlim_cur);
    }
    p.limits.lock()[i] = l;
    Ok(0)
}
