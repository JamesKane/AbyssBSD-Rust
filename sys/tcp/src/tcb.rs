// SPDX-License-Identifier: BSD-2-Clause
//! The control block (`struct tcpcb`) and what `tcp_subr.c` does to it:
//! creation, state changes, the MSS, RTT estimation, and a connection's
//! end (`tcp_drop`, `tcp_close`, `tcp_discardcb`).
//!
//! Behavior reference: sys/netinet/tcp_var.h, sys/netinet/tcp_subr.c,
//! sys/netinet/tcp_input.c (`tcp_xmit_timer`, `tcp_mss_update`,
//! `tcp_mss`, `tcp_mssopt`).

use alloc::vec::Vec;

use crate::cc::{Algo, Cc, CcVar};
use crate::hdr::{self, MAXOLEN, MAXWIN, Opts, SackBlk, Th, opt, th, tof};
use crate::hostcache::{Metrics, Peer};
use crate::reass::Reass;
use crate::sack::{Holes, SackHint};
use crate::seq::Seq;
use crate::timer::{self, Timers};
use crate::{Conf, Ctx, Errno, Fate, Seg, Sock, State, get, on, tcps};

/// `t_flags` (`TF_*`).
pub mod tf {
    pub const ACKNOW: u32 = 0x0000_0001;
    pub const DELACK: u32 = 0x0000_0002;
    pub const NODELAY: u32 = 0x0000_0004;
    pub const NOOPT: u32 = 0x0000_0008;
    pub const SENTFIN: u32 = 0x0000_0010;
    pub const REQ_SCALE: u32 = 0x0000_0020;
    pub const RCVD_SCALE: u32 = 0x0000_0040;
    pub const REQ_TSTMP: u32 = 0x0000_0080;
    pub const RCVD_TSTMP: u32 = 0x0000_0100;
    pub const SACK_PERMIT: u32 = 0x0000_0200;
    pub const NEEDSYN: u32 = 0x0000_0400;
    pub const NEEDFIN: u32 = 0x0000_0800;
    pub const NOPUSH: u32 = 0x0000_1000;
    pub const PREVVALID: u32 = 0x0000_2000;
    pub const WAKESOR: u32 = 0x0000_4000;
    pub const GPUTINPROG: u32 = 0x0000_8000;
    pub const MORETOCOME: u32 = 0x0001_0000;
    pub const SONOTCONN: u32 = 0x0002_0000;
    pub const LASTIDLE: u32 = 0x0004_0000;
    pub const RXWIN0SENT: u32 = 0x0008_0000;
    pub const FASTRECOVERY: u32 = 0x0010_0000;
    pub const WASFRECOVERY: u32 = 0x0020_0000;
    pub const SIGNATURE: u32 = 0x0040_0000;
    pub const FORCEDATA: u32 = 0x0080_0000;
    pub const TSO: u32 = 0x0100_0000;
    pub const TOE: u32 = 0x0200_0000;
    pub const CLOSED: u32 = 0x0400_0000;
    pub const LRD: u32 = 0x1000_0000;
    pub const CONGRECOVERY: u32 = 0x2000_0000;
    pub const WASCRECOVERY: u32 = 0x4000_0000;
    pub const FASTOPEN: u32 = 0x8000_0000;
}

/// `t_flags2` (`TF2_*`).
pub mod tf2 {
    pub const PLPMTU_BLACKHOLE: u32 = 0x0000_0001;
    pub const PLPMTU_PMTUD: u32 = 0x0000_0002;
    pub const PLPMTU_MAXSEGSNT: u32 = 0x0000_0004;
    pub const DROP_AF_DATA: u32 = 0x0000_0010;
    pub const ECN_PERMIT: u32 = 0x0000_0020;
    pub const ECN_SND_CWR: u32 = 0x0000_0040;
    pub const ECN_SND_ECE: u32 = 0x0000_0080;
    pub const ACE_PERMIT: u32 = 0x0000_0100;
    pub const FBYTES_COMPLETE: u32 = 0x0000_0400;
    pub const ECN_USE_ECT1: u32 = 0x0000_0800;
    pub const PROC_SACK_PROHIBIT: u32 = 0x0010_0000;
    pub const NO_ISS_CHECK: u32 = 0x0040_0000;
}

/// `t_oobflags`.
pub mod oob {
    pub const HAVEDATA: u8 = 0x01;
    pub const HADDATA: u8 = 0x02;
}

/// The connection's endpoints (`struct in_conninfo`): addresses in PCB
/// form (IPv4 in the first four bytes), ports in host order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inc {
    pub laddr: [u8; 16],
    pub faddr: [u8; 16],
    pub lport: u16,
    pub fport: u16,
    pub v6: bool,
    /// A link-local IPv6 peer's zone.
    pub zone: u32,
    pub fib: u32,
}

impl Inc {
    /// The peer, as the host cache knows it.
    #[must_use]
    pub const fn peer(&self) -> Peer {
        Peer {
            addr: self.faddr,
            v6: self.v6,
            zone: self.zone,
        }
    }

    /// The IP and TCP headers' length (`sizeof(struct tcpiphdr)` or the
    /// IPv6 one).
    #[must_use]
    pub const fn min_protoh(&self) -> u32 {
        if self.v6 { 40 + 20 } else { 20 + 20 }
    }
}

/// What the route to the peer allows (`tcp_maxmtu`, `struct tcp_ifcap`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Path {
    /// The path's MTU, 0 without a route.
    pub maxmtu: u32,
    /// The peer is on a local network (`in_localaddr`).
    pub local: bool,
    /// The peer's address is this host's (`in_localip`): TIME_WAIT is
    /// short (`net.inet.tcp.msl_local`).
    pub localip: bool,
    /// The interface's TSO limits, if it does TSO for this family.
    pub tso: Option<TsoCap>,
}

/// The interface's TSO limits (`if_hw_tsomax` and the rest).
#[derive(Clone, Copy, Debug, Default)]
pub struct TsoCap {
    pub tsomax: u32,
    pub tsomaxsegcount: u32,
    pub tsomaxsegsize: u32,
}

/// `struct tcpcb`.
#[derive(Debug)]
pub struct Tcpcb {
    pub inc: Inc,
    pub state: State,
    pub flags: u32,
    pub flags2: u32,
    /// `t_maxseg`: the MSS without options.
    pub t_maxseg: u32,
    pub snd_una: Seq,
    pub snd_max: Seq,
    pub snd_nxt: Seq,
    pub snd_up: Seq,
    pub snd_wnd: u32,
    pub snd_cwnd: u32,
    pub ts_offset: u32,
    pub rfbuf_ts: u32,
    pub rfbuf_cnt: u32,
    pub rcv_numsacks: usize,
    pub tsomax: u32,
    pub tsomaxsegcount: u32,
    pub tsomaxsegsize: u32,
    pub rcv_nxt: Seq,
    pub rcv_adv: Seq,
    pub rcv_wnd: u32,
    pub srtt: i32,
    pub rttvar: i32,
    pub ts_recent: u32,
    pub snd_scale: u8,
    pub rcv_scale: u8,
    pub snd_limited: u8,
    pub request_r_scale: u8,
    pub last_ack_sent: Seq,
    /// `t_rcvtime`: when a segment last arrived.
    pub rcvtime: u64,
    pub rcv_up: Seq,
    pub segq: Reass,
    pub snd_ssthresh: u32,
    pub snd_wl1: Seq,
    pub snd_wl2: Seq,
    pub irs: Seq,
    pub iss: Seq,
    /// `t_acktime`: when new data was last acknowledged (0: never).
    pub acktime: u64,
    pub sndtime: u64,
    pub ts_recent_age: u64,
    pub snd_recover: Seq,
    pub oobflags: u8,
    pub iobc: u8,
    /// `t_rxtcur`, ticks.
    pub rxtcur: u32,
    pub rxtshift: u32,
    /// `t_rtttime`: when the timed segment went (0: none timed).
    pub rtttime: u64,
    pub rtseq: Seq,
    pub starttime: u64,
    pub pmtud_saved_maxseg: u32,
    pub blackhole_enter: u32,
    pub blackhole_exit: u32,
    pub rttmin: u32,
    pub softerror: Option<Errno>,
    pub max_sndwnd: u32,
    pub snd_cwnd_prev: u32,
    pub snd_ssthresh_prev: u32,
    pub snd_recover_prev: Seq,
    pub sndzerowin: u32,
    /// `t_badrxtwin`.
    pub badrxtwin: u64,
    pub snd_holes: Holes,
    pub snd_fack: Seq,
    /// `sackblks`: the receiver's blocks to report, most recent first.
    pub sackblks: [SackBlk; hdr::MAX_SACK_BLKS],
    pub sackhint: SackHint,
    pub rttlow: i32,
    pub sndrexmitpack: u32,
    pub rcvoopack: u32,
    pub cc: Cc,
    pub ccv: CcVar,
    pub bytes_acked: u32,
    pub maxunacktime: u32,
    pub keepinit: u32,
    pub keepidle: u32,
    pub keepintvl: u32,
    pub keepcnt: u32,
    pub dupacks: u32,
    pub rcep: u32,
    pub scep: u32,
    pub sndbytes: u64,
    pub snd_rxt_bytes: u64,
    pub dsack_bytes: u32,
    pub dsack_pack: u32,
    pub rttupdated: u8,
    pub challenge_ack_end: u64,
    pub challenge_ack_cnt: u32,
    pub timers: Timers,
    /// `t_delayed_ack` (`TCP_DELACK`): segments to hold the ACK for.
    pub delayed_ack: u32,
    /// The route is suspect (`in_losing` was called).
    pub losing: bool,
    /// `TCP_MAXSEG` set by the user: the most `t_maxseg` may be.
    pub user_maxseg: u32,
    /// `INP_DONTFRAG`-style DF on its packets (path MTU discovery).
    pub pmtud: bool,
    /// The socket's IP type of service or traffic class, whose ECN bits
    /// TCP owns (`inp_ip_tos`).
    pub ip_tos: u8,
}

impl Tcpcb {
    /// `tcp_newtcpcb`: a connection between `inc`'s endpoints, with
    /// congestion control `algo`.
    #[must_use]
    pub fn new(inc: Inc, algo: Algo, conf: &Conf, now: u64) -> Self {
        let mut flags = match get(&conf.rfc1323) {
            0 => 0,
            2 => tf::REQ_SCALE,
            3 => tf::REQ_TSTMP,
            _ => tf::REQ_SCALE | tf::REQ_TSTMP,
        };
        if on(&conf.sack_enable) {
            flags |= tf::SACK_PERMIT;
        }
        if on(&conf.do_lrd) {
            flags |= tf::LRD;
        }
        let rexmit_initial = get(&conf.rexmit_initial);
        let rexmit_min = get(&conf.rexmit_min) as u32;
        let t_maxseg = if inc.v6 {
            get(&conf.v6mssdflt) as u32
        } else {
            get(&conf.mssdflt) as u32
        };
        let maxwin = MAXWIN << hdr::MAX_WINSHIFT;
        Self {
            inc,
            state: State::Closed,
            flags,
            flags2: 0,
            t_maxseg,
            snd_una: 0,
            snd_max: 0,
            snd_nxt: 0,
            snd_up: 0,
            snd_wnd: 0,
            snd_cwnd: maxwin,
            ts_offset: 0,
            rfbuf_ts: 0,
            rfbuf_cnt: 0,
            rcv_numsacks: 0,
            tsomax: 0,
            tsomaxsegcount: 0,
            tsomaxsegsize: 0,
            rcv_nxt: 0,
            rcv_adv: 0,
            rcv_wnd: 0,
            srtt: timer::SRTTBASE as i32,
            rttvar: ((rexmit_initial - timer::SRTTBASE as i32) << timer::RTTVAR_SHIFT) / 4,
            ts_recent: 0,
            snd_scale: 0,
            rcv_scale: 0,
            snd_limited: 0,
            request_r_scale: 0,
            last_ack_sent: 0,
            rcvtime: now,
            rcv_up: 0,
            segq: Reass::default(),
            snd_ssthresh: maxwin,
            snd_wl1: 0,
            snd_wl2: 0,
            irs: 0,
            iss: 0,
            acktime: 0,
            sndtime: 0,
            ts_recent_age: 0,
            snd_recover: 0,
            oobflags: 0,
            iobc: 0,
            rxtcur: rexmit_initial as u32,
            rxtshift: 0,
            rtttime: 0,
            rtseq: 0,
            starttime: 0,
            pmtud_saved_maxseg: 0,
            blackhole_enter: 0,
            blackhole_exit: 0,
            rttmin: rexmit_min,
            softerror: None,
            max_sndwnd: 0,
            snd_cwnd_prev: 0,
            snd_ssthresh_prev: 0,
            snd_recover_prev: 0,
            sndzerowin: 0,
            badrxtwin: 0,
            snd_holes: Holes::default(),
            snd_fack: 0,
            sackblks: [SackBlk::default(); hdr::MAX_SACK_BLKS],
            sackhint: SackHint::default(),
            rttlow: 0,
            sndrexmitpack: 0,
            rcvoopack: 0,
            cc: Cc::new(algo),
            ccv: CcVar::default(),
            bytes_acked: 0,
            maxunacktime: 0,
            keepinit: 0,
            keepidle: 0,
            keepintvl: 0,
            keepcnt: 0,
            dupacks: 0,
            rcep: 0,
            scep: 0,
            sndbytes: 0,
            snd_rxt_bytes: 0,
            dsack_bytes: 0,
            dsack_pack: 0,
            rttupdated: 0,
            challenge_ack_end: 0,
            challenge_ack_cnt: 0,
            timers: Timers::default(),
            delayed_ack: 1,
            losing: false,
            user_maxseg: 0,
            pmtud: true,
            ip_tos: 0,
        }
    }

    /// `tcp_state_change`.
    pub fn set_state(&mut self, s: State, stats: &crate::Stats) {
        stats.state_dec(self.state);
        stats.state_inc(s);
        self.state = s;
    }

    /// `tcp_maxseg`: the data a full segment carries, after the options
    /// it would send.
    #[must_use]
    pub fn maxseg(&self) -> u32 {
        if self.flags & tf::NOOPT != 0 {
            return self.t_maxseg;
        }
        let pad = |l: usize| l.div_ceil(4) * 4;
        let mut optlen = if self.state.have_established() {
            let mut n = if self.flags & tf::RCVD_TSTMP != 0 {
                opt::LEN_TSTAMP_APPA
            } else {
                0
            };
            if self.flags & tf::SACK_PERMIT != 0 && self.rcv_numsacks > 0 {
                n += opt::LEN_SACKHDR + self.rcv_numsacks * opt::LEN_SACK;
                n = pad(n);
            }
            n
        } else {
            self.syn_optlen()
        };
        optlen = optlen.min(MAXOLEN);
        self.t_maxseg.saturating_sub(optlen as u32)
    }

    /// The options a SYN carries, as `tcp_maxseg` counts them.
    fn syn_optlen(&self) -> usize {
        let pad = |l: usize| l.div_ceil(4) * 4;
        let mut n = if self.flags & tf::REQ_TSTMP != 0 {
            opt::LEN_TSTAMP_APPA
        } else {
            pad(opt::LEN_MAXSEG)
        };
        if self.flags & tf::REQ_SCALE != 0 {
            n += pad(opt::LEN_WINDOW);
        }
        if self.flags & tf::SACK_PERMIT != 0 {
            n += pad(opt::LEN_SACK_PERMITTED);
        }
        n
    }

    /// `tcp_fixed_maxseg`: as [`Tcpcb::maxseg`], without SACK blocks, so
    /// congestion control's arithmetic stays in whole segments.
    #[must_use]
    pub fn fixed_maxseg(&self) -> u32 {
        if self.flags & tf::NOOPT != 0 {
            return self.t_maxseg;
        }
        let optlen = if self.state.have_established() {
            if self.flags & tf::RCVD_TSTMP != 0 {
                opt::LEN_TSTAMP_APPA
            } else {
                0
            }
        } else {
            self.syn_optlen()
        };
        self.t_maxseg
            .saturating_sub(optlen.min(MAXOLEN) as u32)
            .max(1)
    }

    /// `TCP_REXMTVAL`.
    #[must_use]
    pub fn rexmtval(&self) -> u32 {
        let v = ((self.srtt >> (timer::RTT_SHIFT - timer::DELTA_SHIFT)) + self.rttvar)
            >> timer::DELTA_SHIFT;
        (v.max(0) as u32).max(self.rttmin)
    }

    /// `tcp_xmit_timer`: a round trip of `rtt` ticks (at least 1) was
    /// measured.
    pub(crate) fn xmit_timer(&mut self, cx: &Ctx, rtt: u32) {
        cx.stats.inc(tcps::RTTUPDATED);
        self.rttupdated = self.rttupdated.saturating_add(1);
        let rtt = rtt as i32;
        if self.srtt != 0 && self.rxtshift <= timer::RTT_INVALIDATE {
            let mut delta = ((rtt - 1) << timer::DELTA_SHIFT)
                - (self.srtt >> (timer::RTT_SHIFT - timer::DELTA_SHIFT));
            self.srtt += delta;
            if self.srtt <= 0 {
                self.srtt = 1;
            }
            delta = delta.abs();
            delta -= self.rttvar >> (timer::RTTVAR_SHIFT - timer::DELTA_SHIFT);
            self.rttvar += delta;
            if self.rttvar <= 0 {
                self.rttvar = 1;
            }
        } else {
            self.srtt = rtt << timer::RTT_SHIFT;
            self.rttvar = rtt << (timer::RTTVAR_SHIFT - 1);
        }
        self.rtttime = 0;
        self.rxtshift = 0;
        self.rxtcur = timer::rangeset(
            cx.conf,
            self.rexmtval(),
            self.rttmin.max(rtt as u32 + 2),
            get(&cx.conf.rexmit_max) as u32,
        );
        self.softerror = None;
    }

    /// `tcp_mss_update`: `t_maxseg` from the path, the host cache, and
    /// the peer's offer (`offer` 0: none; `mtuoffer`: an ICMP-reported
    /// MTU). Returns the host cache's metrics.
    pub fn mss_update(
        &mut self,
        cx: &Ctx,
        hc: &crate::hostcache::HostCache,
        path: &Path,
        offer: i32,
        mtuoffer: Option<u32>,
    ) -> Metrics {
        let min_protoh = self.inc.min_protoh();
        let mut offer = offer;
        if let Some(m) = mtuoffer {
            offer = m as i32 - min_protoh as i32;
        }
        let dflt = if self.inc.v6 {
            get(&cx.conf.v6mssdflt)
        } else {
            get(&cx.conf.mssdflt)
        };
        self.t_maxseg = dflt as u32;
        if path.maxmtu == 0 {
            return Metrics::default();
        }
        let offer = match offer {
            0 => self.t_maxseg as i32,
            o => o.max(get(&cx.conf.minmss)),
        };
        let m = hc.get(&self.inc.peer());
        let mut mss = if m.mtu != 0 {
            m.mtu.min(path.maxmtu) as i32 - min_protoh as i32
        } else {
            let mut mss = path.maxmtu as i32 - min_protoh as i32;
            if !on(&cx.conf.path_mtu_discovery) && !path.local {
                mss = mss.min(dflt);
            }
            mss
        };
        mss = mss.min(offer).max(64);
        if self.user_maxseg != 0 {
            mss = mss.min(self.user_maxseg as i32);
        }
        self.t_maxseg = mss as u32;
        self.sack_prohibit(cx.conf);
        m
    }

    fn sack_prohibit(&mut self, conf: &Conf) {
        if self.t_maxseg < get(&conf.mssdflt) as u32 {
            self.flags2 |= tf2::PROC_SACK_PROHIBIT;
        } else {
            self.flags2 &= !tf2::PROC_SACK_PROHIBIT;
        }
    }

    /// `tcp_mss`: the MSS for the peer's `offer`, and the socket buffers
    /// rounded to whole segments. Returns the host cache's metrics.
    pub fn mss<S: Sock>(
        &mut self,
        so: &mut S,
        cx: &Ctx,
        hc: &crate::hostcache::HostCache,
        path: &Path,
        offer: i32,
    ) -> Metrics {
        let m = self.mss_update(cx, hc, path, offer, None);
        let mut mss = self.t_maxseg;
        let sb_max = get(&cx.conf.sb_max) as u32;
        let bufsize = if so.snd_hiwat() == get(&cx.conf.sendspace) as u32 && m.sendpipe != 0 {
            m.sendpipe
        } else {
            so.snd_hiwat()
        };
        if bufsize < mss {
            mss = bufsize;
        } else {
            let b = bufsize.div_ceil(mss) * mss;
            let b = b.min(sb_max);
            if b > so.snd_hiwat() {
                let _ = so.snd_reserve(b);
            }
        }
        self.t_maxseg = mss.max(64);
        self.sack_prohibit(cx.conf);
        let bufsize = if so.rcv_hiwat() == get(&cx.conf.recvspace) as u32 && m.recvpipe != 0 {
            m.recvpipe
        } else {
            so.rcv_hiwat()
        };
        if bufsize > mss {
            let b = (bufsize.div_ceil(mss) * mss).min(sb_max);
            if b > so.rcv_hiwat() {
                let _ = so.rcv_reserve(b);
            }
        }
        if let Some(t) = path.tso
            && on(&cx.conf.tso)
        {
            self.flags |= tf::TSO;
            self.tsomax = t.tsomax;
            self.tsomaxsegcount = t.tsomaxsegcount;
            self.tsomaxsegsize = t.tsomaxsegsize;
        }
        m
    }

    /// `tcp_drop`: ends the connection with `e`, sending a reset if the
    /// peer knows of it.
    pub fn drop<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, e: Errno) -> Fate {
        self.drop_with(so, cx, Some(e))
    }

    /// `tcp_drop` with an error of 0 (`None`), which clears `so_error`.
    pub fn drop_with<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, e: Option<Errno>) -> Fate {
        if self.state.have_rcvd_syn() {
            self.set_state(State::Closed, cx.stats);
            let _ = self.output(so, cx);
            cx.stats.inc(tcps::DROPS);
        } else {
            cx.stats.inc(tcps::CONNDROPS);
        }
        let e = match (e, self.softerror) {
            (Some(Errno::ETIMEDOUT), Some(s)) => Some(s),
            _ => e,
        };
        so.set_error(e);
        self.close(so, cx)
    }

    /// `tcp_close`: the connection is over; the caller takes it out of
    /// the lookups (`in_pcbdrop`).
    pub fn close<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        self.timer_stop();
        cx.stats.inc(tcps::CLOSED);
        if self.state != State::Closed {
            self.set_state(State::Closed, cx.stats);
        }
        self.free_sackholes(cx.stats);
        so.disconnected();
        Fate::Closed
    }

    /// `tcp_discardcb`: the control block goes; what it learned of the
    /// path goes to the host cache.
    pub fn discard(&mut self, cx: &Ctx, hc: &crate::hostcache::HostCache, snd_hiwat: u32) {
        self.segq.flush();
        cx.stats.state_dec(self.state);
        if self.rttupdated >= 4 {
            let mut ss = self.snd_ssthresh;
            if ss != 0 && ss < snd_hiwat / 2 {
                ss = ((ss + self.t_maxseg / 2) / self.t_maxseg.max(1)).max(2);
                ss *= self.t_maxseg + self.inc.min_protoh();
            } else {
                ss = 0;
            }
            hc.update(
                &self.inc.peer(),
                &Metrics {
                    ssthresh: ss,
                    rtt: self.srtt as u32,
                    rttvar: self.rttvar as u32,
                    cwnd: self.snd_cwnd,
                    ..Metrics::default()
                },
                cx.stats,
            );
        }
    }

    /// The receive window to advertise without scaling limits:
    /// `sbspace`, at most `TCP_MAXWIN << rcv_scale`.
    pub(crate) fn rcv_space_win<S: Sock>(&self, so: &S) -> u32 {
        let w = so.rcv_space().max(0) as u64;
        w.min(u64::from(MAXWIN) << self.rcv_scale) as u32
    }

    /// `tcp_respond` from a connection: a bare segment with `ack`, `seq`,
    /// and `flags` (an ACK, a keepalive probe, a challenge ACK, or a
    /// reset), with timestamps if the connection uses them, advertising
    /// the receive buffer's room.
    pub(crate) fn respond<S: Sock>(
        &mut self,
        so: &S,
        cx: &mut Ctx,
        ack: Seq,
        seq: Seq,
        flags: u16,
    ) {
        let win = self.rcv_space_win(so);
        self.respond_win(cx, ack, seq, flags, Some(win));
    }

    /// [`Tcpcb::respond`] with the window computed from `win` (bytes),
    /// or zero for a reset.
    pub(crate) fn respond_win(
        &mut self,
        cx: &mut Ctx,
        ack: Seq,
        seq: Seq,
        mut flags: u16,
        win: Option<u32>,
    ) {
        let mut to = Opts::default();
        let mut ect = 0;
        let incl_opts = self.flags & tf::NOOPT == 0;
        if incl_opts {
            ect = self.ecn_output_established(cx, &mut flags, 0, false);
            if self.flags & tf::RCVD_TSTMP != 0 {
                to.flags |= tof::TS;
                to.tsval = cx.ts_ticks().wrapping_add(self.ts_offset);
                to.tsecr = self.ts_recent;
            }
        }
        let win = if flags & th::RST != 0 {
            0
        } else {
            win.unwrap_or(0) >> self.rcv_scale
        };
        let t = Th {
            sport: self.inc.lport,
            dport: self.inc.fport,
            seq,
            ack,
            off: 0,
            flags,
            win: win.min(u32::from(u16::MAX)) as u16,
            sum: 0,
            urp: 0,
        };
        if let Some(pkt) = build(t, &to, None) {
            cx.out.push(Seg {
                pkt,
                ecn: ect,
                df: self.pmtud,
                tso_segsz: 0,
            });
        }
    }
}

/// Builds a segment: header `t` (its offset set here), options `to`,
/// then `data`. `None` out of buffers.
#[must_use]
pub fn build(mut t: Th, to: &Opts, data: Option<mbuf::Chain>) -> Option<mbuf::Packet> {
    let mut ob = [0u8; MAXOLEN + 4];
    let olen = to.write(&mut ob);
    t.off = hdr::HDR_LEN + olen;
    // The header goes in the first mbuf, after the room kept for the IP
    // and link headers; the data follows by reference.
    let mut h = [0u8; hdr::HDR_LEN + MAXOLEN];
    t.write(&mut h[..hdr::HDR_LEN]);
    h[hdr::HDR_LEN..t.off].copy_from_slice(&ob[..olen]);
    let mut p = mbuf::Packet::for_output(t.off).ok()?;
    p.data.extend_from_slice(&h[..t.off]).ok()?;
    if let Some(d) = data {
        p.data.append(d);
    }
    Some(p)
}

/// The SACK blocks a connection reports, for tests and `TCP_INFO`.
impl Tcpcb {
    #[must_use]
    pub fn sack_blocks(&self) -> Vec<SackBlk> {
        self.sackblks[..self.rcv_numsacks].to_vec()
    }
}
