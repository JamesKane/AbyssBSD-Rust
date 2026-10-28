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

/// `CPUCLOCK_BIT` and `CPUCLOCK_PROCESS_BIT`.
const CPUCLOCK_BIT: u32 = 0x8000_0000;
const CPUCLOCK_PROCESS_BIT: u32 = 0x4000_0000;

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
        clock::THREAD_CPUTIME_ID => crate::time::ticks_to_ns(sched::current().runtime()),
        clock::PROCESS_CPUTIME_ID | clock::VIRTUAL | clock::PROF => crate::time::ticks_to_ns(
            crate::proc::Process::with_current(crate::proc::Process::runtime)
                .unwrap_or_else(|| sched::current().runtime()),
        ),
        // `clock_getcpuclockid2`'s: a process's or a thread's CPU time.
        id if id & CPUCLOCK_BIT != 0 => {
            let who = (id & !(CPUCLOCK_BIT | CPUCLOCK_PROCESS_BIT)).cast_signed();
            let ticks = if id & CPUCLOCK_PROCESS_BIT != 0 {
                crate::proc::find(who).ok_or(Errno::EINVAL)?.runtime()
            } else {
                crate::uthread::find(who)
                    .and_then(|t| t.sched())
                    .ok_or(Errno::EINVAL)?
                    .runtime()
            };
            crate::time::ticks_to_ns(ticks)
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

/// `setrlimit(which, rlp)` (`kern_proc_setrlimit`): raising either limit
/// past the hard one takes `PRIV_PROC_SETRLIMIT`; a soft limit above the
/// hard one comes down to it, and some limits stop at the system's
/// (`maxdsiz`, `maxssiz`, `maxfilesperproc`, `maxprocperuid`). A negative
/// limit is infinite.
pub(super) fn setrlimit(s: &Sleepable, which: u64, rlp: u64) -> SysResult {
    let i = limit_index(which)?;
    // In range: `limit_index` took it.
    let which = u32::try_from(which).unwrap_or(u32::MAX);
    let mut l = UserPtr::<Rlimit>::new(rlp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    for v in [&mut l.rlim_cur, &mut l.rlim_max] {
        if v.cast_signed() < 0 {
            *v = abi::resource::RLIM_INFINITY;
        }
    }
    let p = process()?;
    let old = p.limits.lock()[i];
    if l.rlim_cur > old.rlim_max || l.rlim_max > old.rlim_max {
        p.priv_check(crate::cred::Priv::ProcSetrlimit)?;
    }
    l.rlim_cur = l.rlim_cur.min(l.rlim_max);
    let clamp = |l: &mut Rlimit, max: u64| {
        l.rlim_cur = l.rlim_cur.min(max);
        l.rlim_max = l.rlim_max.min(max);
    };
    match which {
        rlimit::DATA => clamp(&mut l, crate::exec::MAXDSIZ),
        rlimit::STACK => clamp(&mut l, crate::exec::MAXSSIZ),
        rlimit::NOFILE => clamp(&mut l, crate::proc::MAXFILESPERPROC),
        rlimit::NPROC => {
            clamp(&mut l, crate::proc::MAXPROCPERUID);
            l.rlim_cur = l.rlim_cur.max(1);
            l.rlim_max = l.rlim_max.max(1);
        }
        _ => {}
    }
    p.limits.lock()[i] = l;
    match which {
        // The stack's growth follows the new limit at once.
        rlimit::STACK => p.space().set_stack_limit(s, l.rlim_cur),
        rlimit::CPU => crate::proc::cpu_limit_set(&p, l.rlim_cur),
        _ => {}
    }
    Ok(0)
}
