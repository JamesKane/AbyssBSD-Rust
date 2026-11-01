// SPDX-License-Identifier: BSD-2-Clause
//! Congestion control (`sys/netinet/cc/`): FreeBSD's `cc` interface with
//! its NewReno and CUBIC (the default), and the `cc_*` calls `tcp_input`
//! and the timers make into it.
//!
//! HyStart++ is left out as the default stack leaves it: only RACK and
//! BBR feed it RTT samples and rounds (`CCF_HYSTART_ALLOWED`), so with
//! this stack CUBIC's slow start is NewReno's.
//!
//! Behavior reference: sys/netinet/cc/cc.c, cc.h, cc_newreno.c,
//! cc_cubic.c, cc_cubic.h; sys/netinet/tcp_input.c (`cc_ack_received`,
//! `cc_conn_init`, `cc_cong_signal`, `cc_post_recovery`,
//! `tcp_compute_initwnd`); sys/netinet/tcp_sack.c (`tcp_compute_pipe`).

use crate::hdr::{MAX_WINSHIFT, MAXWIN};
use crate::seq;
use crate::tcb::{Tcpcb, tf, tf2};
use crate::{Conf, Ctx, get, on, timer};

/// `CCF_*`.
pub mod ccf {
    pub const ABC_SENTAWND: u32 = 0x0001;
    pub const CWND_LIMITED: u32 = 0x0002;
    pub const USE_LOCAL_ABC: u32 = 0x0004;
    pub const ACKNOW: u32 = 0x0008;
    pub const IPHDR_CE: u32 = 0x0010;
    pub const TCPHDR_CWR: u32 = 0x0020;
}

/// What an acknowledgement or a congestion signal is (`ccsignal_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// `CC_ACK`.
    Ack,
    /// `CC_DUPACK`.
    DupAck,
    /// `CC_ECN`.
    Ecn,
    /// `CC_RTO`.
    Rto,
    /// `CC_RTO_ERR`.
    RtoErr,
    /// `CC_NDUPACK`.
    NDupAck,
}

/// `struct cc_var` without its back pointers.
#[derive(Clone, Copy, Debug, Default)]
pub struct CcVar {
    pub bytes_this_ack: u32,
    pub curack: u32,
    pub flags: u32,
    pub nsegs: u16,
    pub labc: u8,
}

/// The algorithms (`net.inet.tcp.cc.available`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algo {
    NewReno,
    Cubic,
}

impl Algo {
    /// Its name (`TCP_CONGESTION`, `net.inet.tcp.cc.algorithm`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NewReno => "newreno",
            Self::Cubic => "cubic",
        }
    }

    #[must_use]
    pub fn by_name(n: &[u8]) -> Option<Self> {
        match n {
            b"newreno" => Some(Self::NewReno),
            b"cubic" => Some(Self::Cubic),
            _ => None,
        }
    }

    /// Every algorithm, in `net.inet.tcp.cc.available`'s order.
    pub const ALL: [Self; 2] = [Self::NewReno, Self::Cubic];
}

/// A connection's algorithm and its data (`t_cc`, `cc_data`).
#[derive(Clone, Copy, Debug)]
pub enum Cc {
    NewReno,
    Cubic(Cubic),
}

impl Cc {
    /// `cb_init`.
    #[must_use]
    pub fn new(a: Algo) -> Self {
        match a {
            Algo::NewReno => Self::NewReno,
            Algo::Cubic => Self::Cubic(Cubic::new()),
        }
    }

    #[must_use]
    pub const fn algo(&self) -> Algo {
        match self {
            Self::NewReno => Algo::NewReno,
            Self::Cubic(_) => Algo::Cubic,
        }
    }
}

/// CUBIC's per-connection data (`struct cubic`), the parts the default
/// stack uses.
#[derive(Clone, Copy, Debug)]
pub struct Cubic {
    k: i64,
    sum_rtt_usecs: i64,
    w_max: u32,
    cwnd_epoch: u32,
    flags: u32,
    min_rtt_usecs: i32,
    mean_rtt_usecs: i32,
    epoch_ack_count: i32,
    t_epoch: u64,
    undo_t_epoch: u64,
    undo_k: i64,
    undo_w_max: u32,
    undo_cwnd_epoch: u32,
}

/// `CUBICFLAG_*`.
mod cf {
    pub(super) const CONG_EVENT: u32 = 0x01;
    pub(super) const IN_SLOWSTART: u32 = 0x02;
    pub(super) const IN_APPLIMIT: u32 = 0x04;
    pub(super) const RTO_EVENT: u32 = 0x08;
    pub(super) const HYSTART_ENABLED: u32 = 0x10;
    pub(super) const HYSTART_IN_CSS: u32 = 0x20;
    pub(super) const IN_TF: u32 = 0x40;
}

const CUBIC_SHIFT: u32 = 8;
const CUBIC_SHIFT_4: u32 = 32;
const CUBIC_BETA: u64 = 179;
const CUBIC_C_FACTOR: i64 = 102;
const CUBIC_FC_FACTOR: u64 = 217;
const CUBIC_MIN_RTT_SAMPLES: u8 = 8;
const CUBED_ROOT_MAX_ULONG: i64 = 448_845;
/// Microseconds per tick (`tick`) and ticks per second (`hz`).
const TICK: i64 = 1000;
const HZ: i64 = 1000;

impl Cubic {
    /// `cubic_cb_init`.
    const fn new() -> Self {
        Self {
            k: 0,
            sum_rtt_usecs: 0,
            w_max: 0,
            cwnd_epoch: 0,
            flags: cf::HYSTART_ENABLED,
            min_rtt_usecs: timer::SRTTBASE as i32,
            mean_rtt_usecs: 1,
            epoch_ack_count: 0,
            t_epoch: 0,
            undo_t_epoch: 0,
            undo_k: 0,
            undo_w_max: 0,
            undo_cwnd_epoch: 0,
        }
    }
}

/// `cubic_k`: the time, in seconds with `CUBIC_SHIFT` bits of fraction,
/// to grow from `cwnd_epoch` back to `wmax` (both in segments).
fn cubic_k(wmax_segs: u32, cwnd_epoch_segs: u32) -> i64 {
    if wmax_segs <= cwnd_epoch_segs {
        return 0;
    }
    let mut s = (i64::from(wmax_segs - cwnd_epoch_segs) << (2 * CUBIC_SHIFT)) / CUBIC_C_FACTOR;
    let mut p = 0;
    while s >= 256 {
        s >>= 3;
        p += 1;
    }
    let k = (((s * 275) >> CUBIC_SHIFT) + 98) - (((s * s * 120) >> CUBIC_SHIFT) >> CUBIC_SHIFT);
    k << p
}

/// `cubic_cwnd`: the window `usecs_since_epoch` into an epoch.
fn cubic_cwnd(usecs_since_epoch: i64, wmax: u32, smss: u32, k: i64) -> u32 {
    let mut c = ((usecs_since_epoch << CUBIC_SHIFT) - (k * HZ * TICK)) / (HZ * TICK);
    if c > CUBED_ROOT_MAX_ULONG {
        return i32::MAX as u32;
    }
    if c < -CUBED_ROOT_MAX_ULONG {
        return 0;
    }
    c = c * c * c;
    let w = ((c * CUBIC_C_FACTOR) >> CUBIC_SHIFT_4) * i64::from(smss) + i64::from(wmax);
    w.clamp(0, i64::from(u32::MAX)) as u32
}

/// `tcp_compute_initwnd`: the initial window for segments of `maxseg`.
#[must_use]
pub fn compute_initwnd(conf: &Conf, maxseg: u32) -> u32 {
    let segs = get(&conf.initcwnd_segments) as u32;
    if segs != 0 {
        (segs * maxseg).min((2 * maxseg).max(segs * 1460))
    } else if on(&conf.rfc3390) {
        (4 * maxseg).min((2 * maxseg).max(4380))
    } else if maxseg > 2190 {
        2 * maxseg
    } else if maxseg > 1095 {
        3 * maxseg
    } else {
        4 * maxseg
    }
}

/// `V_newreno_beta`, `V_newreno_beta_ecn` (percent).
const NEWRENO_BETA: u64 = 50;

impl Tcpcb {
    fn in_fastrecovery(&self) -> bool {
        self.flags & tf::FASTRECOVERY != 0
    }

    fn in_congrecovery(&self) -> bool {
        self.flags & tf::CONGRECOVERY != 0
    }

    /// `IN_RECOVERY`.
    pub(crate) fn in_recovery(&self) -> bool {
        self.flags & (tf::FASTRECOVERY | tf::CONGRECOVERY) != 0
    }

    /// `tcp_compute_pipe` (`tcp_do_newsack`'s form).
    #[must_use]
    pub fn compute_pipe(&self) -> u32 {
        let pipe = i64::from(self.snd_max.wrapping_sub(self.snd_una))
            + i64::from(self.sackhint.sack_bytes_rexmit)
            - i64::from(self.sackhint.sacked_bytes)
            - i64::from(self.sackhint.lost_bytes);
        pipe.max(0) as u32
    }

    fn maxwin(&self) -> u32 {
        MAXWIN << self.snd_scale.min(MAX_WINSHIFT)
    }

    /// `newreno_cc_cwnd_on_multiplicative_decrease` with `factor`.
    fn decrease(&self, factor: u64, mss: u32) -> u32 {
        let segs = (u64::from(self.snd_cwnd) * factor) / (100 * u64::from(mss));
        segs.max(2) as u32 * mss
    }

    /// `newreno_cc_cwnd_in_cong_avoid`.
    fn cwnd_in_cong_avoid(&mut self, conf: &Conf) -> u32 {
        let cw = self.snd_cwnd;
        let mut incr = self.fixed_maxseg();
        if on(&conf.rfc3465) {
            if self.ccv.flags & ccf::ABC_SENTAWND != 0 {
                self.ccv.flags &= !ccf::ABC_SENTAWND;
            } else {
                incr = 0;
            }
        } else {
            incr = (incr * incr / cw.max(1)).max(1);
        }
        if incr > 0 {
            (cw + incr).min(self.maxwin())
        } else {
            cw
        }
    }

    /// `newreno_cc_cwnd_in_slow_start` (and CUBIC's slow start, without
    /// HyStart++).
    fn cwnd_in_slow_start(&self, conf: &Conf) -> u32 {
        let cw = self.snd_cwnd;
        let mss = self.fixed_maxseg();
        let mut incr = mss;
        if on(&conf.rfc3465) {
            let abc = if self.ccv.flags & ccf::USE_LOCAL_ABC != 0 {
                u32::from(self.ccv.labc)
            } else {
                get(&conf.abc_l_var) as u32
            };
            incr = if self.snd_nxt == self.snd_max {
                self.ccv
                    .bytes_this_ack
                    .min(u32::from(self.ccv.nsegs) * abc * mss)
            } else {
                self.ccv.bytes_this_ack.min(mss)
            };
        }
        if incr > 0 {
            cw.saturating_add(incr).min(self.maxwin())
        } else {
            cw
        }
    }

    /// `cubic_record_rtt`.
    fn cubic_record_rtt(&self, d: &mut Cubic) {
        if self.rttupdated >= CUBIC_MIN_RTT_SAMPLES {
            // `tcp_get_srtt(tp, TCP_TMR_GRANULARITY_USEC)`.
            let srtt_us = ((i64::from(self.srtt) * TICK) >> timer::RTT_SHIFT) as i32;
            if srtt_us < d.min_rtt_usecs || d.min_rtt_usecs == timer::SRTTBASE as i32 {
                d.min_rtt_usecs = ((TICK >> timer::RTT_SHIFT) as i32).max(srtt_us);
                if d.min_rtt_usecs > d.mean_rtt_usecs {
                    d.mean_rtt_usecs = d.min_rtt_usecs;
                }
            }
            d.sum_rtt_usecs += i64::from(srtt_us);
            d.epoch_ack_count += 1;
        }
    }

    /// The algorithm's `ack_received`.
    fn algo_ack_received(&mut self, cx: &Ctx, ty: Signal) {
        let limited = self.ccv.flags & ccf::CWND_LIMITED != 0;
        match self.cc {
            Cc::NewReno => {
                if ty == Signal::Ack && !self.in_recovery() && limited {
                    self.snd_cwnd = if self.snd_cwnd > self.snd_ssthresh {
                        self.cwnd_in_cong_avoid(cx.conf)
                    } else {
                        self.cwnd_in_slow_start(cx.conf)
                    };
                }
            }
            Cc::Cubic(mut d) => {
                let mss = self.fixed_maxseg();
                let cwin = self.snd_cwnd;
                self.cubic_record_rtt(&mut d);
                if ty == Signal::Ack && !self.in_recovery() && limited {
                    if cwin <= self.snd_ssthresh || d.min_rtt_usecs == timer::SRTTBASE as i32 {
                        d.flags |= cf::IN_SLOWSTART;
                        self.snd_cwnd = self.cwnd_in_slow_start(cx.conf);
                    } else {
                        if d.flags & cf::HYSTART_IN_CSS != 0 {
                            d.flags &= !(cf::HYSTART_IN_CSS | cf::HYSTART_ENABLED);
                        }
                        if d.flags & (cf::IN_SLOWSTART | cf::CONG_EVENT | cf::IN_APPLIMIT) != 0 {
                            d.t_epoch = cx.now;
                            d.cwnd_epoch = cwin;
                            d.k = cubic_k(d.w_max / mss, d.cwnd_epoch / mss);
                            d.flags &= !(cf::IN_SLOWSTART | cf::CONG_EVENT | cf::IN_APPLIMIT);
                            if d.flags & cf::RTO_EVENT != 0 {
                                d.flags &= !cf::RTO_EVENT;
                                d.w_max = cwin;
                                d.k = 0;
                            }
                        }
                        let mut since = (cx.now.wrapping_sub(d.t_epoch) as i64) * TICK;
                        if since < 0 || since > i64::from(i32::MAX) {
                            since = i64::from(i32::MAX);
                            d.t_epoch = cx.now.wrapping_sub((i64::from(i32::MAX) / TICK) as u64);
                        }
                        // `tf_cwnd`: NewReno's growth is the friendly one.
                        let w_est = self.cwnd_in_cong_avoid(cx.conf);
                        let w_cubic =
                            cubic_cwnd(since + i64::from(d.mean_rtt_usecs), d.w_max, mss, d.k);
                        if w_cubic < w_est {
                            self.snd_cwnd = w_est;
                            d.flags |= cf::IN_TF;
                        } else {
                            let target = if w_cubic < cwin {
                                cwin
                            } else if u64::from(w_cubic) > (u64::from(cwin) * 3) >> 1 {
                                ((u64::from(cwin) * 3) >> 1) as u32
                            } else {
                                w_cubic
                            };
                            let incr = ((((u64::from(target - cwin)) << CUBIC_SHIFT)
                                / u64::from(cwin.max(1))
                                * u64::from(mss))
                                >> CUBIC_SHIFT) as u32;
                            self.snd_cwnd = cwin + incr;
                        }
                    }
                } else if ty == Signal::Ack && !self.in_recovery() && !limited {
                    d.flags |= cf::IN_APPLIMIT;
                }
                self.cc = Cc::Cubic(d);
            }
        }
    }

    /// `cubic_get_ssthresh`.
    fn cubic_get_ssthresh(&self, d: &mut Cubic, mss: u32) -> u32 {
        let mut cwnd = self.snd_cwnd;
        if cwnd < d.w_max {
            cwnd = ((u64::from(cwnd) * CUBIC_FC_FACTOR) >> CUBIC_SHIFT) as u32;
        }
        d.w_max = cwnd;
        if d.flags & cf::IN_TF != 0 {
            self.decrease(NEWRENO_BETA, mss)
        } else {
            ((u64::from(self.compute_pipe()) * CUBIC_BETA) >> CUBIC_SHIFT) as u32
        }
    }

    /// The algorithm's `cong_signal`.
    fn algo_cong_signal(&mut self, ty: Signal) {
        let mss = self.fixed_maxseg();
        match self.cc {
            Cc::NewReno => {
                // ABE is off (`net.inet.tcp.cc.abe`): ECN takes the loss beta.
                let cwin = self.decrease(NEWRENO_BETA, mss);
                match ty {
                    Signal::NDupAck => {
                        if !self.in_fastrecovery() {
                            if !self.in_congrecovery() {
                                self.snd_ssthresh = cwin;
                            }
                            self.flags |= tf::FASTRECOVERY | tf::CONGRECOVERY;
                        }
                    }
                    Signal::Ecn => {
                        if !self.in_congrecovery() {
                            self.snd_ssthresh = cwin;
                            self.snd_cwnd = cwin;
                            self.flags |= tf::CONGRECOVERY;
                        }
                    }
                    Signal::Rto => {
                        if self.rxtshift == 1 {
                            let pipe = u64::from(self.compute_pipe());
                            let w = u64::from(self.snd_wnd).min(pipe);
                            self.snd_ssthresh =
                                ((w * NEWRENO_BETA) / (100 * u64::from(mss))).max(2) as u32 * mss;
                        }
                        self.snd_cwnd = mss;
                    }
                    _ => {}
                }
            }
            Cc::Cubic(mut d) => {
                match ty {
                    Signal::NDupAck => {
                        d.flags &= !(cf::HYSTART_ENABLED | cf::HYSTART_IN_CSS);
                        if !self.in_fastrecovery() {
                            if !self.in_congrecovery() {
                                let ss = self.cubic_get_ssthresh(&mut d, mss);
                                self.snd_ssthresh = ss.max(2 * mss);
                                d.flags |= cf::CONG_EVENT;
                            }
                            self.flags |= tf::FASTRECOVERY | tf::CONGRECOVERY;
                        }
                    }
                    Signal::Ecn => {
                        d.flags &= !(cf::HYSTART_ENABLED | cf::HYSTART_IN_CSS);
                        if !self.in_congrecovery() {
                            let ss = self.cubic_get_ssthresh(&mut d, mss);
                            self.snd_ssthresh = ss.max(2 * mss);
                            self.snd_cwnd = ss.max(mss);
                            d.flags |= cf::CONG_EVENT;
                            self.flags |= tf::CONGRECOVERY;
                        }
                    }
                    Signal::Rto => {
                        if self.rxtshift == 1 {
                            d.undo_t_epoch = d.t_epoch;
                            d.undo_cwnd_epoch = d.cwnd_epoch;
                            d.undo_w_max = d.w_max;
                            d.undo_k = d.k;
                            let pipe = self.compute_pipe();
                            let w = u64::from(self.snd_wnd.min(pipe));
                            self.snd_ssthresh = (((w * CUBIC_BETA) >> CUBIC_SHIFT) / u64::from(mss))
                                .max(2) as u32
                                * mss;
                        }
                        d.flags |= cf::RTO_EVENT;
                        self.snd_cwnd = mss;
                    }
                    Signal::RtoErr => {
                        d.flags &= !cf::RTO_EVENT;
                        d.k = d.undo_k;
                        d.w_max = d.undo_w_max;
                        d.cwnd_epoch = d.undo_cwnd_epoch;
                        d.t_epoch = d.undo_t_epoch;
                    }
                    _ => {}
                }
                self.cc = Cc::Cubic(d);
            }
        }
    }

    /// `cc_ack_received`: an acknowledgement of new data (or a
    /// duplicate), `nsegs` segments coalesced, `ack` its number.
    pub(crate) fn cc_ack_received(&mut self, cx: &Ctx, ack: u32, nsegs: u16, ty: Signal) {
        self.ccv.nsegs = nsegs;
        self.ccv.bytes_this_ack = ack.wrapping_sub(self.snd_una);
        let limited = if on(&cx.conf.newcwv) {
            self.snd_cwnd <= self.snd_wnd
                && u64::from(self.snd_cwnd) < u64::from(self.compute_pipe()) * 2
        } else {
            self.snd_cwnd <= self.snd_wnd
        };
        if limited {
            self.ccv.flags |= ccf::CWND_LIMITED;
        } else {
            self.ccv.flags &= !ccf::CWND_LIMITED;
        }
        if ty == Signal::Ack {
            if self.snd_cwnd > self.snd_ssthresh {
                self.bytes_acked += self.ccv.bytes_this_ack;
                if self.bytes_acked >= self.snd_cwnd {
                    self.bytes_acked -= self.snd_cwnd;
                    self.ccv.flags |= ccf::ABC_SENTAWND;
                }
            } else {
                self.ccv.flags &= !ccf::ABC_SENTAWND;
                self.bytes_acked = 0;
            }
        }
        self.ccv.curack = ack;
        self.algo_ack_received(cx, ty);
    }

    /// `cc_conn_init`: the window and the timers a connection starts
    /// with, from what the host cache knows of the peer.
    pub(crate) fn cc_conn_init(&mut self, cx: &Ctx, hc: &crate::hostcache::Metrics) {
        let maxseg = self.maxseg();
        if self.srtt == 0 && hc.rtt != 0 {
            self.srtt = hc.rtt as i32;
            cx.stats.inc(crate::tcps::USEDRTT);
            if hc.rttvar != 0 {
                self.rttvar = hc.rttvar as i32;
                cx.stats.inc(crate::tcps::USEDRTTVAR);
            } else {
                self.rttvar = self.srtt * timer::RTTVAR_SCALE / timer::RTT_SCALE;
            }
            self.rxtcur = timer::rangeset(
                cx.conf,
                (((self.srtt >> 2) + self.rttvar) >> 1) as u32,
                self.rttmin,
                get(&cx.conf.rexmit_max) as u32,
            );
        }
        if hc.ssthresh != 0 {
            self.snd_ssthresh = (2 * maxseg).max(hc.ssthresh);
            cx.stats.inc(crate::tcps::USEDSSTHRESH);
        }
        self.snd_cwnd = if self.snd_cwnd == 1 {
            maxseg
        } else {
            compute_initwnd(cx.conf, maxseg)
        };
        self.cc_algo_conn_init();
    }

    /// The algorithm's `conn_init` (`cubic_conn_init`: no `W_max` yet).
    pub(crate) fn cc_algo_conn_init(&mut self) {
        if let Cc::Cubic(ref mut d) = self.cc {
            d.w_max = u32::MAX;
        }
    }

    /// `cc_cong_signal`.
    pub(crate) fn cc_cong_signal(&mut self, ack: Option<u32>, ty: Signal, stats: &crate::Stats) {
        match ty {
            Signal::NDupAck => {
                if !self.in_fastrecovery() {
                    self.snd_recover = self.snd_max;
                    if self.flags2 & tf2::ECN_PERMIT != 0 {
                        self.flags2 |= tf2::ECN_SND_CWR;
                    }
                }
            }
            Signal::Ecn => {
                if !self.in_congrecovery() || ack.is_some_and(|a| seq::geq(a, self.snd_recover)) {
                    self.flags &= !tf::CONGRECOVERY;
                    stats.inc(crate::tcps::ECN_RCWND);
                    self.snd_recover = self.snd_max.wrapping_add(1);
                    if self.flags2 & tf2::ECN_PERMIT != 0 {
                        self.flags2 |= tf2::ECN_SND_CWR;
                    }
                }
            }
            Signal::Rto => {
                self.dupacks = 0;
                self.bytes_acked = 0;
                self.flags &= !(tf::FASTRECOVERY | tf::CONGRECOVERY);
                if self.flags2 & tf2::ECN_PERMIT != 0 {
                    self.flags2 |= tf2::ECN_SND_CWR;
                }
            }
            Signal::RtoErr => {
                stats.inc(crate::tcps::SNDREXMITBAD);
                self.snd_cwnd = self.snd_cwnd_prev;
                self.snd_ssthresh = self.snd_ssthresh_prev;
                self.snd_recover = self.snd_recover_prev;
                if self.flags & tf::WASFRECOVERY != 0 {
                    self.flags |= tf::FASTRECOVERY;
                }
                if self.flags & tf::WASCRECOVERY != 0 {
                    self.flags |= tf::CONGRECOVERY;
                }
                self.snd_nxt = self.snd_max;
                self.flags &= !tf::PREVVALID;
                self.rxtshift = 0;
                self.badrxtwin = 0;
            }
            Signal::Ack | Signal::DupAck => {}
        }
        if seq::lt(self.snd_fack, self.snd_una) || seq::gt(self.snd_fack, self.snd_max) {
            self.snd_fack = self.snd_una;
        }
        if let Some(a) = ack {
            self.ccv.curack = a;
        }
        self.algo_cong_signal(ty);
    }

    /// `cc_post_recovery`: fast recovery ends with `ack`.
    pub(crate) fn cc_post_recovery(&mut self, ack: u32) {
        if seq::lt(self.snd_fack, ack) || seq::gt(self.snd_fack, self.snd_max) {
            self.snd_fack = ack;
        }
        self.ccv.curack = ack;
        let mss = self.fixed_maxseg();
        let pipe = self.compute_pipe();
        let fast = self.in_fastrecovery();
        match self.cc {
            Cc::NewReno => {
                if fast {
                    self.snd_cwnd = if pipe < self.snd_ssthresh {
                        pipe.max(mss) + mss
                    } else {
                        self.snd_ssthresh
                    };
                }
            }
            Cc::Cubic(mut d) => {
                if fast {
                    self.snd_cwnd = if pipe < self.snd_ssthresh {
                        pipe.max(mss) + mss
                    } else {
                        (((u64::from(d.w_max) * CUBIC_BETA) >> CUBIC_SHIFT) as u32).max(2 * mss)
                    };
                }
                if d.epoch_ack_count > 0 && d.sum_rtt_usecs >= i64::from(d.epoch_ack_count) {
                    d.mean_rtt_usecs = (d.sum_rtt_usecs / i64::from(d.epoch_ack_count)) as i32;
                }
                d.epoch_ack_count = 0;
                d.sum_rtt_usecs = 0;
                self.cc = Cc::Cubic(d);
            }
        }
        self.flags &= !(tf::FASTRECOVERY | tf::CONGRECOVERY);
        self.bytes_acked = 0;
        self.sackhint.delivered_data = 0;
        self.sackhint.prr_delivered = 0;
        self.sackhint.prr_out = 0;
    }

    /// `cc_after_idle`: the window after the connection sat idle longer
    /// than its RTO.
    pub(crate) fn cc_after_idle(&mut self, cx: &Ctx) {
        let mss = self.fixed_maxseg();
        if let Cc::Cubic(ref mut d) = self.cc {
            d.w_max = d.w_max.max(self.snd_cwnd);
            d.k = cubic_k(d.w_max / mss, d.cwnd_epoch / mss);
            if d.flags & cf::HYSTART_ENABLED == 0 {
                d.flags &= !cf::HYSTART_IN_CSS;
                d.flags |= cf::HYSTART_ENABLED;
            }
        }
        // `newreno_cc_after_idle`.
        let rw = compute_initwnd(cx.conf, mss);
        self.snd_ssthresh = self.snd_ssthresh.max(self.snd_cwnd - (self.snd_cwnd >> 2));
        self.snd_cwnd = rw.min(self.snd_cwnd);
        if let Cc::Cubic(ref mut d) = self.cc {
            d.t_epoch = cx.now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_window_is_ten_segments() {
        let c = Conf::new();
        assert_eq!(compute_initwnd(&c, 1448), 14480);
        // Small segments: no more than ten of 1460.
        assert_eq!(compute_initwnd(&c, 9000), 18000);
    }

    #[test]
    fn cubic_k_matches_the_cube_root() {
        // K = cbrt((W_max - cwnd) / C), C = 0.4: 100 segments -> 6.3 s.
        let k = cubic_k(1000, 900);
        assert!((k - (6.3 * 256.0) as i64).abs() < 40, "{k}");
        assert_eq!(cubic_k(10, 20), 0);
    }

    #[test]
    fn cubic_cwnd_reaches_wmax_at_k() {
        let k = cubic_k(1000, 700);
        let at_k = cubic_cwnd((k * 1_000_000) >> 8, 1000 * 1448, 1448, k);
        let wmax = 1000 * 1448;
        assert!(at_k.abs_diff(wmax) < 3 * 1448, "{at_k}");
        assert!(cubic_cwnd(0, wmax, 1448, k) < wmax);
    }
}
