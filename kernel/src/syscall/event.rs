// SPDX-License-Identifier: BSD-2-Clause
//! Waiting for descriptors: `poll`, `ppoll`, `select`, `pselect`,
//! `kqueue`, `kqueuex`, and `kevent` (docs/MULTIUSER.md §7.3, §7.4).
//!
//! Behavior reference: sys/kern/sys_generic.c (`kern_poll`,
//! `kern_select`), sys/kern/kern_event.c (`kern_kevent`, `kqueue_scan`).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::event::{
    EV_ERROR, EV_RECEIPT, EV_SYSFLAGS, FD_SETSIZE, INFTIM, KQUEUE_CLOEXEC, KQUEUE_CPONFORK, Kevent,
    POLLERR, POLLHUP, POLLNVAL, POLLOUT, POLLRDBAND, POLLRDNORM, POLLWRNORM, Pollfd,
};
use abi::fcntl::{FREAD, FWRITE};
use abi::signal::Sigset;
use abi::time::{Timespec, Timeval};
use event::Wake;
use sched::Sleepable;

use super::io::max_files;
use super::{SysResult, int, process, thread};
use crate::event::{Changes, KqueueFile, Poller};
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};
use crate::signal::Woke;
use crate::uaccess::{self, Plain, UserPtr};

// SAFETY: integers only, no padding.
unsafe impl Plain for Pollfd {}
// SAFETY: as above.
unsafe impl Plain for Kevent {}

const NANOS: u64 = 1_000_000_000;

/// A timeout in nanoseconds (`None`: none), from a `timespec`: `EINVAL`
/// unless valid; more than `INT32_MAX` seconds is for ever, as FreeBSD
/// treats it.
fn timespec_timeout(ts: &Timespec) -> Result<Option<u64>, Errno> {
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    if ts.tv_sec > i64::from(i32::MAX) {
        return Ok(None);
    }
    Ok(Some(
        ts.tv_sec.cast_unsigned() * NANOS + ts.tv_nsec.cast_unsigned(),
    ))
}

/// What a wait for descriptors ended with, for the caller's mask.
fn finish(r: Result<u64, Errno>, mask: bool) -> SysResult {
    // Neither `poll` nor `select` restarts after a signal.
    let r = r.map_err(|e| {
        if e == Errno::ERESTART {
            Errno::EINTR
        } else {
            e
        }
    });
    if mask {
        // Interrupted: the handler runs with the temporary mask, as
        // after `sigsuspend`; otherwise the old one is back at once.
        if r != Err(Errno::EINTR)
            && let Ok(t) = thread()
        {
            crate::signal::restore_mask(&t);
        }
    }
    r
}

/// The wait loop of `poll` and `select` (`seltdwait` between scans):
/// `scan` looks once, registering `w` if given, and returns how many are
/// ready.
fn wait_loop(
    s: &Sleepable,
    ns: Option<u64>,
    mut scan: impl FnMut(Option<&Arc<dyn Wake>>) -> Result<u64, Errno>,
) -> Result<u64, Errno> {
    if ns == Some(0) {
        return scan(None);
    }
    let poller = Poller::new();
    let w: Arc<dyn Wake> = Arc::clone(&poller) as Arc<dyn Wake>;
    let deadline = ns.map(|n| crate::time::now_ns().saturating_add(n));
    loop {
        poller.arm();
        let n = scan(Some(&w))?;
        if n != 0 {
            return Ok(n);
        }
        let left = deadline.map(|d| d.saturating_sub(crate::time::now_ns()));
        if left == Some(0) {
            return Ok(0);
        }
        match poller.wait(s, left)? {
            Woke::Done => {}
            Woke::TimedOut => return Ok(0),
        }
    }
}

/// Every open file of `fds` that has `CAP_EVENT` (`None` for one that is
/// not open, or lacks it), looked up together, and why each missing one
/// is (`EBADF` or `ENOTCAPABLE`).
fn files<I: IntoIterator<Item = i32>>(
    s: &Sleepable,
    fds: I,
) -> Result<Vec<Result<Arc<File>, Errno>>, Errno> {
    let p = process()?;
    let t = p.files.lock(s);
    Ok(fds
        .into_iter()
        .map(|fd| {
            if fd < 0 {
                return Err(Errno::EBADF);
            }
            t.get_checked(fd, &crate::capsicum::rights::EVENT)
        })
        .collect())
}

/// `poll(fds, nfds, timeout)`.
pub(super) fn poll(s: &Sleepable, fds: u64, nfds: u64, timeout: u64) -> SysResult {
    let t = int(timeout);
    let ns = if t == INFTIM {
        None
    } else if t < 0 {
        return Err(Errno::EINVAL);
    } else {
        Some(u64::from(t.cast_unsigned()) * 1_000_000)
    };
    kern_poll(s, fds, nfds, ns, None)
}

/// `ppoll(fds, nfds, ts, set)`.
pub(super) fn ppoll(s: &Sleepable, fds: u64, nfds: u64, ts: u64, set: u64) -> SysResult {
    let ns = if ts == 0 {
        None
    } else {
        let t = UserPtr::<Timespec>::new(ts)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        Some(t)
    };
    let mask = if set == 0 {
        None
    } else {
        Some(
            UserPtr::<Sigset>::new(set)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    let ns = match ns {
        None => None,
        Some(t) => timespec_timeout(&t)?,
    };
    kern_poll(s, fds, nfds, ns, mask)
}

/// `kern_poll`.
fn kern_poll(
    s: &Sleepable,
    ufds: u64,
    nfds: u64,
    ns: Option<u64>,
    mask: Option<Sigset>,
) -> SysResult {
    // `nfds_t` is an `unsigned int`.
    let nfds = usize::try_from(nfds & 0xffff_ffff).unwrap_or(usize::MAX);
    let p = process()?;
    if nfds > max_files(&p) && nfds > FD_SETSIZE {
        return Err(Errno::EINVAL);
    }
    let mut kfds = vec![Pollfd::default(); nfds];
    if nfds > 0 {
        // SAFETY: `Pollfd` is plain, so its array is bytes.
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                kfds.as_mut_ptr().cast::<u8>(),
                nfds * size_of::<Pollfd>(),
            )
        };
        uaccess::copy_in(bytes, ufds).map_err(|_| Errno::EFAULT)?;
    }
    if let Some(m) = mask {
        crate::signal::swap_mask(&*thread()?, m);
    }
    let r = wait_loop(s, ns, |w| {
        let fs = files(s, kfds.iter().map(|f| f.fd))?;
        let mut n = 0;
        for (pfd, f) in kfds.iter_mut().zip(fs) {
            if pfd.fd < 0 {
                pfd.revents = 0;
                continue;
            }
            let Ok(f) = f else {
                pfd.revents = POLLNVAL;
                n += 1;
                continue;
            };
            pfd.revents = f.poll(s, pfd.events, w);
            // POSIX: never `POLLOUT` with `POLLHUP`.
            if pfd.revents & POLLHUP != 0 {
                pfd.revents &= !POLLOUT;
            }
            if pfd.revents != 0 {
                n += 1;
            }
        }
        Ok(n)
    });
    let r = r.and_then(|n| {
        if nfds > 0 {
            // SAFETY: as above.
            let bytes = unsafe {
                core::slice::from_raw_parts(kfds.as_ptr().cast::<u8>(), nfds * size_of::<Pollfd>())
            };
            uaccess::copy_out(ufds, bytes).map_err(|_| Errno::EFAULT)?;
        }
        Ok(n)
    });
    finish(r, mask.is_some())
}

/// What each `select` set asks of `poll` (`select_flags`).
const SELECT_FLAGS: [i16; 3] = [
    POLLRDNORM | POLLHUP | POLLERR,
    POLLWRNORM | POLLHUP | POLLERR,
    POLLRDBAND | POLLERR,
];

/// `select(nd, in, ou, ex, tv)`.
pub(super) fn select(s: &Sleepable, nd: u64, sets: [u64; 3], tv: u64) -> SysResult {
    let tv = if tv == 0 {
        None
    } else {
        Some(
            UserPtr::<Timeval>::new(tv)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    kern_select(s, int(nd), sets, tv, None)
}

/// `pselect(nd, in, ou, ex, ts, sm)`.
pub(super) fn pselect(s: &Sleepable, nd: u64, sets: [u64; 3], ts: u64, sm: u64) -> SysResult {
    let tv = if ts == 0 {
        None
    } else {
        let t = UserPtr::<Timespec>::new(ts)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        // `TIMESPEC_TO_TIMEVAL`.
        Some(Timeval {
            tv_sec: t.tv_sec,
            tv_usec: t.tv_nsec / 1000,
        })
    };
    let mask = if sm == 0 {
        None
    } else {
        Some(
            UserPtr::<Sigset>::new(sm)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    kern_select(s, int(nd), sets, tv, mask)
}

/// `kern_select` (with `kern_pselect`'s mask).
fn kern_select(
    s: &Sleepable,
    nd: i32,
    sets: [u64; 3],
    tv: Option<Timeval>,
    mask: Option<Sigset>,
) -> SysResult {
    let ndu = usize::try_from(nd).map_err(|_| Errno::EINVAL)?;
    let p = process()?;
    let lf = p.files.lock(s).len();
    let nd = ndu.min(lf);
    // Bits past the last open descriptor must be clear
    // (`select_check_badfd`).
    for &set in &sets {
        if set == 0 || nd >= ndu {
            continue;
        }
        let (lo, hi) = (nd / 8, ndu.div_ceil(8));
        let mut b = vec![0u8; hi - lo];
        uaccess::copy_in(&mut b, set + lo as u64).map_err(|_| Errno::EFAULT)?;
        for fd in nd..ndu {
            if b[fd / 8 - lo] & (1 << (fd % 8)) != 0 {
                return Err(Errno::EBADF);
            }
        }
    }
    let words = nd.div_ceil(64);
    let mut ibits: [Vec<u64>; 3] = Default::default();
    for (k, &set) in sets.iter().enumerate() {
        if set == 0 {
            continue;
        }
        ibits[k] = vec![0u64; words];
        if words > 0 {
            // SAFETY: `u64`s are plain bytes.
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(ibits[k].as_mut_ptr().cast::<u8>(), words * 8)
            };
            uaccess::copy_in(bytes, set).map_err(|_| Errno::EFAULT)?;
        }
    }
    let ns = match tv {
        None => None,
        Some(t) => {
            if t.tv_sec < 0 || !(0..1_000_000).contains(&t.tv_usec) {
                return Err(Errno::EINVAL);
            }
            if t.tv_sec > i64::from(i32::MAX) {
                None
            } else {
                Some(t.tv_sec.cast_unsigned() * NANOS + t.tv_usec.cast_unsigned() * 1000)
            }
        }
    };
    // The descriptors asked about, and what of each.
    let mut asked: Vec<(i32, i16)> = Vec::new();
    for fd in 0..nd {
        let (w, bit) = (fd / 64, 1u64 << (fd % 64));
        let mut flags = 0;
        for k in 0..3 {
            if ibits[k].get(w).is_some_and(|x| x & bit != 0) {
                flags |= SELECT_FLAGS[k];
            }
        }
        if flags != 0 {
            asked.push((i32::try_from(fd).unwrap_or(i32::MAX), flags));
        }
    }
    if let Some(m) = mask {
        crate::signal::swap_mask(&*thread()?, m);
    }
    let mut obits: [Vec<u64>; 3] = Default::default();
    let r = wait_loop(s, ns, |w| {
        for k in 0..3 {
            obits[k] = vec![0u64; if sets[k] == 0 { 0 } else { words }];
        }
        let fs = files(s, asked.iter().map(|a| a.0))?;
        let mut n = 0;
        for (&(fd, flags), f) in asked.iter().zip(fs) {
            let f = f?;
            let ev = f.poll(s, flags, w);
            let (wd, bit) = (fd.cast_unsigned() as usize / 64, 1u64 << (fd % 64));
            for k in 0..3 {
                if ev & SELECT_FLAGS[k] != 0 && ibits[k].get(wd).is_some_and(|x| x & bit != 0) {
                    obits[k][wd] |= bit;
                    n += 1;
                }
            }
        }
        Ok(n)
    });
    let r = r.and_then(|n| {
        for (k, &set) in sets.iter().enumerate() {
            if set != 0 && words > 0 {
                // SAFETY: as above.
                let bytes = unsafe {
                    core::slice::from_raw_parts(obits[k].as_ptr().cast::<u8>(), words * 8)
                };
                uaccess::copy_out(set, bytes).map_err(|_| Errno::EFAULT)?;
            }
        }
        Ok(n)
    });
    finish(r, mask.is_some())
}

/// `kqueue()`.
pub(super) fn kqueue(s: &Sleepable) -> SysResult {
    kqueuex(s, 0)
}

/// `kqueuex(flags)`.
pub(super) fn kqueuex(s: &Sleepable, flags: u64) -> SysResult {
    let flags = u32::try_from(flags).map_err(|_| Errno::EINVAL)?;
    if flags & !(KQUEUE_CLOEXEC | KQUEUE_CPONFORK) != 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let k = KqueueFile::charged(flags & KQUEUE_CPONFORK != 0)?;
    let kq = Arc::clone(&k.kq);
    let fdf = FdFlags {
        cloexec: flags & KQUEUE_CLOEXEC != 0,
        clofork: false,
        resolve_beneath: false,
    };
    let limit = max_files(&p);
    let mut t = p.files.lock(s);
    let fd = t.alloc(
        File::new(FileKind::Kqueue(k), FREAD | FWRITE),
        fdf,
        0,
        limit,
    )?;
    t.add_kqueue(&kq);
    Ok(u64::from(fd.cast_unsigned()))
}

/// `kevent(fd, changelist, nchanges, eventlist, nevents, timeout)`.
pub(super) fn kevent(
    s: &Sleepable,
    fd: u64,
    changelist: u64,
    nchanges: u64,
    eventlist: u64,
    nevents: u64,
    timeout: u64,
) -> SysResult {
    let timeout = if timeout == 0 {
        None
    } else {
        Some(
            UserPtr::<Timespec>::new(timeout)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    let p = process()?;
    // Changes need `CAP_KQUEUE_CHANGE`, events `CAP_KQUEUE_EVENT`
    // (`kern_kevent`).
    let mut need = crate::capsicum::rights::NO;
    if int(nchanges) > 0 {
        need = need.union(&crate::capsicum::rights::KQUEUE_CHANGE);
    }
    if int(nevents) > 0 {
        need = need.union(&crate::capsicum::rights::KQUEUE_EVENT);
    }
    let f = super::io::file(s, &p, fd, &need)?;
    let FileKind::Kqueue(k) = f.kind() else {
        return Err(Errno::EINVAL);
    };
    let nchanges = int(nchanges);
    let mut nevents = int(nevents);
    if nchanges < 0 {
        return Err(Errno::EINVAL);
    }
    let src = Changes {
        s,
        p: &p,
        kq: &k.kq,
    };
    let mut out = eventlist;
    let mut nerrors = 0u64;
    let mut at = changelist;
    let mut left = nchanges.cast_unsigned() as usize;
    while left > 0 {
        let n = left.min(event::KQ_NEVENTS);
        let mut buf = [Kevent::default(); event::KQ_NEVENTS];
        // SAFETY: `Kevent` is plain.
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), n * size_of::<Kevent>())
        };
        uaccess::copy_in(bytes, at).map_err(|_| Errno::EFAULT)?;
        for kev in &mut buf[..n] {
            if kev.filter == 0 {
                continue;
            }
            kev.flags &= !EV_SYSFLAGS;
            let r = k.kq.register(kev, &src);
            if r.is_err() || kev.flags & EV_RECEIPT != 0 {
                if nevents == 0 {
                    return r.map(|()| 0);
                }
                kev.flags = EV_ERROR;
                kev.data = r.err().map_or(0, |e| i64::from(e.get()));
                UserPtr::<Kevent>::new(out)
                    .write(kev)
                    .map_err(|_| Errno::EFAULT)?;
                out += size_of::<Kevent>() as u64;
                nevents -= 1;
                nerrors += 1;
            }
        }
        left -= n;
        at += (n * size_of::<Kevent>()) as u64;
    }
    if nerrors > 0 {
        return Ok(nerrors);
    }
    scan(s, k, nevents, out, timeout)
}

/// `kqueue_scan`.
fn scan(
    s: &Sleepable,
    k: &KqueueFile,
    maxevents: i32,
    out: u64,
    timeout: Option<Timespec>,
) -> SysResult {
    if maxevents == 0 {
        return Ok(0);
    }
    let max = usize::try_from(maxevents).map_err(|_| Errno::EINVAL)?;
    let ns = match timeout {
        None => None,
        Some(t) => timespec_timeout(&t)?,
    };
    let deadline = ns.map(|n| crate::time::now_ns().saturating_add(n));
    loop {
        if k.kq.pending() == 0 {
            let left = deadline.map(|d| d.saturating_sub(crate::time::now_ns()));
            if left == Some(0) {
                return Ok(0);
            }
            let kq = &k.kq;
            match crate::signal::sleep(s, &k.wait, || kq.pending() > 0, left) {
                Ok(Woke::Done) => continue,
                Ok(Woke::TimedOut) => return Ok(0),
                // Never restarted after a signal.
                Err(Errno::ERESTART) => return Err(Errno::EINTR),
                Err(e) => return Err(e),
            }
        }
        let pass = k.kq.begin_scan();
        let mut total = 0;
        while total < max {
            let mut buf = [Kevent::default(); event::KQ_NEVENTS];
            let room = (max - total).min(buf.len());
            let n = k.kq.collect(pass, &mut buf[..room]);
            if n > 0 {
                // SAFETY: `Kevent` is plain.
                let bytes = unsafe {
                    core::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n * size_of::<Kevent>())
                };
                let at = out + (total * size_of::<Kevent>()) as u64;
                uaccess::copy_out(at, bytes).map_err(|_| Errno::EFAULT)?;
            }
            total += n;
            if n < room {
                break;
            }
        }
        // Everything queued had gone quiet: wait again.
        if total == 0 {
            continue;
        }
        return Ok(total as u64);
    }
}
