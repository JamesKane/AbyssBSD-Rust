// SPDX-License-Identifier: BSD-2-Clause
//! Waiting on a word and waking its waiters (`UMTX_OP_WAIT`,
//! `UMTX_OP_WAIT_UINT`, their private forms, `UMTX_OP_WAKE`,
//! `UMTX_OP_NWAKE_PRIVATE`).
//!
//! Behavior reference: sys/kern/kern_umtx.c (`do_wait`,
//! `kern_umtx_wake`).

use abi::errno::Errno;

use crate::{ALL, Env, Q, Share, Thread, Timeout, Type, Umtx, deadline};

/// How wide the word is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// `long` (`UMTX_OP_WAIT`).
    Long,
    /// `u_int` (`UMTX_OP_WAIT_UINT`).
    Uint,
}

impl<T: Thread> Umtx<T> {
    /// Sleeps while the word at `addr` holds `id`, until woken or
    /// `timeout`. A signal ends the wait with `EINTR`, never a restart.
    pub fn wait<E: Env<T = T>>(
        &self,
        env: &E,
        addr: u64,
        id: u64,
        timeout: Option<&Timeout>,
        width: Width,
        private: bool,
    ) -> Result<(), Errno> {
        let me = env.me();
        let key = env.key(
            addr,
            Type::SimpleWait,
            if private { Share::Thread } else { Share::Auto },
        )?;
        let deadline = timeout.map(|t| deadline(env, t)).transpose()?;
        self.lock(&key).insert(&key, Q::Shared, me);
        // Queued first: a wake after this read finds us.
        let value = match width {
            Width::Long => env.load64(addr),
            Width::Uint => env.load32(addr).map(u64::from),
        };
        let r = match value {
            Ok(v) if v == id => Self::sleep(env, deadline.as_ref()),
            Ok(_) => Ok(()),
            Err(f) => Err(f.into()),
        };
        let mut g = self.lock(&key);
        let r = if me.is_queued() {
            g.remove(&key, Q::Shared, me);
            r
        } else if value.is_ok() {
            // Woken, whatever else happened.
            Ok(())
        } else {
            r
        };
        drop(g);
        match r {
            Err(Errno::ERESTART) => Err(Errno::EINTR),
            r => r,
        }
    }

    /// Wakes up to `n` threads waiting on the word at `addr`.
    pub fn wake<E: Env<T = T>>(
        &self,
        env: &E,
        addr: u64,
        n: usize,
        private: bool,
    ) -> Result<(), Errno> {
        let key = env.key(
            addr,
            Type::SimpleWait,
            if private { Share::Thread } else { Share::Auto },
        )?;
        self.lock(&key).signal(&key, n, Q::Shared);
        Ok(())
    }

    /// Wakes every thread waiting on each of `addrs` (private words).
    pub fn wake_all_private<E: Env<T = T>>(&self, env: &E, addrs: &[u64]) {
        for &a in addrs {
            // A word no key can be made for has no waiters.
            let _ = self.wake(env, a, ALL, true);
        }
    }
}
