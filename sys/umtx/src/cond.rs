// SPDX-License-Identifier: BSD-2-Clause
//! Condition variables (`struct ucond`): `UMTX_OP_CV_WAIT`,
//! `UMTX_OP_CV_SIGNAL`, `UMTX_OP_CV_BROADCAST`.
//!
//! A waiter queues itself and sets `c_has_waiters` before it unlocks the
//! mutex, so a signal after the unlock finds it; `c_has_waiters` tells
//! user code a signal needs the kernel.
//!
//! Behavior reference: sys/kern/kern_umtx.c (`__umtx_op_cv_wait`,
//! `do_cv_wait`, `do_cv_signal`, `do_cv_broadcast`).

use abi::errno::Errno;
use abi::time::{Timespec, clock};
use abi::umtx::offset::{C_CLOCKID, C_FLAGS, C_HAS_WAITERS};
use abi::umtx::*;

use crate::{ALL, Deadline, Env, Q, Share, Thread, Timeout, Type, Umtx, deadline};

/// `UMTX_OP_CV_WAIT`'s timeout, as user code passed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CvTime {
    /// None: wait forever.
    Forever,
    /// A `struct timespec`, on the clock and in the sense the flags say.
    Timespec(Timespec),
    /// A `struct _umtx_time` (`CVWAIT_UMTX_TIME`).
    Umtx(Timeout),
}

impl<T: Thread> Umtx<T> {
    /// Unlocks the mutex at `m` and waits on the condition variable at
    /// `cv`, atomically with respect to a signal, until signalled,
    /// `time`, or a signal (`EINTR`, never a restart).
    pub fn cv_wait<E: Env<T = T>>(
        &self,
        env: &E,
        cv: u64,
        m: u64,
        wflags: u64,
        time: CvTime,
    ) -> Result<(), Errno> {
        let known =
            u64::from(CVWAIT_CHECK_UNPARKING | CVWAIT_ABSTIME | CVWAIT_CLOCKID | CVWAIT_UMTX_TIME);
        let umtx_time = wflags & u64::from(CVWAIT_UMTX_TIME) != 0;
        if wflags & !known != 0
            || (wflags & u64::from(CVWAIT_ABSTIME | CVWAIT_CLOCKID) != 0 && umtx_time)
        {
            return Err(Errno::EINVAL);
        }
        let timeout = match time {
            CvTime::Umtx(t) => Some(t),
            // `CVWAIT_UMTX_TIME` needs its time.
            _ if umtx_time => return Err(Errno::EINVAL),
            CvTime::Forever => None,
            CvTime::Timespec(ts) => Some(Timeout {
                clock: if wflags & u64::from(CVWAIT_CLOCKID) != 0 {
                    env.load32(cv.wrapping_add(C_CLOCKID))?
                } else {
                    clock::REALTIME
                },
                absolute: wflags & u64::from(CVWAIT_ABSTIME) != 0,
                time: ts,
            }),
        };
        // Only the hardware clocks.
        if let Some(t) = &timeout
            && t.clock >= clock::THREAD_CPUTIME_ID
            && t.clock != clock::TAI
        {
            return Err(Errno::EINVAL);
        }
        let deadline = timeout.map(|t| deadline(env, &t)).transpose()?;
        self.do_cv_wait(env, cv, m, deadline.as_ref())
    }

    /// `do_cv_wait`.
    fn do_cv_wait<E: Env<T = T>>(
        &self,
        env: &E,
        cv: u64,
        m: u64,
        deadline: Option<&Deadline>,
    ) -> Result<(), Errno> {
        let me = env.me();
        let flags = env.load32(cv.wrapping_add(C_FLAGS))?;
        let key = env.key(cv, Type::Cv, Share::of(flags))?;
        let mut g = self.busy(env, &key, self.lock(&key));
        g.insert(&key, Q::Shared, me);
        drop(g);
        // Set c_has_waiters before the mutex is released, writing it only
        // if it changes.
        let r = match env.load32(cv.wrapping_add(C_HAS_WAITERS)) {
            Ok(0) => env.store32(cv.wrapping_add(C_HAS_WAITERS), 1),
            Ok(_) => Ok(()),
            Err(f) => Err(f),
        };
        if r.is_err() {
            let mut g = self.lock(&key);
            g.remove(&key, Q::Shared, me);
            Self::unbusy(&mut g);
            return Err(Errno::EFAULT);
        }
        self.unbusy_unlocked(&key);
        let mut error = self.unlock_umutex(env, m, false);
        if error.is_ok() {
            error = Self::sleep(env, deadline);
        }
        let mut g = self.lock(&key);
        if !me.is_queued() {
            return Ok(());
        }
        // A timeout, a signal, or a spurious wakeup: clear c_has_waiters
        // if we were the last.
        g = self.busy(env, &key, g);
        if me.is_queued() {
            let len = g.remove(&key, Q::Shared, me);
            if len == 1 {
                drop(g);
                if env.store32(cv.wrapping_add(C_HAS_WAITERS), 0).is_err() && error.is_ok() {
                    error = Err(Errno::EFAULT);
                }
                g = self.lock(&key);
            }
        }
        Self::unbusy(&mut g);
        match error {
            Err(Errno::ERESTART) => Err(Errno::EINTR),
            e => e,
        }
    }

    /// Wakes one waiter on the condition variable at `cv`, clearing
    /// `c_has_waiters` if none is left.
    pub fn cv_signal<E: Env<T = T>>(&self, env: &E, cv: u64) -> Result<(), Errno> {
        let flags = env.load32(cv.wrapping_add(C_FLAGS))?;
        let key = env.key(cv, Type::Cv, Share::of(flags))?;
        let mut g = self.busy(env, &key, self.lock(&key));
        let count = g.count(&key);
        let woken = g.signal(&key, 1, Q::Shared);
        let mut r = Ok(());
        if count <= woken {
            drop(g);
            r = env
                .store32(cv.wrapping_add(C_HAS_WAITERS), 0)
                .map_err(Errno::from);
            g = self.lock(&key);
        }
        Self::unbusy(&mut g);
        r
    }

    /// Wakes every waiter on the condition variable at `cv`.
    pub fn cv_broadcast<E: Env<T = T>>(&self, env: &E, cv: u64) -> Result<(), Errno> {
        let flags = env.load32(cv.wrapping_add(C_FLAGS))?;
        let key = env.key(cv, Type::Cv, Share::of(flags))?;
        let mut g = self.busy(env, &key, self.lock(&key));
        g.signal(&key, ALL, Q::Shared);
        drop(g);
        let r = env
            .store32(cv.wrapping_add(C_HAS_WAITERS), 0)
            .map_err(Errno::from);
        self.unbusy_unlocked(&key);
        r
    }
}
