// SPDX-License-Identifier: BSD-2-Clause
//! What a connection sends (`tcp_output.c`): the decision to send (data
//! the windows allow, a window update, an ACK, a SYN or FIN, a SACK
//! retransmission), the segment's options and flags, TSO sizing, the
//! sequence-space bookkeeping, and the retransmission and persist
//! timers. Segments go to [`Ctx::out`]; the caller sends them.
//!
//! TCP Fast Open, MD5 signatures, and UDP tunneling are not done.
//!
//! Behavior reference: sys/netinet/tcp_output.c (`tcp_default_output`,
//! `tcp_sndbuf_autoscale`), sys/netinet/tcp_subr.c
//! (`tcp_account_for_send`).

use crate::hdr::{MAXWIN, Opts, Th, th, tof};
use crate::sack::REXMTTHRESH;
use crate::seq;
use crate::tcb::{Tcpcb, build, tf, tf2};
use crate::timer::Tt;
use crate::{Ctx, Errno, Fate, Seg, Sent, Sock, State, ecn, get, on, tcps};

/// `MHLEN` less the room for headers: data this short is copied into the
/// header's mbuf rather than referenced (`len <= MHLEN - hdrlen -
/// max_linkhdr`).
const SMALL: u32 = mbuf::MHLEN - mbuf::MAX_LINKHDR;

impl Tcpcb {
    /// `tcp_output`'s error path, for segments built under the lock that
    /// could not be sent once it was let go: `failed` (in the order they
    /// were built, the first the one that met `e`, the rest left unsent
    /// after it) are taken back as each would have been (`snd_nxt`, or
    /// the SACK hole's `rxmit` and `sack_bytes_rexmit`, and `prr_out`),
    /// if nothing has moved past them since; then `ENOBUFS` cuts the
    /// window to a segment and an unreachable path is a soft error. A
    /// segment something went out after stays sent, for recovery to find.
    pub fn send_failed(&mut self, failed: &[Sent], e: Errno) {
        if e != Errno::EPERM {
            for s in failed.iter().rev() {
                if s.sack {
                    let hole = self.snd_holes.0.iter_mut().find(|p| {
                        seq::geq(s.seq, p.start)
                            && seq::lt(s.seq, p.end)
                            && seq::min(p.end, p.rxmit).wrapping_sub(s.len) == s.seq
                    });
                    if let Some(p) = hole {
                        p.rxmit = s.seq;
                        let len = i32::try_from(s.len).unwrap_or(i32::MAX);
                        self.sackhint.sack_bytes_rexmit =
                            (self.sackhint.sack_bytes_rexmit - len).max(0);
                    }
                } else if self.snd_nxt == s.seq.wrapping_add(s.len + u32::from(s.fin)) {
                    self.snd_nxt = s.seq;
                }
                if self.in_recovery() {
                    self.sackhint.prr_out = self.sackhint.prr_out.wrapping_sub(s.len);
                }
            }
        }
        match e {
            Errno::EACCES | Errno::EPERM => self.softerror = Some(e),
            Errno::ENOBUFS => self.snd_cwnd = self.maxseg(),
            Errno::EHOSTDOWN | Errno::EHOSTUNREACH | Errno::ENETDOWN | Errno::ENETUNREACH
                if self.state.have_rcvd_syn() =>
            {
                self.softerror = Some(e);
            }
            _ => {}
        }
    }

    /// `tcp_output`: sends what there is to send. A connection only ends
    /// here through [`Tcpcb::drop`]; this returns [`Fate::Alive`].
    pub fn output<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        let mut idle = self.flags & tf::LASTIDLE != 0 || self.snd_max == self.snd_una;
        if idle
            && (self.idle(cx.now) >= u64::from(self.rxtcur)
                || (self.sndtime != 0
                    && cx.now.saturating_sub(self.sndtime) >= u64::from(self.rxtcur)))
        {
            self.cc_after_idle(cx);
        }
        self.flags &= !tf::LASTIDLE;
        if idle && self.flags & tf::MORETOCOME != 0 {
            self.flags |= tf::LASTIDLE;
            idle = false;
        }
        // `again`: once per segment while there is more to send.
        while self.output_one(so, cx, idle) {}
        Fate::Alive
    }

    /// One pass of `tcp_output`: sends at most one segment; true if
    /// another should follow (`sendalot`).
    #[allow(clippy::too_many_lines)]
    fn output_one<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, idle: bool) -> bool {
        let mut sendwin: u32 = 0;
        if self.flags & tf::SACK_PERMIT != 0
            && self.sackhint.nexthole.is_some()
            && self.flags & tf::FASTRECOVERY == 0
        {
            sendwin = self.sack_adjust().max(0) as u32;
        }
        let mut sendalot = false;
        let mut tso = false;
        let mut off = self.snd_nxt.wrapping_sub(self.snd_una) as i32;
        sendwin = self.snd_wnd.min(self.snd_cwnd.saturating_add(sendwin));
        let mut flags = self.state.outflags();
        let sack_bytes_rxmt = self.sackhint.sack_bytes_rexmit;
        let mut len: i32 = 0;
        let mut sack_rxmit: Option<usize> = None;

        if self.flags & tf::SACK_PERMIT != 0
            && (self.flags & tf::FASTRECOVERY != 0
                || (seq::lt(self.snd_nxt, self.snd_max) && self.dupacks >= REXMTTHRESH))
            && let Some(i) = self.sack_output()
        {
            {
                let p = self.snd_holes.0[i];
                let cwin = if self.flags & tf::FASTRECOVERY != 0 {
                    (sendwin as i32 - self.compute_pipe() as i32).max(0)
                } else {
                    (sendwin as i32 - off).max(0)
                };
                let mut skip = false;
                if seq::gt(p.end, self.snd_recover) {
                    if seq::geq(p.rxmit, self.snd_recover) {
                        // Nothing below the recovery point is left.
                        skip = true;
                    } else {
                        len = seq::sub(self.snd_recover, p.rxmit);
                    }
                } else {
                    len = seq::sub(p.end, p.rxmit);
                }
                if !skip {
                    if cwin <= len {
                        len = cwin;
                    } else {
                        sendalot = true;
                    }
                    if len > 0 {
                        off = seq::sub(p.rxmit, self.snd_una);
                    }
                    sack_rxmit = Some(i);
                } else {
                    len = 0;
                }
            }
        }
        if self.flags & tf::NEEDFIN != 0 {
            flags |= th::FIN;
        }
        if self.flags & tf::NEEDSYN != 0 {
            flags |= th::SYN;
        }
        let sbavail = so.snd_cc() as i32;
        if self.flags & tf::FORCEDATA != 0 {
            if sendwin == 0 {
                // A probe of one byte into the closed window; a FIN only
                // if that byte is the last.
                if off < sbavail {
                    flags &= !th::FIN;
                }
                sendwin = 1;
            } else {
                self.timer_activate(Tt::Persist, 0, cx.now);
                self.rxtshift = 0;
            }
        }
        if sack_rxmit.is_none() {
            len = if sack_bytes_rxmt == 0 || seq::lt(self.snd_nxt, self.snd_max) {
                sbavail.min(sendwin as i32) - off
            } else {
                (sbavail.min(sendwin as i32) - (self.compute_pipe() as i32).max(off)).max(0)
            };
        }
        // A SYN already sent is not sent again (except SYN_RECEIVED's
        // SYN-ACK); its sequence number is not data.
        if flags & th::SYN != 0 && seq::gt(self.snd_nxt, self.snd_una) {
            if self.state != State::SynReceived {
                flags &= !th::SYN;
            }
            off -= 1;
            len += 1;
        }
        if flags & th::SYN != 0 && self.flags & tf::NOOPT != 0 {
            len = 0;
            flags &= !th::FIN;
        }
        // No data with a SYN (no Fast Open).
        if flags & th::SYN != 0 {
            len = 0;
        }
        if len <= 0 {
            // A window shrunk to nothing with data to send: back to the
            // left edge, and probe.
            len = 0;
            if sendwin == 0
                && self.state.have_established()
                && off < sbavail
                && !self.timer_active(Tt::Persist)
            {
                self.timer_activate(Tt::Rexmt, 0, cx.now);
                self.rxtshift = 0;
                self.snd_nxt = self.snd_una;
                if !self.timer_active(Tt::Persist) {
                    self.setpersist(cx);
                }
            }
        }
        self.sndbuf_autoscale(so, cx, sendwin);
        let maxseg = self.t_maxseg as i32;
        if self.flags & tf::TSO != 0
            && on(&cx.conf.tso)
            && len > maxseg
            && sack_rxmit.is_none()
            && flags & th::SYN == 0
        {
            tso = true;
        }
        let from = match sack_rxmit {
            Some(i) => self.snd_holes.0[i].rxmit,
            None => self.snd_nxt,
        };
        if seq::lt(
            from.wrapping_add(len as u32),
            self.snd_una.wrapping_add(so.snd_cc()),
        ) {
            flags &= !th::FIN;
        }
        let hiwat = so.rcv_hiwat() as i64;
        let mut recwin = (so.rcv_space().max(0)).min(i64::from(MAXWIN) << self.rcv_scale);

        // Should anything go?
        let send = 'decide: {
            if len > 0 {
                if len >= maxseg {
                    break 'decide true;
                }
                let tsopt = if self.flags & tf::RCVD_TSTMP != 0 {
                    12
                } else {
                    0
                };
                if tsopt + len >= maxseg {
                    break 'decide true;
                }
                // All that is queued, and Nagle allows it.
                if self.flags & tf::MORETOCOME == 0
                    && (idle || self.flags & tf::NODELAY != 0)
                    && len as u32 + off as u32 >= so.snd_cc()
                    && self.flags & tf::NOPUSH == 0
                {
                    break 'decide true;
                }
                if self.flags & tf::FORCEDATA != 0 {
                    break 'decide true;
                }
                if self.max_sndwnd > 0 && len as u32 >= self.max_sndwnd / 2 {
                    break 'decide true;
                }
                if seq::lt(self.snd_nxt, self.snd_max) {
                    break 'decide true;
                }
                if sack_rxmit.is_some() {
                    break 'decide true;
                }
            }
            // A window update worth sending.
            if recwin > 0
                && self.flags & tf::NEEDSYN == 0
                && self.flags & tf::DELACK == 0
                && !self.state.have_rcvd_fin()
            {
                let mut adv = recwin;
                let oldwin = if seq::gt(self.rcv_adv, self.rcv_nxt) {
                    let o = i64::from(self.rcv_adv.wrapping_sub(self.rcv_nxt));
                    adv = if adv > o { adv - o } else { 0 };
                    o
                } else {
                    0
                };
                if oldwin >> self.rcv_scale < (adv + oldwin) >> self.rcv_scale {
                    let ms = i64::from(self.t_maxseg);
                    if adv >= 2 * ms
                        && (adv >= hiwat / 4
                            || recwin <= hiwat / 8
                            || hiwat <= 8 * ms
                            || adv >= i64::from(MAXWIN) << self.rcv_scale)
                    {
                        break 'decide true;
                    }
                    if 2 * adv >= hiwat {
                        break 'decide true;
                    }
                }
            }
            if self.flags & tf::ACKNOW != 0 {
                break 'decide true;
            }
            if flags & th::RST != 0 || (flags & th::SYN != 0 && self.flags & tf::NEEDSYN == 0) {
                break 'decide true;
            }
            if seq::gt(self.snd_up, self.snd_una) {
                break 'decide true;
            }
            if flags & th::FIN != 0
                && (self.flags & tf::SENTFIN == 0 || self.snd_nxt == self.snd_una)
            {
                break 'decide true;
            }
            false
        };
        if !send {
            // `just_return`, with the timers that keep things moving.
            if self.flags & tf::SACK_PERMIT != 0
                && seq::gt(self.snd_max, self.snd_una)
                && !self.timer_active(Tt::Rexmt)
                && !self.timer_active(Tt::Persist)
            {
                let d = self.rxtcur_bounded(cx);
                self.timer_activate(Tt::Rexmt, d, cx.now);
                return false;
            }
            if so.snd_cc() > 0 && !self.timer_active(Tt::Rexmt) && !self.timer_active(Tt::Persist) {
                self.rxtshift = 0;
                self.setpersist(cx);
            }
            return false;
        }

        // `send`.
        if len > 0 {
            if len >= maxseg {
                self.flags2 |= tf2::PLPMTU_MAXSEGSNT;
            } else {
                self.flags2 &= !tf2::PLPMTU_MAXSEGSNT;
            }
        }
        if flags & th::SYN != 0 {
            self.snd_nxt = self.iss;
        }
        let mut to = Opts::default();
        if self.flags & tf::NOOPT == 0 {
            if flags & th::SYN != 0 {
                to.mss = self.mssopt(cx) as u16;
                to.flags |= tof::MSS;
            }
            if flags & th::SYN != 0 && self.flags & tf::REQ_SCALE != 0 {
                to.wscale = self.request_r_scale;
                to.flags |= tof::SCALE;
            }
            if self.flags & tf::RCVD_TSTMP != 0
                || (flags & th::SYN != 0 && self.flags & tf::REQ_TSTMP != 0)
            {
                let curticks = cx.ts_ticks();
                to.tsval = curticks.wrapping_add(self.ts_offset);
                to.tsecr = self.ts_recent;
                to.flags |= tof::TS;
                if self.rxtshift == 1 {
                    self.badrxtwin = u64::from(curticks);
                }
            }
            if self.rfbuf_ts == 0 && so.rcv_autosize() {
                self.rfbuf_ts = cx.ts_ticks();
            }
            if self.flags & tf::SACK_PERMIT != 0 {
                if flags & th::SYN != 0 {
                    to.flags |= tof::SACKPERM;
                } else if self.state.have_established() && self.rcv_numsacks > 0 {
                    self.sack_opts(&mut to);
                }
            }
        }
        let optlen = if self.flags & tf::NOOPT == 0 {
            to.len() as i32
        } else {
            0
        };
        if len + optlen > maxseg {
            flags &= !th::FIN;
            if tso {
                let max_len = maxseg - optlen;
                if self.tsomax != 0 {
                    let hdrlen = self.inc.min_protoh() as i32 + optlen;
                    let m = self.tsomax as i32 - hdrlen - mbuf::MAX_LINKHDR as i32;
                    if m <= 0 {
                        len = 0;
                    } else if len > m {
                        sendalot = true;
                        len = m;
                    }
                }
                if (off as u32 + len as u32) < so.snd_cc() {
                    let moff = len % max_len;
                    if moff != 0 {
                        len -= moff;
                        sendalot = true;
                    }
                }
                if len <= max_len {
                    len = max_len;
                    sendalot = true;
                    tso = false;
                }
                if self.flags & tf::NEEDFIN != 0 {
                    sendalot = true;
                }
            } else {
                if optlen >= maxseg {
                    return false;
                }
                len = maxseg - optlen;
                sendalot = true;
            }
        } else {
            tso = false;
        }
        let mut data = None;
        if len > 0 {
            if self.flags & tf::FORCEDATA != 0 && len == 1 {
                cx.stats.inc(tcps::SNDPROBE);
            } else if seq::lt(self.snd_nxt, self.snd_max) || sack_rxmit.is_some() {
                self.sndrexmitpack += 1;
                cx.stats.inc(tcps::SNDREXMITPACK);
                cx.stats.add(tcps::SNDREXMITBYTE, len as u64);
                if sack_rxmit.is_some() {
                    cx.stats.inc(tcps::SACK_REXMITS);
                    if tso {
                        cx.stats.inc(tcps::SACK_REXMITS_TSO);
                    }
                    cx.stats.add(tcps::SACK_REXMIT_BYTES, len as u64);
                }
            } else {
                cx.stats.inc(tcps::SNDPACK);
                cx.stats.add(tcps::SNDBYTE, len as u64);
            }
            let Some(d) = so.snd_copy(off as u32, len as u32) else {
                // ENOBUFS: back to one segment.
                self.snd_cwnd = self.maxseg();
                return false;
            };
            // A short piece goes by copy (it then fits the header's mbuf).
            let d = if (len as u32) <= SMALL {
                let mut b = alloc::vec![0u8; len as usize];
                d.copy_out(0, &mut b);
                mbuf::Chain::from_slice(&b).ok()
            } else {
                Some(d)
            };
            let Some(d) = d else {
                self.snd_cwnd = self.maxseg();
                return false;
            };
            if len <= maxseg - optlen {
                tso = false;
            }
            data = Some(d);
            if off as u32 + len as u32 == so.snd_cc() && flags & th::SYN == 0 {
                flags |= th::PUSH;
            }
        } else if self.flags & tf::ACKNOW != 0 {
            cx.stats.inc(tcps::SNDACKS);
        } else if flags & (th::SYN | th::FIN | th::RST) != 0 {
            cx.stats.inc(tcps::SNDCTRL);
        } else if seq::gt(self.snd_up, self.snd_una) {
            cx.stats.inc(tcps::SNDURG);
        } else {
            cx.stats.inc(tcps::SNDWINUP);
        }

        // A FIN sent before goes again with its own sequence number.
        if flags & th::FIN != 0 && self.flags & tf::SENTFIN != 0 && self.snd_nxt == self.snd_max {
            self.snd_nxt = self.snd_nxt.wrapping_sub(1);
        }
        if self.state == State::SynSent && get(&cx.conf.ecn_enable) != 0 {
            flags |= self.ecn_output_syn_sent(cx);
        }
        let mut ipecn = ecn::NOTECT;
        if self.state.have_rcvd_syn() && self.flags2 & (tf2::ECN_PERMIT | tf2::ACE_PERMIT) != 0 {
            ipecn = self.ecn_output_established(cx, &mut flags, len as u32, sack_rxmit.is_some());
            if self.state == State::SynReceived && self.flags2 & tf2::ECN_SND_ECE != 0 {
                self.flags2 &= !tf2::ECN_SND_ECE;
            }
        }
        let th_seq = match sack_rxmit {
            None => {
                if len > 0 || flags & (th::SYN | th::FIN) != 0 || self.timer_active(Tt::Persist) {
                    self.snd_nxt
                } else {
                    self.snd_max
                }
            }
            Some(i) => {
                let lrd = self.flags & tf::LRD != 0;
                let recover = self.snd_recover;
                let p = &mut self.snd_holes.0[i];
                let s = p.rxmit;
                p.rxmit = p.rxmit.wrapping_add(len as u32);
                if lrd && seq::geq(p.rxmit, p.end) {
                    p.rxmit = seq::max(p.rxmit, recover);
                }
                self.sackhint.sack_bytes_rexmit += len;
                s
            }
        };
        if self.in_recovery() {
            self.sackhint.prr_out = self.sackhint.prr_out.wrapping_add(len as u32);
        }
        // The window: not a silly one, and never shrinking what was
        // offered.
        if flags & th::RST != 0 {
            recwin = 0;
        } else {
            if recwin < hiwat / 4 && recwin < i64::from(self.t_maxseg) {
                recwin = 0;
            }
            if seq::gt(self.rcv_adv, self.rcv_nxt)
                && recwin < i64::from(self.rcv_adv.wrapping_sub(self.rcv_nxt))
            {
                recwin = i64::from(self.rcv_adv.wrapping_sub(self.rcv_nxt));
            }
        }
        let win = if flags & th::SYN != 0 {
            so.rcv_space().clamp(0, i64::from(MAXWIN)) as u16
        } else {
            let r = (recwin + (1 << self.rcv_scale) - 1) & !((1 << self.rcv_scale) - 1);
            (r >> self.rcv_scale).min(i64::from(u16::MAX)) as u16
        };
        if win == 0 {
            self.sndzerowin += 1;
            self.flags |= tf::RXWIN0SENT;
        } else {
            self.flags &= !tf::RXWIN0SENT;
        }
        let mut urp = 0;
        if seq::gt(self.snd_up, self.snd_nxt) {
            urp = self.snd_up.wrapping_sub(self.snd_nxt).min(0xffff) as u16;
            flags |= th::URG;
        } else {
            self.snd_up = self.snd_una;
        }
        let t = Th {
            sport: self.inc.lport,
            dport: self.inc.fport,
            seq: th_seq,
            ack: self.rcv_nxt,
            off: 0,
            flags,
            win,
            sum: 0,
            urp,
        };
        if on(&cx.conf.path_mtu_discovery) && self.t_maxseg > get(&cx.conf.minmss) as u32 {
            self.flags2 |= tf2::PLPMTU_PMTUD;
        } else {
            self.flags2 &= !tf2::PLPMTU_PMTUD;
        }
        let df = self.flags2 & tf2::PLPMTU_PMTUD != 0 && self.pmtud;
        let tso_segsz = if tso {
            (maxseg - optlen).max(0) as u16
        } else {
            0
        };
        let Some(pkt) = build(t, &to, data) else {
            // ENOBUFS: as if sent, with the window cut to a segment.
            self.snd_cwnd = self.maxseg();
            return false;
        };
        // `tcp_output`'s error path takes back what this segment moved,
        // unless it is a SYN or a forced byte while persisting.
        let sent = (flags & (th::SYN | th::RST) == 0
            && (self.flags & tf::FORCEDATA == 0 || !self.timer_active(Tt::Persist)))
        .then_some(Sent {
            seq: th_seq,
            len: len as u32,
            fin: flags & th::FIN != 0,
            sack: sack_rxmit.is_some(),
        });
        cx.out.push(Seg {
            pkt,
            ecn: ipecn,
            df,
            tso_segsz,
            sent,
        });

        // `out`: the sequence space and the timers.
        if seq::lt(self.snd_nxt, self.snd_max) || sack_rxmit.is_some() {
            self.snd_rxt_bytes += len as u64;
        } else {
            self.sndbytes += len as u64;
        }
        if flags & th::RST != 0 {
            cx.stats.inc(tcps::SNDTOTAL);
            return false;
        }
        if self.flags & tf::FORCEDATA == 0 || !self.timer_active(Tt::Persist) {
            let startseq = self.snd_nxt;
            if flags & th::SYN != 0 {
                self.snd_nxt = self.snd_nxt.wrapping_add(1);
            }
            if flags & th::FIN != 0 {
                self.snd_nxt = self.snd_nxt.wrapping_add(1);
                self.flags |= tf::SENTFIN;
            }
            if sack_rxmit.is_none() {
                self.snd_nxt = self.snd_nxt.wrapping_add(len as u32);
                if seq::gt(self.snd_nxt, self.snd_max) {
                    if self.snd_una == self.snd_max {
                        self.acktime = cx.now;
                    }
                    self.snd_max = self.snd_nxt;
                    self.sndtime = cx.now;
                    if self.rtttime == 0 {
                        self.rtttime = cx.now.max(1);
                        self.rtseq = startseq;
                        cx.stats.inc(tcps::SEGSTIMED);
                    }
                }
            }
            // `timer`.
            if !self.timer_active(Tt::Rexmt)
                && ((sack_rxmit.is_some() && self.snd_nxt != self.snd_max)
                    || self.snd_nxt != self.snd_una)
            {
                if self.timer_active(Tt::Persist) {
                    self.timer_activate(Tt::Persist, 0, cx.now);
                    self.rxtshift = 0;
                }
                let d = self.rxtcur_bounded(cx);
                self.timer_activate(Tt::Rexmt, d, cx.now);
            } else if len == 0
                && so.snd_cc() > 0
                && !self.timer_active(Tt::Rexmt)
                && !self.timer_active(Tt::Persist)
            {
                self.rxtshift = 0;
                self.setpersist(cx);
            }
        } else {
            // A persist probe: the byte (or FIN) counts as sent.
            let mut xlen = len as u32;
            if flags & th::SYN != 0 {
                xlen += 1;
            }
            if flags & th::FIN != 0 {
                xlen += 1;
                self.flags |= tf::SENTFIN;
            }
            if seq::gt(self.snd_nxt.wrapping_add(xlen), self.snd_max) {
                self.snd_max = self.snd_nxt.wrapping_add(xlen);
            }
        }
        if self.rcv_numsacks > 0
            && self.state.have_established()
            && self.flags & tf::SACK_PERMIT != 0
        {
            self.clean_dsack_blocks();
        }
        if let Some(i) = sack_rxmit {
            let p = self.snd_holes.0[i];
            let lim = seq::min(p.rxmit, p.end);
            if seq::lt(self.snd_nxt, lim) {
                self.snd_nxt = lim;
            }
        }
        cx.stats.inc(tcps::SNDTOTAL);
        let adv = self.rcv_nxt.wrapping_add(recwin as u32);
        if seq::gt(adv, self.rcv_adv) {
            self.rcv_adv = adv;
        }
        self.last_ack_sent = self.rcv_nxt;
        self.flags &= !(tf::ACKNOW | tf::DELACK);
        if self.timer_active(Tt::Delack) {
            self.timer_activate(Tt::Delack, 0, cx.now);
        }
        sendalot
    }

    /// `tcp_mssopt`: the MSS a SYN offers, from the route and the host
    /// cache.
    pub(crate) fn mssopt(&self, cx: &Ctx) -> u32 {
        let maxmtu = (cx.path)(&self.inc).maxmtu;
        let hcmtu = cx.hc.mtu(&self.inc.peer());
        let min_protoh = self.inc.min_protoh();
        let dflt = if self.inc.v6 {
            get(&cx.conf.v6mssdflt) as u32
        } else {
            get(&cx.conf.mssdflt) as u32
        };
        if maxmtu != 0 && hcmtu != 0 {
            maxmtu.min(hcmtu) - min_protoh
        } else if maxmtu != 0 || hcmtu != 0 {
            maxmtu.max(hcmtu) - min_protoh
        } else {
            dflt
        }
    }

    /// `tcp_sndbuf_autoscale`: grows the send buffer while the peer's
    /// window and the data waiting keep it full.
    fn sndbuf_autoscale<S: Sock>(&mut self, so: &mut S, cx: &Ctx, sendwin: u32) {
        if !on(&cx.conf.sendbuf_auto) || !so.snd_autosize() {
            return;
        }
        let hiwat = so.snd_hiwat();
        let used = so.snd_cc();
        let max = get(&cx.conf.sendbuf_max) as u32;
        if self.snd_wnd / 4 * 5 >= hiwat
            && used >= hiwat / 8 * 7
            && used < max
            && sendwin >= used.saturating_sub(self.snd_nxt.wrapping_sub(self.snd_una))
        {
            let inc = get(&cx.conf.sendbuf_inc) as u32;
            if !so.snd_reserve((hiwat + inc).min(max)) {
                so.snd_autosize_off();
            }
        }
    }
}
