// SPDX-License-Identifier: BSD-2-Clause
//! Time-sharing priorities.
//!
//! A time-sharing thread's priority follows its recent CPU usage: the
//! share of a sliding window it spent running. Time spent sleeping or
//! waiting on a run queue counts toward the window but not toward usage,
//! so CPU-bound threads sink toward the bottom of the time-sharing range,
//! threads that mostly sleep stay near the top, and a thread that has been
//! waiting to run rises until it does: no time-sharing thread starves.
//!
//! The window is kept as two counters (running time, total time) that are
//! halved together whenever the total passes the window, an exponential
//! decay with a half-life of about one window.
//!
//! Usage alone does not prevent starvation when CPUs are oversubscribed:
//! with 64 busy threads on 8 CPUs each uses an eighth of a CPU, which may
//! not lift a low-niceness thread above a waiting high-niceness one. So a
//! queued thread also gains one level per [`WAIT_STEP_MS`] it has waited,
//! up to the top of the range: every waiting time-sharing thread runs
//! within about half a second, and equals then take turns.
//!
//! Behavior reference: sys/kern/sched_ule.c (interactivity and the
//! %CPU-based batch priority; the scoring here is simpler).

use crate::Priority;

/// The decay window, in the scheduler clock's units (set at init from the
/// clock rate: one second).
pub(crate) const WINDOW_SECONDS: u64 = 1;

/// Nice values map to priority offsets in `0..=NICE_RANGE`.
pub(crate) const NICE_RANGE: u8 = 40;
/// The span usage moves a thread across.
const USAGE_RANGE: u64 = 63;
/// Waiting this long on a run queue is worth one priority level.
pub(crate) const WAIT_STEP_MS: u64 = 5;

/// One thread's time-sharing state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Usage {
    /// Priority offset from niceness, `0..=NICE_RANGE`.
    nice: u8,
    run: u64,
    total: u64,
    /// When the state was last brought up to date.
    stamp: u64,
    /// When the thread was queued to run, while it waits.
    queued_since: Option<u64>,
}

impl Usage {
    pub(crate) const fn new(nice: u8, now: u64) -> Self {
        Self {
            nice: if nice > NICE_RANGE { NICE_RANGE } else { nice },
            run: 0,
            total: 0,
            stamp: now,
            queued_since: None,
        }
    }

    /// The thread went on a run queue.
    pub(crate) fn enqueue(&mut self, now: u64) {
        self.queued_since = Some(now);
    }

    /// The thread came off its run queue onto a CPU.
    pub(crate) fn dequeue(&mut self) {
        self.queued_since = None;
    }

    /// Accounts the time since the last update as running (`ran`) or not.
    pub(crate) fn update(&mut self, now: u64, ran: bool, window: u64) {
        let dt = now.saturating_sub(self.stamp);
        self.stamp = now;
        if ran {
            self.run = self.run.saturating_add(dt);
        }
        self.total = self.total.saturating_add(dt);
        let window = window.max(1);
        while self.total > window {
            self.run /= 2;
            self.total /= 2;
        }
    }

    /// Recent CPU usage in per mille.
    pub(crate) fn per_mille(&self) -> u64 {
        (self.run.min(self.total) * 1000)
            .checked_div(self.total)
            .unwrap_or(0)
    }

    /// The priority for this usage at `now`; `step` is [`WAIT_STEP_MS`] in
    /// clock ticks.
    pub(crate) fn priority(&self, now: u64, step: u64) -> Priority {
        let penalty = u64::from(self.nice) + self.per_mille() * USAGE_RANGE / 1000;
        let bonus = self
            .queued_since
            .map_or(0, |q| now.saturating_sub(q) / step.max(1));
        let p = u64::from(Priority::TIMESHARE.get()) + penalty.saturating_sub(bonus);
        Priority::new(u8::try_from(p).unwrap_or(u8::MAX))
    }
}

const _: () = assert!(
    Priority::TIMESHARE.get() as u64 + NICE_RANGE as u64 + USAGE_RANGE
        < Priority::IDLE.get() as u64
);

#[cfg(test)]
mod tests {
    use super::*;

    const W: u64 = 1000;

    #[test]
    fn hogs_sink_and_sleepers_rise() {
        let mut hog = Usage::new(0, 0);
        let mut sleeper = Usage::new(0, 0);
        let mut now = 0;
        for _ in 0..100 {
            now += 10;
            hog.update(now, true, W);
            sleeper.update(now, now % 100 == 0, W);
        }
        assert!(hog.per_mille() > 900, "{hog:?}");
        assert!(sleeper.per_mille() < 200, "{sleeper:?}");
        assert!(sleeper.priority(now, 1).beats(hog.priority(now, 1)));
        assert!(hog.priority(now, 1).get() <= 120 + 63);
    }

    #[test]
    fn waiting_threads_rise_past_hogs() {
        // A nice thread that has been running a lot, then starved.
        let mut starved = Usage::new(NICE_RANGE, 0);
        starved.update(1000, true, W);
        let mut hog = Usage::new(0, 0);
        hog.update(1000, true, W);
        assert!(hog.priority(1000, 1000).beats(starved.priority(1000, 1000)));
        // The hog keeps running; the other waits.
        let mut now = 1000;
        while !starved.priority(now, 1000).beats(hog.priority(now, 1000)) {
            now += 10;
            hog.update(now, true, W);
            starved.update(now, false, W);
            assert!(now < 20_000, "starved thread never rose");
        }
    }

    #[test]
    fn waiting_earns_a_bonus() {
        // Busy at an eighth of a CPU: usage alone keeps a nice thread
        // below a less nice one.
        let mut busy = Usage::new(0, 0);
        let mut nice = Usage::new(NICE_RANGE, 0);
        let mut now = 0;
        for k in 0..800 {
            now += 10;
            busy.update(now, k % 8 == 0, W);
            nice.update(now, false, W);
        }
        assert!(busy.priority(now, 10).beats(nice.priority(now, 10)));
        nice.enqueue(now);
        let later = now + 10 * 60;
        assert!(nice.priority(later, 10).beats(busy.priority(later, 10)));
        nice.dequeue();
        assert!(busy.priority(later, 10).beats(nice.priority(later, 10)));
    }

    #[test]
    fn window_bounds_counters() {
        let mut u = Usage::new(0, 0);
        u.update(1_000_000, true, W);
        assert!(u.total <= W);
        assert_eq!(u.per_mille(), 1000);
        u.update(1_000_000 + 3 * W, false, W);
        assert!(u.per_mille() < 300);
    }
}
