// SPDX-License-Identifier: BSD-2-Clause
//! The page daemon (`vm_pageout`): keeps enough pages free that
//! allocations rarely have to reclaim for themselves.
//!
//! It sleeps until an allocation of object pages finds fewer than
//! `vmd_pageout_wakeup_thresh` pages free and wakes it
//! ([`wakeup`], `pagedaemon_wakeup`), or for a tenth of a second
//! (`VM_INACT_SCAN_RATE`), then frees clean file pages nothing maps until
//! `v_free_target` pages are free. With no swap and no inactive queue,
//! what it frees is what FreeBSD's inactive scan frees first: clean pages
//! of files, oldest objects first. Dirty pages are not its business: the
//! syncer and the writers' own dirty limit write them back
//! (`vfs::pagecache`), after which they are clean.
//!
//! Short of its target, it trims the heap's caches every ten seconds
//! (`vm_pageout_lowmem`'s `lowmem_period`), and drains them past
//! `v_free_severe`.
//!
//! Behavior reference: sys/vm/vm_pageout.c (`vm_pageout_worker`,
//! `pagedaemon_wakeup`, `vm_pageout_lowmem`).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sched::Sleepable;

use super::{free_count, thresholds};

/// The daemon is running or has been woken (`vmd_pageout_wanted`): wakers
/// that find it set do not wake it again.
static WANTED: AtomicBool = AtomicBool::new(false);
/// Where the daemon sleeps.
static QUEUE: sched::WaitQueue = sched::WaitQueue::new();

/// `v_pdwakeups`: times the daemon was woken (not timed out).
pub(crate) static PDWAKEUPS: AtomicU64 = AtomicU64::new(0);
/// `v_pdpages`: pages the daemon looked at.
pub(crate) static PDPAGES: AtomicU64 = AtomicU64::new(0);
/// `v_dfree`: pages the daemon freed.
pub(crate) static DFREE: AtomicU64 = AtomicU64::new(0);
/// `v_pdshortfalls`: passes that ended short of the target.
pub(crate) static PDSHORTFALLS: AtomicU64 = AtomicU64::new(0);

/// How long the daemon sleeps between looks (`hz / VM_INACT_SCAN_RATE`).
const SCAN_PERIOD_NS: u64 = 100_000_000;
/// How often it trims the heap's caches while short
/// (`vm.lowmem_period`).
const LOWMEM_PERIOD_NS: u64 = 10_000_000_000;

/// Whether free pages are low enough to want the daemon
/// (`vm_paging_needed`).
pub(crate) fn needed() -> bool {
    thresholds().paging_needed(free_count())
}

/// Wakes the daemon unless it is already running or woken
/// (`pagedaemon_wakeup`). From thread context, outside the scheduler.
pub(crate) fn wakeup() {
    if !WANTED.swap(true, Ordering::SeqCst) {
        QUEUE.wake_one();
    }
}

/// Starts the daemon.
pub(crate) fn start() {
    if sched::Builder::new("pagedaemon")
        .priority(sched::Priority::KERNEL)
        .spawn(daemon)
        .is_err()
    {
        kprintln!("vm: no page daemon (out of memory)");
    }
}

fn daemon(s: &Sleepable) {
    let mut target_met = true;
    let mut last_lowmem: Option<u64> = None;
    loop {
        // Cleared before the limits are looked at, so a waker that
        // crosses them meanwhile sees it clear and wakes us.
        WANTED.store(false, Ordering::SeqCst);
        if needed() {
            // Short: look again soon, after a pause if the last pass
            // could not free enough.
            if !target_met {
                crate::time::sleep_ns(s, SCAN_PERIOD_NS);
            }
        } else if crate::time::wait_timeout(
            s,
            &QUEUE,
            || WANTED.load(Ordering::SeqCst),
            SCAN_PERIOD_NS,
        ) {
            PDWAKEUPS.fetch_add(1, Ordering::Relaxed);
        }
        WANTED.store(true, Ordering::SeqCst);
        let t = thresholds();
        let free = free_count();
        if !t.paging_needed(free) {
            target_met = true;
            continue;
        }
        let shortage = t.shortage(free);
        let now = crate::time::now_ns();
        if last_lowmem.is_none_or(|l| now.saturating_sub(l) >= LOWMEM_PERIOD_NS) {
            last_lowmem = Some(now);
            super::heap_trim();
        }
        let r = vm::object::reclaim_clean_counted(usize::try_from(shortage).unwrap_or(usize::MAX));
        PDPAGES.fetch_add(r.scanned as u64, Ordering::Relaxed);
        DFREE.fetch_add(r.freed as u64, Ordering::Relaxed);
        target_met = r.freed as u64 >= shortage;
        if !target_met {
            PDSHORTFALLS.fetch_add(1, Ordering::Relaxed);
            if t.severe(free_count()) {
                super::heap_reclaim();
            }
        }
        // What it freed went to this CPU's page cache first.
        super::drain_page_cache();
    }
}
