// SPDX-License-Identifier: BSD-2-Clause
//! Semaphores: `struct _usem2` (`UMTX_OP_SEM2_WAIT`, `UMTX_OP_SEM2_WAKE`),
//! which `sem_init` and `sem_open` build on, and COMPAT_FREEBSD10's
//! `struct _usem` (`UMTX_OP_SEM_WAIT`, `UMTX_OP_SEM_WAKE`).
//!
//! The count's top bit says threads wait, so `sem_post` knows to come
//! here; the last waiter's wakeup clears it.
//!
//! Behavior reference: sys/kern/kern_umtx.c (`do_sem2_wait`,
//! `do_sem2_wake`, `do_sem_wait`, `do_sem_wake`).

use abi::errno::Errno;
use abi::time::Timespec;
use abi::umtx::offset::{SEM_COUNT, SEM_FLAGS, SEM_HAS_WAITERS, SEM2_FLAGS};
use abi::umtx::*;

use crate::{Env, Q, Share, Thread, Timeout, Type, Umtx, deadline};

impl<T: Thread> Umtx<T> {
    /// Waits for the semaphore at `sem` to be posted (the caller then
    /// tries to take it). A relative timeout a signal cuts short is not
    /// restarted, and `timeout`'s time becomes what was left of it.
    pub fn sem2_wait<E: Env<T = T>>(
        &self,
        env: &E,
        sem: u64,
        timeout: Option<&mut Timeout>,
    ) -> Result<(), Errno> {
        let me = env.me();
        // FreeBSD reads the flags with `fuword32`, so a fault reads as
        // all ones.
        let flags = env.load32(sem.wrapping_add(SEM2_FLAGS)).unwrap_or(u32::MAX);
        let deadline = timeout.as_deref().map(|t| deadline(env, t)).transpose()?;
        /// What the count says.
        enum Seen {
            /// Zero, with the waiters bit set: sleep.
            Wait,
            /// Posted: go take it.
            Posted,
            /// It changed under us: look again.
            Changed,
        }
        let key = loop {
            let key = env.key(sem, Type::Sem, Share::of(flags))?;
            let mut g = self.busy(env, &key, self.lock(&key));
            g.insert(&key, Q::Shared, me);
            drop(g);
            let seen = (|| {
                let count = env.load32(sem)?;
                if count & USEM_MAX_COUNT != 0 {
                    return Ok(Seen::Posted);
                }
                if count == USEM_HAS_WAITERS || env.cas32(sem, 0, USEM_HAS_WAITERS)? == 0 {
                    return Ok(Seen::Wait);
                }
                Ok(Seen::Changed)
            })();
            match seen {
                Ok(Seen::Wait) => break key,
                seen => {
                    let mut g = self.lock(&key);
                    Self::unbusy(&mut g);
                    g.remove(&key, Q::Shared, me);
                    drop(g);
                    match seen {
                        Err(crate::Fault) => return Err(Errno::EFAULT),
                        Ok(Seen::Posted) => return Ok(()),
                        _ => env.check_susp(true)?,
                    }
                }
            }
        };
        Self::unbusy(&mut self.lock(&key));
        let mut error = Self::sleep(env, deadline.as_ref());
        let mut g = self.lock(&key);
        if !me.is_queued() {
            return Ok(());
        }
        g.remove(&key, Q::Shared, me);
        drop(g);
        if let (Some(t), Some(d)) = (timeout, deadline.as_ref())
            && !t.absolute
        {
            if error == Err(Errno::ERESTART) {
                error = Err(Errno::EINTR);
            }
            if error == Err(Errno::EINTR) {
                let now = env.now(d.clock)?;
                t.time = remaining(&d.end, &now);
            }
        }
        error
    }

    /// Wakes one waiter on the semaphore at `sem`, clearing the waiters
    /// bit if it was the last.
    pub fn sem2_wake<E: Env<T = T>>(&self, env: &E, sem: u64) -> Result<(), Errno> {
        let flags = env.load32(sem.wrapping_add(SEM2_FLAGS))?;
        let key = env.key(sem, Type::Sem, Share::of(flags))?;
        let mut g = self.busy(env, &key, self.lock(&key));
        let count = g.count(&key);
        let mut r = Ok(());
        if count > 0 {
            if count == 1 {
                drop(g);
                r = (|| {
                    let mut c = env.load32(sem)?;
                    while c & USEM_HAS_WAITERS != 0 {
                        let old = env.cas32(sem, c, c & !USEM_HAS_WAITERS)?;
                        if old == c {
                            break;
                        }
                        c = old;
                        env.check_susp(false)?;
                    }
                    Ok(())
                })();
                g = self.lock(&key);
            }
            g.signal(&key, 1, Q::Shared);
        }
        Self::unbusy(&mut g);
        r
    }

    /// COMPAT_FREEBSD10's semaphore wait (`do_sem_wait`).
    pub fn sem_wait<E: Env<T = T>>(
        &self,
        env: &E,
        sem: u64,
        timeout: Option<&Timeout>,
    ) -> Result<(), Errno> {
        let me = env.me();
        let flags = env.load32(sem.wrapping_add(SEM_FLAGS))?;
        let key = env.key(sem, Type::Sem, Share::of(flags))?;
        let deadline = timeout.map(|t| deadline(env, t)).transpose()?;
        let mut g = self.busy(env, &key, self.lock(&key));
        g.insert(&key, Q::Shared, me);
        drop(g);
        // Set the waiters flag, then look at the count.
        let seen = env
            .cas32(sem.wrapping_add(SEM_HAS_WAITERS), 0, 1)
            .and_then(|old| Ok((old, env.load32(sem.wrapping_add(SEM_COUNT))?)));
        match seen {
            Ok((_, 0)) => {}
            seen => {
                if let Ok((0, _)) = seen {
                    let _ = env.store32(sem.wrapping_add(SEM_HAS_WAITERS), 0);
                }
                let mut g = self.lock(&key);
                Self::unbusy(&mut g);
                g.remove(&key, Q::Shared, me);
                return seen.map(|_| ()).map_err(Errno::from);
            }
        }
        Self::unbusy(&mut self.lock(&key));
        let mut error = Self::sleep(env, deadline.as_ref());
        let mut g = self.lock(&key);
        if !me.is_queued() {
            return Ok(());
        }
        g.remove(&key, Q::Shared, me);
        // A relative timeout cannot be restarted.
        if error == Err(Errno::ERESTART) && timeout.is_some_and(|t| !t.absolute) {
            error = Err(Errno::EINTR);
        }
        error
    }

    /// COMPAT_FREEBSD10's semaphore wake (`do_sem_wake`).
    pub fn sem_wake<E: Env<T = T>>(&self, env: &E, sem: u64) -> Result<(), Errno> {
        let flags = env.load32(sem.wrapping_add(SEM_FLAGS))?;
        let key = env.key(sem, Type::Sem, Share::of(flags))?;
        let mut g = self.busy(env, &key, self.lock(&key));
        let count = g.count(&key);
        let mut r = Ok(());
        if count > 0 {
            // Waiters mean the memory is still in use: the flag may be
            // cleared.
            if count == 1 {
                drop(g);
                r = env
                    .store32(sem.wrapping_add(SEM_HAS_WAITERS), 0)
                    .map_err(Errno::from);
                g = self.lock(&key);
            }
            g.signal(&key, 1, Q::Shared);
        }
        Self::unbusy(&mut g);
        r
    }
}

/// `end - now`, or zero once passed.
fn remaining(end: &Timespec, now: &Timespec) -> Timespec {
    let mut sec = end.tv_sec - now.tv_sec;
    let mut nsec = end.tv_nsec - now.tv_nsec;
    if nsec < 0 {
        nsec += 1_000_000_000;
        sec -= 1;
    }
    if sec < 0 {
        Timespec::default()
    } else {
        Timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    }
}
