// SPDX-License-Identifier: BSD-2-Clause
//! Robust mutexes: when a thread exits (or its process execs) holding
//! robust mutexes, each is unlocked as `UMUTEX_RB_OWNERDEAD`, so the next
//! locker gets `EOWNERDEAD` instead of waiting forever.
//!
//! libthr keeps the mutexes a thread holds on two lists (process-shared
//! and private) linked through `m_rb_lnk`, and names the one it is in the
//! middle of locking or unlocking (`inact`); `UMTX_OP_ROBUST_LISTS`
//! registers where the three heads are.
//!
//! Behavior reference: sys/kern/kern_umtx.c (`__umtx_op_robust_lists`,
//! `umtx_handle_rb`, `umtx_cleanup_rb_list`, `umtx_thread_cleanup`).

use abi::errno::Errno;
use abi::umtx::offset::{M_FLAGS, M_OWNER, M_RB_LNK};
use abi::umtx::*;

use crate::{Env, Thread, Umtx};

/// How many mutexes a list may hold before the walk stops
/// (`kern.ipc.umtx_max_robust`).
const MAX_ROBUST: usize = 1000;

/// A thread's robust-list heads (`td_rb_list`, `td_rbp_list`,
/// `td_rb_inact`), as user addresses of the head pointers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Robust {
    pub list: u64,
    pub priv_list: u64,
    pub inact: u64,
    /// Registered by a 32-bit caller: the pointers are 32 bits wide
    /// (`TDP2_COMPAT32RB`).
    pub compat32: bool,
}

/// Reads a user pointer (`umtx_read_uptr`).
fn read_ptr<E: Env>(env: &E, at: u64, compat32: bool) -> Result<u64, Errno> {
    Ok(if compat32 {
        u64::from(env.load32(at)?)
    } else {
        env.load64(at)?
    })
}

impl<T: Thread> Umtx<T> {
    /// `UMTX_OP_ROBUST_LISTS`: registers the caller's lists. A thread that
    /// registered them with one width cannot switch to the other while
    /// they are set (`EBUSY`).
    pub fn set_robust_lists<E: Env<T = T>>(&self, env: &E, lists: Robust) -> Result<(), Errno> {
        let mut r = env.me().robust.lock();
        if lists.compat32 {
            if !r.compat32 && (r.list != 0 || r.priv_list != 0 || r.inact != 0) {
                return Err(Errno::EBUSY);
            }
        } else if r.compat32 {
            return Err(Errno::EBUSY);
        }
        *r = Robust {
            compat32: lists.compat32 || r.compat32,
            ..lists
        };
        Ok(())
    }

    /// Unlocks the robust mutex at `rbp` if the caller owns it, as owner
    /// dead (`umtx_handle_rb`); returns the next on its list.
    fn handle_rb<E: Env<T = T>>(
        &self,
        env: &E,
        rbp: u64,
        inact: bool,
        compat32: bool,
    ) -> Result<u64, Errno> {
        let owner = env.load32(rbp.wrapping_add(M_OWNER))?;
        let flags = env.load32(rbp.wrapping_add(M_FLAGS))?;
        let next = read_ptr(env, rbp.wrapping_add(M_RB_LNK), compat32)?;
        if flags & UMUTEX_ROBUST == 0 {
            return Err(Errno::EINVAL);
        }
        if owner & !UMUTEX_CONTESTED != env.me().tid {
            // `inact` is cleared after the unlock: allow the
            // inconsistency.
            return if inact { Ok(next) } else { Err(Errno::EINVAL) };
        }
        self.unlock_umutex(env, rbp, true)?;
        Ok(next)
    }

    /// Walks the list whose head is at `head` (`umtx_cleanup_rb_list`);
    /// `inact` is cleared if met on the way.
    fn cleanup_list<E: Env<T = T>>(&self, env: &E, head: u64, inact: &mut u64, compat32: bool) {
        if head == 0 {
            return;
        }
        // An error ends the walk; FreeBSD reports it on the terminal.
        let Ok(mut rbp) = read_ptr(env, head, compat32) else {
            return;
        };
        for _ in 0..MAX_ROBUST {
            if rbp == 0 {
                return;
            }
            let was_inact = rbp == *inact;
            if was_inact {
                *inact = 0;
            }
            match self.handle_rb(env, rbp, was_inact, compat32) {
                Ok(next) => rbp = next,
                Err(_) => return,
            }
        }
    }
}

/// The robust half of thread cleanup: unlocks, as owner dead, every
/// robust mutex the thread still holds. Runs after its
/// priority-inheritance mutexes are disowned, so their unlocks find them
/// unowned.
pub(crate) fn cleanup<T: Thread, E: Env<T = T>>(u: &Umtx<T>, env: &E) {
    let r = core::mem::take(&mut *env.me().robust.lock());
    if r.inact == 0 && r.list == 0 && r.priv_list == 0 {
        return;
    }
    let mut inact = r.inact;
    if inact != 0
        && let Ok(p) = read_ptr(env, inact, r.compat32)
    {
        inact = p;
    }
    u.cleanup_list(env, r.list, &mut inact, r.compat32);
    u.cleanup_list(env, r.priv_list, &mut inact, r.compat32);
    if inact != 0 {
        let _ = u.handle_rb(env, inact, true, r.compat32);
    }
}
