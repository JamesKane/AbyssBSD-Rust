// SPDX-License-Identifier: BSD-2-Clause
//! Priority-inheritance mutexes (`UMUTEX_PRIO_INHERIT`).
//!
//! A contested mutex has a [`Pi`]: its owner, and the threads blocked on
//! it in priority order. A blocked thread lends its priority to the owner,
//! and on along the chain of owners blocked on further mutexes; releasing
//! or giving up recomputes what each owner still needs. All of it is under
//! [`Umtx::pi_lock`]; the per-object locks only make that state reachable
//! from shared references and are never contended.
//!
//! Behavior reference: sys/kern/kern_umtx.c (`umtx_pi_*`,
//! `umtx_propagate_priority`, `umtxq_sleep_pi`, `do_lock_pi`,
//! `do_unlock_pi`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::umtx::*;
use sync::PreemptLock;

use crate::{
    Deadline, Env, Guard, Key, PRI_MAX, Q, Share, Thread, Type, Umtx, Waiter, mutex::unlock_val,
    upri,
};

/// A thread's share of priority-inheritance state.
pub(crate) struct WaiterPi<T: Thread> {
    /// The mutex it sleeps on (`uq_pi_blocked`).
    blocked: Option<Arc<Pi<T>>>,
    /// Contested mutexes it owns (`uq_pi_contested`).
    contested: Vec<Arc<Pi<T>>>,
    /// The ceiling a priority-protection mutex it holds gives it
    /// (`uq_inherited_pri`).
    pub(crate) inherited_pri: u8,
    /// What umtx lends it now (`td_lend_user_pri`).
    lent: u8,
}

impl<T: Thread> WaiterPi<T> {
    pub(crate) const fn new() -> Self {
        Self {
            blocked: None,
            contested: Vec::new(),
            inherited_pri: PRI_MAX,
            lent: PRI_MAX,
        }
    }
}

/// A contested priority-inheritance mutex (`struct umtx_pi`).
pub(crate) struct Pi<T: Thread> {
    key: Key,
    /// Operations using it (under the chain lock).
    refs: AtomicUsize,
    inner: PreemptLock<PiInner<T>>,
}

struct PiInner<T: Thread> {
    owner: Option<Arc<Waiter<T>>>,
    /// Best priority first; equal priorities in arrival order.
    blocked: Vec<Arc<Waiter<T>>>,
}

fn same<T: Thread>(a: &Waiter<T>, b: &Waiter<T>) -> bool {
    core::ptr::eq(a, b)
}

/// Lends `w` `pri` (`sched_lend_user_prio`).
fn lend<T: Thread>(w: &Waiter<T>, pri: u8) {
    w.pi.lock().lent = pri;
    w.thread.lend_user_pri(pri);
}

/// The best priority among the first waiters of the mutexes `w` owns.
fn contested_best<T: Thread>(w: &Waiter<T>) -> u8 {
    let contested = w.pi.lock().contested.clone();
    contested
        .iter()
        .filter_map(|pi| pi.inner.lock().blocked.first().map(|t| upri(t)))
        .min()
        .unwrap_or(PRI_MAX)
}

/// Where `w` goes in `blocked`: after every thread at least as urgent.
fn position<T: Thread>(blocked: &[Arc<Waiter<T>>], pri: u8) -> usize {
    blocked
        .iter()
        .position(|t| upri(t) > pri)
        .unwrap_or(blocked.len())
}

impl<T: Thread> Pi<T> {
    fn new(key: Key) -> Self {
        Self {
            key,
            refs: AtomicUsize::new(0),
            inner: PreemptLock::new(PiInner {
                owner: None,
                blocked: Vec::new(),
            }),
        }
    }

    /// Re-sorts `w` after its priority changed (`umtx_pi_adjust_thread`).
    fn adjust_thread(&self, w: &Waiter<T>) {
        let mut inner = self.inner.lock();
        let Some(i) = inner.blocked.iter().position(|t| same(t, w)) else {
            return;
        };
        let pri = upri(w);
        let before = i > 0 && pri < upri(&inner.blocked[i - 1]);
        let after = i + 1 < inner.blocked.len() && pri > upri(&inner.blocked[i + 1]);
        if before || after {
            let t = inner.blocked.remove(i);
            let at = position(&inner.blocked, pri);
            inner.blocked.insert(at, t);
        }
    }

    /// The mutex this one's owner is blocked on (`umtx_pi_next`).
    fn next(&self) -> Option<Arc<Pi<T>>> {
        let owner = self.inner.lock().owner.clone()?;
        owner.pi.lock().blocked.clone()
    }

    fn set_owner(self: &Arc<Self>, owner: &Arc<Waiter<T>>) {
        let mut inner = self.inner.lock();
        debug_assert!(inner.owner.is_none());
        inner.owner = Some(owner.clone());
        owner.pi.lock().contested.push(self.clone());
    }

    fn disown(&self) {
        let Some(owner) = self.inner.lock().owner.take() else {
            return;
        };
        owner
            .pi
            .lock()
            .contested
            .retain(|p| !core::ptr::eq(&**p, self));
    }

    fn is_owner(&self, w: &Waiter<T>) -> bool {
        self.inner
            .lock()
            .owner
            .as_deref()
            .is_some_and(|o| same(o, w))
    }
}

/// Whether following owners from `pi` comes back around (Floyd's cycle
/// finding, `umtx_pi_check_loop`).
fn check_loop<T: Thread>(pi: &Arc<Pi<T>>) -> bool {
    let mut slow = pi.clone();
    let mut fast = pi.clone();
    loop {
        let Some(s) = slow.next() else {
            return false;
        };
        slow = s;
        let Some(f) = fast.next() else {
            return false;
        };
        let Some(f) = f.next() else {
            return false;
        };
        fast = f;
        if Arc::ptr_eq(&slow, &fast) {
            return true;
        }
    }
}

impl<T: Thread> Umtx<T> {
    /// Lends `w`'s priority along the chain of owners from the mutex it
    /// is blocked on (`umtx_propagate_priority`).
    fn propagate(&self, me: &Waiter<T>, w: &Waiter<T>) {
        let pri = upri(w);
        let Some(mut pi) = w.pi.lock().blocked.clone() else {
            return;
        };
        if check_loop(&pi) {
            return;
        }
        loop {
            let Some(owner) = pi.inner.lock().owner.clone() else {
                return;
            };
            if same(&owner, me) {
                return;
            }
            if owner.pi.lock().lent > pri {
                lend(&owner, pri);
            } else {
                break;
            }
            let Some(next) = owner.pi.lock().blocked.clone() else {
                break;
            };
            pi = next;
            pi.adjust_thread(&owner);
        }
    }

    /// Recomputes the priorities lent along the chain from `pi` after a
    /// waiter left it (`umtx_repropagate_priority`).
    fn repropagate(&self, pi: &Arc<Pi<T>>) {
        if check_loop(pi) {
            return;
        }
        let mut pi = Some(pi.clone());
        while let Some(p) = pi {
            let Some(owner) = p.inner.lock().owner.clone() else {
                break;
            };
            let inherited = owner.pi.lock().inherited_pri;
            lend(&owner, contested_best(&owner).min(inherited));
            pi = owner.pi.lock().blocked.clone();
            if let Some(next) = &pi {
                next.adjust_thread(&owner);
            }
        }
    }

    /// Records `owner` as the owner of `pi`, which it just took
    /// (`umtx_pi_claim`): `EPERM` if another thread is recorded.
    fn claim(&self, pi: &Arc<Pi<T>>, owner: &Arc<Waiter<T>>) -> Result<(), Errno> {
        let _pi = self.pi_lock.lock();
        if pi.is_owner(owner) {
            return Ok(());
        }
        if pi.inner.lock().owner.is_some() {
            // User code has messed the mutex up.
            return Err(Errno::EPERM);
        }
        pi.set_owner(owner);
        let first = pi.inner.lock().blocked.first().map(|t| upri(t));
        if let Some(pri) = first
            && pri < upri(owner)
        {
            lend(owner, pri);
        }
        Ok(())
    }

    /// Lends `me` what its contested mutexes' waiters and its
    /// priority-protection ceiling need, as `do_lock_pp` and
    /// `do_unlock_pp` do.
    pub(crate) fn relend(&self, me: &Waiter<T>) {
        let _pi = self.pi_lock.lock();
        let inherited = me.pi.lock().inherited_pri;
        lend(me, contested_best(me).min(inherited));
    }

    /// The thread's priority-protection ceiling (`uq_inherited_pri`).
    pub(crate) fn inherited(&self, me: &Waiter<T>) -> u8 {
        let _pi = self.pi_lock.lock();
        me.pi.lock().inherited_pri
    }

    /// Sets it, and lends it if it beats the thread's priority.
    pub(crate) fn inherit(&self, me: &Waiter<T>, pri: u8) {
        let _pi = self.pi_lock.lock();
        let mut p = me.pi.lock();
        if pri < p.inherited_pri {
            p.inherited_pri = pri;
            drop(p);
            if pri < upri(me) {
                lend(me, pri);
            }
        }
    }

    /// Sets the thread's ceiling outright (`do_unlock_pp`, cleanup).
    pub(crate) fn set_inherited(&self, me: &Waiter<T>, pri: u8) {
        let _pi = self.pi_lock.lock();
        me.pi.lock().inherited_pri = pri;
    }

    /// Finds or makes `key`'s [`Pi`], with a reference for the caller.
    fn pi_get(&self, key: &Key) -> Arc<Pi<T>> {
        let mut g = self.lock(key);
        let pi = match g.pis.iter().find(|p| p.key.matches(key)) {
            Some(p) => p.clone(),
            None => {
                let p = Arc::new(Pi::new(key.clone()));
                g.pis.push(p.clone());
                p
            }
        };
        pi.refs.fetch_add(1, Ordering::Relaxed);
        pi
    }

    /// Drops the caller's reference; the last frees it (`umtx_pi_unref`).
    fn pi_put(&self, pi: &Arc<Pi<T>>) {
        let mut g = self.lock(&pi.key);
        if pi.refs.fetch_sub(1, Ordering::Relaxed) == 1 {
            {
                let _pi = self.pi_lock.lock();
                pi.disown();
                debug_assert!(pi.inner.lock().blocked.is_empty());
            }
            g.pis.retain(|p| !Arc::ptr_eq(p, pi));
        }
    }

    /// Sleeps on `pi`, owned by thread `owner`, lending it our priority
    /// (`umtxq_sleep_pi`). `g` is the busy chain's lock.
    #[allow(clippy::too_many_arguments)]
    fn sleep_pi<E: Env<T = T>>(
        &self,
        env: &E,
        mut g: Guard<'_, T>,
        key: &Key,
        pi: &Arc<Pi<T>>,
        owner: u32,
        deadline: Option<&Deadline>,
        shared: bool,
    ) -> Result<(), Errno> {
        let me = env.me();
        g.insert(key, Q::Shared, me);
        {
            let mut held = self.pi_lock.lock();
            if pi.inner.lock().owner.is_none() {
                drop(held);
                let found = env.find(owner, shared);
                held = self.pi_lock.lock();
                if let Some(t) = found
                    && pi.inner.lock().owner.is_none()
                {
                    pi.set_owner(&t);
                }
            }
            {
                let mut inner = pi.inner.lock();
                let at = position(&inner.blocked, upri(me));
                inner.blocked.insert(at, me.clone());
            }
            me.pi.lock().blocked = Some(pi.clone());
            self.propagate(me, me);
            drop(held);
        }
        Self::unbusy(&mut g);
        drop(g);

        let r = Self::sleep(env, deadline);

        let mut g = self.lock(key);
        g.remove(key, Q::Shared, me);
        {
            let _pi = self.pi_lock.lock();
            me.pi.lock().blocked = None;
            pi.inner.lock().blocked.retain(|t| !same(t, me));
            self.repropagate(pi);
        }
        drop(g);
        r
    }

    /// Gives up `pi` (if `me` owns it) on unlock and wakes its most urgent
    /// waiter still asleep (`umtx_pi_drop`). Returns how many wait.
    fn pi_drop(
        &self,
        g: &mut Guard<'_, T>,
        key: &Key,
        me: &Waiter<T>,
        rb: bool,
    ) -> Result<usize, Errno> {
        let (first, count) = g.first(key);
        if let Some(first) = first {
            let next = {
                let _pi = self.pi_lock.lock();
                let pi = first.pi.lock().blocked.clone();
                let Some(pi) = pi else {
                    return Err(Errno::EPERM);
                };
                let owned = pi.is_owner(me);
                if !owned && !(rb && pi.inner.lock().owner.is_none()) {
                    // User code has messed the mutex up.
                    return Err(Errno::EPERM);
                }
                if owned {
                    pi.disown();
                }
                let next = pi
                    .inner
                    .lock()
                    .blocked
                    .iter()
                    .find(|t| t.is_queued())
                    .cloned();
                lend(me, contested_best(me));
                next
            };
            if let Some(t) = next {
                g.signal_thread(key, &t);
            }
        } else if let Some(pi) = g.pis.iter().find(|p| p.key.matches(key)).cloned() {
            // A waiter a signal or timeout took off the queue may still be
            // in `lock_pi` with the `Pi`.
            let _pi = self.pi_lock.lock();
            if pi.is_owner(me) {
                pi.disown();
            }
        }
        Ok(count)
    }

    /// `do_lock_pi`.
    pub(crate) fn lock_pi<E: Env<T = T>>(
        &self,
        env: &E,
        m: u64,
        flags: u32,
        deadline: Option<&Deadline>,
        try_only: bool,
    ) -> Result<(), Errno> {
        let me = env.me();
        let id = me.tid;
        let key = env.key(m, pi_type(flags), Share::of(flags))?;
        let pi = self.pi_get(&key);
        let mut error = Ok(());
        let result = loop {
            let owner = match env.cas32(m, UMUTEX_UNOWNED, id) {
                Ok(o) => o,
                Err(f) => break Err(f.into()),
            };
            if owner == UMUTEX_UNOWNED {
                break Ok(());
            }
            if owner == UMUTEX_RB_NOTRECOV {
                break Err(Errno::ENOTRECOVERABLE);
            }
            // Keep an error from the sleep rather than the check's.
            if error.is_ok()
                && let Err(e) = env.check_susp(true)
            {
                break Err(e);
            }
            // Nobody owns it, but it is contested: take it.
            if owner == UMUTEX_CONTESTED || owner == UMUTEX_RB_OWNERDEAD {
                match env.cas32(m, owner, id | UMUTEX_CONTESTED) {
                    Err(f) => break Err(f.into()),
                    Ok(o) if o != owner => {
                        if error.is_ok()
                            && let Err(e) = env.check_susp(true)
                        {
                            break Err(e);
                        }
                        continue;
                    }
                    Ok(_) => {}
                }
                let mut g = self.busy(env, &key, self.lock(&key));
                let r = self.claim(&pi, me);
                Self::unbusy(&mut g);
                drop(g);
                if r.is_err() {
                    // Put the word back as it was rather than compound the
                    // damage.
                    let _ = env.cas32(m, id | UMUTEX_CONTESTED, owner);
                }
                break match r {
                    Ok(()) if owner == UMUTEX_RB_OWNERDEAD => Err(Errno::EOWNERDEAD),
                    r => r,
                };
            }
            if owner & !UMUTEX_CONTESTED == id {
                break Err(Errno::EDEADLK);
            }
            if try_only {
                break Err(Errno::EBUSY);
            }
            // A signal or timeout came during the last sleep, and we have
            // tried once more.
            if error.is_err() {
                break error;
            }
            self.busy_unlocked(env, &key);
            // Set the contested bit, so the owner's unlock comes here.
            match env.cas32(m, owner, owner | UMUTEX_CONTESTED) {
                Err(f) => {
                    self.unbusy_unlocked(&key);
                    break Err(f.into());
                }
                Ok(o) if o != owner => {
                    self.unbusy_unlocked(&key);
                    if let Err(e) = env.check_susp(true) {
                        break Err(e);
                    }
                    continue;
                }
                Ok(_) => {}
            }
            let g = self.lock(&key);
            error = self.sleep_pi(
                env,
                g,
                &key,
                &pi,
                owner & !UMUTEX_CONTESTED,
                deadline,
                flags & USYNC_PROCESS_SHARED != 0,
            );
            if error.is_err() {
                continue;
            }
            if let Err(e) = env.check_susp(false) {
                break Err(e);
            }
        };
        self.pi_put(&pi);
        result
    }

    /// `do_unlock_pi`.
    pub(crate) fn unlock_pi<E: Env<T = T>>(
        &self,
        env: &E,
        m: u64,
        flags: u32,
        rb: bool,
    ) -> Result<(), Errno> {
        let me = env.me();
        let id = me.tid;
        let (owner, mut new_owner) = loop {
            let owner = env.load32(m)?;
            if owner & !UMUTEX_CONTESTED != id {
                return Err(Errno::EPERM);
            }
            let new_owner = unlock_val(flags, rb);
            if owner & UMUTEX_CONTESTED == 0 {
                if env.cas32(m, owner, new_owner)? == owner {
                    return Ok(());
                }
                env.check_susp(true)?;
                continue;
            }
            break (owner, new_owner);
        };
        let key = env.key(m, pi_type(flags), Share::of(flags))?;
        let mut g = self.busy(env, &key, self.lock(&key));
        let count = match self.pi_drop(&mut g, &key, me, rb) {
            Ok(n) => n,
            Err(e) => {
                Self::unbusy(&mut g);
                return Err(e);
            }
        };
        drop(g);
        // Unowned with one waiter or none, so the next lock stays in user
        // space; contested with more.
        if count > 1 {
            new_owner |= UMUTEX_CONTESTED;
        }
        let r = loop {
            match env.cas32(m, owner, new_owner) {
                Err(f) => break Err(f.into()),
                Ok(o) if o == owner => break Ok(()),
                Ok(_) => {
                    if let Err(e) = env.check_susp(false) {
                        break Err(e);
                    }
                }
            }
        };
        self.unbusy_unlocked(&key);
        r
    }

    /// Thread exit: forgets the mutexes it owned and the priority it was
    /// lent (the first half of `umtx_thread_cleanup`).
    pub(crate) fn pi_cleanup(&self, me: &Waiter<T>) {
        {
            let _pi = self.pi_lock.lock();
            let contested = {
                let mut p = me.pi.lock();
                p.inherited_pri = PRI_MAX;
                core::mem::take(&mut p.contested)
            };
            for pi in contested {
                pi.inner.lock().owner = None;
            }
        }
        if me.pi.lock().lent != PRI_MAX {
            lend(me, PRI_MAX);
        }
    }
}

const fn pi_type(flags: u32) -> Type {
    if flags & UMUTEX_ROBUST != 0 {
        Type::PiRobustUmutex
    } else {
        Type::PiUmutex
    }
}
