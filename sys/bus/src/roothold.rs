// SPDX-License-Identifier: BSD-2-Clause
//! Root mount holds: a bus still finding its devices holds the root mount
//! back, so a root on a device it has not found yet (a USB disk) is waited
//! for (FreeBSD's `root_mount_hold`, `root_mount_rel`, and
//! `vfs_mountroot_wait`).
//!
//! Behavior reference: sys/kern/vfs_mountroot.c.

use alloc::string::String;
use alloc::vec::Vec;

use sched::Sleepable;
use sync::SpinLock;

/// The holds, by token, with who holds each.
static HOLDS: SpinLock<Vec<(u64, String)>> = SpinLock::new(Vec::new());
static NEXT: SpinLock<u64> = SpinLock::new(0);

/// A hold on the root mount; released when dropped.
#[derive(Debug)]
pub struct RootHold(u64);

/// Holds the root mount back for `who` (`usbus0`).
#[must_use]
pub fn root_mount_hold(who: String) -> RootHold {
    let id = {
        let mut n = NEXT.lock();
        *n += 1;
        *n
    };
    HOLDS.lock().push((id, who));
    RootHold(id)
}

impl Drop for RootHold {
    fn drop(&mut self) {
        HOLDS.lock().retain(|(id, _)| *id != self.0);
    }
}

/// Waits until nothing holds the root mount, saying once a second (after
/// the first) who does (`vfs_mountroot_wait`).
pub fn root_mount_wait(s: &Sleepable) {
    let mut first = true;
    loop {
        let who: Vec<String> = HOLDS.lock().iter().map(|(_, w)| w.clone()).collect();
        if who.is_empty() {
            return;
        }
        if !first {
            let mut line = String::from("Root mount waiting for:");
            for w in &who {
                line.push(' ');
                line.push_str(w);
            }
            crate::manager().log(format_args!("{line}"));
        }
        first = false;
        // A second at most, as FreeBSD's `msleep(..., hz)`.
        for _ in 0..10 {
            if HOLDS.lock().is_empty() {
                return;
            }
            crate::manager().sleep_ms(s, 100);
        }
    }
}
