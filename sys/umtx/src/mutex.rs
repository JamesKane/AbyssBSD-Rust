// SPDX-License-Identifier: BSD-2-Clause
//! POSIX mutexes (`struct umutex`): the normal protocol, priority
//! protection, and the operations around them; priority inheritance is in
//! [`crate::pi`]. Also COMPAT_FREEBSD10's `struct umtx` lock.
//!
//! The owner word holds the owner's tid and the contested bit, which tells
//! an unlocking thread to come here. Robust mutexes add two markers: the
//! owner died (`UMUTEX_RB_OWNERDEAD`, taken with `EOWNERDEAD`), and it was
//! never made consistent (`UMUTEX_RB_NOTRECOV`, `ENOTRECOVERABLE`).
//!
//! Behavior reference: sys/kern/kern_umtx.c (`do_lock_normal`,
//! `do_unlock_normal`, `do_wake_umutex`, `do_wake2_umutex`,
//! `do_lock_pp`, `do_unlock_pp`, `do_set_ceiling`, `do_lock_umutex`,
//! `do_unlock_umutex`, `do_lock_umtx`, `do_unlock_umtx`).

use abi::errno::Errno;
use abi::umtx::offset::{M_CEILINGS, M_FLAGS};
use abi::umtx::*;

use crate::{
    ALL, Deadline, Env, PRI_MAX, PRI_MIN_REALTIME, Q, Share, Thread, Timeout, Type, Umtx, deadline,
};

/// What a lock operation does when the mutex is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Waits for it (`UMTX_OP_MUTEX_LOCK`).
    Lock,
    /// Fails with `EBUSY` (`UMTX_OP_MUTEX_TRYLOCK`).
    Try,
    /// Waits for it to be released without taking it
    /// (`UMTX_OP_MUTEX_WAIT`).
    Wait,
}

/// The word an unlock leaves (`umtx_unlock_val`): the owner died (`rb`,
/// from robust-list cleanup), never made consistent, or unowned.
pub(crate) const fn unlock_val(flags: u32, rb: bool) -> u32 {
    if rb {
        UMUTEX_RB_OWNERDEAD
    } else if flags & UMUTEX_NONCONSISTENT != 0 {
        UMUTEX_RB_NOTRECOV
    } else {
        UMUTEX_UNOWNED
    }
}

const fn pp_type(flags: u32) -> Type {
    if flags & UMUTEX_ROBUST != 0 {
        Type::PpRobustUmutex
    } else {
        Type::PpUmutex
    }
}

/// The priority a ceiling (`RTP_PRIO_MAX` the most urgent) stands for.
fn ceiling_pri(ceiling: u32) -> Result<u8, Errno> {
    let c = u32::from(abi::thr::RTP_PRIO_MAX)
        .checked_sub(ceiling)
        .ok_or(Errno::EINVAL)?;
    #[allow(clippy::cast_possible_truncation, reason = "at most RTP_PRIO_MAX")]
    Ok(PRI_MIN_REALTIME + c as u8)
}

impl<T: Thread> Umtx<T> {
    /// `do_lock_normal`.
    fn lock_normal<E: Env<T = T>>(
        &self,
        env: &E,
        m: u64,
        flags: u32,
        deadline: Option<&Deadline>,
        mode: Mode,
    ) -> Result<(), Errno> {
        let me = env.me();
        let id = me.tid;
        let mut error = Ok(());
        loop {
            let mut owner = env.load32(m)?;
            if mode == Mode::Wait {
                if matches!(
                    owner,
                    UMUTEX_UNOWNED | UMUTEX_CONTESTED | UMUTEX_RB_OWNERDEAD | UMUTEX_RB_NOTRECOV
                ) {
                    return Ok(());
                }
            } else {
                // A robust mutex whose owner died: the caller takes it,
                // told so, and user code marks it inconsistent.
                if owner == UMUTEX_RB_OWNERDEAD {
                    if env.cas32(m, owner, id | UMUTEX_CONTESTED)? == owner {
                        return Err(Errno::EOWNERDEAD);
                    }
                    env.check_susp(false)?;
                    continue;
                }
                if owner == UMUTEX_RB_NOTRECOV {
                    return Err(Errno::ENOTRECOVERABLE);
                }
                // The uncontested case, which user code should have done.
                owner = env.cas32(m, UMUTEX_UNOWNED, id)?;
                if owner == UMUTEX_UNOWNED {
                    return Ok(());
                }
                // Nobody owns it, but it is contested: take it.
                if owner == UMUTEX_CONTESTED {
                    if env.cas32(m, owner, id | UMUTEX_CONTESTED)? == owner {
                        return Ok(());
                    }
                    env.check_susp(false)?;
                    continue;
                }
                env.check_susp(false)?;
            }
            if mode == Mode::Try {
                return Err(Errno::EBUSY);
            }
            // A signal or timeout came during the last sleep, and we have
            // tried once more.
            error?;
            let key = env.key(m, Type::NormalUmutex, Share::of(flags))?;
            let mut g = self.busy(env, &key, self.lock(&key));
            g.insert(&key, Q::Shared, me);
            drop(g);
            // Set the contested bit, so the owner's unlock comes here; if
            // the word changed, someone took or released it: start again.
            let old = env.cas32(m, owner, owner | UMUTEX_CONTESTED);
            if old != Ok(owner) {
                let mut g = self.lock(&key);
                g.remove(&key, Q::Shared, me);
                Self::unbusy(&mut g);
                drop(g);
                old?;
                env.check_susp(false)?;
                continue;
            }
            Self::unbusy(&mut self.lock(&key));
            error = Self::sleep(env, deadline);
            self.lock(&key).remove(&key, Q::Shared, me);
            if error.is_ok() {
                error = env.check_susp(false);
            }
        }
    }

    /// `do_unlock_normal`.
    fn unlock_normal<E: Env<T = T>>(
        &self,
        env: &E,
        m: u64,
        flags: u32,
        rb: bool,
    ) -> Result<(), Errno> {
        let id = env.me().tid;
        loop {
            let owner = env.load32(m)?;
            if owner & !UMUTEX_CONTESTED != id {
                return Err(Errno::EPERM);
            }
            let mut new = unlock_val(flags, rb);
            if owner & UMUTEX_CONTESTED == 0 {
                if env.cas32(m, owner, new)? == owner {
                    return Ok(());
                }
                env.check_susp(false)?;
                continue;
            }
            let key = env.key(m, Type::NormalUmutex, Share::of(flags))?;
            let mut g = self.busy(env, &key, self.lock(&key));
            let count = g.count(&key);
            drop(g);
            // Unowned with one waiter or none, so the next lock stays in
            // user space; contested with more.
            if count > 1 {
                new |= UMUTEX_CONTESTED;
            }
            let old = env.cas32(m, owner, new);
            let mut g = self.lock(&key);
            g.signal(&key, 1, Q::Shared);
            Self::unbusy(&mut g);
            drop(g);
            if old? == owner {
                return Ok(());
            }
            return Err(Errno::EINVAL);
        }
    }

    /// Wakes a waiter if a normal mutex is free (`UMTX_OP_MUTEX_WAKE`,
    /// `do_wake_umutex`), clearing the contested bit when it was the last.
    pub fn wake_umutex<E: Env<T = T>>(&self, env: &E, m: u64) -> Result<(), Errno> {
        loop {
            let owner = env.load32(m)?;
            let dead = owner == UMUTEX_RB_OWNERDEAD || owner == UMUTEX_RB_NOTRECOV;
            if owner & !UMUTEX_CONTESTED != 0 && !dead {
                return Ok(());
            }
            let flags = env.load32(m.wrapping_add(M_FLAGS))?;
            let key = env.key(m, Type::NormalUmutex, Share::of(flags))?;
            let mut g = self.busy(env, &key, self.lock(&key));
            let count = g.count(&key);
            drop(g);
            let mut r = Ok(());
            let mut owner = owner;
            if count <= 1 && !dead {
                match env.cas32(m, UMUTEX_CONTESTED, UMUTEX_UNOWNED) {
                    Err(f) => r = Err(f.into()),
                    Ok(o) if o != UMUTEX_CONTESTED => {
                        self.unbusy_unlocked(&key);
                        env.check_susp(false)?;
                        continue;
                    }
                    Ok(o) => owner = o,
                }
            }
            let mut g = self.lock(&key);
            if r.is_ok()
                && count != 0
                && (owner & !UMUTEX_CONTESTED == 0
                    || owner == UMUTEX_RB_OWNERDEAD
                    || owner == UMUTEX_RB_NOTRECOV)
            {
                g.signal(&key, 1, Q::Shared);
            }
            Self::unbusy(&mut g);
            return r;
        }
    }

    /// Repairs the contested bit after an unlock in user space and wakes
    /// a waiter (`UMTX_OP_MUTEX_WAKE2`, `do_wake2_umutex`).
    pub fn wake2_umutex<E: Env<T = T>>(&self, env: &E, m: u64, flags: u32) -> Result<(), Errno> {
        let ty = match flags & (UMUTEX_PRIO_INHERIT | UMUTEX_PRIO_PROTECT | UMUTEX_ROBUST) {
            0 | UMUTEX_ROBUST => Type::NormalUmutex,
            UMUTEX_PRIO_INHERIT => Type::PiUmutex,
            x if x == UMUTEX_PRIO_INHERIT | UMUTEX_ROBUST => Type::PiRobustUmutex,
            UMUTEX_PRIO_PROTECT => Type::PpUmutex,
            x if x == UMUTEX_PRIO_PROTECT | UMUTEX_ROBUST => Type::PpRobustUmutex,
            _ => return Err(Errno::EINVAL),
        };
        let key = env.key(m, ty, Share::of(flags))?;
        let mut g = self.busy(env, &key, self.lock(&key));
        let count = g.count(&key);
        drop(g);
        let mut owner = 0;
        let mut r = env.load32(m).map(|o| owner = o).map_err(Errno::from);
        // Only with waiters, so the mutex is still in use: otherwise
        // leave its memory alone.
        while r.is_ok()
            && owner & UMUTEX_CONTESTED == 0
            && (count > 1 || (count == 1 && owner & !UMUTEX_CONTESTED != 0))
        {
            match env.cas32(m, owner, owner | UMUTEX_CONTESTED) {
                Err(f) => r = Err(f.into()),
                Ok(o) if o == owner => break,
                Ok(o) => {
                    owner = o;
                    r = env.check_susp(false);
                }
            }
        }
        let mut g = self.lock(&key);
        match r {
            Err(Errno::EFAULT) => {
                g.signal(&key, ALL, Q::Shared);
            }
            _ if count != 0
                && (owner & !UMUTEX_CONTESTED == 0
                    || owner == UMUTEX_RB_OWNERDEAD
                    || owner == UMUTEX_RB_NOTRECOV) =>
            {
                g.signal(&key, 1, Q::Shared);
            }
            _ => {}
        }
        Self::unbusy(&mut g);
        r
    }

    /// `do_lock_pp`: takes a priority-protection mutex, raising the
    /// caller to its ceiling first (if it may have real-time priority).
    fn lock_pp<E: Env<T = T>>(
        &self,
        env: &E,
        m: u64,
        flags: u32,
        deadline: Option<&Deadline>,
        try_only: bool,
    ) -> Result<(), Errno> {
        let me = env.me();
        let id = me.tid;
        let key = env.key(m, pp_type(flags), Share::of(flags))?;
        let su = env.may_rtprio();
        let mut error = Ok(());
        let mut old_inherited;
        loop {
            old_inherited = self.inherited(me);
            self.busy_unlocked(env, &key);
            let ceiling = match env.load32(m.wrapping_add(M_CEILINGS)) {
                Ok(c) => c,
                Err(f) => {
                    self.unbusy_unlocked(&key);
                    return Err(f.into());
                }
            };
            let new_pri = match ceiling_pri(ceiling) {
                Ok(p) if me.thread.base_user_pri() >= p => p,
                _ => {
                    self.unbusy_unlocked(&key);
                    return Err(Errno::EINVAL);
                }
            };
            if su {
                self.inherit(me, new_pri);
            }
            let owner = match env.cas32(m, UMUTEX_CONTESTED, id | UMUTEX_CONTESTED) {
                Ok(o) => o,
                Err(f) => {
                    error = Err(f.into());
                    break;
                }
            };
            if owner == UMUTEX_CONTESTED {
                error = Ok(());
                break;
            }
            if owner == UMUTEX_RB_OWNERDEAD {
                match env.cas32(m, owner, id | UMUTEX_CONTESTED) {
                    Err(f) => {
                        error = Err(f.into());
                        break;
                    }
                    Ok(o) if o == owner => {
                        error = Err(Errno::EOWNERDEAD);
                        break;
                    }
                    Ok(_) => {
                        // Only check for a stop if no signal came yet; a
                        // stop found here the sleep below sees too.
                        if error.is_ok() {
                            if env.check_susp(false).is_ok() && !try_only {
                                self.unbusy_unlocked(&key);
                                continue;
                            }
                            error = Ok(());
                        }
                    }
                }
            } else if owner == UMUTEX_RB_NOTRECOV {
                error = Err(Errno::ENOTRECOVERABLE);
            } else if owner == UMUTEX_CONTESTED {
                self.unbusy_unlocked(&key);
                continue;
            }
            if try_only {
                error = Err(Errno::EBUSY);
            }
            // A signal or timeout came during the last sleep, and we have
            // tried once more.
            if error.is_err() {
                break;
            }
            let mut g = self.lock(&key);
            g.insert(&key, Q::Shared, me);
            Self::unbusy(&mut g);
            drop(g);
            error = Self::sleep(env, deadline);
            self.lock(&key).remove(&key, Q::Shared, me);
            self.set_inherited(me, old_inherited);
            self.relend(me);
        }
        if let Err(e) = error
            && e != Errno::EOWNERDEAD
        {
            self.set_inherited(me, old_inherited);
            self.relend(me);
        }
        self.unbusy_unlocked(&key);
        error
    }

    /// `do_unlock_pp`: releases a priority-protection mutex, leaving it
    /// contested so the next lock comes here, and drops the caller to the
    /// ceiling user code says it keeps (`m_ceilings[1]`).
    fn unlock_pp<E: Env<T = T>>(&self, env: &E, m: u64, flags: u32, rb: bool) -> Result<(), Errno> {
        let me = env.me();
        let id = me.tid;
        let su = env.may_rtprio();
        let owner = env.load32(m)?;
        if owner & !UMUTEX_CONTESTED != id {
            return Err(Errno::EPERM);
        }
        let rceiling = env.load32(m.wrapping_add(M_CEILINGS + 4))?;
        let new_inherited = if rceiling == u32::MAX {
            PRI_MAX
        } else {
            ceiling_pri(rceiling)?
        };
        let key = env.key(m, pp_type(flags), Share::of(flags))?;
        self.busy_unlocked(env, &key);
        let r = env.store32(m, unlock_val(flags, rb) | UMUTEX_CONTESTED);
        let mut g = self.lock(&key);
        if r.is_ok() {
            g.signal(&key, 1, Q::Shared);
        }
        Self::unbusy(&mut g);
        drop(g);
        r?;
        if su || new_inherited == PRI_MAX {
            self.set_inherited(me, new_inherited);
        }
        self.relend(me);
        Ok(())
    }

    /// Sets a priority-protection mutex's ceiling, taking the mutex for
    /// the change if nobody holds it (`UMTX_OP_SET_CEILING`). Returns the
    /// old ceiling.
    pub fn set_ceiling<E: Env<T = T>>(&self, env: &E, m: u64, ceiling: u32) -> Result<u32, Errno> {
        let me = env.me();
        let id = me.tid;
        let flags = env.load32(m.wrapping_add(M_FLAGS))?;
        if flags & UMUTEX_PRIO_PROTECT == 0 || ceiling > u32::from(abi::thr::RTP_PRIO_MAX) {
            return Err(Errno::EINVAL);
        }
        let key = env.key(m, pp_type(flags), Share::of(flags))?;
        let mut saved = 0;
        let mut error = Ok(());
        let r = loop {
            self.busy_unlocked(env, &key);
            saved = match env.load32(m.wrapping_add(M_CEILINGS)) {
                Ok(c) => c,
                Err(f) => break Err(f.into()),
            };
            let owner = match env.cas32(m, UMUTEX_CONTESTED, id | UMUTEX_CONTESTED) {
                Ok(o) => o,
                Err(f) => break Err(f.into()),
            };
            if owner == UMUTEX_CONTESTED {
                let a = env.store32(m.wrapping_add(M_CEILINGS), ceiling);
                let b = env.store32(m, UMUTEX_CONTESTED);
                break a.and(b).map_err(Errno::from);
            }
            if owner & !UMUTEX_CONTESTED == id {
                break env
                    .store32(m.wrapping_add(M_CEILINGS), ceiling)
                    .map_err(Errno::from);
            }
            if owner == UMUTEX_RB_OWNERDEAD {
                break Err(Errno::EOWNERDEAD);
            }
            if owner == UMUTEX_RB_NOTRECOV {
                break Err(Errno::ENOTRECOVERABLE);
            }
            // A signal came during the last sleep, and we have tried once
            // more.
            if error.is_err() {
                break error;
            }
            // Held: wait for its unlock.
            let mut g = self.lock(&key);
            g.insert(&key, Q::Shared, me);
            Self::unbusy(&mut g);
            drop(g);
            error = Self::sleep(env, None);
            self.lock(&key).remove(&key, Q::Shared, me);
        };
        let mut g = self.lock(&key);
        if r.is_ok() {
            g.signal(&key, ALL, Q::Shared);
        }
        Self::unbusy(&mut g);
        drop(g);
        r.map(|()| saved)
    }

    /// Locks a mutex, waits for it, or tries it (`do_lock_umutex`): a
    /// lock without a timeout that a signal interrupts restarts, a timed
    /// one does not.
    pub fn lock_umutex<E: Env<T = T>>(
        &self,
        env: &E,
        m: u64,
        timeout: Option<&Timeout>,
        mode: Mode,
    ) -> Result<(), Errno> {
        let flags = env.load32(m.wrapping_add(M_FLAGS))?;
        let deadline = timeout.map(|t| deadline(env, t)).transpose()?;
        let d = deadline.as_ref();
        let r = match flags & (UMUTEX_PRIO_INHERIT | UMUTEX_PRIO_PROTECT) {
            0 => self.lock_normal(env, m, flags, d, mode),
            UMUTEX_PRIO_INHERIT => self.lock_pi(env, m, flags, d, mode == Mode::Try),
            UMUTEX_PRIO_PROTECT => self.lock_pp(env, m, flags, d, mode == Mode::Try),
            _ => return Err(Errno::EINVAL),
        };
        match (r, timeout) {
            (Err(Errno::EINTR), None) if mode != Mode::Wait => Err(Errno::ERESTART),
            (Err(Errno::ERESTART), Some(_)) => Err(Errno::EINTR),
            (r, _) => r,
        }
    }

    /// Unlocks a mutex (`do_unlock_umutex`); `rb` from robust-list
    /// cleanup, for a dead owner.
    pub fn unlock_umutex<E: Env<T = T>>(&self, env: &E, m: u64, rb: bool) -> Result<(), Errno> {
        let flags = env.load32(m.wrapping_add(M_FLAGS))?;
        match flags & (UMUTEX_PRIO_INHERIT | UMUTEX_PRIO_PROTECT) {
            0 => self.unlock_normal(env, m, flags, rb),
            UMUTEX_PRIO_INHERIT => self.unlock_pi(env, m, flags, rb),
            UMUTEX_PRIO_PROTECT => self.unlock_pp(env, m, flags, rb),
            _ => Err(Errno::EINVAL),
        }
    }

    /// COMPAT_FREEBSD10's `_umtx_lock` (`UMTX_OP_LOCK`, `do_lock_umtx`):
    /// a `long` lock word owned by `id`, the timeout relative to the real
    /// time clock.
    pub fn lock_umtx<E: Env<T = T>>(
        &self,
        env: &E,
        u: u64,
        id: u64,
        timeout: Option<&Timeout>,
    ) -> Result<(), Errno> {
        let me = env.me();
        let deadline = timeout.map(|t| deadline(env, t)).transpose()?;
        let mut error = Ok(());
        loop {
            let owner = env.cas64(u, UMTX_UNOWNED, id)?;
            if owner == UMTX_UNOWNED {
                return Ok(());
            }
            if owner == UMTX_CONTESTED {
                if env.cas64(u, owner, id | UMTX_CONTESTED)? == owner {
                    return Ok(());
                }
                error = env.check_susp(false);
                if error.is_err() {
                    break;
                }
                continue;
            }
            if error.is_err() {
                break;
            }
            let key = env.key(u, Type::SimpleLock, Share::Auto)?;
            let mut g = self.busy(env, &key, self.lock(&key));
            g.insert(&key, Q::Shared, me);
            Self::unbusy(&mut g);
            drop(g);
            let old = env.cas64(u, owner, owner | UMTX_CONTESTED);
            let Ok(old) = old else {
                self.lock(&key).remove(&key, Q::Shared, me);
                return Err(Errno::EFAULT);
            };
            if old == owner {
                error = Self::sleep(env, deadline.as_ref());
            }
            self.lock(&key).remove(&key, Q::Shared, me);
            if error.is_ok() {
                error = env.check_susp(false);
            }
        }
        match (error, timeout) {
            (Err(Errno::EINTR), None) => Err(Errno::ERESTART),
            (Err(Errno::ERESTART), Some(_)) => Err(Errno::EINTR),
            (r, _) => r,
        }
    }

    /// COMPAT_FREEBSD10's `_umtx_unlock` (`UMTX_OP_UNLOCK`,
    /// `do_unlock_umtx`).
    pub fn unlock_umtx<E: Env<T = T>>(&self, env: &E, u: u64, id: u64) -> Result<(), Errno> {
        let mut owner = env.load64(u)?;
        if owner & !UMTX_CONTESTED != id {
            return Err(Errno::EPERM);
        }
        if owner & UMTX_CONTESTED == 0 {
            let old = env.cas64(u, owner, UMTX_UNOWNED)?;
            if old == owner {
                return Ok(());
            }
            owner = old;
        }
        let key = env.key(u, Type::SimpleLock, Share::Auto)?;
        let mut g = self.busy(env, &key, self.lock(&key));
        let count = g.count(&key);
        drop(g);
        let old = env.cas64(
            u,
            owner,
            if count <= 1 {
                UMTX_UNOWNED
            } else {
                UMTX_CONTESTED
            },
        );
        let mut g = self.lock(&key);
        g.signal(&key, 1, Q::Shared);
        Self::unbusy(&mut g);
        drop(g);
        if old? != owner {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }
}
