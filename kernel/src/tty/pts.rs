// SPDX-License-Identifier: BSD-2-Clause
//! Pseudo-terminals (docs/MULTIUSER.md §9.2): `posix_openpt` makes a
//! terminal, `/dev/pts/N`, and returns its master, a file of its own
//! kind. There is no `/dev/ptmx`: it is FreeBSD's `pty` module's, which
//! GENERIC does not load. The master reads what the terminal's
//! discipline output and writes its input; its requests are the
//! terminal's, plus `TIOCPTMASTER` (libc's `grantpt` and `unlockpt`),
//! `FIODGNAME` (`ptsname`), `TIOCSIG`, and packet mode (`TIOCPKT`), in
//! which each read starts with a byte saying what happened: data, or
//! flushes, stops, and starts.
//!
//! A user's terminal is theirs, group `tty`, mode 0620. Once its last
//! slave closes, the master reads end of file (after what is queued is
//! thrown away) and writes fail with `EIO`; once the master closes, the
//! terminal is gone and is freed, with its node and number, when nothing
//! else holds it.
//!
//! Behavior reference: sys/kern/tty_pts.c, lib/libc/stdlib/ptsname.c.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use abi::errno::Errno;
use abi::ioctl::*;
use abi::stat::{S_IFCHR, Stat};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vfs::Uio;

use super::{CHUNK, Driver, Make, NO_PID, Output, Tty, get_int, put_int};

/// The numbers in use (`pts_pool`).
static UNITS: SpinLock<Vec<bool>> = SpinLock::new(Vec::new());

/// The lowest free number (`alloc_unr`).
fn alloc_unit() -> u32 {
    let mut u = UNITS.lock();
    let i = u.iter().position(|&used| !used).unwrap_or(u.len());
    if i == u.len() {
        u.push(true);
    } else {
        u[i] = true;
    }
    u32::try_from(i).unwrap_or(u32::MAX)
}

fn free_unit(unit: u32) {
    if let Some(used) = UNITS.lock().get_mut(unit as usize) {
        *used = false;
    }
}

/// What the master and the terminal's device share (`pts_softc`).
struct Softc {
    unit: u32,
    /// The slave is closed after being open (`PTS_FINISHED`).
    finished: AtomicBool,
    /// Packet mode (`PTS_PKT`), and its events not yet read (`pts_pkt`).
    pkt_mode: AtomicBool,
    pkt: AtomicU8,
    /// The master's readers (`pts_outwait`) and writers (`pts_inwait`),
    /// and its pollers and knotes.
    outwait: WaitQueue,
    inwait: WaitQueue,
    notify: event::Notify,
}

/// The terminal's device: the master (`pts_class`).
struct PtsDriver(Arc<Softc>);

impl Driver for PtsDriver {
    /// `ptsdrv_outwakeup`: the master's readers.
    fn outwakeup(&self, _out: &mut Output<'_>) {
        self.0.outwait.wake_all();
        self.0.notify.notify(0);
    }

    /// `ptsdrv_inwakeup`: the master's writers.
    fn inwakeup(&self) {
        self.0.inwait.wake_all();
        self.0.notify.notify(0);
    }

    fn open(&self) -> Result<(), Errno> {
        self.0.finished.store(false, Ordering::Release);
        Ok(())
    }

    /// `ptsdrv_close`: the master's sleepers wake to find it closed.
    fn close(&self) {
        self.0.finished.store(true, Ordering::Release);
        self.0.outwait.wake_all();
        self.0.inwait.wake_all();
        self.0.notify.notify(0);
    }

    /// `ptsdrv_pktnotify`: an event clears its opposite.
    fn pktnotify(&self, event: u8) {
        let opposite = match event {
            TIOCPKT_STOP => TIOCPKT_START,
            TIOCPKT_START => TIOCPKT_STOP,
            TIOCPKT_NOSTOP => TIOCPKT_DOSTOP,
            TIOCPKT_DOSTOP => TIOCPKT_NOSTOP,
            _ => 0,
        };
        let _ = self
            .0
            .pkt
            .try_update(Ordering::AcqRel, Ordering::Acquire, |p| {
                Some((p & !opposite) | event)
            });
        self.0.outwait.wake_all();
        self.0.notify.notify(0);
    }

    /// `ptsdrv_free`: the number is free again.
    fn free(&self) {
        free_unit(self.0.unit);
    }
}

/// A pseudo-terminal's master, as an open file (`DTYPE_PTS`).
pub(crate) struct Master {
    tty: Arc<Tty>,
    sc: Arc<Softc>,
    /// Its count against its maker's `RLIMIT_NPTS`.
    _charge: crate::uidinfo::Charge,
}

/// `posix_openpt` (`pts_alloc`): a new terminal, owned by the caller's
/// real user, and its master; `EAGAIN` past the caller's `RLIMIT_NPTS`.
pub(crate) fn alloc() -> Result<Master, abi::errno::Errno> {
    let charge =
        crate::uidinfo::Charge::take(crate::uidinfo::Kind::Ptys, abi::errno::Errno::EAGAIN)?;
    let unit = alloc_unit();
    let sc = Arc::new(Softc {
        unit,
        finished: AtomicBool::new(false),
        pkt_mode: AtomicBool::new(false),
        pkt: AtomicU8::new(0),
        outwait: WaitQueue::new(),
        inwait: WaitQueue::new(),
        notify: event::Notify::new(),
    });
    let uid = crate::fs::cred().ruid;
    let tty = super::create(
        &alloc::format!("pts/{unit}"),
        Arc::new(PtsDriver(Arc::clone(&sc))),
        Make {
            user: Some(uid),
            ..Make::default()
        },
    );
    Ok(Master {
        tty,
        sc,
        _charge: charge,
    })
}

impl Drop for Master {
    /// `ptsdev_close`: the terminal is gone.
    fn drop(&mut self) {
        // SAFETY: files close in thread context.
        let s = unsafe { Sleepable::assume() };
        self.tty.rel_gone(&s);
    }
}

impl Master {
    /// Its terminal (`ptsdev_fill_kinfo`).
    pub(crate) fn tty(&self) -> &Arc<Tty> {
        &self.tty
    }

    /// What the master could read now (`ttydisc_getc_poll`).
    fn readable(&self) -> usize {
        self.tty.out_used.load(Ordering::Acquire)
    }

    /// `ptsdev_read`.
    pub(crate) fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        if uio.resid() == 0 {
            return Ok(0);
        }
        loop {
            {
                let mut st = self.tty.st.lock(s);
                // Packet mode: the events first, alone.
                if self.sc.pkt_mode.load(Ordering::Acquire) {
                    let pkt = self.sc.pkt.swap(0, Ordering::AcqRel);
                    if pkt != 0 {
                        drop(st);
                        return uio.copy_out(&[pkt]);
                    }
                }
                if !st.d.stopped() && st.d.output_len() > 0 {
                    let mut done = 0;
                    if self.sc.pkt_mode.load(Ordering::Acquire) {
                        drop(st);
                        done += uio.copy_out(&[TIOCPKT_DATA])?;
                        st = self.tty.st.lock(s);
                    }
                    let mut buf = [0u8; CHUNK * 4];
                    while uio.resid() > 0 {
                        let want = uio.resid().min(buf.len());
                        let n = self.tty.getc(&mut st, &mut buf[..want]);
                        if n == 0 {
                            break;
                        }
                        self.tty.mirror(&st);
                        drop(st);
                        done += uio.copy_out(&buf[..n])?;
                        st = self.tty.st.lock(s);
                    }
                    self.tty.mirror(&st);
                    return Ok(done);
                }
                if self.sc.finished.load(Ordering::Acquire) {
                    return Ok(0);
                }
                if nonblock {
                    return Err(Errno::EAGAIN);
                }
            }
            let _w = sched::wait_message("ptsout");
            crate::signal::sleep(
                s,
                &self.sc.outwait,
                || {
                    self.readable() > 0
                        || (self.sc.pkt_mode.load(Ordering::Acquire)
                            && self.sc.pkt.load(Ordering::Acquire) != 0)
                        || self.sc.finished.load(Ordering::Acquire)
                },
                None,
            )?;
        }
    }

    /// `ptsdev_write`: what is written is typed at the terminal, a chunk
    /// at a time, waiting while its input queue is full.
    pub(crate) fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        if uio.resid() == 0 {
            return Ok(0);
        }
        let mut buf = [0u8; CHUNK];
        let mut done = 0;
        let mut signals: Vec<u32> = Vec::new();
        let result = 'chunks: loop {
            let n = match uio.copy_in(&mut buf) {
                Ok(n) => n,
                Err(e) => break Err(e),
            };
            let mut at = 0;
            loop {
                {
                    let mut st = self.tty.st.lock(s);
                    let taken = st.d.input_bulk(&buf[at..n], &mut |sig| signals.push(sig));
                    at += taken;
                    self.tty.received.fetch_add(taken as u64, Ordering::Release);
                    if at == n {
                        self.tty.rint_done(&mut st);
                        self.tty.mirror(&st);
                        break;
                    }
                    if self.sc.finished.load(Ordering::Acquire) {
                        self.tty.rint_done(&mut st);
                        self.tty.mirror(&st);
                        break 'chunks Err(Errno::EIO);
                    }
                    // Wake the slave's readers, then wait for room.
                    self.tty.rint_done(&mut st);
                    self.tty.mirror(&st);
                }
                if nonblock {
                    break 'chunks Err(Errno::EAGAIN);
                }
                let _w = sched::wait_message("ptsin");
                if let Err(e) = crate::signal::sleep(
                    s,
                    &self.sc.inwait,
                    || {
                        self.tty.in_room.load(Ordering::Acquire) > 0
                            || self.sc.finished.load(Ordering::Acquire)
                    },
                    None,
                ) {
                    break 'chunks Err(e);
                }
            }
            done += at;
            if uio.resid() == 0 {
                break Ok(());
            }
        };
        for sig in signals {
            self.tty.signal_foreground(sig);
        }
        // Only what reached the terminal counts.
        match result {
            Err(e) if done == 0 => Err(e),
            _ => Ok(done),
        }
    }

    /// `ptsdev_ioctl`, for a file with flags `fflag`.
    pub(crate) fn ioctl(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &mut [u8],
        fflag: u32,
    ) -> Result<usize, Errno> {
        let tty = &self.tty;
        if matches!(req, TIOCNOTTY | TIOCSCTTY | TIOCSPGRP) {
            return tty.ioctl_proctree(s, req, arg, None).map(|()| 0);
        }
        let mut req = req;
        match req {
            FIODTYPE => {
                put_int(arg, vfs::D_TTY.cast_signed());
                return Ok(0);
            }
            FIONBIO | FIOASYNC => return Ok(0),
            FIONREAD => {
                put_int(arg, i32::try_from(self.readable()).unwrap_or(i32::MAX));
                return Ok(0);
            }
            FIODGNAME => {
                fiodgname(arg, tty.name().as_bytes())?;
                return Ok(0);
            }
            // Draining or flushing on the master's behalf could wait for
            // itself.
            TIOCSETAF | TIOCSETAW => req = TIOCSETA,
            // Without the controlling-terminal check of the slave's.
            TIOCGPGRP => {
                let pgrp = tty.ctty.lock().pgrp;
                put_int(arg, if pgrp == 0 { NO_PID } else { pgrp });
                return Ok(0);
            }
            TIOCGSID => {
                let sid = tty.ctty.lock().sid;
                if sid == 0 {
                    return Err(Errno::ENOTTY);
                }
                put_int(arg, sid);
                return Ok(0);
            }
            TIOCPTMASTER => return Ok(0),
            TIOCSIG => {
                let sig = get_int(arg);
                // FreeBSD's `NSIG`: the old signals only.
                if !(1..32).contains(&sig) {
                    return Err(Errno::EINVAL);
                }
                let mut st = tty.st.lock(s);
                st.d.signaled();
                tty.mirror(&st);
                drop(st);
                tty.signal_foreground(sig.cast_unsigned());
                return Ok(0);
            }
            TIOCPKT => {
                self.sc.pkt_mode.store(get_int(arg) != 0, Ordering::Release);
                return Ok(0);
            }
            _ => {}
        }
        tty.tty_ioctl(s, req, arg, fflag).map(|()| 0)
    }

    /// `ptsdev_poll`.
    pub(crate) fn poll(&self, events: i16, w: Option<&Arc<dyn event::Wake>>) -> i16 {
        use abi::event::{POLLHUP, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM};
        if self.sc.finished.load(Ordering::Acquire) {
            return (events & (POLLIN | POLLRDNORM)) | POLLHUP;
        }
        if let Some(w) = w {
            self.sc.notify.record(w);
        }
        let mut revents = 0;
        if events & (POLLIN | POLLRDNORM) != 0
            && (self.readable() > 0
                || (self.sc.pkt_mode.load(Ordering::Acquire)
                    && self.sc.pkt.load(Ordering::Acquire) != 0))
        {
            revents |= events & (POLLIN | POLLRDNORM);
        }
        if events & (POLLOUT | POLLWRNORM) != 0 && self.tty.in_room.load(Ordering::Acquire) > 0 {
            revents |= events & (POLLOUT | POLLWRNORM);
        }
        revents
    }

    /// `ptsdev_kqfilter`.
    pub(crate) fn kqfilter(
        self: &Arc<Self>,
        kev: &abi::event::Kevent,
    ) -> Result<Box<dyn event::Filter>, Errno> {
        match kev.filter {
            abi::event::EVFILT_READ => Ok(Box::new(MasterFilter(Arc::clone(self), false))),
            abi::event::EVFILT_WRITE => Ok(Box::new(MasterFilter(Arc::clone(self), true))),
            _ => Err(Errno::EINVAL),
        }
    }

    /// `ptsdev_stat`: the slave's number, times, owner, and mode.
    pub(crate) fn stat(&self) -> Stat {
        let name = self.tty.name().as_bytes();
        let rdev = crate::fs::devfs::rdev_of(name).unwrap_or(abi::kinfo::NODEV);
        let a = crate::fs::devfs::attr_of(name).unwrap_or_default();
        Stat {
            st_ino: rdev,
            st_rdev: rdev,
            st_atim: a.atime,
            st_mtim: a.mtime,
            st_ctim: a.ctime,
            st_uid: a.uid,
            st_gid: a.gid,
            st_mode: a.mode | S_IFCHR,
            ..Stat::default()
        }
    }
}

/// The master's `EVFILT_READ` (`false`) and `EVFILT_WRITE`.
struct MasterFilter(Arc<Master>, bool);

impl event::Filter for MasterFilter {
    fn attach(&self, kn: &Arc<event::Knote>) -> Result<(), Errno> {
        self.0.sc.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &event::Knote) {
        self.0.sc.notify.remove(kn);
    }

    fn event(&self, _kn: &event::Knote, note: &mut event::Note, _hint: u64) -> bool {
        let m = &self.0;
        if m.sc.finished.load(Ordering::Acquire) {
            note.kev.flags |= abi::event::EV_EOF;
            return true;
        }
        let n = if self.1 {
            m.tty.in_room.load(Ordering::Acquire)
        } else {
            m.readable()
        };
        note.kev.data = i64::try_from(n).unwrap_or(0);
        n > 0
    }

    fn fork(&self, _kn: &event::Knote) -> Option<Box<dyn event::Filter>> {
        Some(Box::new(Self(Arc::clone(&self.0), self.1)))
    }
}

/// `FIODGNAME`: copies `name` and its NUL to the caller's buffer, if it
/// fits (`struct fiodgname_arg`: an `int` length, a pointer).
pub(crate) fn fiodgname(arg: &[u8], name: &[u8]) -> Result<(), Errno> {
    let len = usize::try_from(get_int(arg)).unwrap_or(0);
    let buf = arg
        .get(8..16)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u64::from_ne_bytes);
    if name.len() + 1 > len {
        return Err(Errno::EINVAL);
    }
    let mut out = Vec::with_capacity(name.len() + 1);
    out.extend_from_slice(name);
    out.push(0);
    crate::uaccess::copy_out(buf, &out).map_err(|_| Errno::EFAULT)
}
