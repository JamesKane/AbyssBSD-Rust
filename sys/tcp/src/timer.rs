// SPDX-License-Identifier: BSD-2-Clause
//! A connection's timers (`tcp_timer.c`): delayed acknowledgement,
//! retransmission, persist, keepalive, and 2MSL, each a deadline, all
//! served by one callout set to the earliest, as FreeBSD 15 does. The
//! caller arms its callout for [`Tcpcb::next_deadline`] and calls
//! [`Tcpcb::timers`] when it fires.
//!
//! Behavior reference: sys/netinet/tcp_timer.c, tcp_timer.h,
//! sys/netinet/tcp_output.c (`tcp_setpersist`).

use crate::cc::Signal;
use crate::tcb::{Tcpcb, tf, tf2};
use crate::{Conf, Ctx, Errno, Fate, Sock, State, get, on, so, tcps};

/// Ticks of a millisecond.
pub const MSL: u32 = 30_000;
pub const MSL_LOCAL: u32 = 10;
pub const SRTTBASE: u32 = 0;
pub const RTOBASE: u32 = 1000;
pub const PERSMIN: u32 = 5000;
pub const PERSMAX: u32 = 60_000;
pub const KEEP_INIT: u32 = 75_000;
pub const KEEP_IDLE: u32 = 120 * 60 * 1000;
pub const KEEPINTVL: u32 = 75_000;
pub const KEEPCNT: u32 = 8;
pub const FINWAIT2_TIMEOUT: u32 = 60_000;
pub const MIN: u32 = 30;
pub const CPU_VAR: u32 = 200;
pub const REXMTMAX: u32 = 64_000;
pub const MAXRXTSHIFT: u32 = 12;
pub const DELACK: u32 = 40;
/// `TCP_RTT_INVALIDATE`.
pub const RTT_INVALIDATE: u32 = MAXRXTSHIFT / 4;

/// `TCP_RTT_SCALE`, `TCP_RTT_SHIFT`, `TCP_RTTVAR_SCALE`,
/// `TCP_RTTVAR_SHIFT`, `TCP_DELTA_SHIFT`.
pub const RTT_SCALE: i32 = 32;
pub const RTT_SHIFT: u32 = 5;
pub const RTTVAR_SCALE: i32 = 16;
pub const RTTVAR_SHIFT: u32 = 4;
pub const DELTA_SHIFT: u32 = 2;

/// `tcp_backoff[]`.
pub const BACKOFF: [u32; MAXRXTSHIFT as usize + 1] =
    [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 512, 512, 512];
/// `tcp_totbackoff`.
pub const TOTBACKOFF: u32 = 2559;

/// `TCPT_RANGESET`: `value` plus `tcp_rexmit_slop`, clamped.
#[must_use]
pub fn rangeset(conf: &Conf, value: u32, min: u32, max: u32) -> u32 {
    let v = value.saturating_add(get(&conf.rexmit_slop) as u32);
    v.max(min).min(max)
}

/// A timer (`tt_which`), in FreeBSD's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tt {
    Delack,
    Rexmt,
    Persist,
    Keep,
    TwoMsl,
}

impl Tt {
    pub const N: usize = 5;
    const ALL: [Self; Self::N] = [
        Self::Delack,
        Self::Rexmt,
        Self::Persist,
        Self::Keep,
        Self::TwoMsl,
    ];
}

/// The deadlines (`t_timers`), `u64::MAX` when stopped.
#[derive(Clone, Copy, Debug)]
pub struct Timers(pub [u64; Tt::N]);

impl Default for Timers {
    fn default() -> Self {
        Self([u64::MAX; Tt::N])
    }
}

impl Tcpcb {
    /// `tcp_timer_activate`: starts `which` `delta` ticks from now, or
    /// stops it (0).
    pub fn timer_activate(&mut self, which: Tt, delta: u32, now: u64) {
        self.timers.0[which as usize] = if delta > 0 {
            now + u64::from(delta)
        } else {
            u64::MAX
        };
    }

    /// `tcp_timer_active`.
    #[must_use]
    pub fn timer_active(&self, which: Tt) -> bool {
        self.timers.0[which as usize] != u64::MAX
    }

    /// When `which` fires, if running.
    #[must_use]
    pub fn timer_deadline(&self, which: Tt) -> Option<u64> {
        let t = self.timers.0[which as usize];
        (t != u64::MAX).then_some(t)
    }

    /// `tcp_timer_stop`.
    pub fn timer_stop(&mut self) {
        self.timers = Timers::default();
    }

    /// When the callout should next fire.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        let t = self.timers.0.iter().copied().min().unwrap_or(u64::MAX);
        (t != u64::MAX).then_some(t)
    }

    /// `tcp_timer_enter`: runs every timer due by `cx.now`, earliest
    /// first, while the connection lives.
    pub fn timers<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        loop {
            let Some((i, t)) = self
                .timers
                .0
                .iter()
                .copied()
                .enumerate()
                .min_by_key(|&(_, t)| t)
            else {
                return Fate::Alive;
            };
            if t > cx.now {
                return Fate::Alive;
            }
            self.timers.0[i] = u64::MAX;
            let fate = match Tt::ALL[i] {
                Tt::Delack => self.timer_delack(so, cx),
                Tt::Rexmt => self.timer_rexmt(so, cx),
                Tt::Persist => self.timer_persist(so, cx),
                Tt::Keep => self.timer_keep(so, cx),
                Tt::TwoMsl => self.timer_2msl(so, cx),
            };
            if fate == Fate::Closed {
                return fate;
            }
        }
    }

    /// `TP_KEEPINIT` and the rest: the socket's value, or the system's.
    pub(crate) fn keepinit(&self, conf: &Conf) -> u32 {
        if self.keepinit != 0 {
            self.keepinit
        } else {
            get(&conf.keepinit) as u32
        }
    }

    pub(crate) fn keepidle(&self, conf: &Conf) -> u32 {
        if self.keepidle != 0 {
            self.keepidle
        } else {
            get(&conf.keepidle) as u32
        }
    }

    pub(crate) fn keepintvl(&self, conf: &Conf) -> u32 {
        if self.keepintvl != 0 {
            self.keepintvl
        } else {
            get(&conf.keepintvl) as u32
        }
    }

    pub(crate) fn keepcnt(&self, conf: &Conf) -> u32 {
        if self.keepcnt != 0 {
            self.keepcnt
        } else {
            get(&conf.keepcnt) as u32
        }
    }

    /// `TP_MAXIDLE`.
    pub(crate) fn maxidle(&self, conf: &Conf) -> u32 {
        self.keepcnt(conf) * self.keepintvl(conf)
    }

    fn maxunacktime(&self, conf: &Conf) -> u32 {
        if self.maxunacktime != 0 {
            self.maxunacktime
        } else {
            get(&conf.maxunacktime) as u32
        }
    }

    /// `TP_RXTCUR`: the retransmission timeout, held to the progress
    /// deadline (`TCP_MAXUNACKTIME`).
    pub(crate) fn rxtcur_bounded(&self, cx: &Ctx) -> u32 {
        let mu = self.maxunacktime(cx.conf);
        if mu == 0 || self.acktime == 0 {
            self.rxtcur
        } else {
            let left = (self.acktime + u64::from(mu)).saturating_sub(cx.now);
            (self.rxtcur as u64).min(left).max(1) as u32
        }
    }

    /// `tcp_maxunacktime_check`.
    fn maxunacktime_check(&self, cx: &Ctx) -> bool {
        let mu = self.maxunacktime(cx.conf);
        if mu == 0 || self.acktime == 0 || self.acktime + u64::from(mu) > cx.now {
            return false;
        }
        cx.stats.inc(tcps::PROGDROPS);
        true
    }

    /// Ticks since the last segment arrived.
    pub(crate) fn idle(&self, now: u64) -> u64 {
        now.saturating_sub(self.rcvtime)
    }

    fn timer_delack<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        self.flags |= tf::ACKNOW;
        cx.stats.inc(tcps::DELACK);
        self.output(so, cx)
    }

    fn timer_2msl<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        self.free_sackholes(cx.stats);
        let close = if self.state == State::TimeWait {
            true
        } else if self.state == State::FinWait2
            && on(&cx.conf.fast_finwait2_recycle)
            && so.rcv_closed()
        {
            cx.stats.inc(tcps::FINWAIT2_DROPS);
            true
        } else if self.idle(cx.now) <= u64::from(self.maxidle(cx.conf)) {
            let d = self.keepintvl(cx.conf);
            self.timer_activate(Tt::TwoMsl, d, cx.now);
            false
        } else {
            true
        };
        if close {
            return self.close(so, cx);
        }
        Fate::Alive
    }

    fn timer_keep<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        if self.state.have_established() {
            let idle = self.idle(cx.now);
            let ki = u64::from(self.keepidle(cx.conf));
            if idle < ki {
                self.timer_activate(Tt::Keep, (ki - idle) as u32, cx.now);
                return Fate::Alive;
            }
        }
        cx.stats.inc(tcps::KEEPTIMEO);
        if self.state < State::Established {
            cx.stats.inc(tcps::KEEPDROPS);
            return self.drop(so, cx, Errno::ETIMEDOUT);
        }
        if (on(&cx.conf.always_keepalive) || so.options() & so::KEEPALIVE != 0)
            && self.state <= State::Closing
        {
            if self.idle(cx.now)
                >= u64::from(self.keepidle(cx.conf)) + u64::from(self.maxidle(cx.conf))
            {
                cx.stats.inc(tcps::KEEPDROPS);
                return self.drop(so, cx, Errno::ETIMEDOUT);
            }
            // An acknowledgement of one byte before the window, which the
            // peer must answer (`tcp_respond` with `snd_una - 1`).
            cx.stats.inc(tcps::KEEPPROBE);
            self.respond(
                cx,
                self.rcv_nxt,
                self.snd_una.wrapping_sub(1),
                crate::hdr::th::ACK,
            );
            let d = self.keepintvl(cx.conf);
            self.timer_activate(Tt::Keep, d, cx.now);
        } else {
            let d = self.keepidle(cx.conf);
            self.timer_activate(Tt::Keep, d, cx.now);
        }
        Fate::Alive
    }

    fn timer_persist<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        cx.stats.inc(tcps::PERSISTTIMEO);
        let progdrop = self.maxunacktime_check(cx);
        let idle = self.idle(cx.now);
        if progdrop
            || (self.rxtshift >= get(&cx.conf.retries) as u32
                && (idle >= get(&cx.conf.maxpersistidle) as u64
                    || idle >= u64::from(self.rexmtval()) * u64::from(TOTBACKOFF)))
        {
            if !progdrop {
                cx.stats.inc(tcps::PERSISTDROP);
            }
            return self.drop(so, cx, Errno::ETIMEDOUT);
        }
        // Closed by the user: give up after a much shorter wait.
        if self.state > State::CloseWait && idle >= u64::from(PERSMAX) {
            cx.stats.inc(tcps::PERSISTDROP);
            return self.drop(so, cx, Errno::ETIMEDOUT);
        }
        self.setpersist(cx);
        self.flags |= tf::FORCEDATA;
        let fate = self.output(so, cx);
        self.flags &= !tf::FORCEDATA;
        fate
    }

    /// `tcp_setpersist`: (re)starts the persist timer, backing off.
    pub(crate) fn setpersist(&mut self, cx: &Ctx) {
        let t = (((self.srtt >> 2) + self.rttvar) >> 1).max(0) as u32;
        self.flags &= !tf::PREVVALID;
        if self.state == State::Closed {
            return;
        }
        let mut tt = rangeset(
            cx.conf,
            t * BACKOFF[self.rxtshift as usize],
            get(&cx.conf.persmin) as u32,
            get(&cx.conf.persmax) as u32,
        );
        let mu = self.maxunacktime(cx.conf);
        if mu != 0 && self.acktime != 0 {
            let left = (self.acktime + u64::from(mu)).saturating_sub(cx.now).max(1);
            tt = (tt as u64).min(left) as u32;
        }
        self.timer_activate(Tt::Persist, tt, cx.now);
        if self.rxtshift < get(&cx.conf.retries) as u32 {
            self.rxtshift += 1;
        }
    }

    fn timer_rexmt<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        let retries = get(&cx.conf.retries) as u32;
        self.rxtshift += 1;
        if self.rxtshift > retries || self.maxunacktime_check(cx) {
            if self.rxtshift > retries {
                cx.stats.inc(tcps::TIMEOUTDROP);
            }
            self.rxtshift = retries;
            return self.drop(so, cx, Errno::ETIMEDOUT);
        }
        if self.state == State::SynSent {
            // `cc_conn_init` then starts from one segment.
            self.snd_cwnd = 1;
        } else if self.rxtshift == 1 {
            // Remember the window, to put back if this proves a spurious
            // retransmission.
            self.snd_cwnd_prev = self.snd_cwnd;
            self.snd_ssthresh_prev = self.snd_ssthresh;
            self.snd_recover_prev = self.snd_recover;
            if self.flags & tf::FASTRECOVERY != 0 {
                self.flags |= tf::WASFRECOVERY;
            } else {
                self.flags &= !tf::WASFRECOVERY;
            }
            if self.flags & tf::CONGRECOVERY != 0 {
                self.flags |= tf::WASCRECOVERY;
            } else {
                self.flags &= !tf::WASCRECOVERY;
            }
            if self.flags & tf::RCVD_TSTMP == 0 {
                self.badrxtwin = cx.now + (self.srtt as u64 >> (RTT_SHIFT + 1));
            }
            self.flags |= tf::PREVVALID;
            self.resend_sackholes();
        } else {
            self.flags &= !tf::PREVVALID;
            self.free_sackholes(cx.stats);
        }
        cx.stats.inc(tcps::REXMTTIMEO);
        let rexmt = if matches!(self.state, State::SynSent | State::SynReceived) {
            get(&cx.conf.rexmit_initial) as u32 * BACKOFF[self.rxtshift as usize]
        } else {
            self.rexmtval() * BACKOFF[self.rxtshift as usize]
        };
        self.rxtcur = rangeset(cx.conf, rexmt, self.rttmin, get(&cx.conf.rexmit_max) as u32);
        self.pmtud_blackhole(cx);
        // Past a few backoffs the route may be bad (`in_losing`).
        if self.rxtshift > RTT_INVALIDATE {
            self.losing = true;
        }
        self.snd_nxt = self.snd_una;
        self.snd_recover = self.snd_max;
        self.flags |= tf::ACKNOW;
        self.rtttime = 0;
        if self.state != State::SynSent {
            self.cc_cong_signal(None, Signal::Rto, cx.stats);
        }
        self.output(so, cx)
    }

    /// Path MTU black-hole detection on retransmission timeouts
    /// (`net.inet.tcp.pmtud_blackhole_detection`, off by default).
    fn pmtud_blackhole(&mut self, cx: &Ctx) {
        let detect = get(&cx.conf.pmtud_blackhole_detect);
        let v6 = self.inc.v6;
        let on = detect == 1 || (detect == 2 && !v6) || (detect == 3 && v6);
        if !on || !matches!(self.state, State::Established | State::FinWait1) {
            return;
        }
        let (bh_mss, dflt) = if v6 {
            (
                get(&cx.conf.v6pmtud_blackhole_mss) as u32,
                get(&cx.conf.v6mssdflt) as u32,
            )
        } else {
            (
                get(&cx.conf.pmtud_blackhole_mss) as u32,
                get(&cx.conf.mssdflt) as u32,
            )
        };
        if self.rxtshift == 1 {
            self.blackhole_enter = 2;
            self.blackhole_exit = 2;
            if self.t_maxseg > bh_mss {
                self.blackhole_exit += 2;
            }
            if self.t_maxseg > dflt && bh_mss > dflt {
                self.blackhole_exit += 2;
            }
        }
        let want = tf2::PLPMTU_PMTUD | tf2::PLPMTU_MAXSEGSNT;
        if self.flags2 & want == want
            && self.rxtshift >= self.blackhole_enter
            && self.rxtshift < self.blackhole_exit
            && (self.rxtshift - self.blackhole_enter).is_multiple_of(2)
        {
            if self.flags2 & tf2::PLPMTU_BLACKHOLE == 0 {
                self.flags2 |= tf2::PLPMTU_BLACKHOLE;
                self.pmtud_saved_maxseg = self.t_maxseg;
            }
            if self.t_maxseg > bh_mss && bh_mss > dflt {
                self.t_maxseg = bh_mss;
                cx.stats.inc(tcps::PMTUD_BLACKHOLE_ACTIVATED);
            } else {
                self.t_maxseg = dflt;
                self.flags2 &= !tf2::PLPMTU_PMTUD;
                cx.stats.inc(tcps::PMTUD_BLACKHOLE_ACTIVATED_MIN_MSS);
            }
            self.cc_algo_conn_init();
        } else if self.flags2 & tf2::PLPMTU_BLACKHOLE != 0 && self.rxtshift >= self.blackhole_exit {
            self.flags2 |= tf2::PLPMTU_PMTUD;
            self.flags2 &= !tf2::PLPMTU_BLACKHOLE;
            self.t_maxseg = self.pmtud_saved_maxseg;
            if self.t_maxseg < get(&cx.conf.mssdflt) as u32 {
                self.flags2 |= tf2::PROC_SACK_PROHIBIT;
            } else {
                self.flags2 &= !tf2::PROC_SACK_PROHIBIT;
            }
            cx.stats.inc(tcps::PMTUD_BLACKHOLE_FAILED);
            self.cc_algo_conn_init();
        }
    }
}
