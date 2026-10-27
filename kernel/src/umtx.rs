// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of `sys/umtx`: each user thread's queue entry, and
//! what the protocols need from the calling thread ([`umtx::Env`]) — its
//! memory, keys, sleeping, the clocks, other threads.
//!
//! A private key is the address space (its address, unique while it
//! lives) and the address. A shared key is the memory object and the
//! offset in it, found by a write lookup of the map, and holds the object
//! while in use.
//!
//! Behavior reference: sys/kern/kern_umtx.c (`umtx_key_get`,
//! `umtxq_sleep`, `umtx_abs_timeout_getsbt`), sys/kern/kern_thread.c
//! (`thread_check_susp`).

use alloc::sync::{Arc, Weak};
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::time::Timespec;
use sched::{Sleepable, WaitQueue};
use umtx::{Deadline, Fault, Key, Share, Type, Waiter};
use vm::fault::FaultError;

use crate::mm::space::AddressSpace;
use crate::proc::{Mode, Suspension};
use crate::uaccess;
use crate::uthread::UserThread;

/// Every chain.
pub(crate) static UMTX: umtx::Umtx<Parker> = umtx::Umtx::new();

/// How umtx reaches a user thread: where it sleeps, and its scheduling.
pub(crate) struct Parker {
    thread: Weak<UserThread>,
    wait: WaitQueue,
}

impl Parker {
    pub(crate) fn new(thread: Weak<UserThread>) -> Self {
        Self {
            thread,
            wait: WaitQueue::new(),
        }
    }

    fn sched(&self) -> Option<Arc<sched::Thread>> {
        self.thread.upgrade()?.sched()
    }
}

impl umtx::Thread for Parker {
    fn unpark(&self) {
        self.wait.wake_all();
    }

    fn user_pri(&self) -> u8 {
        self.sched()
            .map_or(umtx::PRI_MAX, |t| sched::user_priority(&t).get())
    }

    fn base_user_pri(&self) -> u8 {
        self.sched()
            .map_or(umtx::PRI_MAX, |t| sched::base_priority(&t).get())
    }

    fn lend_user_pri(&self, pri: u8) {
        if let Some(t) = self.sched() {
            sched::lend_user_priority(
                &t,
                (pri != umtx::PRI_MAX).then(|| sched::Priority::new(pri)),
            );
        }
    }
}

/// A user thread's queue entry.
pub(crate) type Entry = Waiter<Parker>;

/// Thread `t`'s view, for an operation it makes (or, at exec, one made
/// for it).
pub(crate) struct Ctx<'a> {
    pub(crate) s: &'a Sleepable,
    pub(crate) t: &'a UserThread,
}

impl Ctx<'_> {
    fn space(&self) -> Arc<AddressSpace> {
        self.t.process().space()
    }

    /// `p_umtx_min_timeout`, in nanoseconds.
    fn min_timeout(&self) -> u64 {
        self.t.process().umtx_min_timeout.load(Ordering::Relaxed)
    }
}

fn ns(t: &Timespec) -> u64 {
    u64::try_from(t.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(t.tv_nsec).unwrap_or(0))
}

impl umtx::Env for Ctx<'_> {
    type T = Parker;

    fn me(&self) -> &Arc<Entry> {
        self.t.umtx()
    }

    fn load32(&self, addr: u64) -> Result<u32, Fault> {
        uaccess::load_u32(addr).map_err(|_| Fault)
    }

    fn load64(&self, addr: u64) -> Result<u64, Fault> {
        uaccess::load_u64(addr).map_err(|_| Fault)
    }

    fn store32(&self, addr: u64, v: u32) -> Result<(), Fault> {
        uaccess::store_u32(addr, v).map_err(|_| Fault)
    }

    fn cas32(&self, addr: u64, old: u32, new: u32) -> Result<u32, Fault> {
        uaccess::cas_u32(addr, old, new).map_err(|_| Fault)
    }

    fn cas64(&self, addr: u64, old: u64, new: u64) -> Result<u64, Fault> {
        uaccess::cas_u64(addr, old, new).map_err(|_| Fault)
    }

    fn key(&self, addr: u64, ty: Type, share: Share) -> Result<Key, Errno> {
        let space = self.space();
        let private = || Key::private(ty, Arc::as_ptr(&space) as usize, addr);
        if share == Share::Thread {
            return Ok(private());
        }
        if addr >= crate::mm::space::USER_END {
            return Err(Errno::EFAULT);
        }
        match space.object_at(self.s, addr::VirtAddr::new(addr), share == Share::Process) {
            Ok(Some((object, offset))) => Ok(Key {
                ty,
                id: umtx::Id::Shared {
                    object: Arc::as_ptr(&object) as usize,
                    offset,
                },
                hold: Some(object),
            }),
            Ok(None) => Ok(private()),
            Err(FaultError::OutOfMemory) => Err(Errno::ENOMEM),
            Err(_) => Err(Errno::EFAULT),
        }
    }

    fn check_susp(&self, sleep: bool) -> Result<(), Errno> {
        let p = self.t.process();
        match p.suspension(self.t) {
            Suspension::None => Ok(()),
            Suspension::Single(Mode::Exit) => Err(Errno::EINTR),
            Suspension::Stop | Suspension::Single(Mode::Boundary) if sleep => {
                p.park(self.s, self.t);
                match p.suspension(self.t) {
                    Suspension::Single(Mode::Exit) => Err(Errno::EINTR),
                    _ => Ok(()),
                }
            }
            Suspension::Stop | Suspension::Single(Mode::Boundary) => Err(Errno::ERESTART),
        }
    }

    fn sleep(&self, done: &dyn Fn() -> bool, deadline: Option<&Deadline>) -> Result<(), Errno> {
        let limit = match deadline {
            None => None,
            Some(d) => {
                let now = crate::syscall::clock_ns(d.clock)?;
                let end = ns(&d.end);
                if end <= now {
                    return Err(Errno::ETIMEDOUT);
                }
                // A process may make every wait last at least so long
                // (`umtx_abs_timeout_enforce_min`).
                Some((end - now).max(self.min_timeout()))
            }
        };
        let wait = &self.t.umtx().thread.wait;
        match crate::signal::sleep(self.s, wait, done, limit) {
            Ok(crate::signal::Woke::Done) => Ok(()),
            Ok(crate::signal::Woke::TimedOut) => Err(Errno::ETIMEDOUT),
            Err(e) => Err(e),
        }
    }

    fn park(&self, done: &dyn Fn() -> bool) {
        self.t.umtx().thread.wait.wait_until(self.s, done);
    }

    fn now(&self, clock: u32) -> Result<Timespec, Errno> {
        let ns = crate::syscall::clock_ns(clock)?;
        Ok(Timespec {
            tv_sec: (ns / 1_000_000_000).cast_signed(),
            tv_nsec: (ns % 1_000_000_000).cast_signed(),
        })
    }

    fn find(&self, tid: u32, shared: bool) -> Option<Arc<Entry>> {
        let t = crate::uthread::find(i32::try_from(tid).ok()?)?;
        if !shared && !Arc::ptr_eq(t.process(), self.t.process()) {
            return None;
        }
        Some(Arc::clone(t.umtx()))
    }

    fn may_rtprio(&self) -> bool {
        // Everyone is root until P6.7 brings credentials that are not.
        self.t.process().is_root()
    }
}

/// Thread `t` is leaving (`umtx_thread_exit`): the robust mutexes it
/// holds are released as owner dead, and the priority it was lent goes.
/// Runs in `t`'s context, its address space still loaded.
pub(crate) fn thread_exit(s: &Sleepable, t: &UserThread) {
    UMTX.thread_exit(&Ctx { s, t });
}

/// The process is replacing its image (`umtx_exec`), single-threaded:
/// the caller's thread cleans up as at exit (the others did as they
/// left), and the minimum timeout goes.
pub(crate) fn exec(s: &Sleepable, t: &UserThread) {
    thread_exit(s, t);
    t.process().umtx_min_timeout.store(0, Ordering::Relaxed);
}
