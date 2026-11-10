// SPDX-License-Identifier: BSD-2-Clause
//! The free-page policy: FreeBSD's paging thresholds, which allocations
//! may take the last free pages, and when the page daemon runs.
//!
//! Free pages are kept from running out the way FreeBSD keeps them:
//!
//! - Every allocation has a class ([`AllocClass`]). Pages for memory
//!   objects (the page cache, anonymous memory: `VM_ALLOC_NORMAL`) stop
//!   at [`Thresholds::free_reserved`]; the kernel's own memory (`malloc`,
//!   `kmem`: `VM_ALLOC_SYSTEM`) may go down to
//!   [`Thresholds::interrupt_free_min`]; interrupt handlers
//!   (`VM_ALLOC_INTERRUPT`) may take the last page. So however much the
//!   page cache wants, the kernel finds pages for the structures that let
//!   it give them back.
//! - Below [`Thresholds::wakeup`] the page daemon is wanted: it frees
//!   pages until [`Thresholds::free_target`] are free.
//!
//! The arithmetic lives here, apart from the allocator, so that host tests
//! check it against FreeBSD's formulas.
//!
//! Behavior reference: sys/vm/vm_pageout.c (`vm_pageout_init_domain`),
//! sys/vm/vm_page.c (`_vm_domain_allocate`), sys/vm/vm_pagequeue.h
//! (`vm_paging_needed`, `vm_paging_target`, `vm_paging_min`,
//! `vm_paging_severe`).

/// `vm_pageout_page_count`: pages the pageout clusters at most.
const PAGEOUT_PAGE_COUNT: u64 = 32;
/// `MAXBSIZE / PAGE_SIZE`: the largest file system block, in pages.
const MAXBSIZE_PAGES: u64 = 65536 / 4096;

/// Who an allocation is for, and so how deep into the free pages it may
/// go (`VM_ALLOC_CLASS_MASK`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocClass {
    /// Pages for memory objects: the page cache and anonymous memory
    /// (`VM_ALLOC_NORMAL`). Stops at [`Thresholds::free_reserved`].
    Normal,
    /// The kernel's own memory from thread context (`VM_ALLOC_SYSTEM`,
    /// what `malloc(9)` and `kmem` ask). Stops at
    /// [`Thresholds::interrupt_free_min`].
    System,
    /// From an interrupt handler (`VM_ALLOC_INTERRUPT`): anything free.
    Interrupt,
}

/// The paging thresholds of a domain with a given number of pages, as
/// `vm_pageout_init_domain` sets them, in pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// `v_interrupt_free_min`: only interrupt handlers go below.
    pub interrupt_free_min: u64,
    /// `v_pageout_free_min`: the page daemon's own minimum.
    pub pageout_free_min: u64,
    /// `v_free_reserved`: [`AllocClass::Normal`] stops here.
    pub free_reserved: u64,
    /// `v_free_min`: the first low-memory mark.
    pub free_min: u64,
    /// `v_free_severe`: the second, past which caches are drained.
    pub free_severe: u64,
    /// `v_free_target`: what the page daemon frees up to.
    pub free_target: u64,
    /// `v_inactive_target`.
    pub inactive_target: u64,
    /// `vmd_pageout_wakeup_thresh`: below this, the page daemon is woken.
    pub wakeup: u64,
}

impl Thresholds {
    /// The thresholds for a domain of `pages` pages, `free` of them free
    /// when it is set up (which caps the inactive target).
    #[must_use]
    pub const fn new(pages: u64, free: u64) -> Self {
        let interrupt_free_min = 2;
        let pageout_free_min = 2 * MAXBSIZE_PAGES + interrupt_free_min;
        let free_reserved = PAGEOUT_PAGE_COUNT + pageout_free_min + pages / 768;
        let min = pages / 200;
        let free_target = 4 * min + free_reserved;
        let mut inactive_target = 3 * free_target / 2;
        if inactive_target > free / 3 {
            inactive_target = free / 3;
        }
        Self {
            interrupt_free_min,
            pageout_free_min,
            free_reserved,
            free_min: min + free_reserved,
            free_severe: min / 2 + free_reserved,
            free_target,
            inactive_target,
            // 10% below the target, so the steady state stays out of
            // shortfall.
            wakeup: free_target / 10 * 9,
        }
    }

    /// The fewest free pages an allocation of `class` leaves.
    #[must_use]
    pub const fn floor(&self, class: AllocClass) -> u64 {
        match class {
            AllocClass::Interrupt => 0,
            AllocClass::System => self.interrupt_free_min,
            AllocClass::Normal => self.free_reserved,
        }
    }

    /// Whether `npages` may be taken of `free` for `class`
    /// (`_vm_domain_allocate`'s test).
    #[must_use]
    pub const fn allows(&self, free: u64, class: AllocClass, npages: u64) -> bool {
        free >= self.floor(class) + npages
    }

    /// Whether the page daemon is wanted with `free` pages free
    /// (`vm_paging_needed`).
    #[must_use]
    pub const fn paging_needed(&self, free: u64) -> bool {
        free < self.wakeup
    }

    /// Pages the page daemon should free with `free` free
    /// (`vm_paging_target`, none when past it).
    #[must_use]
    pub const fn shortage(&self, free: u64) -> u64 {
        self.free_target.saturating_sub(free)
    }

    /// Whether `free` is below the first low-memory mark
    /// (`vm_paging_min`).
    #[must_use]
    pub const fn below_min(&self, free: u64) -> bool {
        free < self.free_min
    }

    /// Whether `free` is below the severe mark (`vm_paging_severe`).
    #[must_use]
    pub const fn severe(&self, free: u64) -> bool {
        free < self.free_severe
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FreeBSD's `vm_pageout_init_domain`, transcribed step by step as
    /// it assigns the fields, to check the closed forms above.
    fn freebsd(pages: u64, free: u64) -> Thresholds {
        let interrupt_free_min = 2;
        let pageout_free_min = 2 * 65536 / 4096 + interrupt_free_min;
        let free_reserved = 32 + pageout_free_min + pages / 768;
        let mut free_min = pages / 200;
        let mut free_severe = free_min / 2;
        let free_target = 4 * free_min + free_reserved;
        free_min += free_reserved;
        free_severe += free_reserved;
        let mut inactive_target = (3 * free_target) / 2;
        if inactive_target > free / 3 {
            inactive_target = free / 3;
        }
        Thresholds {
            interrupt_free_min,
            pageout_free_min,
            free_reserved,
            free_min,
            free_severe,
            free_target,
            inactive_target,
            wakeup: (free_target / 10) * 9,
        }
    }

    #[test]
    fn matches_freebsd() {
        for pages in [0, 1, 767, 768, 65536, 131_072, 1 << 20, 8_257_536, 1 << 25] {
            for free in [0, pages / 2, pages] {
                assert_eq!(
                    Thresholds::new(pages, free),
                    freebsd(pages, free),
                    "{pages}"
                );
            }
        }
    }

    #[test]
    fn matches_freebsd_on_the_orange_pi() {
        // tests/corpus/opi6p/freebsd/sysctl-a.txt: FreeBSD on the Orange
        // Pi 6 Plus (one domain, `vm.stats.vm.v_page_count` 8150149,
        // `v_free_count` 8007295).
        let t = Thresholds::new(8_150_149, 8_007_295);
        assert_eq!(t.free_reserved, 10678);
        assert_eq!(t.free_min, 51428);
        assert_eq!(t.free_severe, 31053);
        assert_eq!(t.free_target, 173_678);
        assert_eq!(t.inactive_target, 260_517);
        assert_eq!(t.pageout_free_min, 34);
        assert_eq!(t.interrupt_free_min, 2);
    }

    #[test]
    fn known_values() {
        // 4 GiB, all free.
        let t = Thresholds::new(1 << 20, 1 << 20);
        assert_eq!(t.interrupt_free_min, 2);
        assert_eq!(t.pageout_free_min, 34);
        assert_eq!(t.free_reserved, 66 + 1365);
        assert_eq!(t.free_min, 1431 + 5242);
        assert_eq!(t.free_severe, 1431 + 2621);
        assert_eq!(t.free_target, 4 * 5242 + 1431);
        assert_eq!(t.wakeup, t.free_target / 10 * 9);
        assert!(t.interrupt_free_min < t.free_reserved);
        assert!(t.free_reserved < t.free_severe);
        assert!(t.free_severe < t.free_min);
        assert!(t.free_min < t.wakeup);
        assert!(t.wakeup < t.free_target);
    }

    #[test]
    fn classes_reach_different_depths() {
        let t = Thresholds::new(65536, 65536);
        let r = t.free_reserved;
        // The page cache stops at the reserve...
        assert!(t.allows(r + 1, AllocClass::Normal, 1));
        assert!(!t.allows(r, AllocClass::Normal, 1));
        assert!(!t.allows(r + 15, AllocClass::Normal, 16));
        // ...which the kernel may take, down to the interrupt minimum...
        assert!(t.allows(r, AllocClass::System, 1));
        assert!(t.allows(3, AllocClass::System, 1));
        assert!(!t.allows(2, AllocClass::System, 1));
        assert!(!t.allows(17, AllocClass::System, 16));
        // ...which only interrupt handlers take.
        assert!(t.allows(1, AllocClass::Interrupt, 1));
        assert!(!t.allows(0, AllocClass::Interrupt, 1));
    }

    #[test]
    fn daemon_marks() {
        let t = Thresholds::new(65536, 65536);
        assert!(!t.paging_needed(t.wakeup));
        assert!(t.paging_needed(t.wakeup - 1));
        assert_eq!(t.shortage(t.wakeup - 1), t.free_target - t.wakeup + 1);
        assert_eq!(t.shortage(t.free_target + 5), 0);
        assert!(t.below_min(t.free_min - 1) && !t.below_min(t.free_min));
        assert!(t.severe(t.free_severe - 1) && !t.severe(t.free_severe));
    }
}
