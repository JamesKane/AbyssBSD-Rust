// SPDX-License-Identifier: BSD-2-Clause
//! A thread's recent CPU share, as ULE reports it (`ki_pctcpu`, `ps`'s
//! `%CPU`, `top`'s `WCPU`).
//!
//! ULE keeps, per thread, the running time within a sliding window of
//! about ten seconds (`ts_ticks` between `ts_ftick` and `ts_ltick`), in
//! hardclock ticks shifted left for precision. It is brought up to date
//! whenever the thread goes on or off a CPU, wakes, takes a clock tick,
//! or is asked about: a span since the last update counts as running if
//! the thread ran through it. When the window has grown past its maximum,
//! what it holds is first scaled down to the part of the target window
//! the new span leaves, an exponential decay of 10/11 a second for a
//! thread updated often, and linear toward 0 for one that slept long.
//!
//! The clock here is milliseconds (FreeBSD's `hz` of 1000), converted
//! from the scheduler's own.
//!
//! Behavior reference: sys/kern/sched_ule.c (`sched_pctcpu_update`,
//! `sched_ule_pctcpu`).

/// FreeBSD's `hz`: the window's clock ticks per second.
pub(crate) const HZ: u64 = 1000;
/// `SCHED_TICK_SECS`: the window's maximum, in seconds.
const TICK_SECS: u64 = 11;
/// `SCHED_TICK_SHIFT`.
const TICK_SHIFT: u32 = 10;
/// `SCHED_CPU_DECAY_NUMER` and `_DENOM`.
const DECAY_NUMER: u64 = 10;
const DECAY_DENOM: u64 = 11;
/// `FSHIFT`: `ki_pctcpu` is a fixed-point fraction of `FSCALE`.
pub const FSHIFT: u32 = 11;
/// `FSCALE`.
pub const FSCALE: u32 = 1 << FSHIFT;

/// One thread's window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PctCpu {
    /// Running time in the window, in ticks shifted by `TICK_SHIFT`.
    ticks: u64,
    /// The window's start and its last update, in ticks.
    ftick: u64,
    ltick: u64,
}

impl PctCpu {
    /// A window opened at tick `now` (a new thread has run for none of
    /// it).
    pub(crate) const fn new(now: u64) -> Self {
        Self {
            ticks: 0,
            ftick: now,
            ltick: now,
        }
    }

    /// Brings the window up to tick `t`; `run` if the thread ran since the
    /// last update (`sched_pctcpu_update`).
    pub(crate) fn update(&mut self, t: u64, run: bool) {
        let max = TICK_SECS * HZ;
        let target = ((max << TICK_SHIFT) * DECAY_NUMER / DECAY_DENOM) >> TICK_SHIFT;
        let span = t.saturating_sub(self.ltick);
        if span >= target {
            // Too long since the last look: forget it all.
            self.ticks = if run { target << TICK_SHIFT } else { 0 };
            self.ftick = t - target;
            self.ltick = t;
            return;
        }
        if t.saturating_sub(self.ftick) >= max {
            self.ticks = self.ticks / self.length() * (target - span);
            self.ftick = t - target;
        }
        if run {
            self.ticks += span << TICK_SHIFT;
        }
        self.ltick = t;
    }

    /// `SCHED_TICK_LENGTH`.
    fn length(&self) -> u64 {
        self.ltick.saturating_sub(self.ftick).max(1)
    }

    /// The share as `ki_pctcpu` holds it (a fraction of [`FSCALE`]), as of
    /// the last update.
    pub(crate) fn fraction(&self) -> u32 {
        let len = self.length();
        let shifted = if FSHIFT >= TICK_SHIFT {
            self.ticks << (FSHIFT - TICK_SHIFT)
        } else {
            self.ticks >> (TICK_SHIFT - FSHIFT)
        };
        u32::try_from((shifted + len / 2) / len).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hog_nears_one_and_a_sleeper_falls() {
        let mut hog = PctCpu::new(0);
        let mut t = 0;
        for _ in 0..20_000 {
            t += 1;
            hog.update(t, true);
        }
        let f = hog.fraction();
        assert!(f > FSCALE * 95 / 100 && f <= FSCALE, "{f}");
        // Asleep for 5 seconds: about half the window's running remains.
        hog.update(t + 5 * HZ, false);
        let half = hog.fraction();
        assert!(half < f * 6 / 10 && half > f * 3 / 10, "{half}");
        // Asleep past the window: nothing.
        hog.update(t + 20 * HZ, false);
        assert_eq!(hog.fraction(), 0);
    }

    #[test]
    fn half_the_time_is_half() {
        let mut p = PctCpu::new(0);
        let mut t = 0;
        for i in 0..40_000 {
            t += 1;
            p.update(t, i % 2 == 0);
        }
        let f = p.fraction();
        assert!(f > FSCALE * 45 / 100 && f < FSCALE * 55 / 100, "{f}");
    }

    #[test]
    fn a_new_thread_has_none() {
        let mut p = PctCpu::new(1000);
        assert_eq!(p.fraction(), 0);
        p.update(1000, false);
        assert_eq!(p.fraction(), 0);
    }
}
