// SPDX-License-Identifier: BSD-2-Clause
//! Command timeouts: the periods and their tunables, and the deadline
//! bookkeeping the watchdog keeps per outstanding command.
//!
//! FreeBSD stamps each command's deadline as it is submitted and checks
//! them from a callout every half second while commands are outstanding.
//! Here the watchdog stamps a command the first time it sees it, so the
//! submission path reads no clock: a command times out between its
//! period and its period plus one check after it was sent (FreeBSD's:
//! within one check after its period).
//!
//! Behavior reference: sys/dev/nvme/nvme_qpair.c (`nvme_qpair_timeout`,
//! `nvme_qpair_submit_tracker`), sys/dev/nvme/nvme_ctrlr.c (the
//! tunables in `nvme_ctrlr_construct`), sys/dev/nvme/nvme_sysctl.c
//! (`nvme_sysctl_timeout_period`).

use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

/// `NVME_DEFAULT_TIMEOUT_PERIOD`, seconds.
pub(crate) const DEFAULT_PERIOD: u32 = 30;
/// `NVME_ADMIN_TIMEOUT_PERIOD`, seconds.
pub(crate) const ADMIN_PERIOD: u32 = 60;
/// `NVME_MIN_TIMEOUT_PERIOD`.
const MIN_PERIOD: u32 = 5;
/// `NVME_MAX_TIMEOUT_PERIOD`.
const MAX_PERIOD: u32 = 120;
/// `NVME_DEFAULT_RETRY_COUNT`.
const DEFAULT_RETRY_COUNT: i32 = 4;
/// How often the watchdog looks while commands are outstanding
/// (`SBT_1S / 2`).
pub(crate) const CHECK_NS: u64 = 500_000_000;

/// `hw.nvme.timeout_period`: a new controller's I/O timeout, seconds.
static TIMEOUT_PERIOD: AtomicU32 = AtomicU32::new(DEFAULT_PERIOD);
/// `hw.nvme.admin_timeout_period`.
static ADMIN_TIMEOUT_PERIOD: AtomicU32 = AtomicU32::new(ADMIN_PERIOD);
/// `hw.nvme.retry_count` (`nvme_retry_count`, global as FreeBSD's).
static RETRY_COUNT: AtomicI32 = AtomicI32::new(DEFAULT_RETRY_COUNT);

/// A period tunable's value as `nvme_ctrlr_construct` takes it: clamped
/// to the allowed range.
pub(crate) fn clamp_period(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0).clamp(MIN_PERIOD, MAX_PERIOD)
}

/// A period written to `dev.nvme.N.timeout_period`: out of range is
/// refused (`EINVAL`), as `nvme_sysctl_timeout_period` does.
pub(crate) fn check_period(v: i32) -> Option<u32> {
    u32::try_from(v)
        .ok()
        .filter(|p| (MIN_PERIOD..=MAX_PERIOD).contains(p))
}

/// Takes the boot-time tunables (`TUNABLE_INT_FETCH`): `get` gives a
/// name's integer value, if set. The kernel calls it before drivers
/// attach.
pub fn set_tunables(get: &dyn Fn(&str) -> Option<i32>) {
    // Relaxed: tunables, read at attach.
    if let Some(v) = get("hw.nvme.timeout_period") {
        TIMEOUT_PERIOD.store(clamp_period(v), Ordering::Relaxed);
    }
    if let Some(v) = get("hw.nvme.admin_timeout_period") {
        ADMIN_TIMEOUT_PERIOD.store(clamp_period(v), Ordering::Relaxed);
    }
    if let Some(v) = get("hw.nvme.retry_count") {
        RETRY_COUNT.store(v, Ordering::Relaxed);
    }
}

/// The I/O and admin periods a new controller starts with.
pub(crate) fn periods() -> (u32, u32) {
    (
        TIMEOUT_PERIOD.load(Ordering::Relaxed),
        ADMIN_TIMEOUT_PERIOD.load(Ordering::Relaxed),
    )
}

/// `nvme_retry_count`.
pub(crate) fn retry_count() -> i32 {
    RETRY_COUNT.load(Ordering::Relaxed)
}

/// An outstanding command's deadline, in the watchdog's clock
/// (nanoseconds). Not yet seen by the watchdog, or held while the
/// controller resets (FreeBSD's `SBT_MAX`), is `Unset`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Deadline {
    #[default]
    Unset,
    At(u64),
}

impl Deadline {
    /// The watchdog's look at it at `now`: stamps an unset one to
    /// `now + period`, and says whether it has passed.
    pub(crate) fn check(&mut self, now: u64, period_ns: u64) -> bool {
        match *self {
            Self::Unset => {
                *self = Self::At(now.saturating_add(period_ns));
                false
            }
            Self::At(t) => now > t,
        }
    }

    /// Whether it has passed at `now`, without stamping.
    pub(crate) fn passed(self, now: u64) -> bool {
        matches!(self, Self::At(t) if now > t)
    }
}

/// What one look at a queue's outstanding commands found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scan {
    /// Commands outstanding.
    pub(crate) outstanding: usize,
    /// Of those, past their deadline.
    pub(crate) expired: usize,
}

impl Scan {
    /// Looks at `deadlines` at `now`, stamping the unset ones.
    pub(crate) fn of<'a>(
        deadlines: impl Iterator<Item = &'a mut Deadline>,
        now: u64,
        period_ns: u64,
    ) -> Self {
        let mut s = Self::default();
        for d in deadlines {
            s.outstanding += 1;
            if d.check(now, period_ns) {
                s.expired += 1;
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    #[test]
    fn periods_clamp_and_check_as_freebsd() {
        assert_eq!(clamp_period(30), 30);
        assert_eq!(clamp_period(1), 5);
        assert_eq!(clamp_period(-3), 5);
        assert_eq!(clamp_period(1000), 120);
        assert_eq!(check_period(5), Some(5));
        assert_eq!(check_period(120), Some(120));
        assert_eq!(check_period(4), None);
        assert_eq!(check_period(121), None);
        assert_eq!(check_period(-1), None);
    }

    #[test]
    fn a_command_times_out_one_period_after_it_is_first_seen() {
        let p = 30 * 1_000_000_000;
        let mut d = Deadline::Unset;
        assert!(!d.check(1_000, p), "stamped, not expired");
        assert_eq!(d, Deadline::At(1_000 + p));
        assert!(!d.check(1_000 + p, p), "not before the deadline has passed");
        assert!(d.check(1_001 + p, p));
        assert!(d.passed(1_001 + p));
        assert!(
            !Deadline::Unset.passed(u64::MAX),
            "a held command never expires"
        );
    }

    #[test]
    fn a_scan_counts_outstanding_and_expired() {
        let p = 1_000;
        let mut ds: Vec<Deadline> =
            std::vec![Deadline::Unset, Deadline::At(500), Deadline::At(5_000)];
        let s = Scan::of(ds.iter_mut(), 1_000, p);
        assert_eq!(
            s,
            Scan {
                outstanding: 3,
                expired: 1
            }
        );
        assert_eq!(ds[0], Deadline::At(2_000));
        // Idle.
        assert_eq!(Scan::of(core::iter::empty(), 0, p), Scan::default());
        // Later, the first one has passed too.
        let s = Scan::of(ds.iter_mut(), 2_001, p);
        assert_eq!(
            s,
            Scan {
                outstanding: 3,
                expired: 2
            }
        );
    }
}
