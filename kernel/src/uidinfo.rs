// SPDX-License-Identifier: BSD-2-Clause
//! What each real user holds, for the per-user limits (FreeBSD's
//! `struct uidinfo`): kqueues (`RLIMIT_KQUEUES`) and pseudo-terminals
//! (`RLIMIT_NPTS`). A [`Charge`] counts one against its user while it
//! lives.
//!
//! Behavior reference: sys/kern/kern_resource.c (`chgkqcnt`,
//! `chgptscnt`).

use alloc::collections::BTreeMap;

use abi::errno::Errno;
use abi::resource::{RLIM_INFINITY, rlimit};
use sync::SpinLock;

/// What is counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Kqueues,
    Ptys,
}

impl Kind {
    const fn limit(self) -> u32 {
        match self {
            Self::Kqueues => rlimit::KQUEUES,
            Self::Ptys => rlimit::NPTS,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

static COUNTS: SpinLock<BTreeMap<u32, [u64; 2]>> = SpinLock::new(BTreeMap::new());

/// How many `kind` real user `ruid` holds (`ui_kqcnt`, `ui_ptscnt`).
pub(crate) fn count(ruid: u32, kind: Kind) -> u64 {
    COUNTS.lock().get(&ruid).map_or(0, |n| n[kind.index()])
}

/// One `kind` counted against real user `ruid`.
#[derive(Debug)]
pub(crate) struct Charge {
    ruid: u32,
    kind: Kind,
}

impl Charge {
    /// Counts one `kind` against the calling process's real user, held to
    /// its soft limit: `fail` if that is reached (FreeBSD's `chg*cnt`
    /// failing, which each caller turns into its own error).
    pub(crate) fn take(kind: Kind, fail: Errno) -> Result<Self, Errno> {
        let (ruid, max) = crate::proc::Process::with_current(|p| {
            (
                p.cred().ruid,
                p.limits.lock()[kind.limit() as usize].rlim_cur,
            )
        })
        .unwrap_or((0, RLIM_INFINITY));
        let mut c = COUNTS.lock();
        let n = &mut c.entry(ruid).or_default()[kind.index()];
        if max != RLIM_INFINITY && *n + 1 > max {
            return Err(fail);
        }
        *n += 1;
        Ok(Self { ruid, kind })
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let mut c = COUNTS.lock();
        if let Some(n) = c.get_mut(&self.ruid) {
            n[self.kind.index()] -= 1;
            if n.iter().all(|&v| v == 0) {
                c.remove(&self.ruid);
            }
        }
    }
}
