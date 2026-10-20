// SPDX-License-Identifier: BSD-2-Clause
//! Advisory locks (docs/VFS.md §12): `fcntl`'s byte-range locks and
//! `flock`'s whole-file ones, in one list per vnode, as FreeBSD keeps them
//! (so the two kinds conflict with each other).
//!
//! A POSIX lock belongs to a process: setting one over a range the process
//! already locked replaces it there, ranges of one type merge, and the
//! process's locks on a file go when it closes any descriptor for it. A
//! `flock` lock belongs to an open file, covers the whole file, and goes
//! when the file's last descriptor closes. Shared locks conflict only with
//! exclusive ones of other owners.
//!
//! A blocking request that would wait on an owner already waiting, through
//! a chain, on the requester is refused with `EDEADLK` instead.
//!
//! Behavior reference: sys/kern/kern_lockf.c.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;

/// Who holds a lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    /// A process (POSIX locks): its pid.
    Process(i32),
    /// An open file (`flock` locks): an identity the caller keeps unique
    /// while the file is open.
    File(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockType {
    Shared,
    Exclusive,
}

/// One lock: `[start, end]`, `end` inclusive (`u64::MAX`: to the end of
/// the file, however far it grows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lock {
    pub owner: Owner,
    /// The process to report (`l_pid`).
    pub pid: i32,
    pub ty: LockType,
    pub start: u64,
    pub end: u64,
}

impl Lock {
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start <= end && start <= self.end
    }

    fn conflicts(&self, owner: Owner, ty: LockType, start: u64, end: u64) -> bool {
        self.owner != owner
            && self.overlaps(start, end)
            && (self.ty == LockType::Exclusive || ty == LockType::Exclusive)
    }
}

/// Who waits on whom, for deadlock detection.
static WAITING: SpinLock<BTreeMap<Owner, Owner>> = SpinLock::new(BTreeMap::new());

/// A vnode's advisory locks.
#[derive(Debug)]
pub struct Lockf {
    locks: SpinLock<Vec<Lock>>,
    /// Bumped by every change, for waiters.
    changes: AtomicU64,
    wait: WaitQueue,
}

impl Default for Lockf {
    fn default() -> Self {
        Self::new()
    }
}

impl Lockf {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            locks: SpinLock::new(Vec::new()),
            changes: AtomicU64::new(0),
            wait: WaitQueue::new(),
        }
    }

    /// The first lock that would keep `owner` from taking `ty` over
    /// `[start, end]` (`F_GETLK`).
    #[must_use]
    pub fn conflict(&self, owner: Owner, ty: LockType, start: u64, end: u64) -> Option<Lock> {
        self.locks
            .lock()
            .iter()
            .find(|l| l.conflicts(owner, ty, start, end))
            .copied()
    }

    /// Takes `ty` over `[start, end]` for `owner`; with `wait`, sleeps
    /// (interruptibly) until it can, else fails with `EAGAIN`.
    pub fn lock(&self, s: &Sleepable, req: Lock, wait: bool) -> Result<(), Errno> {
        loop {
            let seen = self.changes.load(Ordering::Acquire);
            let blocker = {
                let mut locks = self.locks.lock();
                match locks
                    .iter()
                    .find(|l| l.conflicts(req.owner, req.ty, req.start, req.end))
                {
                    Some(b) => b.owner,
                    None => {
                        apply(&mut locks, Some(req), req.owner, req.start, req.end);
                        return Ok(());
                    }
                }
            };
            if !wait {
                return Err(Errno::EAGAIN);
            }
            {
                let mut w = WAITING.lock();
                let mut at = blocker;
                for _ in 0..1024 {
                    if at == req.owner {
                        return Err(Errno::EDEADLK);
                    }
                    match w.get(&at) {
                        Some(&next) => at = next,
                        None => break,
                    }
                }
                w.insert(req.owner, blocker);
            }
            let r = self
                .wait
                .wait_until_interruptible(s, || self.changes.load(Ordering::Acquire) != seen);
            WAITING.lock().remove(&req.owner);
            if r.is_err() {
                return Err(Errno::EINTR);
            }
        }
    }

    /// Releases `owner`'s locks over `[start, end]`.
    pub fn unlock(&self, owner: Owner, start: u64, end: u64) {
        apply(&mut self.locks.lock(), None, owner, start, end);
        self.changed();
    }

    /// Releases every lock `owner` holds here.
    pub fn release(&self, owner: Owner) {
        let mut locks = self.locks.lock();
        let before = locks.len();
        locks.retain(|l| l.owner != owner);
        let any = locks.len() != before;
        drop(locks);
        if any {
            self.changed();
        }
    }

    /// Whether anyone holds a lock here.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.locks.lock().is_empty()
    }

    fn changed(&self) {
        self.changes.fetch_add(1, Ordering::Release);
        self.wait.wake_all();
    }
}

/// Removes `owner`'s locks from `[start, end]` (splitting those that run
/// past it), then adds `new` (merging it with `owner`'s locks of the same
/// type that touch it).
fn apply(locks: &mut Vec<Lock>, new: Option<Lock>, owner: Owner, start: u64, end: u64) {
    let mut out = Vec::with_capacity(locks.len() + 2);
    for l in locks.drain(..) {
        if l.owner != owner || !l.overlaps(start, end) {
            out.push(l);
            continue;
        }
        if l.start < start {
            out.push(Lock {
                end: start - 1,
                ..l
            });
        }
        if l.end > end {
            out.push(Lock {
                start: end + 1,
                ..l
            });
        }
    }
    if let Some(mut n) = new {
        // Merge with same-owner, same-type neighbors that touch or overlap.
        out.retain(|l| {
            let touches = l.owner == owner
                && l.ty == n.ty
                && l.start <= n.end.saturating_add(1)
                && n.start <= l.end.saturating_add(1);
            if touches {
                n.start = n.start.min(l.start);
                n.end = n.end.max(l.end);
            }
            !touches
        });
        out.push(n);
    }
    *locks = out;
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)]

    use super::*;

    fn s() -> Sleepable {
        // SAFETY: a host test thread may sleep.
        unsafe { Sleepable::assume() }
    }

    fn lock(owner: Owner, ty: LockType, start: u64, end: u64) -> Lock {
        Lock {
            owner,
            pid: match owner {
                Owner::Process(p) => p,
                Owner::File(_) => 0,
            },
            ty,
            start,
            end,
        }
    }

    const A: Owner = Owner::Process(1);
    const B: Owner = Owner::Process(2);

    #[test]
    fn shared_locks_coexist_and_exclusive_ones_conflict() {
        let l = Lockf::new();
        let s = s();
        l.lock(&s, lock(A, LockType::Shared, 0, 99), false).unwrap();
        l.lock(&s, lock(B, LockType::Shared, 50, 149), false)
            .unwrap();
        assert_eq!(
            l.lock(&s, lock(B, LockType::Exclusive, 0, 9), false),
            Err(Errno::EAGAIN)
        );
        let c = l.conflict(B, LockType::Exclusive, 0, 9).unwrap();
        assert_eq!((c.owner, c.start, c.end), (A, 0, 99));
        // Outside A's range, B may.
        l.lock(&s, lock(B, LockType::Exclusive, 100, 200), false)
            .unwrap();
        // A's own lock never conflicts with A.
        l.lock(&s, lock(A, LockType::Exclusive, 0, 49), false)
            .unwrap();
    }

    #[test]
    fn ranges_split_merge_and_replace() {
        let l = Lockf::new();
        let s = s();
        l.lock(&s, lock(A, LockType::Shared, 0, 99), false).unwrap();
        // Exclusive over the middle splits the shared lock in two.
        l.lock(&s, lock(A, LockType::Exclusive, 40, 59), false)
            .unwrap();
        let mut got: Vec<(u64, u64, LockType)> = l
            .locks
            .lock()
            .iter()
            .map(|x| (x.start, x.end, x.ty))
            .collect();
        got.sort_by_key(|x| (x.0, x.1));
        assert_eq!(
            got,
            [
                (0, 39, LockType::Shared),
                (40, 59, LockType::Exclusive),
                (60, 99, LockType::Shared)
            ]
        );
        // Unlocking the middle and relocking it shared merges them back.
        l.unlock(A, 40, 59);
        l.lock(&s, lock(A, LockType::Shared, 40, 59), false)
            .unwrap();
        let got: Vec<(u64, u64)> = l.locks.lock().iter().map(|x| (x.start, x.end)).collect();
        assert_eq!(got, [(0, 99)]);
        // To the end of the file.
        l.lock(&s, lock(A, LockType::Shared, 100, u64::MAX), false)
            .unwrap();
        assert_eq!(l.locks.lock().len(), 1);
        l.release(A);
        assert!(l.is_empty());
    }

    #[test]
    fn flock_and_posix_locks_conflict() {
        let l = Lockf::new();
        let s = s();
        let f = Owner::File(0x1000);
        l.lock(&s, lock(f, LockType::Exclusive, 0, u64::MAX), false)
            .unwrap();
        assert_eq!(
            l.lock(&s, lock(A, LockType::Shared, 10, 10), false),
            Err(Errno::EAGAIN)
        );
        l.release(f);
        l.lock(&s, lock(A, LockType::Shared, 10, 10), false)
            .unwrap();
    }

    #[test]
    fn a_wait_that_closes_a_cycle_is_a_deadlock() {
        let l = Lockf::new();
        let s = s();
        l.lock(&s, lock(A, LockType::Exclusive, 0, 0), false)
            .unwrap();
        l.lock(&s, lock(B, LockType::Exclusive, 1, 1), false)
            .unwrap();
        // B waits on A (recorded as if B were asleep).
        WAITING.lock().insert(B, A);
        assert_eq!(
            l.lock(&s, lock(A, LockType::Exclusive, 1, 1), true),
            Err(Errno::EDEADLK)
        );
        WAITING.lock().remove(&B);
    }
}
