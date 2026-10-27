// SPDX-License-Identifier: BSD-2-Clause
//! Reader-writer locks (`struct urwlock`): `UMTX_OP_RW_RDLOCK`,
//! `UMTX_OP_RW_WRLOCK`, `UMTX_OP_RW_UNLOCK`.
//!
//! The state word holds the reader count and three bits: a writer owns
//! it, writers wait, readers wait. Readers wait on a key's shared queue
//! and writers on its exclusive one; `rw_blocked_readers` and
//! `rw_blocked_writers` count them, under the busy chain, so the last
//! leaving clears its waiters bit. Writers go first unless the lock or
//! the caller prefers readers.
//!
//! Behavior reference: sys/kern/kern_umtx.c (`do_rw_rdlock`,
//! `do_rw_wrlock`, `do_rw_unlock`).

use abi::errno::Errno;
use abi::umtx::offset::{RW_BLOCKED_READERS, RW_BLOCKED_WRITERS, RW_FLAGS};
use abi::umtx::*;

use crate::{ALL, Env, Q, Share, Thread, Timeout, Type, Umtx, deadline};

const fn readers(state: u32) -> u32 {
    state & URWLOCK_MAX_READERS
}

impl<T: Thread> Umtx<T> {
    /// Takes the lock at `rw` for reading; `fflag` may ask for reader
    /// preference.
    pub fn rw_rdlock<E: Env<T = T>>(
        &self,
        env: &E,
        rw: u64,
        fflag: u64,
        timeout: Option<&Timeout>,
    ) -> Result<(), Errno> {
        let me = env.me();
        let flags = env.load32(rw.wrapping_add(RW_FLAGS))?;
        let key = env.key(rw, Type::Rwlock, Share::of(flags))?;
        let deadline = timeout.map(|t| deadline(env, t)).transpose()?;
        let prefer = u64::from(URWLOCK_PREFER_READER);
        let wrflags = if fflag & prefer == 0 && flags & URWLOCK_PREFER_READER == 0 {
            URWLOCK_WRITE_OWNER | URWLOCK_WRITE_WAITERS
        } else {
            URWLOCK_WRITE_OWNER
        };
        let mut error = Ok(());
        loop {
            let mut state = env.load32(rw)?;
            // Try to take it.
            while state & wrflags == 0 {
                if readers(state) == URWLOCK_MAX_READERS {
                    return Err(Errno::EAGAIN);
                }
                let old = env.cas32(rw, state, state + 1)?;
                if old == state {
                    return Ok(());
                }
                error = env.check_susp(true);
                if error.is_err() {
                    break;
                }
                state = old;
            }
            error.map_err(retry_as_intr)?;

            self.busy_unlocked(env, &key);
            // It may have changed since.
            let mut r = env.load32(rw).map(|s| state = s).map_err(Errno::from);
            // Set the readers-waiting bit.
            let mut set = false;
            while r.is_ok() && state & wrflags != 0 && state & URWLOCK_READ_WAITERS == 0 {
                match env.cas32(rw, state, state | URWLOCK_READ_WAITERS) {
                    Err(f) => r = Err(f.into()),
                    Ok(o) if o == state => {
                        set = true;
                        break;
                    }
                    Ok(o) => {
                        state = o;
                        r = env.check_susp(false);
                    }
                }
            }
            if let Err(e) = r {
                self.unbusy_unlocked(&key);
                return Err(retry_as_intr(e));
            }
            // Free again while we set the bit: start over.
            if !set && state & wrflags == 0 {
                self.unbusy_unlocked(&key);
                env.check_susp(true).map_err(retry_as_intr)?;
                continue;
            }

            // Count ourselves among the blocked readers.
            if let Err(f) = add(env, rw.wrapping_add(RW_BLOCKED_READERS), 1) {
                self.unbusy_unlocked(&key);
                return Err(f.into());
            }
            while state & wrflags != 0 {
                let mut g = self.lock(&key);
                g.insert(&key, Q::Shared, me);
                Self::unbusy(&mut g);
                drop(g);
                error = Self::sleep(env, deadline.as_ref());
                let mut g = self.busy(env, &key, self.lock(&key));
                g.remove(&key, Q::Shared, me);
                drop(g);
                if error.is_err() {
                    break;
                }
                match env.load32(rw) {
                    Ok(s) => state = s,
                    Err(f) => {
                        error = Err(f.into());
                        break;
                    }
                }
            }
            // Uncount ourselves; the last clears the bit.
            let blocked = match add(env, rw.wrapping_add(RW_BLOCKED_READERS), -1) {
                Ok(b) => b,
                Err(f) => {
                    self.unbusy_unlocked(&key);
                    return Err(f.into());
                }
            };
            if blocked == 1
                && let Err(e) = self.clear_bit(env, rw, URWLOCK_READ_WAITERS, &mut error)
            {
                self.unbusy_unlocked(&key);
                return Err(e);
            }
            self.unbusy_unlocked(&key);
            error.map_err(retry_as_intr)?;
        }
    }

    /// Takes the lock at `rw` for writing.
    pub fn rw_wrlock<E: Env<T = T>>(
        &self,
        env: &E,
        rw: u64,
        timeout: Option<&Timeout>,
    ) -> Result<(), Errno> {
        let me = env.me();
        let flags = env.load32(rw.wrapping_add(RW_FLAGS))?;
        let key = env.key(rw, Type::Rwlock, Share::of(flags))?;
        let deadline = timeout.map(|t| deadline(env, t)).transpose()?;
        let mut blocked_readers = 0;
        let mut error = Ok(());
        loop {
            let mut state = env.load32(rw)?;
            while state & URWLOCK_WRITE_OWNER == 0 && readers(state) == 0 {
                let old = env.cas32(rw, state, state | URWLOCK_WRITE_OWNER)?;
                if old == state {
                    return Ok(());
                }
                state = old;
                error = env.check_susp(true);
                if error.is_err() {
                    break;
                }
            }
            if let Err(e) = error {
                // Readers we kept waiting while we were a waiting writer
                // may go now.
                if state & (URWLOCK_WRITE_OWNER | URWLOCK_WRITE_WAITERS) == 0
                    && blocked_readers != 0
                {
                    let mut g = self.busy(env, &key, self.lock(&key));
                    g.signal(&key, ALL, Q::Shared);
                    Self::unbusy(&mut g);
                }
                return Err(retry_as_intr(e));
            }

            self.busy_unlocked(env, &key);
            let mut r = env.load32(rw).map(|s| state = s).map_err(Errno::from);
            let held = |s: u32| s & URWLOCK_WRITE_OWNER != 0 || readers(s) != 0;
            let mut set = false;
            while r.is_ok() && held(state) && state & URWLOCK_WRITE_WAITERS == 0 {
                match env.cas32(rw, state, state | URWLOCK_WRITE_WAITERS) {
                    Err(f) => r = Err(f.into()),
                    Ok(o) if o == state => {
                        set = true;
                        break;
                    }
                    Ok(o) => {
                        state = o;
                        r = env.check_susp(false);
                    }
                }
            }
            if let Err(e) = r {
                self.unbusy_unlocked(&key);
                return Err(retry_as_intr(e));
            }
            if !set && !held(state) {
                self.unbusy_unlocked(&key);
                env.check_susp(false).map_err(retry_as_intr)?;
                continue;
            }

            if let Err(f) = add(env, rw.wrapping_add(RW_BLOCKED_WRITERS), 1) {
                self.unbusy_unlocked(&key);
                return Err(f.into());
            }
            while held(state) {
                let mut g = self.lock(&key);
                g.insert(&key, Q::Exclusive, me);
                Self::unbusy(&mut g);
                drop(g);
                error = Self::sleep(env, deadline.as_ref());
                let mut g = self.busy(env, &key, self.lock(&key));
                g.remove(&key, Q::Exclusive, me);
                drop(g);
                if error.is_err() {
                    break;
                }
                match env.load32(rw) {
                    Ok(s) => state = s,
                    Err(f) => {
                        error = Err(f.into());
                        break;
                    }
                }
            }
            let blocked = match add(env, rw.wrapping_add(RW_BLOCKED_WRITERS), -1) {
                Ok(b) => b,
                Err(f) => {
                    self.unbusy_unlocked(&key);
                    return Err(f.into());
                }
            };
            if blocked == 1 {
                // Leaving the bit set on a stop does no harm.
                if let Err(e) = self.clear_bit(env, rw, URWLOCK_WRITE_WAITERS, &mut error) {
                    self.unbusy_unlocked(&key);
                    return Err(e);
                }
                match env.load32(rw.wrapping_add(RW_BLOCKED_READERS)) {
                    Ok(b) => blocked_readers = b,
                    Err(f) => {
                        self.unbusy_unlocked(&key);
                        return Err(f.into());
                    }
                }
            } else {
                blocked_readers = 0;
            }
            self.unbusy_unlocked(&key);
            // A signal or timeout breaks the loop at its top, after one
            // more try.
        }
    }

    /// Clears `bit` in the state word; a stop ends the attempt, kept in
    /// `error` unless it holds one already. `Err` only for a fault.
    fn clear_bit<E: Env<T = T>>(
        &self,
        env: &E,
        rw: u64,
        bit: u32,
        error: &mut Result<(), Errno>,
    ) -> Result<(), Errno> {
        let mut state = env.load32(rw)?;
        loop {
            let old = env.cas32(rw, state, state & !bit)?;
            if old == state {
                return Ok(());
            }
            state = old;
            if let Err(e) = env.check_susp(false) {
                if error.is_ok() {
                    *error = Err(e);
                }
                return Ok(());
            }
        }
    }

    /// Releases the lock at `rw` (a writer's or one reader's), waking a
    /// writer or every reader as the preference says.
    pub fn rw_unlock<E: Env<T = T>>(&self, env: &E, rw: u64) -> Result<(), Errno> {
        let flags = env.load32(rw.wrapping_add(RW_FLAGS))?;
        let key = env.key(rw, Type::Rwlock, Share::of(flags))?;
        let mut state = env.load32(rw)?;
        if state & URWLOCK_WRITE_OWNER != 0 {
            loop {
                let old = env.cas32(rw, state, state & !URWLOCK_WRITE_OWNER)?;
                if old == state {
                    break;
                }
                state = old;
                if old & URWLOCK_WRITE_OWNER == 0 {
                    return Err(Errno::EPERM);
                }
                env.check_susp(true)?;
            }
        } else if readers(state) != 0 {
            loop {
                let old = env.cas32(rw, state, state - 1)?;
                if old == state {
                    break;
                }
                state = old;
                if readers(old) == 0 {
                    return Err(Errno::EPERM);
                }
                env.check_susp(true)?;
            }
        } else {
            return Err(Errno::EPERM);
        }
        let writer = (state & URWLOCK_WRITE_WAITERS != 0).then_some((1, Q::Exclusive));
        let reader = (state & URWLOCK_READ_WAITERS != 0).then_some((ALL, Q::Shared));
        let wake = if flags & URWLOCK_PREFER_READER == 0 {
            writer.or(reader)
        } else {
            reader.or(writer)
        };
        if let Some((n, q)) = wake {
            let mut g = self.busy(env, &key, self.lock(&key));
            g.signal(&key, n, q);
            Self::unbusy(&mut g);
        }
        Ok(())
    }
}

/// Adds `d` to the counter at `addr`; returns its value before.
fn add<E: Env>(env: &E, addr: u64, d: i32) -> Result<u32, crate::Fault> {
    let v = env.load32(addr)?;
    env.store32(addr, v.wrapping_add_signed(d))?;
    Ok(v)
}

/// The rwlock operations never restart.
fn retry_as_intr(e: Errno) -> Errno {
    if e == Errno::ERESTART {
        Errno::EINTR
    } else {
        e
    }
}
