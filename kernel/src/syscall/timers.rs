// SPDX-License-Identifier: BSD-2-Clause
//! Timers and clocks: `setitimer`, `getitimer`, the POSIX timers
//! (`ktimer_*`), `timerfd_*`, `eventfd` (`__specialfd`),
//! `clock_nanosleep`, setting the time, and `clock_getcpuclockid2`
//! (docs/MULTIUSER.md §7.5).
//!
//! Behavior reference: sys/kern/kern_time.c, sys/kern/sys_timerfd.c,
//! sys/kern/sys_eventfd.c, sys/kern/kern_descrip.c (`sys___specialfd`).

use alloc::sync::Arc;

use abi::errno::Errno;
use abi::event::{
    EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE, ITIMER_PROF, Itimerspec, Itimerval, SIGEV_THREAD_ID,
    SPECIALFD_EVENTFD, Sigevent, SpecialfdEventfd, TIMER_ABSTIME,
};
use abi::fcntl::{FREAD, FWRITE, O_CLOEXEC, O_NONBLOCK};
use abi::time::{Timespec, Timeval, clock};
use sched::Sleepable;

use super::io::max_files;
use super::{SysResult, int, process};
use crate::cred::Priv;
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};
use crate::timers::{NANOS, Sigev, Timerfd, ns_ts, ts_ns};
use crate::uaccess::{Plain, UserPtr};

// SAFETY: integers only, no padding.
unsafe impl Plain for Itimerval {}
// SAFETY: as above.
unsafe impl Plain for Itimerspec {}
// SAFETY: as above.
unsafe impl Plain for Sigevent {}
// SAFETY: as above.
unsafe impl Plain for SpecialfdEventfd {}

fn tv_ns(tv: &Timeval) -> u64 {
    tv.tv_sec
        .cast_unsigned()
        .saturating_mul(NANOS)
        .saturating_add(tv.tv_usec.cast_unsigned().saturating_mul(1000))
}

/// `ns` as a `timeval`, rounded up to the microsecond (a timer that has
/// not fired reads as not quite done).
fn ns_tv(ns: u64) -> Timeval {
    let us = ns.div_ceil(1000);
    Timeval {
        tv_sec: i64::try_from(us / 1_000_000).unwrap_or(i64::MAX),
        tv_usec: (us % 1_000_000).cast_signed(),
    }
}

/// `itimerfix`: valid, and at least `tick / 16` (62 µs) if not zero.
fn itimerfix(tv: &Timeval) -> Result<u64, Errno> {
    if tv.tv_sec < 0 || !(0..1_000_000).contains(&tv.tv_usec) {
        return Err(Errno::EINVAL);
    }
    if tv.tv_sec == 0 && tv.tv_usec != 0 && tv.tv_usec < 1000 / 16 {
        return Ok(1000 / 16 * 1000);
    }
    Ok(tv_ns(tv))
}

/// `getitimer(which, itv)`.
pub(super) fn getitimer(s: &Sleepable, which: u64, itv: u64) -> SysResult {
    let which = u32::try_from(which).map_err(|_| Errno::EINVAL)?;
    if which > ITIMER_PROF {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let (value, interval) = p.timers().getitimer(which as usize);
    let it = Itimerval {
        it_interval: ns_tv(interval),
        it_value: ns_tv(value),
    };
    crate::ktrace::structure_of(s, "itimerval", &it);
    UserPtr::<Itimerval>::new(itv)
        .write(&it)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `setitimer(which, itv, oitv)`: with no new value, `getitimer`.
pub(super) fn setitimer(s: &Sleepable, which: u64, itv: u64, oitv: u64) -> SysResult {
    if itv == 0 {
        return getitimer(s, which, oitv);
    }
    let it = UserPtr::<Itimerval>::new(itv)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let which = u32::try_from(which).map_err(|_| Errno::EINVAL)?;
    if which > ITIMER_PROF {
        return Err(Errno::EINVAL);
    }
    crate::ktrace::structure_of(s, "itimerval", &it);
    let value = itimerfix(&it.it_value)?;
    if it.it_value.tv_sec > i64::from(i32::MAX / 2) {
        return Err(Errno::EINVAL);
    }
    let interval = if value == 0 {
        0
    } else {
        if it.it_interval.tv_sec > i64::from(i32::MAX / 2) {
            return Err(Errno::EINVAL);
        }
        itimerfix(&it.it_interval)?
    };
    let p = process()?;
    let (ov, oi) = p.timers().setitimer(which as usize, value, interval);
    let old = Itimerval {
        it_interval: ns_tv(oi),
        it_value: ns_tv(ov),
    };
    crate::ktrace::structure_of(s, "itimerval", &old);
    if oitv != 0 {
        UserPtr::<Itimerval>::new(oitv)
            .write(&old)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `ktimer_create(clock_id, evp, timerid)`.
pub(super) fn ktimer_create(clock_id: u64, evp: u64, timerid: u64) -> SysResult {
    let sigev = if evp == 0 {
        None
    } else {
        let e = UserPtr::<Sigevent>::new(evp)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        Some(Sigev {
            notify: e.sigev_notify,
            signo: e.sigev_signo.cast_unsigned(),
            value: e.sigev_value,
            // `sigev_notify_thread_id` is the union's first `int`.
            tid: if e.sigev_notify == SIGEV_THREAD_ID {
                i32::try_from(e.sigev_un[0] & 0xffff_ffff).unwrap_or(0)
            } else {
                0
            },
        })
    };
    let p = process()?;
    let id = p.timers().create(int(clock_id).cast_unsigned(), sigev)?;
    if UserPtr::<i32>::new(timerid).write(&id).is_err() {
        let _ = p.timers().delete(id);
        return Err(Errno::EFAULT);
    }
    Ok(0)
}

/// `ktimer_delete(timerid)`.
pub(super) fn ktimer_delete(timerid: u64) -> SysResult {
    process()?.timers().delete(int(timerid)).map(|()| 0)
}

/// `ktimer_settime(timerid, flags, value, ovalue)`.
pub(super) fn ktimer_settime(timerid: u64, flags: u64, value: u64, ovalue: u64) -> SysResult {
    let val = UserPtr::<Itimerspec>::new(value)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let old = process()?
        .timers()
        .settime(int(timerid), int(flags).cast_unsigned(), &val)?;
    if ovalue != 0 {
        UserPtr::<Itimerspec>::new(ovalue)
            .write(&old)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `ktimer_gettime(timerid, value)`.
pub(super) fn ktimer_gettime(timerid: u64, value: u64) -> SysResult {
    let v = process()?.timers().gettime(int(timerid))?;
    UserPtr::<Itimerspec>::new(value)
        .write(&v)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `ktimer_getoverrun(timerid)`.
pub(super) fn ktimer_getoverrun(timerid: u64) -> SysResult {
    let n = process()?.timers().getoverrun(int(timerid))?;
    Ok(u64::from(n.cast_unsigned()))
}

/// Installs `kind` as a new descriptor.
fn install(s: &Sleepable, kind: FileKind, status: u32, cloexec: bool) -> SysResult {
    let p = process()?;
    let limit = max_files(&p);
    let fd = p.files.lock(s).alloc(
        File::new(kind, status),
        FdFlags {
            cloexec,
            clofork: false,
            resolve_beneath: false,
        },
        0,
        limit,
    )?;
    Ok(u64::from(fd.cast_unsigned()))
}

/// `timerfd_create(clockid, flags)`.
pub(super) fn timerfd_create(s: &Sleepable, clockid: u64, flags: u64) -> SysResult {
    let flags = int(flags).cast_unsigned();
    let t = Timerfd::new(
        int(clockid).cast_unsigned(),
        process()?.cred().euid(),
        process()?.cred().egid(),
    )?;
    if flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    install(
        s,
        FileKind::Timerfd(t),
        FREAD | (flags & O_NONBLOCK),
        flags & O_CLOEXEC != 0,
    )
}

fn timerfd_of(
    s: &Sleepable,
    fd: u64,
    need: &abi::capsicum::CapRights,
) -> Result<(Arc<File>, Arc<Timerfd>), Errno> {
    let f = super::io::current_file(s, fd, need)?;
    let FileKind::Timerfd(t) = f.kind() else {
        return Err(Errno::EINVAL);
    };
    let t = Arc::clone(t);
    Ok((f, t))
}

/// `timerfd_gettime(fd, curr_value)`.
pub(super) fn timerfd_gettime(s: &Sleepable, fd: u64, curr: u64) -> SysResult {
    let (_f, t) = timerfd_of(s, fd, &crate::capsicum::rights::READ)?;
    UserPtr::<Itimerspec>::new(curr)
        .write(&t.gettime())
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `timerfd_settime(fd, flags, new_value, old_value)`.
pub(super) fn timerfd_settime(s: &Sleepable, fd: u64, flags: u64, new: u64, old: u64) -> SysResult {
    let new = UserPtr::<Itimerspec>::new(new)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let flags = int(flags).cast_unsigned();
    // The flags and values are checked before the descriptor.
    if flags & !(abi::event::TFD_TIMER_ABSTIME | abi::event::TFD_TIMER_CANCEL_ON_SET) != 0 {
        return Err(Errno::EINVAL);
    }
    ts_ns(&new.it_value)?;
    ts_ns(&new.it_interval)?;
    let (_f, t) = timerfd_of(s, fd, &crate::capsicum::rights::WRITE)?;
    let prev = t.settime(flags, &new)?;
    if old != 0 {
        UserPtr::<Itimerspec>::new(old)
            .write(&prev)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `__specialfd(type, req, len)`: an `eventfd`.
pub(super) fn specialfd(s: &Sleepable, ty: u64, req: u64, len: u64) -> SysResult {
    match int(ty) {
        SPECIALFD_EVENTFD => {
            if len != size_of::<SpecialfdEventfd>() as u64 {
                return Err(Errno::EINVAL);
            }
            let ae = UserPtr::<SpecialfdEventfd>::new(req)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            let flags = ae.flags.cast_unsigned();
            if flags & !(EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE) != 0 {
                return Err(Errno::EINVAL);
            }
            let e = crate::fs::eventfd::Eventfd::new(ae.initval, flags);
            let status = FREAD
                | FWRITE
                | if flags & EFD_NONBLOCK != 0 {
                    O_NONBLOCK
                } else {
                    0
                };
            install(s, FileKind::Eventfd(e), status, flags & EFD_CLOEXEC != 0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `clock_nanosleep(clock_id, flags, rqtp, rmtp)`: the error is the
/// return value, as libc's wrapper expects.
pub(super) fn clock_nanosleep(
    s: &Sleepable,
    id: u64,
    flags: u64,
    rqtp: u64,
    rmtp: u64,
) -> SysResult {
    let r = user_clock_nanosleep(
        s,
        int(id).cast_unsigned(),
        int(flags).cast_unsigned(),
        rqtp,
        rmtp,
    );
    Ok(u64::from(r.err().map_or(0, |e| e.get().cast_unsigned())))
}

/// `nanosleep(rqtp, rmtp)`: on `CLOCK_REALTIME`, relative.
pub(super) fn nanosleep(s: &Sleepable, rqtp: u64, rmtp: u64) -> SysResult {
    user_clock_nanosleep(s, clock::REALTIME, 0, rqtp, rmtp).map(|()| 0)
}

/// `user_clock_nanosleep`.
fn user_clock_nanosleep(
    s: &Sleepable,
    id: u32,
    flags: u32,
    rqtp: u64,
    rmtp: u64,
) -> Result<(), Errno> {
    let rq = UserPtr::<Timespec>::new(rqtp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    match kern_clock_nanosleep(s, id, flags, &rq) {
        Err((e, left)) => {
            if e == Errno::EINTR
                && flags & TIMER_ABSTIME == 0
                && rmtp != 0
                && let Some(left) = left
            {
                UserPtr::<Timespec>::new(rmtp)
                    .write(&ns_ts(left))
                    .map_err(|_| Errno::EFAULT)?;
            }
            Err(e)
        }
        Ok(()) => Ok(()),
    }
}

/// `kern_clock_nanosleep`: on `EINTR`, the time left.
fn kern_clock_nanosleep(
    s: &Sleepable,
    id: u32,
    flags: u32,
    rq: &Timespec,
) -> Result<(), (Errno, Option<u64>)> {
    if !(0..1_000_000_000).contains(&rq.tv_nsec) {
        return Err((Errno::EINVAL, None));
    }
    if flags & !TIMER_ABSTIME != 0 {
        return Err((Errno::EINVAL, None));
    }
    let real = match id {
        clock::REALTIME
        | clock::TAI
        | clock::REALTIME_PRECISE
        | clock::REALTIME_FAST
        | clock::SECOND => true,
        clock::MONOTONIC
        | clock::UPTIME
        | clock::MONOTONIC_PRECISE
        | clock::UPTIME_PRECISE
        | clock::MONOTONIC_FAST
        | clock::UPTIME_FAST => false,
        clock::VIRTUAL | clock::PROF | clock::PROCESS_CPUTIME_ID => {
            return Err((Errno::ENOTSUP, None));
        }
        _ => return Err((Errno::EINVAL, None)),
    };
    let abs = flags & TIMER_ABSTIME != 0;
    let q = sched::WaitQueue::new();
    loop {
        let want = if rq.tv_sec < 0 {
            None
        } else {
            Some(
                rq.tv_sec
                    .cast_unsigned()
                    .saturating_mul(NANOS)
                    .saturating_add(rq.tv_nsec.cast_unsigned()),
            )
        };
        let ns = if abs {
            let now = super::clock_ns(id).map_err(|e| (e, None))?;
            want.map_or(0, |w| w.saturating_sub(now))
        } else {
            want.unwrap_or(0)
        };
        if ns == 0 {
            return Ok(());
        }
        let start = crate::time::now_ns();
        let _w = sched::wait_message("nanslp");
        match crate::signal::sleep(s, &q, || false, Some(ns)) {
            // An absolute real-time sleep looks again: the clock may
            // have been set meanwhile.
            Ok(_) if abs && real => {}
            Ok(_) => return Ok(()),
            Err(e) => {
                let left = ns.saturating_sub(crate::time::now_ns().saturating_sub(start));
                if left == 0 {
                    return Ok(());
                }
                let e = if e == Errno::ERESTART {
                    Errno::EINTR
                } else {
                    e
                };
                return Err((e, Some(left)));
            }
        }
    }
}

/// `clock_settime(clock_id, tp)`: only the real-time clock, and only by
/// the superuser.
pub(super) fn clock_settime(id: u64, tp: u64) -> SysResult {
    let ts = UserPtr::<Timespec>::new(tp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    process()?.priv_check(Priv::ClockSettime)?;
    if int(id).cast_unsigned() != clock::REALTIME {
        return Err(Errno::EINVAL);
    }
    if ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 || ts.tv_sec < 0 {
        return Err(Errno::EINVAL);
    }
    set_time(ts_ns(&ts)?)?;
    Ok(0)
}

/// `settimeofday(tv, tzp)`.
pub(super) fn settimeofday(tv: u64, tzp: u64) -> SysResult {
    let t = if tv == 0 {
        None
    } else {
        Some(
            UserPtr::<Timeval>::new(tv)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    if tzp != 0 {
        UserPtr::<[i32; 2]>::new(tzp)
            .read()
            .map_err(|_| Errno::EFAULT)?;
    }
    process()?.priv_check(Priv::Settimeofday)?;
    if let Some(t) = t {
        if t.tv_usec < 0 || t.tv_usec >= 1_000_000 || t.tv_sec < 0 {
            return Err(Errno::EINVAL);
        }
        set_time(tv_ns(&t))?;
    }
    Ok(0)
}

/// The latest time of day seen, and the second of the last step forward,
/// for securelevel 2 (`settime`'s `maxtime` and `laststep`).
static SECURE_TIME: sync::SpinLock<(u64, u64)> = sync::SpinLock::new((0, u64::MAX));

/// Steps the time of day to `unix_ns` (`settime`): the absolute real-time
/// timers see the jump. Above securelevel 1 the clock goes back no more
/// than a second past the latest time seen, and forward no more than a
/// second at a time, once a second (`EPERM` for a second step).
fn set_time(unix_ns: u64) -> Result<(), Errno> {
    let mut to = unix_ns;
    if crate::fs::cred().securelevel_gt(1).is_err() {
        const SEC: u64 = 1_000_000_000;
        let now = super::clock_ns(clock::REALTIME)?;
        let mut st = SECURE_TIME.lock();
        if to < now {
            st.0 = st.0.max(now);
            if to / SEC + 1 < st.0 / SEC {
                to = (st.0 / SEC - 1) * SEC + to % SEC;
                kprintln!("Time adjustment clamped to -1 second");
            }
        } else {
            if now / SEC == st.1 {
                return Err(Errno::EPERM);
            }
            if (to - now) / SEC > 1 {
                to = (now / SEC + 1) * SEC + to % SEC;
                kprintln!("Time adjustment clamped to +1 second");
            }
            st.1 = now / SEC;
        }
    }
    crate::time::set_time_of_day(to);
    crate::timers::clock_jumped();
    Ok(())
}

/// `adjtime(delta, olddelta)`: the clock is stepped, not slewed (no
/// `ntpd` here yet); the old delta is always none.
pub(super) fn adjtime(delta: u64, olddelta: u64) -> SysResult {
    let d = if delta == 0 {
        None
    } else {
        Some(
            UserPtr::<Timeval>::new(delta)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    if let Some(d) = d {
        process()?.priv_check(Priv::Adjtime)?;
        let now = i128::from(super::clock_ns(clock::REALTIME)?);
        let step = i128::from(d.tv_sec) * i128::from(NANOS) + i128::from(d.tv_usec) * 1000;
        set_time(u64::try_from((now + step).max(0)).unwrap_or(0))?;
    }
    if olddelta != 0 {
        UserPtr::<Timeval>::new(olddelta)
            .write(&Timeval::default())
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `clock_getcpuclockid2(id, which, clock_id)`: a process's or thread's
/// CPU-time clock, encoded as FreeBSD does (`CPUCLOCK_*`).
pub(super) fn clock_getcpuclockid2(id: u64, which: u64, clock_id: u64) -> SysResult {
    /// `CPUCLOCK_WHICH_PID`, `CPUCLOCK_WHICH_TID`.
    const WHICH_PID: i32 = 0;
    const WHICH_TID: i32 = 1;
    /// `CPUCLOCK_BIT`, `CPUCLOCK_PROCESS_BIT`.
    const BIT: u32 = 0x8000_0000;
    const PROCESS_BIT: u32 = 0x4000_0000;
    let id = i32::try_from(id.cast_signed()).unwrap_or(-1);
    let cid = match int(which) {
        WHICH_PID => {
            let pid = if id == 0 { process()?.pid() } else { id };
            crate::proc::find(pid).ok_or(Errno::ESRCH)?;
            pid.cast_unsigned() | BIT | PROCESS_BIT
        }
        WHICH_TID => {
            let tid = if id == 0 { super::thread()?.tid() } else { id };
            crate::uthread::find(tid).ok_or(Errno::ESRCH)?;
            tid.cast_unsigned() | BIT
        }
        _ => return Err(Errno::EINVAL),
    };
    UserPtr::<u32>::new(clock_id)
        .write(&cid)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}
