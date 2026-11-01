// SPDX-License-Identifier: BSD-2-Clause
//! What a segment does to a connection (`tcp_input.c`'s
//! `tcp_do_segment`): the header-prediction fast paths, the SYN_SENT
//! and SYN_RECEIVED handshakes, RFC 5961's checks of resets, SYNs, and
//! acknowledgements (with challenge ACKs), PAWS, trimming to the window,
//! acknowledgement processing (RTT, the send buffer, fast retransmit and
//! recovery with SACK, PRR, and limited transmit), the window update,
//! urgent data, in-order and reassembled data with SACK and D-SACK
//! reports, the FIN, and TIME_WAIT (`tcp_twstart`, `tcp_twcheck`).
//!
//! The caller has found the connection, checked the checksum, and passes
//! the header, the options' bytes, and the data alone.
//!
//! Behavior reference: sys/netinet/tcp_input.c, sys/netinet/tcp_timewait.c,
//! sys/netinet/tcp_subr.c (`tcp_send_challenge_ack`,
//! `tcp_challenge_ack_check`).

use mbuf::Chain;

use crate::cc::Signal;
use crate::hdr::{Accept, MAXWIN, Opts, Th, th, tof};
use crate::sack::{REXMTTHRESH, SackStatus};
use crate::seq::{self, Seq};
use crate::tcb::{Tcpcb, oob, tf, tf2};
use crate::timer::Tt;
use crate::{Ctx, Errno, Fate, Sock, State, get, on, so, tcps};

/// `TCP_PAWS_IDLE`: a timestamp older than this is stale, not a replay.
const PAWS_IDLE: u32 = 24 * 24 * 60 * 60 * 1000;

/// A received segment, as `tcp_do_segment` gets it.
#[derive(Debug)]
pub struct Segment<'a> {
    pub th: Th,
    /// The option bytes after the fixed header.
    pub opts: &'a [u8],
    /// The data, the headers gone.
    pub data: Chain,
    /// The IP header's TOS or traffic class (its ECN bits).
    pub iptos: u8,
    /// Segments coalesced into this one (`lro_nsegs`), at least 1.
    pub nsegs: u16,
}

/// What `tcp_twcheck` decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TwVerdict {
    /// Handled (dropped, acknowledged, or reset).
    Done,
    /// A new SYN recycles the connection: it is closed, and the segment
    /// should be looked up again (`goto findpcb`).
    Recycle,
}

/// The reset `tcp_dropwithreset` answers a segment with when there is no
/// connection: `None` for a reset (never answered).
#[must_use]
pub fn reset_for(t: &Th, tlen: u32) -> Option<Th> {
    if t.has(th::RST) {
        return None;
    }
    let (sq, ack, flags) = if t.has(th::ACK) {
        (t.ack, 0, th::RST)
    } else {
        let mut l = tlen;
        if t.has(th::SYN) {
            l += 1;
        }
        if t.has(th::FIN) {
            l += 1;
        }
        (0, t.seq.wrapping_add(l), th::RST | th::ACK)
    };
    Some(Th {
        sport: t.dport,
        dport: t.sport,
        seq: sq,
        ack,
        off: 0,
        flags,
        win: 0,
        sum: 0,
        urp: 0,
    })
}

impl Tcpcb {
    /// `tcp_rcvseqinit`.
    pub(crate) fn rcvseqinit(&mut self) {
        self.rcv_nxt = self.irs.wrapping_add(1);
        self.rcv_adv = self.rcv_nxt;
    }

    /// `tcp_sendseqinit`.
    pub fn sendseqinit(&mut self) {
        self.snd_una = self.iss;
        self.snd_nxt = self.iss;
        self.snd_max = self.iss;
        self.snd_up = self.iss;
        self.snd_recover = self.iss;
    }

    /// `DELAY_ACK`.
    fn delay_ack(&self, cx: &Ctx, tlen: u32) -> bool {
        !self.timer_active(Tt::Delack)
            && self.flags & tf::RXWIN0SENT == 0
            && tlen <= self.t_maxseg
            && (on(&cx.conf.delayed_ack) || self.flags & tf::NEEDSYN != 0)
    }

    /// `tcp_is_sack_recovery`.
    fn is_sack_recovery(&self, to: &Opts) -> bool {
        self.flags & tf::SACK_PERMIT != 0 && (to.has(tof::SACK) || !self.snd_holes.is_empty())
    }

    /// `tcp_packets_this_ack`.
    fn packets_this_ack(&self, ack: Seq) -> u32 {
        let d = ack.wrapping_sub(self.snd_una);
        let m = self.t_maxseg.max(1);
        d / m + u32::from(!d.is_multiple_of(m))
    }

    /// `tcp_handle_wakeup`: readers were woken as data arrived.
    fn handle_wakeup(&mut self) {
        self.flags &= !tf::WAKESOR;
    }

    /// `tcp_challenge_ack_check`: RFC 5961's limit on challenge ACKs.
    fn challenge_ack_check(&mut self, cx: &Ctx) -> bool {
        let window = get(&cx.conf.ack_war_timewindow) as u64;
        let cnt = get(&cx.conf.ack_war_cnt) as u32;
        if window == 0 || cnt == 0 {
            return true;
        }
        if self.challenge_ack_end < cx.now {
            self.challenge_ack_cnt = 0;
            self.challenge_ack_end = cx.now + window;
        }
        if self.challenge_ack_cnt < cnt {
            self.challenge_ack_cnt += 1;
            true
        } else {
            false
        }
    }

    /// `tcp_send_challenge_ack`.
    fn send_challenge_ack<S: Sock>(&mut self, so: &S, cx: &mut Ctx) {
        if self.challenge_ack_check(cx) {
            let win = self.rcv_space_win(so);
            self.respond_win(cx, self.rcv_nxt, self.snd_nxt, th::ACK, Some(win));
            self.last_ack_sent = self.rcv_nxt;
        }
    }

    /// `tcp_dropwithreset` with a connection: a reset with its
    /// timestamps.
    fn dropwithreset(&mut self, cx: &mut Ctx, t: &Th, tlen: u32) {
        if let Some(r) = reset_for(t, tlen) {
            self.respond_win(cx, r.ack, r.seq, r.flags, None);
        }
    }

    /// `tcp_autorcvbuf`: the receive buffer's new size, if it should grow.
    fn autorcvbuf<S: Sock>(&mut self, so: &S, cx: &Ctx, tlen: u32) -> u32 {
        let mut newsize = 0;
        if on(&cx.conf.recvbuf_auto)
            && so.rcv_autosize()
            && self.srtt != 0
            && self.rfbuf_ts != 0
            && cx.ts_ticks().wrapping_sub(self.rfbuf_ts)
                > ((self.srtt >> crate::timer::RTT_SHIFT) / 2) as u32
        {
            let hiwat = so.rcv_hiwat();
            let max = get(&cx.conf.recvbuf_max) as u32;
            if self.rfbuf_cnt > (hiwat / 2) / 4 * 3 && hiwat < max {
                newsize = (hiwat + hiwat / 2).min(max);
            }
            self.rfbuf_ts = 0;
            self.rfbuf_cnt = 0;
        } else {
            self.rfbuf_cnt = self.rfbuf_cnt.wrapping_add(tlen);
        }
        newsize
    }

    /// An RTT sample from an acknowledgement: by its echoed timestamp, or
    /// by the timed segment.
    fn rtt_sample(&mut self, cx: &Ctx, to: &Opts, ack: Seq) {
        if to.has(tof::TS) && to.tsecr != 0 {
            let t = cx.ts_ticks().wrapping_sub(to.tsecr);
            if self.rttlow == 0 || self.rttlow as u32 > t {
                self.rttlow = t as i32;
            }
            self.xmit_timer(cx, t + 1);
        } else if self.rtttime != 0 && seq::gt(ack, self.rtseq) {
            let t = cx.now.saturating_sub(self.rtttime) as u32;
            if self.rttlow == 0 || self.rttlow as u32 > t {
                self.rttlow = t as i32;
            }
            self.xmit_timer(cx, t);
        }
    }

    /// `tcp_newreno_partial_ack`.
    fn newreno_partial_ack<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, ack: Seq) {
        let onxt = self.snd_nxt;
        let ocwnd = self.snd_cwnd;
        let maxseg = self.maxseg();
        let acked = ack.wrapping_sub(self.snd_una);
        self.timer_activate(Tt::Rexmt, 0, cx.now);
        self.rtttime = 0;
        if self.flags & tf::FASTRECOVERY != 0 {
            self.snd_nxt = ack;
            self.snd_cwnd = maxseg + acked;
            self.flags |= tf::ACKNOW;
            let _ = self.output(so, cx);
            self.snd_cwnd = ocwnd;
            if seq::gt(onxt, self.snd_nxt) {
                self.snd_nxt = onxt;
            }
        }
        self.snd_cwnd = self.snd_cwnd.saturating_sub(acked) + maxseg;
    }

    /// `tcp_do_prr_ack`: RFC 6937's proportional rate reduction.
    fn do_prr_ack(&mut self, to: &Opts, ack: Seq, changed: SackStatus, maxseg: &mut u32) {
        let _ = ack;
        if *maxseg == 0 {
            *maxseg = self.maxseg();
        }
        let maxseg = *maxseg as i64;
        let (del_data, pipe): (i64, i64);
        if self.is_sack_recovery(to)
            || (self.flags & tf::CONGRECOVERY != 0 && self.flags & tf::FASTRECOVERY == 0)
        {
            del_data = i64::from(self.sackhint.delivered_data);
            pipe = i64::from(self.compute_pipe());
        } else {
            del_data = if i64::from(self.sackhint.prr_delivered)
                < i64::from(REXMTTHRESH) * maxseg
                    + i64::from(self.snd_recover.wrapping_sub(self.snd_una))
            {
                maxseg
            } else {
                0
            };
            pipe = (i64::from(self.snd_max.wrapping_sub(self.snd_una))
                - i64::from(self.dupacks.min(i32::MAX as u32 / 65536)) * maxseg)
                .max(0);
        }
        self.sackhint.prr_delivered = (i64::from(self.sackhint.prr_delivered) + del_data) as u32;
        let ssthresh = i64::from(self.snd_ssthresh);
        let snd_cnt = if pipe >= ssthresh {
            if self.sackhint.recover_fs == 0 {
                self.sackhint.recover_fs = self.snd_nxt.wrapping_sub(self.snd_una).max(1);
            }
            let fs = i64::from(self.sackhint.recover_fs).max(1);
            let d = i64::from(self.sackhint.prr_delivered) * ssthresh;
            (d + fs - 1) / fs - i64::from(self.sackhint.prr_out) + maxseg - 1
        } else {
            let po = i64::from(self.sackhint.prr_delivered) - i64::from(self.sackhint.prr_out);
            let limit = if changed == SackStatus::NewLoss || del_data == 0 {
                po
            } else {
                po.max(del_data) + maxseg
            };
            (ssthresh - pipe).min(limit)
        };
        let snd_cnt = snd_cnt.max(0) / maxseg;
        if self.flags & tf::FASTRECOVERY != 0 {
            self.snd_cwnd = if self.is_sack_recovery(to) {
                (pipe - del_data + snd_cnt * maxseg).max(0) as u32
            } else {
                (i64::from(self.snd_max.wrapping_sub(self.snd_una)) + snd_cnt * maxseg) as u32
            };
        } else if self.flags & tf::CONGRECOVERY != 0 {
            self.snd_cwnd = (pipe - del_data + snd_cnt * maxseg).max(0) as u32;
        }
        self.snd_cwnd = self.snd_cwnd.max(maxseg as u32);
    }

    /// `tcp_twstart`: into TIME_WAIT for twice the MSL (ten milliseconds
    /// to a local address).
    pub(crate) fn twstart<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        self.set_state(State::TimeWait, cx.stats);
        self.free_sackholes(cx.stats);
        so.disconnected();
        if self.flags & tf::ACKNOW != 0 {
            let _ = self.output(so, cx);
        }
        let local = (cx.path)(&self.inc).localip;
        if on(&cx.conf.nolocaltimewait) && local {
            return self.close(so, cx);
        }
        let msl = if local {
            get(&cx.conf.msl_local)
        } else {
            get(&cx.conf.msl)
        } as u32;
        self.timer_activate(Tt::TwoMsl, 2 * msl, cx.now);
        Fate::Alive
    }

    /// `tcp_twcheck`: a segment for a connection in TIME_WAIT.
    pub fn twcheck<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, seg: &Segment) -> TwVerdict {
        let t = &seg.th;
        let flags = t.flags;
        let tlen = seg.data.len() as u32;
        let to = Opts::parse(
            seg.opts,
            Accept {
                syn: t.has(th::SYN),
                sack: on(&cx.conf.sack_enable),
            },
        );
        if flags & th::RST != 0 {
            return TwVerdict::Done;
        }
        let ack = |tp: &mut Self, cx: &mut Ctx| {
            tp.respond_win(cx, tp.rcv_nxt, tp.snd_nxt, th::ACK, Some(0));
            cx.stats.inc(tcps::TW_RESPONDS);
        };
        if to.has(tof::TS) && self.ts_recent != 0 && seq::ts_lt(to.tsval, self.ts_recent) {
            if flags & th::ACK != 0 {
                ack(self, cx);
            }
            return TwVerdict::Done;
        }
        if flags & th::SYN != 0 && flags & th::FIN != 0 && on(&cx.conf.drop_synfin) {
            return TwVerdict::Done;
        }
        // A new SYN beyond the old connection recycles it.
        if flags & (th::SYN | th::ACK) == th::SYN
            && ((self.flags & tf::RCVD_TSTMP != 0
                && to.has(tof::TS)
                && seq::ts_lt(self.ts_recent, to.tsval))
                || (self.flags & tf::RCVD_TSTMP == 0
                    && to.has(tof::TS)
                    && !on(&cx.conf.tolerate_missing_ts))
                || seq::gt(t.seq, self.rcv_nxt))
        {
            let _ = self.close(so, cx);
            cx.stats.inc(tcps::TW_RECYCLES);
            return TwVerdict::Recycle;
        }
        if flags & th::ACK == 0 {
            return TwVerdict::Done;
        }
        if !to.has(tof::TS) && self.ts_recent != 0 && !on(&cx.conf.tolerate_missing_ts) {
            return TwVerdict::Done;
        }
        if flags & th::FIN != 0 {
            let s = t
                .seq
                .wrapping_add(tlen)
                .wrapping_add(u32::from(flags & th::SYN != 0));
            if s.wrapping_add(1) == self.rcv_nxt {
                let local = (cx.path)(&self.inc).localip;
                let msl = if local {
                    get(&cx.conf.msl_local)
                } else {
                    get(&cx.conf.msl)
                } as u32;
                self.timer_activate(Tt::TwoMsl, 2 * msl, cx.now);
            }
        }
        if flags & (th::SYN | th::FIN) != 0
            || tlen != 0
            || t.seq != self.rcv_nxt
            || t.ack != self.snd_nxt
        {
            ack(self, cx);
        }
        TwVerdict::Done
    }

    /// `tcp_do_segment`: what segment `seg` does to the connection.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub fn do_segment<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, seg: Segment) -> Fate {
        let Segment {
            th: mut t,
            opts,
            data: mut m,
            iptos,
            nsegs,
        } = seg;
        let nsegs = nsegs.max(1);
        let mut tlen = m.len() as u32;
        let no_data = tlen == 0;
        let mut thflags = t.flags;
        let mut needoutput = false;
        let mut incforsyn = false;
        let ourfinisacked;
        let mut maxseg: u32 = 0;
        let mut sack_changed = SackStatus::NoChange;
        self.sackhint.last_sack_ack = 0;

        // Where the processing goes after the checks.
        enum Next {
            ProcessAck,
            Step6,
            Normal,
        }

        if thflags & th::SYN != 0 && thflags & th::FIN != 0 && on(&cx.conf.drop_synfin) {
            return Fate::Alive;
        }
        if self.state == State::SynSent
            && thflags & th::ACK != 0
            && (seq::leq(t.ack, self.iss) || seq::gt(t.ack, self.snd_max))
        {
            self.dropwithreset(cx, &t, tlen);
            return Fate::Alive;
        }
        self.rcvtime = cx.now;
        let tiwin = u32::from(t.win) << self.snd_scale;
        let pkts = self.packets_this_ack(t.ack);
        if self.ecn_input_segment(cx, thflags, pkts, iptos) != 0 {
            self.cc_cong_signal(Some(t.ack), Signal::Ecn, cx.stats);
        }
        let mut to = Opts::parse(
            opts,
            Accept {
                syn: thflags & th::SYN != 0,
                sack: on(&cx.conf.sack_enable),
            },
        );
        if to.has(tof::SACK) {
            cx.stats.add(tcps::SACK_RCV_BLOCKS, 1);
        }
        if self.flags2 & tf2::PROC_SACK_PROHIBIT != 0 {
            to.flags &= !tof::SACK;
        }
        if to.has(tof::TS) && to.tsecr != 0 {
            to.tsecr = to.tsecr.wrapping_sub(self.ts_offset);
            if seq::ts_gt(to.tsecr, cx.ts_ticks()) {
                to.tsecr = 0;
            }
        }
        // The SYN-ACK (or a crossing SYN) settles the options.
        if self.state == State::SynSent && thflags & th::SYN != 0 {
            self.ecn_input_parallel_syn(cx, thflags, iptos);
            if to.has(tof::SCALE) && self.flags & tf::REQ_SCALE != 0 && self.flags & tf::NOOPT == 0
            {
                self.flags |= tf::RCVD_SCALE;
                self.snd_scale = to.wscale;
            } else {
                self.flags &= !tf::REQ_SCALE;
            }
            self.snd_wnd = u32::from(t.win);
            if to.has(tof::TS) && self.flags & tf::REQ_TSTMP != 0 && self.flags & tf::NOOPT == 0 {
                self.flags |= tf::RCVD_TSTMP;
                self.ts_recent = to.tsval;
                self.ts_recent_age = u64::from(cx.ts_ticks());
            } else {
                self.flags &= !tf::REQ_TSTMP;
            }
            if to.has(tof::MSS) {
                let path = (cx.path)(&self.inc);
                let hc = cx.hc;
                self.mss(so, cx, hc, &path, i32::from(to.mss));
            }
            if self.flags & tf::SACK_PERMIT != 0
                && (!to.has(tof::SACKPERM) || self.flags & tf::NOOPT != 0)
            {
                self.flags &= !tf::SACK_PERMIT;
            }
        }
        // A connection with timestamps sees them on every segment.
        if self.flags & tf::RCVD_TSTMP != 0
            && !to.has(tof::TS)
            && thflags & th::RST == 0
            && !on(&cx.conf.tolerate_missing_ts)
        {
            return Fate::Alive;
        }

        // Header prediction: an in-order pure ACK, or in-order data.
        if self.state == State::Established
            && t.seq == self.rcv_nxt
            && thflags & (th::SYN | th::FIN | th::RST | th::URG | th::ACK) == th::ACK
            && self.snd_nxt == self.snd_max
            && tiwin != 0
            && tiwin == self.snd_wnd
            && self.flags & (tf::NEEDSYN | tf::NEEDFIN) == 0
            && self.segq.is_empty()
            && (!to.has(tof::TS) || seq::ts_geq(to.tsval, self.ts_recent))
        {
            if to.has(tof::TS) && seq::leq(t.seq, self.last_ack_sent) {
                self.ts_recent_age = u64::from(cx.ts_ticks());
                self.ts_recent = to.tsval;
            }
            if no_data {
                if seq::gt(t.ack, self.snd_una)
                    && seq::leq(t.ack, self.snd_max)
                    && !self.in_recovery()
                    && !to.has(tof::SACK)
                    && self.snd_holes.is_empty()
                {
                    cx.stats.inc(tcps::PREDACK);
                    if self.rxtshift == 1
                        && self.flags & tf::PREVVALID != 0
                        && self.badrxtwin != 0
                        && ((to.has(tof::TS)
                            && to.tsecr != 0
                            && seq::ts_lt(to.tsecr, self.badrxtwin as u32))
                            || (!to.has(tof::TS) && cx.now < self.badrxtwin))
                    {
                        self.cc_cong_signal(Some(t.ack), Signal::RtoErr, cx.stats);
                    }
                    self.rtt_sample(cx, &to, t.ack);
                    let acked = t.ack.wrapping_sub(self.snd_una);
                    cx.stats.add(tcps::RCVACKPACK, u64::from(nsegs));
                    cx.stats.add(tcps::RCVACKBYTE, u64::from(acked));
                    so.snd_drop(acked);
                    if seq::gt(self.snd_una, self.snd_recover) && seq::leq(t.ack, self.snd_recover)
                    {
                        self.snd_recover = t.ack.wrapping_sub(1);
                    }
                    self.cc_ack_received(cx, t.ack, nsegs, Signal::Ack);
                    self.snd_una = t.ack;
                    self.snd_wl2 = t.ack;
                    self.dupacks = 0;
                    if so.snd_cc() == 0 {
                        self.acktime = 0;
                    } else if acked > 1 {
                        self.acktime = cx.now;
                    }
                    if self.snd_una == self.snd_max {
                        self.timer_activate(Tt::Rexmt, 0, cx.now);
                    } else if !self.timer_active(Tt::Persist) {
                        let d = self.rxtcur_bounded(cx);
                        self.timer_activate(Tt::Rexmt, d, cx.now);
                    }
                    so.wake_writers();
                    if self.flags & tf::ACKNOW != 0
                        || so.snd_cc() >= self.snd_max.wrapping_sub(self.snd_una)
                    {
                        let _ = self.output(so, cx);
                    }
                    return self.check_delack(cx);
                }
            } else if t.ack == self.snd_una && i64::from(tlen) <= so.rcv_space() {
                if self.flags & tf::SACK_PERMIT != 0 && self.rcv_numsacks > 0 {
                    self.clean_sackreport();
                }
                cx.stats.inc(tcps::PREDDAT);
                self.rcv_nxt = self.rcv_nxt.wrapping_add(tlen);
                self.snd_wl1 = t.seq;
                self.rcv_up = self.rcv_nxt;
                cx.stats.add(tcps::RCVPACK, u64::from(nsegs));
                cx.stats.add(tcps::RCVBYTE, u64::from(tlen));
                let newsize = self.autorcvbuf(so, cx, tlen);
                if !so.rcv_closed() {
                    if newsize != 0 && !so.rcv_reserve(newsize) {
                        so.rcv_autosize_off();
                    }
                    so.rcv_append(m);
                }
                if self.delay_ack(cx, tlen) {
                    self.flags |= tf::DELACK;
                } else {
                    self.flags |= tf::ACKNOW;
                    let _ = self.output(so, cx);
                }
                return self.check_delack(cx);
            }
        }

        // The slow path.
        let win = so.rcv_space().max(0) as u32;
        self.rcv_wnd = win.max(self.rcv_adv.wrapping_sub(self.rcv_nxt) as i32 as u32);
        if (self.rcv_adv.wrapping_sub(self.rcv_nxt) as i32) < 0 {
            self.rcv_wnd = win;
        }

        let mut next = Next::Normal;
        match self.state {
            State::SynReceived => {
                if thflags & th::RST == 0
                    && thflags & th::ACK != 0
                    && (seq::leq(t.ack, self.snd_una) || seq::gt(t.ack, self.snd_max))
                {
                    self.dropwithreset(cx, &t, tlen);
                    return Fate::Alive;
                }
            }
            State::SynSent => {
                if thflags & (th::ACK | th::RST) == th::ACK | th::RST {
                    return self.drop(so, cx, Errno::ECONNREFUSED);
                }
                if thflags & th::RST != 0 || thflags & th::SYN == 0 {
                    return Fate::Alive;
                }
                self.irs = t.seq;
                self.rcvseqinit();
                if thflags & th::ACK != 0 {
                    cx.stats.inc(tcps::CONNECTS);
                    so.connected();
                    if self.flags & (tf::RCVD_SCALE | tf::REQ_SCALE)
                        == tf::RCVD_SCALE | tf::REQ_SCALE
                    {
                        self.rcv_scale = self.request_r_scale;
                    }
                    self.rcv_adv = self
                        .rcv_adv
                        .wrapping_add(self.rcv_wnd.min(MAXWIN << self.rcv_scale));
                    self.snd_una = self.snd_una.wrapping_add(1);
                    if seq::lt(self.snd_nxt, self.snd_una) {
                        self.snd_nxt = self.snd_una;
                    }
                    if self.delay_ack(cx, tlen) && tlen != 0 {
                        let d = get(&cx.conf.delacktime) as u32;
                        self.timer_activate(Tt::Delack, d, cx.now);
                    } else {
                        self.flags |= tf::ACKNOW;
                    }
                    self.ecn_input_syn_sent(cx, thflags, iptos);
                    self.starttime = cx.now;
                    if self.flags & tf::NEEDFIN != 0 {
                        self.acktime = cx.now;
                        self.set_state(State::FinWait1, cx.stats);
                        self.flags &= !tf::NEEDFIN;
                        thflags &= !th::SYN;
                    } else {
                        self.set_state(State::Established, cx.stats);
                        let hc = cx.hc.get(&self.inc.peer());
                        self.cc_conn_init(cx, &hc);
                        let d = self.keepidle(cx.conf);
                        self.timer_activate(Tt::Keep, d, cx.now);
                    }
                } else {
                    // Simultaneous open: SYN_RECEIVED, our SYN again with
                    // an ACK.
                    self.flags |= tf::ACKNOW | tf::NEEDSYN | tf::SONOTCONN;
                    self.timer_activate(Tt::Rexmt, 0, cx.now);
                    self.set_state(State::SynReceived, cx.stats);
                }
                t.seq = t.seq.wrapping_add(1);
                if tlen > self.rcv_wnd {
                    let todrop = tlen - self.rcv_wnd;
                    let _ = m.split_off(self.rcv_wnd as usize);
                    tlen = self.rcv_wnd;
                    thflags &= !th::FIN;
                    cx.stats.inc(tcps::RCVPACKAFTERWIN);
                    cx.stats.add(tcps::RCVBYTEAFTERWIN, u64::from(todrop));
                }
                self.snd_wl1 = t.seq.wrapping_sub(1);
                self.rcv_up = t.seq;
                next = if thflags & th::ACK != 0 {
                    Next::ProcessAck
                } else {
                    Next::Step6
                };
            }
            _ => {}
        }

        if let Next::Normal = next {
            // RFC 5961 3.2: a reset exactly at the edge, or a challenge.
            if thflags & th::RST != 0 {
                if (seq::geq(t.seq, self.last_ack_sent)
                    && seq::lt(t.seq, self.last_ack_sent.wrapping_add(self.rcv_wnd)))
                    || (self.rcv_wnd == 0 && self.last_ack_sent == t.seq)
                {
                    if on(&cx.conf.insecure_rst) || self.last_ack_sent == t.seq {
                        cx.stats.inc(tcps::DROPS);
                        match self.state {
                            State::SynReceived => so.set_error(Some(Errno::ECONNREFUSED)),
                            State::Established
                            | State::FinWait1
                            | State::FinWait2
                            | State::CloseWait
                            | State::Closing
                            | State::LastAck => so.set_error(Some(Errno::ECONNRESET)),
                            _ => {}
                        }
                        return self.close(so, cx);
                    }
                    cx.stats.inc(tcps::BADRST);
                    self.send_challenge_ack(so, cx);
                }
                return Fate::Alive;
            }
            // RFC 5961 4.2: a SYN in a synchronized state is challenged.
            if thflags & th::SYN != 0
                && self.state != State::SynSent
                && self.state != State::SynReceived
            {
                cx.stats.inc(tcps::BADSYN);
                if on(&cx.conf.insecure_syn)
                    && seq::geq(t.seq, self.last_ack_sent)
                    && seq::lt(t.seq, self.last_ack_sent.wrapping_add(self.rcv_wnd))
                {
                    return self.drop(so, cx, Errno::ECONNRESET);
                }
                self.ecn_input_syn_sent(cx, thflags, iptos);
                self.send_challenge_ack(so, cx);
                return Fate::Alive;
            }
            // PAWS.
            if to.has(tof::TS) && self.ts_recent != 0 && seq::ts_lt(to.tsval, self.ts_recent) {
                if cx.ts_ticks().wrapping_sub(self.ts_recent_age as u32) > PAWS_IDLE {
                    self.ts_recent = 0;
                } else {
                    cx.stats.inc(tcps::RCVDUPPACK);
                    cx.stats.add(tcps::RCVDUPBYTE, u64::from(tlen));
                    cx.stats.inc(tcps::PAWSDROP);
                    if tlen != 0 {
                        return self.dropafterack(so, cx, &t, thflags, tlen);
                    }
                    return Fate::Alive;
                }
            }
            if self.state == State::SynReceived && seq::lt(t.seq, self.irs) {
                self.dropwithreset(cx, &t, tlen);
                return Fate::Alive;
            }
            // Data before the window: trim it.
            let mut todrop = seq::sub(self.rcv_nxt, t.seq);
            if todrop > 0 {
                if thflags & th::SYN != 0 {
                    thflags &= !th::SYN;
                    t.seq = t.seq.wrapping_add(1);
                    if t.urp > 1 {
                        t.urp -= 1;
                    } else {
                        thflags &= !th::URG;
                    }
                    todrop -= 1;
                }
                if todrop as u32 > tlen || (todrop as u32 == tlen && thflags & th::FIN == 0) {
                    thflags &= !th::FIN;
                    self.flags |= tf::ACKNOW;
                    todrop = tlen as i32;
                    cx.stats.inc(tcps::RCVDUPPACK);
                    cx.stats.add(tcps::RCVDUPBYTE, todrop as u64);
                } else {
                    cx.stats.inc(tcps::RCVPARTDUPPACK);
                    cx.stats.add(tcps::RCVPARTDUPBYTE, todrop as u64);
                }
                if todrop > 0 && self.flags & tf::SACK_PERMIT != 0 {
                    // D-SACK the duplicate.
                    self.update_sack_list(t.seq, t.seq.wrapping_add(todrop as u32));
                    self.flags |= tf::ACKNOW;
                }
                m.trim_front(todrop as usize);
                t.seq = t.seq.wrapping_add(todrop as u32);
                tlen -= todrop as u32;
                if u32::from(t.urp) > todrop as u32 {
                    t.urp -= todrop as u16;
                } else {
                    thflags &= !th::URG;
                    t.urp = 0;
                }
            }
            // Data for a closed socket ends the connection.
            if self.flags & tf::CLOSED != 0 && tlen > 0 && !self.state.have_rcvd_fin() {
                let _ = self.close(so, cx);
                cx.stats.inc(tcps::RCVAFTERCLOSE);
                self.dropwithreset(cx, &t, tlen);
                return Fate::Closed;
            }
            // Data past the window: trim it.
            let todrop = seq::sub(
                t.seq.wrapping_add(tlen),
                self.rcv_nxt.wrapping_add(self.rcv_wnd),
            );
            if todrop > 0 {
                cx.stats.inc(tcps::RCVPACKAFTERWIN);
                if todrop as u32 >= tlen {
                    cx.stats.add(tcps::RCVBYTEAFTERWIN, u64::from(tlen));
                    if self.rcv_wnd == 0 && t.seq == self.rcv_nxt {
                        // A window probe.
                        self.flags |= tf::ACKNOW;
                        cx.stats.inc(tcps::RCVWINPROBE);
                    } else {
                        return self.dropafterack(so, cx, &t, thflags, tlen);
                    }
                } else {
                    cx.stats.add(tcps::RCVBYTEAFTERWIN, todrop as u64);
                }
                let keep = tlen.saturating_sub(todrop as u32);
                let _ = m.split_off(keep as usize);
                tlen = keep;
                thflags &= !(th::PUSH | th::FIN);
            }
            // The timestamp to echo.
            if to.has(tof::TS)
                && seq::leq(t.seq, self.last_ack_sent)
                && seq::leq(
                    self.last_ack_sent,
                    t.seq
                        .wrapping_add(tlen)
                        .wrapping_add(u32::from(thflags & (th::SYN | th::FIN) != 0)),
                )
            {
                self.ts_recent_age = u64::from(cx.ts_ticks());
                self.ts_recent = to.tsval;
            }
            if thflags & th::ACK == 0 {
                if self.state == State::SynReceived || self.flags & tf::NEEDSYN != 0 {
                    next = Next::Step6;
                } else if self.flags & tf::ACKNOW != 0 {
                    return self.dropafterack(so, cx, &t, thflags, tlen);
                } else {
                    return Fate::Alive;
                }
            }
        }

        if let Next::Normal = next {
            // RFC 5961 5.2: an acknowledgement too old is challenged.
            if seq::geq(
                self.snd_una,
                self.iss.wrapping_add(MAXWIN << self.snd_scale),
            ) {
                self.flags2 |= tf2::NO_ISS_CHECK;
            }
            if !on(&cx.conf.insecure_ack) {
                let (seq_min, ghost) = if self.flags2 & tf2::NO_ISS_CHECK != 0 {
                    (self.snd_una.wrapping_sub(self.max_sndwnd), false)
                } else if seq::gt(
                    self.iss.wrapping_add(1),
                    self.snd_una.wrapping_sub(self.max_sndwnd),
                ) {
                    (self.iss.wrapping_add(1), true)
                } else {
                    (self.snd_una.wrapping_sub(self.max_sndwnd), false)
                };
                if seq::lt(t.ack, seq_min) {
                    cx.stats.inc(if ghost {
                        tcps::RCVGHOSTACK
                    } else {
                        tcps::RCVACKTOOOLD
                    });
                    self.send_challenge_ack(so, cx);
                    return Fate::Alive;
                }
            }
            if self.state == State::SynReceived {
                // The handshake completes.
                cx.stats.inc(tcps::CONNECTS);
                if self.flags & tf::SONOTCONN != 0 {
                    self.flags &= !tf::SONOTCONN;
                    so.connected();
                }
                if self.flags & (tf::RCVD_SCALE | tf::REQ_SCALE) == tf::RCVD_SCALE | tf::REQ_SCALE {
                    self.rcv_scale = self.request_r_scale;
                }
                self.snd_wnd = tiwin;
                self.starttime = cx.now;
                if self.flags & tf::NEEDFIN != 0 {
                    self.acktime = cx.now;
                    self.set_state(State::FinWait1, cx.stats);
                    self.flags &= !tf::NEEDFIN;
                } else {
                    self.set_state(State::Established, cx.stats);
                    let hc = cx.hc.get(&self.inc.peer());
                    self.cc_conn_init(cx, &hc);
                    let d = self.keepidle(cx.conf);
                    self.timer_activate(Tt::Keep, d, cx.now);
                }
                if seq::gt(t.ack, self.snd_una) && self.flags & tf::NEEDSYN == 0 {
                    incforsyn = true;
                }
                if tlen == 0 && thflags & th::FIN == 0 {
                    let _ = self.reass(so, cx, None);
                    self.handle_wakeup();
                }
                self.snd_wl1 = t.seq.wrapping_sub(1);
            }
            // ESTABLISHED and on (SYN_RECEIVED falls through).
            if seq::gt(t.ack, self.snd_max) {
                cx.stats.inc(tcps::RCVACKTOOMUCH);
                return self.dropafterack(so, cx, &t, thflags, tlen);
            }
            if self.is_sack_recovery(&to) {
                sack_changed = self.sack_doack(cx, &to, t.ack);
                if sack_changed != SackStatus::NoChange && self.flags & tf::LRD != 0 {
                    self.sack_lost_retransmission(cx, t.ack);
                }
            } else {
                self.sackhint.sacked_bytes = 0;
            }
            if seq::lt(t.ack, self.snd_una) {
                next = Next::Step6;
            } else if t.ack == self.snd_una {
                // A duplicate acknowledgement?
                let dup = if self.flags & tf::SACK_PERMIT != 0 {
                    sack_changed != SackStatus::NoChange
                } else {
                    no_data && tiwin == self.snd_wnd
                };
                if !dup {
                    next = Next::Step6;
                } else if thflags & th::FIN != 0 && !self.state.have_rcvd_fin() {
                    self.dupacks = 0;
                    next = Next::Step6;
                } else {
                    cx.stats.inc(tcps::RCVDUPACK);
                    maxseg = self.maxseg();
                    if !self.timer_active(Tt::Rexmt) {
                        self.dupacks = 0;
                        next = Next::Step6;
                    } else {
                        self.dupacks += 1;
                        if self.dupacks > REXMTTHRESH || self.flags & tf::FASTRECOVERY != 0 {
                            // More duplicates in recovery: inflate.
                            self.cc_ack_received(cx, t.ack, nsegs, Signal::DupAck);
                            if on(&cx.conf.do_prr)
                                && self.flags & tf::FASTRECOVERY != 0
                                && self.flags & tf::SACK_PERMIT != 0
                            {
                                self.do_prr_ack(&to, t.ack, sack_changed, &mut maxseg);
                            } else if self.is_sack_recovery(&to)
                                && self.flags & tf::FASTRECOVERY != 0
                                && self.snd_nxt == self.snd_max
                            {
                                let awnd = self.compute_pipe();
                                if awnd < self.snd_ssthresh {
                                    self.snd_cwnd += maxseg.max(
                                        (2 * maxseg)
                                            .min(self.sackhint.delivered_data.max(0) as u32),
                                    );
                                    if self.snd_cwnd > self.snd_ssthresh {
                                        self.snd_cwnd = self.snd_ssthresh;
                                    }
                                }
                            } else if self.is_sack_recovery(&to)
                                && self.flags & tf::FASTRECOVERY != 0
                                && seq::lt(self.snd_nxt, self.snd_max)
                            {
                                self.snd_cwnd += maxseg.max(
                                    (2 * maxseg).min(self.sackhint.delivered_data.max(0) as u32),
                                );
                            } else {
                                self.snd_cwnd += maxseg;
                            }
                            let _ = self.output(so, cx);
                            return Fate::Alive;
                        } else if self.dupacks == REXMTTHRESH
                            || (self.flags & tf::SACK_PERMIT != 0
                                && self.sackhint.sacked_bytes > ((REXMTTHRESH - 1) * maxseg) as i32)
                        {
                            if let Some(f) = self.enter_recovery(so, cx, &to, t.ack, nsegs, maxseg)
                            {
                                return f;
                            }
                            next = Next::Step6;
                        } else if on(&cx.conf.rfc3042) {
                            // Limited transmit.
                            self.cc_ack_received(cx, t.ack, nsegs, Signal::DupAck);
                            let oldcwnd = self.snd_cwnd;
                            let oldsndmax = self.snd_max;
                            if self.dupacks == 1 {
                                self.snd_limited = 0;
                            }
                            if self.snd_nxt == self.snd_max && self.rxtshift == 0 {
                                self.snd_cwnd = self.snd_nxt.wrapping_sub(self.snd_una);
                            }
                            self.snd_cwnd = self.snd_cwnd.wrapping_add(
                                (self.dupacks - u32::from(self.snd_limited)) * maxseg,
                            );
                            let adj = self.sack_adjust();
                            self.snd_cwnd = (self.snd_cwnd as i64 - i64::from(adj)).max(0) as u32;
                            let avail = so.snd_cc();
                            if self.flags & tf::ACKNOW != 0
                                || avail >= self.snd_nxt.wrapping_sub(self.snd_una)
                            {
                                let _ = self.output(so, cx);
                            }
                            let sent = self.snd_max.wrapping_sub(oldsndmax);
                            if sent > maxseg {
                                self.snd_limited = 2;
                            } else if sent > 0 {
                                self.snd_limited += 1;
                            }
                            self.snd_cwnd = oldcwnd;
                            return Fate::Alive;
                        } else {
                            next = Next::Step6;
                        }
                    }
                }
            } else {
                // New data acknowledged.
                self.dupacks = 0;
                let mut enter = false;
                if self.is_sack_recovery(&to)
                    && ((self.rxtshift == 0 && sack_changed != SackStatus::NoChange)
                        || (self.rxtshift > 0 && sack_changed == SackStatus::NewLoss))
                    && self.snd_nxt == self.snd_max
                {
                    self.dupacks += 1;
                    maxseg = self.maxseg();
                    if self.flags & tf::FASTRECOVERY == 0
                        && self.sackhint.sacked_bytes > ((REXMTTHRESH - 1) * maxseg) as i32
                    {
                        enter = true;
                    }
                }
                if enter {
                    if let Some(f) = self.enter_recovery(so, cx, &to, t.ack, nsegs, maxseg) {
                        return f;
                    }
                    next = Next::Step6;
                } else {
                    if seq::lt(t.ack, self.snd_recover) {
                        if self.flags & tf::FASTRECOVERY != 0 {
                            if self.flags & tf::SACK_PERMIT != 0 {
                                if on(&cx.conf.do_prr) && to.has(tof::SACK) {
                                    self.timer_activate(Tt::Rexmt, 0, cx.now);
                                    self.rtttime = 0;
                                    self.do_prr_ack(&to, t.ack, sack_changed, &mut maxseg);
                                    self.flags |= tf::ACKNOW;
                                    let _ = self.output(so, cx);
                                } else {
                                    let _ = self.sack_partialack(so, cx, t.ack, maxseg);
                                }
                            } else {
                                self.newreno_partial_ack(so, cx, t.ack);
                            }
                        } else if self.flags & tf::CONGRECOVERY != 0 && on(&cx.conf.do_prr) {
                            self.sackhint.delivered_data = t.ack.wrapping_sub(self.snd_una) as i32;
                            self.snd_fack = t.ack;
                            self.do_prr_ack(&to, t.ack, SackStatus::Change, &mut maxseg);
                            let _ = self.output(so, cx);
                        }
                    }
                    if self.flags & tf::NEEDSYN != 0 {
                        // Our SYN is acknowledged by this ACK too.
                        self.flags &= !tf::NEEDSYN;
                        self.snd_una = self.snd_una.wrapping_add(1);
                        if self.flags & (tf::RCVD_SCALE | tf::REQ_SCALE)
                            == tf::RCVD_SCALE | tf::REQ_SCALE
                        {
                            self.rcv_scale = self.request_r_scale;
                        }
                    }
                    next = Next::ProcessAck;
                }
            }
        }

        if let Next::ProcessAck = next {
            if incforsyn {
                self.snd_una = self.snd_una.wrapping_add(1);
            }
            let acked = t.ack.wrapping_sub(self.snd_una);
            cx.stats.add(tcps::RCVACKPACK, u64::from(nsegs));
            cx.stats.add(tcps::RCVACKBYTE, u64::from(acked));
            // A retransmission that was not needed: undo its window cut.
            if self.rxtshift == 1
                && self.flags & tf::PREVVALID != 0
                && self.badrxtwin != 0
                && to.has(tof::TS)
                && to.tsecr != 0
                && seq::ts_lt(to.tsecr, self.badrxtwin as u32)
            {
                self.cc_cong_signal(Some(t.ack), Signal::RtoErr, cx.stats);
            }
            self.rtt_sample(cx, &to, t.ack);
            let sbavail = so.snd_cc();
            if (self.state <= State::CloseWait && acked == sbavail) || acked > sbavail {
                self.acktime = 0;
            } else if acked > 1 {
                self.acktime = cx.now;
            }
            if t.ack == self.snd_max {
                self.timer_activate(Tt::Rexmt, 0, cx.now);
                needoutput = true;
            } else if !self.timer_active(Tt::Persist) {
                let d = self.rxtcur_bounded(cx);
                self.timer_activate(Tt::Rexmt, d, cx.now);
            }
            if acked != 0 {
                self.cc_ack_received(cx, t.ack, nsegs, Signal::Ack);
                if acked > sbavail {
                    // The FIN is acknowledged too.
                    self.snd_wnd = self.snd_wnd.saturating_sub(sbavail);
                    so.snd_drop(sbavail);
                    ourfinisacked = true;
                } else {
                    so.snd_drop(acked);
                    self.snd_wnd = self.snd_wnd.saturating_sub(acked);
                    ourfinisacked = false;
                }
                so.wake_writers();
                if !self.in_recovery()
                    && seq::gt(self.snd_una, self.snd_recover)
                    && seq::leq(t.ack, self.snd_recover)
                {
                    self.snd_recover = t.ack.wrapping_sub(1);
                }
                self.snd_una = t.ack;
                if self.in_recovery() && seq::geq(t.ack, self.snd_recover) {
                    self.cc_post_recovery(t.ack);
                }
                if seq::gt(self.snd_una, self.snd_recover) {
                    self.snd_recover = self.snd_una;
                }
                if seq::lt(self.snd_nxt, self.snd_una) {
                    self.snd_nxt = self.snd_una;
                }
                match self.state {
                    State::FinWait1 if ourfinisacked => {
                        // The other side may never close: if this one
                        // cannot read any more, give up after a while.
                        if so.rcv_closed() {
                            self.free_sackholes(cx.stats);
                            so.disconnected();
                            let d = if on(&cx.conf.fast_finwait2_recycle) {
                                get(&cx.conf.finwait2_timeout) as u32
                            } else {
                                self.maxidle(cx.conf)
                            };
                            self.timer_activate(Tt::TwoMsl, d, cx.now);
                        }
                        self.set_state(State::FinWait2, cx.stats);
                    }
                    State::Closing if ourfinisacked => {
                        return self.twstart(so, cx);
                    }
                    State::LastAck if ourfinisacked => {
                        return self.close(so, cx);
                    }
                    _ => {}
                }
            }
        }

        // `step6`: the window update.
        if thflags & th::ACK != 0
            && (seq::lt(self.snd_wl1, t.seq)
                || (self.snd_wl1 == t.seq
                    && (seq::lt(self.snd_wl2, t.ack)
                        || (self.snd_wl2 == t.ack && tiwin > self.snd_wnd))))
        {
            if no_data && self.snd_wl2 == t.ack && tiwin > self.snd_wnd {
                cx.stats.inc(tcps::RCVWINUPD);
            }
            self.snd_wnd = tiwin;
            self.snd_wl1 = t.seq;
            self.snd_wl2 = t.ack;
            if self.snd_wnd > self.max_sndwnd {
                self.max_sndwnd = self.snd_wnd;
            }
            needoutput = true;
        }

        // Urgent data.
        let mut urgent_taken = false;
        if thflags & th::URG != 0 && t.urp != 0 && !self.state.have_rcvd_fin() {
            let sb_max = get(&cx.conf.sb_max) as u64;
            if u64::from(t.urp) + u64::from(so.rcv_cc()) > sb_max {
                t.urp = 0;
                thflags &= !th::URG;
            } else {
                let up = t.seq.wrapping_add(u32::from(t.urp));
                if seq::gt(up, self.rcv_up) {
                    self.rcv_up = up;
                    let mark = so
                        .rcv_cc()
                        .wrapping_add(self.rcv_up.wrapping_sub(self.rcv_nxt))
                        .wrapping_sub(1);
                    so.urgent(mark);
                    self.oobflags &= !(oob::HAVEDATA | oob::HADDATA);
                }
                if u32::from(t.urp) <= tlen && so.options() & so::OOBINLINE == 0 {
                    // `tcp_pulloutofband`: the urgent byte leaves the data.
                    let at = u32::from(t.urp) - 1;
                    let mut tail = m.split_off(at as usize).unwrap_or_default();
                    let mut c = [0u8; 1];
                    if tail.copy_out(0, &mut c) == 1 {
                        tail.trim_front(1);
                        self.iobc = c[0];
                        self.oobflags |= oob::HAVEDATA;
                        so.oob_byte(c[0]);
                        tlen -= 1;
                        urgent_taken = true;
                    }
                    m.append(tail);
                }
            }
        } else if seq::gt(self.rcv_nxt, self.rcv_up) {
            self.rcv_up = self.rcv_nxt;
        }
        let _ = urgent_taken;

        // `dodata`.
        if (tlen != 0 || thflags & th::FIN != 0) && !self.state.have_rcvd_fin() {
            let save_start = t.seq;
            let save_rnxt = self.rcv_nxt;
            let save_tlen = tlen;
            if t.seq == self.rcv_nxt && self.segq.is_empty() && self.state.have_established() {
                if self.delay_ack(cx, tlen) {
                    self.flags |= tf::DELACK;
                } else {
                    self.flags |= tf::ACKNOW;
                }
                self.rcv_nxt = self.rcv_nxt.wrapping_add(tlen);
                // FreeBSD takes the FIN from the header again here.
                thflags = t.flags & th::FIN;
                cx.stats.inc(tcps::RCVPACK);
                cx.stats.add(tcps::RCVBYTE, u64::from(tlen));
                if !so.rcv_closed() && tlen > 0 {
                    so.rcv_append(m);
                }
                self.flags |= tf::WAKESOR;
            } else {
                (thflags, tlen) = self.reass(so, cx, Some((t.seq, thflags, tlen, m)));
                self.flags |= tf::ACKNOW;
            }
            if self.flags & tf::SACK_PERMIT != 0 && save_tlen > 0 && self.state.have_established() {
                let save_end = save_start.wrapping_add(save_tlen);
                if tlen == 0 && seq::lt(save_start, save_rnxt) {
                    self.update_sack_list(save_start, save_end);
                } else if tlen > 0 && seq::gt(self.rcv_nxt, save_rnxt) {
                    if self.rcv_numsacks >= 1 && self.sackblks[0].end == save_start {
                        let b = self.sackblks[0];
                        self.update_sack_list(b.start, b.end);
                    } else {
                        self.update_dsack_list(save_start, save_end);
                    }
                } else if tlen >= save_tlen {
                    self.update_dsack_list(save_start, save_end);
                } else if tlen > 0 {
                    self.update_dsack_list(save_start, save_start.wrapping_add(tlen));
                }
            }
            self.handle_wakeup();
        } else {
            thflags &= !th::FIN;
        }

        // The FIN.
        if thflags & th::FIN != 0 {
            if !self.state.have_rcvd_fin() {
                so.cantrcvmore();
                if self.flags & tf::NEEDSYN != 0 {
                    self.flags |= tf::DELACK;
                } else {
                    self.flags |= tf::ACKNOW;
                }
                self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            }
            match self.state {
                State::SynReceived => {
                    self.starttime = cx.now;
                    self.set_state(State::CloseWait, cx.stats);
                }
                State::Established => self.set_state(State::CloseWait, cx.stats),
                State::FinWait1 => self.set_state(State::Closing, cx.stats),
                State::FinWait2 => return self.twstart(so, cx),
                _ => {}
            }
        }
        if needoutput || self.flags & tf::ACKNOW != 0 {
            let _ = self.output(so, cx);
        }
        self.check_delack(cx)
    }

    /// `enter_recovery`: fast retransmit on the third duplicate (or on
    /// enough SACKed data). `Some` when done with the segment.
    fn enter_recovery<S: Sock>(
        &mut self,
        so: &mut S,
        cx: &mut Ctx,
        to: &Opts,
        ack: Seq,
        nsegs: u16,
        mut maxseg: u32,
    ) -> Option<Fate> {
        if maxseg == 0 {
            maxseg = self.maxseg();
        }
        self.dupacks = REXMTTHRESH;
        let onxt = self.snd_nxt;
        if self.is_sack_recovery(to) {
            if self.flags & tf::FASTRECOVERY != 0 {
                self.dupacks = 0;
                return None;
            }
        } else if seq::leq(ack, self.snd_recover) {
            self.dupacks = 0;
            return None;
        }
        self.cc_cong_signal(Some(ack), Signal::NDupAck, cx.stats);
        self.cc_ack_received(cx, ack, nsegs, Signal::DupAck);
        self.timer_activate(Tt::Rexmt, 0, cx.now);
        self.rtttime = 0;
        if on(&cx.conf.do_prr) {
            self.sackhint.prr_delivered = if self.is_sack_recovery(to) {
                self.snd_max
                    .wrapping_sub(ack)
                    .min((u32::from(self.snd_limited) + 1) * maxseg)
            } else {
                maxseg
            };
            self.sackhint.recover_fs = self.snd_nxt.wrapping_sub(self.snd_una).max(1);
        }
        self.snd_limited = 0;
        if self.is_sack_recovery(to) {
            cx.stats.inc(tcps::SACK_RECOVERY_EPISODE);
            self.resend_sackholes();
            self.snd_nxt = self.snd_max;
            self.snd_cwnd = self.compute_pipe() + maxseg;
            let _ = self.output(so, cx);
            self.snd_cwnd = self.snd_ssthresh;
            return Some(Fate::Alive);
        }
        self.snd_nxt = ack;
        self.snd_cwnd = maxseg;
        let _ = self.output(so, cx);
        self.snd_cwnd =
            self.snd_ssthresh + maxseg * (self.dupacks.saturating_sub(u32::from(self.snd_limited)));
        if seq::gt(onxt, self.snd_nxt) {
            self.snd_nxt = onxt;
        }
        Some(Fate::Alive)
    }

    /// `dropafterack`: acknowledge (so the peer resynchronizes), unless
    /// the segment's ACK is bogus in SYN_RECEIVED.
    fn dropafterack<S: Sock>(
        &mut self,
        so: &mut S,
        cx: &mut Ctx,
        t: &Th,
        thflags: u16,
        tlen: u32,
    ) -> Fate {
        if self.state == State::SynReceived
            && thflags & th::ACK != 0
            && (seq::gt(self.snd_una, t.ack) || seq::gt(t.ack, self.snd_max))
        {
            self.dropwithreset(cx, t, tlen);
            return Fate::Alive;
        }
        self.flags |= tf::ACKNOW;
        self.output(so, cx)
    }

    /// `check_delack`: a delayed ACK starts its timer.
    fn check_delack(&mut self, cx: &Ctx) -> Fate {
        if self.flags & tf::DELACK != 0 {
            self.flags &= !tf::DELACK;
            let d = get(&cx.conf.delacktime) as u32;
            self.timer_activate(Tt::Delack, d, cx.now);
        }
        Fate::Alive
    }
}
