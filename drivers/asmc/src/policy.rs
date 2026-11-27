// SPDX-License-Identifier: BSD-2-Clause
//! The fan floor and the platform limit: decisions only, from readings, as
//! the FreeBSD fork's `asmc_fan.c` and `asmc_plimit.c` make them. The
//! driver reads the keys, feeds these, and writes what they return.
//!
//! **The fan floor.** On a machine whose firmware gives the SMC's fan
//! table as device properties (ADR-0027 §3; the Mac Pro (Late 2013)), the
//! driver raises the fan's minimum, `F<n>Mn`, with temperature, and leaves
//! the SMC in automatic mode: the SMC still runs its own curve above the
//! floor. Each sensor class (GPU dies, CPU) asks for a floor rising
//! linearly from the firmware minimum at `start` °C to the top at `full`
//! °C, from its hottest sensor; a failed read, or one at or below 0 °C or
//! at or above 127 °C (the SMC's "absent" values), counts as hot (spec §04
//! item 4). The floor rises at once and falls by at most
//! [`FALL_STEP`] rpm a sample, once demand is [`FALL_BAND`] below it, and
//! all the way back to the firmware minimum once demand is there (the
//! fork's rule leaves it up to [`FALL_BAND`] above for good). It is
//! rewritten every sample while raised, so a stray write cannot keep the
//! fan low.
//!
//! **The platform limit.** Under combined CPU and GPU load the Mac Pro's
//! SMC limits the platform: it raises `MSLT` (0 to 100) and asserts PROCHOT
//! on the CPU itself; the GPUs are the OS's to slow. The limit is on while
//! `MSLT` is positive, and off only after [`OFF_SAMPLES`] zero samples in a
//! row, since `MSLT` drops to 0 for about 50 s at onset. The state can be
//! read at any time ([`Limit::active`]), so a listener that registers
//! while the limit is latched sees it (the fork's `421c666bda5a`).

/// Seconds between samples, for both.
pub const INTERVAL_S: u64 = 2;
/// The most the floor falls in one sample, rpm.
pub const FALL_STEP: u32 = 50;
/// How far below the floor demand must be before it falls, rpm.
pub const FALL_BAND: u32 = 100;
/// Zero `MSLT` samples in a row before the limit goes off (30 s).
pub const OFF_SAMPLES: u32 = 15;

/// One sensor class's curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Curve {
    /// Where the floor starts to rise, °C.
    pub start: i32,
    /// Where it reaches the top, °C.
    pub full: i32,
}

/// The fan floor's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Floor {
    /// The firmware's own minimum, rpm: the floor's bottom, and what the
    /// fan gets back when the policy stops.
    pub fwmin: u32,
    /// The highest floor written, rpm.
    pub top: u32,
    /// The GPU dies' curve.
    pub gpu: Curve,
    /// The CPU's curve.
    pub cpu: Curve,
}

/// A sensor reading for the floor: millidegrees, or `None` if it failed.
pub type Reading = Option<i32>;

impl Floor {
    /// What one class asks for, from its sensors' readings
    /// (`asmc_fanpol_demand`).
    #[must_use]
    pub fn demand(&self, curve: Curve, readings: &[Reading]) -> u32 {
        let mut hottest = None::<i32>;
        for r in readings {
            match *r {
                Some(m) if m > 0 && m < 127_000 => {
                    hottest = Some(hottest.map_or(m / 1000, |h| h.max(m / 1000)));
                }
                // Failed or "absent": hot.
                _ => return self.top,
            }
        }
        let Some(t) = hottest else {
            return self.fwmin;
        };
        if t <= curve.start {
            self.fwmin
        } else if t >= curve.full || curve.full <= curve.start {
            self.top
        } else {
            let span = u64::from(self.top.saturating_sub(self.fwmin));
            let num = u64::try_from(t - curve.start).unwrap_or(0);
            let den = u64::try_from(curve.full - curve.start).unwrap_or(1);
            self.fwmin + u32::try_from(span * num / den).unwrap_or(0)
        }
    }

    /// The next floor after `last` (`None` before the first write), from
    /// the classes' readings (`asmc_fanpol_task`), and whether to write it.
    #[must_use]
    pub fn step(&self, last: Option<u32>, gpu: &[Reading], cpu: &[Reading]) -> (u32, bool) {
        let demand = self
            .demand(self.gpu, gpu)
            .max(self.demand(self.cpu, cpu))
            .clamp(self.fwmin, self.top.max(self.fwmin));
        let Some(floor) = last else {
            // Unknown until written (a warm reboot can leave one behind):
            // the first sample always writes.
            return (demand, true);
        };
        // The band keeps the floor from hunting, except on the way back to
        // the firmware minimum: with the fork's rule alone a raised floor
        // stops up to FALL_BAND above it for good (fwmin 790, floor 890).
        let next = if demand > floor {
            demand
        } else if demand + FALL_BAND < floor || (demand == self.fwmin && floor > demand) {
            demand.max(floor.saturating_sub(FALL_STEP))
        } else {
            floor
        };
        (next, next != floor || next > self.fwmin)
    }
}

/// The platform limit's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limit {
    active: bool,
    zeros: u32,
    /// `MSLT`'s last level, 0 to 100.
    pub level: u32,
}

impl Limit {
    /// Whether the limit is on.
    #[must_use]
    pub fn active(&self) -> bool {
        self.active
    }

    /// One `MSLT` sample (`None`: the read failed, which keeps the state).
    /// Whether the state changed.
    pub fn sample(&mut self, mslt: Option<(u32, bool)>) -> bool {
        let Some((level, on)) = mslt else {
            return false;
        };
        self.level = level;
        let was = self.active;
        if on {
            self.zeros = 0;
            self.active = true;
        } else if self.active {
            self.zeros += 1;
            if self.zeros >= OFF_SAMPLES {
                self.active = false;
                self.zeros = 0;
            }
        }
        was != self.active
    }

    /// The limit reported off (detach): whether it was on.
    pub fn release(&mut self) -> bool {
        let was = self.active;
        *self = Self::default();
        was
    }
}
