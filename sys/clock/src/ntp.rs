// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's clock discipline for NTP: the hybrid phase- and
//! frequency-lock loop `ntpd` steers through `ntp_adjtime`, the leap
//! second state machine, and `adjtime(2)`'s slew.
//!
//! The timekeeping calls [`Discipline::update_second`] once for every
//! second the time of day passes. It returns the adjustment for the
//! coming second, in nanoseconds per second with a 32-bit binary
//! fraction, which the timekeeping folds into the rate it scales the
//! counter by; and it may move the second itself, which is how a leap
//! second is inserted or deleted. Between seconds nothing here runs.
//!
//! Quantities follow FreeBSD's: a time offset is nanoseconds with a 32-bit
//! fraction (`l_fp`), a frequency nanoseconds per second with one; shifts
//! of negative values round toward zero, as its `L_RSHIFT` and `L_GINT`
//! do, so the loop's arithmetic, and what `ntp_adjtime` reports, are
//! FreeBSD's to the bit.
//!
//! There is no PPS source, so the PPS fields keep their initial values;
//! the status bits that depend on a PPS signal never come on, but the
//! loop tests them as FreeBSD's does.
//!
//! Behavior reference: sys/kern/kern_ntptime.c, sys/sys/timex.h.

use abi::timex::status::{
    STA_CLOCKERR, STA_FLL, STA_FREQHOLD, STA_MODE, STA_NANO, STA_PLL, STA_PPSERROR, STA_PPSFREQ,
    STA_PPSJITTER, STA_PPSSIGNAL, STA_PPSTIME, STA_PPSWANDER, STA_RONLY, STA_UNSYNC,
};
use abi::timex::{MAXFREQ, MAXPHASE, MAXSEC, MAXTC, MINSEC, SCALE_PPM, Timex, modes, state};

/// `SHIFT_PLL`: the PLL's loop gain (a shift).
const SHIFT_PLL: i64 = 4;
/// `SHIFT_FLL`: the FLL's loop gain.
const SHIFT_FLL: i64 = 2;
/// `PPS_FAVG`: the shortest PPS frequency averaging interval (log2 s).
const PPS_FAVG: i32 = 2;
/// `PPS_FAVGDEF`: the default longest one.
const PPS_FAVGDEF: i32 = 8;
/// `PPS_FAVGMAX`: the longest one allowed.
const PPS_FAVGMAX: i32 = 15;

/// The leap second state (`time_state`, `TIME_*` but `TIME_ERROR`, which
/// is only ever reported).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeapState {
    /// No leap second pending.
    Ok,
    /// A second is to be inserted at the end of the day.
    Insert,
    /// A second is to be deleted at the end of the day.
    Delete,
    /// The inserted second is in progress.
    InProgress,
    /// A leap second has happened; waiting for the request to go.
    Wait,
}

impl LeapState {
    /// Its `TIME_*` value.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Ok => state::TIME_OK,
            Self::Insert => state::TIME_INS,
            Self::Delete => state::TIME_DEL,
            Self::InProgress => state::TIME_OOP,
            Self::Wait => state::TIME_WAIT,
        }
    }
}

/// `L_RSHIFT`: an arithmetic shift that rounds toward zero.
const fn rshift(v: i64, n: i64) -> i64 {
    if v < 0 { -((-v) >> n) } else { v >> n }
}

/// `L_GINT`: the integer part, rounded toward zero.
const fn gint(v: i64) -> i64 {
    rshift(v, 32)
}

/// `L_LINT`: an integer as a fixed-point value.
const fn lint(a: i64) -> i64 {
    a.wrapping_shl(32)
}

/// What `ntp_gettime` reports besides the time (`struct ntptimeval`'s
/// other fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Microseconds.
    pub maxerror: i64,
    /// Microseconds.
    pub esterror: i64,
    /// Seconds.
    pub tai: i64,
    /// `TIME_*`: the leap state, or `TIME_ERROR` if the clock cannot be
    /// trusted.
    pub state: i32,
}

/// The discipline's state (FreeBSD's `time_*` and `pps_*` variables).
#[derive(Clone, Debug)]
pub struct Discipline {
    state: LeapState,
    /// `STA_*`.
    status: i32,
    tai: i64,
    /// The last offset `hardupdate` took, clamped (nanoseconds).
    monitor: i64,
    /// The time constant (log2 of the poll interval).
    constant: i64,
    /// Nanoseconds.
    precision: i64,
    /// Microseconds.
    maxerror: i64,
    /// Microseconds.
    esterror: i64,
    /// The uptime, in seconds, of the last frequency update.
    reftime: i64,
    /// The phase offset still to be slewed (`l_fp` nanoseconds).
    offset: i64,
    /// The frequency offset (`l_fp` nanoseconds per second).
    freq: i64,
    /// The adjustment for the current second (`l_fp` ns/s).
    adj: i64,
    /// `adjtime(2)`'s correction still to slew, in microseconds.
    adjtime: i64,
    pps_freq: i64,
    pps_jitter: i64,
    pps_stabil: i64,
    pps_valid: i32,
    pps_shift: i32,
    pps_shiftmax: i32,
    pps_calcnt: i64,
    pps_jitcnt: i64,
    pps_stbcnt: i64,
    pps_errcnt: i64,
}

impl Default for Discipline {
    fn default() -> Self {
        Self::new()
    }
}

impl Discipline {
    /// The state at boot: unsynchronized, no offset, no frequency.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: LeapState::Ok,
            status: STA_UNSYNC,
            tai: 0,
            monitor: 0,
            constant: 0,
            precision: 1,
            maxerror: MAXPHASE / 1000,
            esterror: MAXPHASE / 1000,
            reftime: 0,
            offset: 0,
            freq: 0,
            adj: 0,
            adjtime: 0,
            pps_freq: 0,
            pps_jitter: 0,
            pps_stabil: 0,
            pps_valid: 0,
            pps_shift: PPS_FAVG,
            pps_shiftmax: PPS_FAVGDEF,
            pps_calcnt: 0,
            pps_jitcnt: 0,
            pps_stbcnt: 0,
            pps_errcnt: 0,
        }
    }

    /// `ntp_is_time_error`: whether the status says the clock cannot be
    /// trusted.
    #[must_use]
    pub const fn is_time_error(status: i32) -> bool {
        status & (STA_UNSYNC | STA_CLOCKERR) != 0
            || (status & (STA_PPSFREQ | STA_PPSTIME) != 0 && status & STA_PPSSIGNAL == 0)
            || (status & STA_PPSTIME != 0 && status & STA_PPSJITTER != 0)
            || (status & STA_PPSFREQ != 0 && status & (STA_PPSWANDER | STA_PPSERROR) != 0)
    }

    /// The status bits (`time_status`).
    #[must_use]
    pub const fn status(&self) -> i32 {
        self.status
    }

    /// The TAI offset, in seconds.
    #[must_use]
    pub const fn tai(&self) -> i64 {
        self.tai
    }

    /// The leap state.
    #[must_use]
    pub const fn leap(&self) -> LeapState {
        self.state
    }

    /// `time_freq`, as `kern.ntp_pll.time_freq` shows it.
    #[must_use]
    pub const fn time_freq(&self) -> i64 {
        self.freq
    }

    /// `pps_freq`.
    #[must_use]
    pub const fn pps_freq(&self) -> i64 {
        self.pps_freq
    }

    /// `time_monitor`.
    #[must_use]
    pub const fn time_monitor(&self) -> i64 {
        self.monitor
    }

    /// `pps_shift`.
    #[must_use]
    pub const fn pps_shift(&self) -> i32 {
        self.pps_shift
    }

    /// Sets `pps_shift` (`kern.ntp_pll.pps_shift` takes any `int`).
    pub fn set_pps_shift(&mut self, v: i32) {
        self.pps_shift = v;
    }

    /// `pps_shiftmax`.
    #[must_use]
    pub const fn pps_shiftmax(&self) -> i32 {
        self.pps_shiftmax
    }

    /// Sets `pps_shiftmax` (`kern.ntp_pll.pps_shiftmax` takes any `int`).
    pub fn set_pps_shiftmax(&mut self, v: i32) {
        self.pps_shiftmax = v;
    }

    /// `adjtime(2)`'s correction still to slew, in microseconds.
    #[must_use]
    pub const fn adjtime_left(&self) -> i64 {
        self.adjtime
    }

    /// `ntp_gettime1`: everything but the time itself.
    #[must_use]
    pub const fn reading(&self) -> Reading {
        Reading {
            maxerror: self.maxerror,
            esterror: self.esterror,
            tai: self.tai,
            state: if Self::is_time_error(self.status) {
                state::TIME_ERROR
            } else {
                self.state.code()
            },
        }
    }

    /// `kern_ntp_adjtime` after its privilege check: applies what
    /// `tx.modes` selects, then fills `tx` with the state. `uptime` is the
    /// whole seconds since boot (`time_uptime`). Returns the clock state,
    /// `TIME_ERROR` if the clock cannot be trusted.
    pub fn adjust(&mut self, tx: &mut Timex, uptime: i64) -> i32 {
        let m = tx.modes;
        if m & modes::MOD_MAXERROR != 0 {
            self.maxerror = tx.maxerror;
        }
        if m & modes::MOD_ESTERROR != 0 {
            self.esterror = tx.esterror;
        }
        if m & modes::MOD_STATUS != 0 {
            // Dropping STA_PLL resets the state and status to boot's.
            if self.status & STA_PLL != 0 && tx.status & STA_PLL == 0 {
                self.state = LeapState::Ok;
                self.status = STA_UNSYNC;
                self.pps_shift = PPS_FAVG;
            }
            self.status &= STA_RONLY;
            self.status |= tx.status & !STA_RONLY;
        }
        if m & modes::MOD_TIMECONST != 0 {
            self.constant = tx.constant.clamp(0, MAXTC);
        }
        if m & modes::MOD_TAI != 0 && tx.constant > 0 {
            self.tai = tx.constant;
        }
        if m & modes::MOD_PPSMAX != 0 {
            self.pps_shiftmax = tx.shift.clamp(PPS_FAVG, PPS_FAVGMAX);
        }
        if m & modes::MOD_NANO != 0 {
            self.status |= STA_NANO;
        }
        if m & modes::MOD_MICRO != 0 {
            self.status &= !STA_NANO;
        }
        if m & modes::MOD_CLKB != 0 {
            self.status |= abi::timex::status::STA_CLK;
        }
        if m & modes::MOD_CLKA != 0 {
            self.status &= !abi::timex::status::STA_CLK;
        }
        if m & modes::MOD_FREQUENCY != 0 {
            // `freq` is scaled parts per million (us/s × 2^16); the loop's
            // is ns/s × 2^32.
            let ns = tx.freq.wrapping_mul(1000) >> 16;
            self.freq = if ns > MAXFREQ {
                lint(MAXFREQ)
            } else if ns < -MAXFREQ {
                lint(-MAXFREQ)
            } else {
                tx.freq.wrapping_mul(1000).wrapping_mul(65536)
            };
            self.pps_freq = self.freq;
        }
        if m & modes::MOD_OFFSET != 0 {
            let ns = if self.status & STA_NANO != 0 {
                tx.offset
            } else {
                tx.offset.wrapping_mul(1000)
            };
            self.hardupdate(ns, uptime);
        }

        let nano = self.status & STA_NANO != 0;
        tx.offset = if nano {
            gint(self.offset)
        } else {
            gint(self.offset) / 1000
        };
        tx.freq = gint((self.freq / 1000).wrapping_shl(16));
        tx.maxerror = self.maxerror;
        tx.esterror = self.esterror;
        tx.status = self.status;
        tx.constant = self.constant;
        tx.precision = if nano {
            self.precision
        } else {
            self.precision / 1000
        };
        tx.tolerance = MAXFREQ * SCALE_PPM;
        tx.shift = self.pps_shift;
        tx.ppsfreq = gint((self.pps_freq / 1000).wrapping_shl(16));
        tx.jitter = if nano {
            self.pps_jitter
        } else {
            self.pps_jitter / 1000
        };
        tx.stabil = self.pps_stabil;
        tx.calcnt = self.pps_calcnt;
        tx.errcnt = self.pps_errcnt;
        tx.jitcnt = self.pps_jitcnt;
        tx.stbcnt = self.pps_stbcnt;
        if Self::is_time_error(self.status) {
            state::TIME_ERROR
        } else {
            self.state.code()
        }
    }

    /// `hardupdate`: a new offset measurement, in nanoseconds, at
    /// `uptime` seconds. Sets the phase to slew and, from the time since
    /// the last update, corrects the frequency (PLL, and FLL as well for
    /// long intervals or with `STA_FLL`).
    fn hardupdate(&mut self, offset: i64, uptime: i64) {
        if self.status & STA_PLL == 0 {
            return;
        }
        if !(self.status & STA_PPSTIME != 0 && self.status & STA_PPSSIGNAL != 0) {
            self.monitor = offset.clamp(-MAXPHASE, MAXPHASE);
            self.offset = lint(self.monitor);
        }
        if self.status & STA_PPSFREQ != 0 && self.status & STA_PPSSIGNAL != 0 {
            self.reftime = uptime;
            return;
        }
        if self.status & STA_FREQHOLD != 0 || self.reftime == 0 {
            self.reftime = uptime;
        }
        let mtemp = uptime - self.reftime;
        let pll = rshift(lint(self.monitor), (SHIFT_PLL + 2 + self.constant) << 1);
        self.freq = self.freq.wrapping_add(pll.wrapping_mul(mtemp));
        self.status &= !STA_MODE;
        if mtemp >= MINSEC && (self.status & STA_FLL != 0 || mtemp > MAXSEC) {
            let fll = rshift(lint((self.monitor << 4) / mtemp), SHIFT_FLL + 4);
            self.freq = self.freq.wrapping_add(fll);
            self.status |= STA_MODE;
        }
        self.reftime = uptime;
        if gint(self.freq) > MAXFREQ {
            self.freq = lint(MAXFREQ);
        } else if gint(self.freq) < -MAXFREQ {
            self.freq = lint(-MAXFREQ);
        }
    }

    /// `ntp_update_second`: the time of day passed into second `*sec`
    /// (seconds since the Epoch). Cranks the leap state, which may move
    /// `*sec` back (an inserted second) or on (a deleted one), and returns
    /// the adjustment for the coming second: the phase slewed this second
    /// plus the frequency, plus `adjtime`'s slew (`l_fp` ns/s).
    pub fn update_second(&mut self, sec: &mut i64) -> i64 {
        self.maxerror = self.maxerror.saturating_add(MAXFREQ / 1000);

        self.state = match self.state {
            LeapState::Ok if self.status & abi::timex::status::STA_INS != 0 => LeapState::Insert,
            LeapState::Ok if self.status & abi::timex::status::STA_DEL != 0 => LeapState::Delete,
            LeapState::Ok => LeapState::Ok,
            LeapState::Insert if self.status & abi::timex::status::STA_INS == 0 => LeapState::Ok,
            LeapState::Insert if *sec % 86400 == 0 => {
                // 23:59:60 follows 23:59:59.
                *sec -= 1;
                self.tai += 1;
                LeapState::InProgress
            }
            LeapState::Delete if self.status & abi::timex::status::STA_DEL == 0 => LeapState::Ok,
            LeapState::Delete if (*sec + 1) % 86400 == 0 => {
                // 23:59:59 is skipped.
                *sec += 1;
                self.tai -= 1;
                LeapState::Wait
            }
            s @ (LeapState::Insert | LeapState::Delete) => s,
            LeapState::InProgress => LeapState::Wait,
            LeapState::Wait
                if self.status & (abi::timex::status::STA_INS | abi::timex::status::STA_DEL)
                    == 0 =>
            {
                LeapState::Ok
            }
            LeapState::Wait => LeapState::Wait,
        };

        // This second's share of the phase: a PPS-driven loop takes it
        // faster.
        let shift = if self.status & STA_PPSTIME != 0 && self.status & STA_PPSSIGNAL != 0 {
            i64::from(self.pps_shift)
        } else {
            SHIFT_PLL + self.constant
        };
        let phase = rshift(self.offset, shift);
        self.offset -= phase;
        self.adj = phase.wrapping_add(self.freq);

        // `adjtime(2)`: 5000 ppm while more than a second is left, 500
        // ppm down to the last 500 microseconds.
        if self.adjtime != 0 {
            let rate = if self.adjtime.abs() > 1_000_000 {
                5000 * self.adjtime.signum()
            } else {
                self.adjtime.clamp(-500, 500)
            };
            self.adjtime -= rate;
            self.adj = self.adj.wrapping_add(lint(rate * 1000));
        }

        if self.pps_valid > 0 {
            self.pps_valid -= 1;
        } else {
            self.status &= !STA_PPSSIGNAL;
        }
        self.adj
    }

    /// `kern_adjtime`: replaces the correction still to slew with `delta`
    /// microseconds, if given, and returns the one it replaced.
    pub fn set_adjtime(&mut self, delta: Option<i64>) -> i64 {
        let old = self.adjtime;
        if let Some(d) = delta {
            self.adjtime = d;
        }
        old
    }
}

/// `recalculate_scaling_factor_and_large_delta`'s scale, computed
/// exactly: the bintime fraction per counter tick for a counter of
/// `frequency` Hz read shifted right `shift` bits, sped up by
/// `adjustment` (`l_fp` ns/s). FreeBSD divides before it multiplies and
/// so undercompensates the adjustment by about 10 ppm of itself; this
/// does not.
#[must_use]
pub fn scale(frequency: u64, shift: u32, adjustment: i64) -> u64 {
    let base = (1u128 << (64 + shift)) / u128::from(frequency.max(1));
    // base × (1 + adjustment / (10^9 × 2^32)).
    let num = i128::try_from(base).unwrap_or(i128::MAX) * i128::from(adjustment);
    let den = 1_000_000_000i128 << 32;
    let scaled = i128::try_from(base).unwrap_or(i128::MAX) + num / den;
    u64::try_from(scaled.max(1)).unwrap_or(u64::MAX)
}

/// How much longer, as a fraction of 2^32, a span measured on the raw
/// counter must be to last at least as long on the clock disciplined by
/// `adjustment` (`l_fp` ns/s): 0 unless the clock runs slower than the
/// counter. Timers armed on the counter add this so that a sleep is never
/// short by the clock's reckoning.
#[must_use]
pub fn slow_pad(adjustment: i64) -> u64 {
    if adjustment >= 0 {
        return 0;
    }
    // A clock slower by `a` (a fraction) needs a / (1 - a) more counter
    // time; one more unit rounds up.
    let a = -i128::from(adjustment);
    let den = (1_000_000_000i128 << 32) - a;
    if den <= 0 {
        return u64::MAX;
    }
    u64::try_from((a << 32) / den + 1).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
