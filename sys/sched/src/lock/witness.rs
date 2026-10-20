// SPDX-License-Identifier: BSD-2-Clause
//! The lock witness: runtime checks of lock order and sleep rules.
//!
//! Every sleepable lock belongs to a [`LockClass`] with a level. A thread
//! may only acquire a lock whose level is above every level it holds, so
//! any two classes are always taken in the same order and lock-order
//! deadlocks cannot occur. The witness also catches sleeping where it is
//! not allowed: with interrupts masked (inside a spin lock), or while
//! holding a lock whose class is not sleepable (rw locks).
//!
//! Violations are counted and the first few are reported; tests and the
//! stress run assert there are none.
//!
//! The witness is compiled into builds with debug assertions only, as
//! FreeBSD's `WITNESS` is a debug option (release builds are what the
//! performance comparison measures, docs/PERFORMANCE.md §2).
//!
//! Behavior reference: sys/kern/subr_witness.c (the checks, not its
//! learned lock-order graph: here the order is declared).

use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

/// A class of locks sharing a name, level, and sleep rule.
#[derive(Debug)]
pub struct LockClass {
    /// A name for reports.
    pub name: &'static str,
    /// The order: locks are acquired in increasing level.
    pub level: u16,
    /// Whether holders may sleep (mutexes and sx locks: yes; rw: no).
    pub sleepable: bool,
    /// Whether a thread may hold several locks of this class at once
    /// (FreeBSD's `LO_DUPOK`): the class's users order them by a rule of
    /// their own, which the witness cannot see (vnodes: parent before
    /// child).
    pub dup_ok: bool,
}

impl LockClass {
    /// A sleepable class.
    #[must_use]
    pub const fn new(name: &'static str, level: u16) -> Self {
        Self {
            name,
            level,
            sleepable: true,
            dup_ok: false,
        }
    }

    /// A sleepable class whose locks may be held several at once, in an
    /// order its users keep.
    #[must_use]
    pub const fn new_dup_ok(name: &'static str, level: u16) -> Self {
        Self {
            dup_ok: true,
            ..Self::new(name, level)
        }
    }

    /// A class whose holders must not sleep.
    #[must_use]
    pub const fn non_sleepable(name: &'static str, level: u16) -> Self {
        Self {
            name,
            level,
            sleepable: false,
            dup_ok: false,
        }
    }
}

const MAX_HELD: usize = 16;

#[derive(Clone, Copy, Debug)]
struct Entry {
    class: *const LockClass,
    /// Identifies this acquisition for release (the lock's address).
    lock: usize,
}

const EMPTY: Entry = Entry {
    class: ptr::null(),
    lock: 0,
};

/// A thread's held locks, in acquisition order.
#[derive(Debug)]
pub struct Held {
    entries: [Entry; MAX_HELD],
    len: usize,
}

impl Held {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [EMPTY; MAX_HELD],
            len: 0,
        }
    }
}

static VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// Witness violations so far.
#[must_use]
pub fn violations() -> u64 {
    VIOLATIONS.load(Ordering::Relaxed)
}

/// Reports a violation (the first few in detail).
fn violation(args: core::fmt::Arguments<'_>) {
    let n = VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    if n < 8
        && let Some(report) = super::REPORTER.get()
    {
        report(args);
    }
}

fn held() -> &'static mut Held {
    // SAFETY: only the current thread touches its own `held`, and the
    // thread outlives this borrow (it is running).
    unsafe { &mut *crate::current().held.get() }
}

/// Whether the checks are compiled in.
pub const ENABLED: bool = cfg!(debug_assertions);

/// Checks and records the acquisition of `lock` (of `class`).
pub(crate) fn acquire(class: &'static LockClass, lock: usize) {
    if !ENABLED {
        return;
    }
    let h = held();
    for e in &h.entries[..h.len] {
        // SAFETY: classes are statics.
        let c = unsafe { &*e.class };
        if c.level >= class.level && !(class.dup_ok && ptr::eq(c, class)) {
            violation(format_args!(
                "witness: acquiring {} (level {}) while holding {} (level {})",
                class.name, class.level, c.name, c.level
            ));
            break;
        }
    }
    if h.len == MAX_HELD {
        violation(format_args!("witness: more than {MAX_HELD} locks held"));
        return;
    }
    h.entries[h.len] = Entry { class, lock };
    h.len += 1;
}

/// Records the release of `lock`.
pub(crate) fn release(lock: usize) {
    if !ENABLED {
        return;
    }
    let h = held();
    match h.entries[..h.len].iter().rposition(|e| e.lock == lock) {
        Some(i) => {
            h.entries.copy_within(i + 1..h.len, i);
            h.len -= 1;
        }
        None => violation(format_args!("witness: releasing a lock not held")),
    }
}

/// Checks that the current thread may sleep. `irqs_were_enabled` is the
/// interrupt state before the sleeping code masked them.
pub(crate) fn check_sleep(irqs_were_enabled: bool) {
    if !ENABLED {
        return;
    }
    if !irqs_were_enabled {
        violation(format_args!("witness: sleeping with interrupts masked"));
    }
    if crate::preemption_disabled() {
        violation(format_args!("witness: sleeping with preemption disabled"));
    }
    let h = held();
    for e in &h.entries[..h.len] {
        // SAFETY: classes are statics.
        let c = unsafe { &*e.class };
        if !c.sleepable {
            violation(format_args!("witness: sleeping while holding {}", c.name));
        }
    }
}
