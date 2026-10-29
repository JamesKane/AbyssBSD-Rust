// SPDX-License-Identifier: BSD-2-Clause
//! `eventfd`: a 64-bit counter a descriptor reads and writes
//! (`__specialfd(SPECIALFD_EVENTFD)`). Writing adds; reading takes the
//! count and leaves 0 (or, with `EFD_SEMAPHORE`, takes 1). Reading waits
//! while it is 0, and writing while the sum would reach `u64::MAX`.
//!
//! Behavior reference: sys/kern/sys_eventfd.c.

use alloc::boxed::Box;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::event::{
    EFD_SEMAPHORE, EVFILT_READ, EVFILT_WRITE, Kevent, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM,
};
use abi::stat::{S_IFIFO, Stat};
use event::{Filter, Knote, Note, Notify, Wake};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vfs::Uio;

/// An eventfd's counter.
pub(crate) struct Eventfd {
    count: SpinLock<u64>,
    flags: u32,
    wait: WaitQueue,
    notify: Notify,
}

impl Eventfd {
    pub(crate) fn new(initval: u32, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            count: SpinLock::new(u64::from(initval)),
            flags,
            wait: WaitQueue::new(),
            notify: Notify::new(),
        })
    }

    fn count(&self) -> u64 {
        *self.count.lock()
    }

    /// Its counter and flags, for process information
    /// (`eventfd_fill_kinfo`).
    pub(crate) fn kinfo(&self) -> (u64, u32) {
        (self.count(), self.flags)
    }

    /// The counter changed (`eventfd_wakeup`).
    fn wakeup(&self) {
        self.notify.notify(0);
        self.wait.wake_all();
    }

    pub(crate) fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        if uio.resid() < 8 {
            return Err(Errno::EINVAL);
        }
        let n = loop {
            {
                let mut c = self.count.lock();
                if *c > 0 {
                    let n = if self.flags & EFD_SEMAPHORE != 0 {
                        1
                    } else {
                        *c
                    };
                    *c -= n;
                    break n;
                }
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _w = sched::wait_message("efdrd");
            crate::signal::sleep(s, &self.wait, || *self.count.lock() > 0, None)?;
        };
        self.wakeup();
        uio.copy_out(&n.to_ne_bytes())
    }

    pub(crate) fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        if uio.resid() < 8 {
            return Err(Errno::EINVAL);
        }
        let mut b = [0; 8];
        let n = uio.copy_in(&mut b)?;
        if n < 8 {
            return Err(Errno::EFAULT);
        }
        let add = u64::from_ne_bytes(b);
        if add == u64::MAX {
            return Err(Errno::EINVAL);
        }
        loop {
            {
                let mut c = self.count.lock();
                if u64::MAX - *c > add {
                    *c += add;
                    break;
                }
            }
            if nonblock {
                // Not counted as written.
                return Err(Errno::EAGAIN);
            }
            let _w = sched::wait_message("efdwr");
            crate::signal::sleep(s, &self.wait, || u64::MAX - *self.count.lock() > add, None)?;
        }
        self.wakeup();
        Ok(8)
    }

    /// `eventfd_poll`.
    pub(crate) fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if let Some(w) = w {
            self.notify.record(w);
        }
        let c = self.count();
        let mut revents = 0;
        if events & (POLLIN | POLLRDNORM) != 0 && c > 0 {
            revents |= events & (POLLIN | POLLRDNORM);
        }
        if events & (POLLOUT | POLLWRNORM) != 0 && u64::MAX - 1 > c {
            revents |= events & (POLLOUT | POLLWRNORM);
        }
        revents
    }

    /// `eventfd_kqfilter`.
    pub(crate) fn kqfilter(self: &Arc<Self>, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        match kev.filter {
            EVFILT_READ => Ok(Box::new(EventfdFilter(Arc::clone(self), false))),
            EVFILT_WRITE => Ok(Box::new(EventfdFilter(Arc::clone(self), true))),
            _ => Err(Errno::EINVAL),
        }
    }

    /// `eventfd_stat`: a FIFO, and nothing else.
    pub(crate) fn stat() -> Stat {
        Stat {
            st_mode: S_IFIFO,
            ..Stat::default()
        }
    }
}

/// `eventfd_rfiltops` (`false`) and `eventfd_wfiltops` (`true`).
struct EventfdFilter(Arc<Eventfd>, bool);

impl Filter for EventfdFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.0.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.0.notify.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let c = self.0.count();
        if self.1 {
            note.kev.data = (u64::MAX - 1 - c).cast_signed();
            u64::MAX - 1 > c
        } else {
            note.kev.data = c.cast_signed();
            c > 0
        }
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self(Arc::clone(&self.0), self.1)))
    }
}
