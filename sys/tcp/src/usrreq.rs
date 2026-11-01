// SPDX-License-Identifier: BSD-2-Clause
//! What the socket's user asks of a connection (`tcp_usrreq.c`): to
//! connect, to send (with urgent data and a FIN after it), to take what
//! arrived (a window update), to shut down, to disconnect, to abort, and
//! to close. Address binding and the PCB tables stay the caller's.
//!
//! Behavior reference: sys/netinet/tcp_usrreq.c (`tcp_connect`,
//! `tcp_usr_connect`, `tcp_usr_send`, `tcp_usr_rcvd`, `tcp_usr_shutdown`,
//! `tcp_usrclosed`, `tcp_disconnect`, `tcp_usr_abort`, `tcp_usr_close`).

use crate::hdr::{MAX_WINSHIFT, MAXWIN};
use crate::tcb::{Tcpcb, tf};
use crate::timer::Tt;
use crate::{Ctx, Errno, Fate, Sock, State, get, on, so, tcps};

/// `TTCP_CLIENT_SND_WND`: an implied connect's send window before the
/// peer's arrives.
const TTCP_CLIENT_SND_WND: u32 = 4096;

impl Tcpcb {
    /// `tcp_connect` after `in_pcbconnect`, and `tcp_usr_connect`'s
    /// start: SYN_SENT with initial sequence number `iss` and timestamp
    /// offset `ts_offset`, the connection timer, and the SYN.
    pub fn connect<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, iss: u32, ts_offset: u32) -> Fate {
        self.start(cx, iss, ts_offset);
        self.output(so, cx)
    }

    /// `tcp_usr_send`'s implied connect (`sendto` naming the peer of an
    /// unconnected socket, the data already queued): as [`Self::connect`],
    /// with `TTCP_CLIENT_SND_WND` as the send window until the peer's comes
    /// and the MSS of a connection that has seen no SYN (`tcp_mss(tp, -1)`).
    pub fn connect_implied<S: Sock>(
        &mut self,
        so: &mut S,
        cx: &mut Ctx,
        iss: u32,
        ts_offset: u32,
    ) -> Fate {
        self.start(cx, iss, ts_offset);
        self.snd_wnd = TTCP_CLIENT_SND_WND;
        let path = (cx.path)(&self.inc);
        let hc = cx.hc;
        self.mss(so, cx, hc, &path, -1);
        self.output(so, cx)
    }

    /// `tcp_connect`'s part after `in_pcbconnect`.
    fn start(&mut self, cx: &mut Ctx, iss: u32, ts_offset: u32) {
        let sb_max = get(&cx.conf.sb_max) as u64;
        while self.request_r_scale < MAX_WINSHIFT
            && (u64::from(MAXWIN) << self.request_r_scale) < sb_max
        {
            self.request_r_scale += 1;
        }
        cx.stats.inc(tcps::CONNATTEMPT);
        self.set_state(State::SynSent, cx.stats);
        self.iss = iss;
        if self.flags & tf::REQ_TSTMP != 0 {
            self.ts_offset = ts_offset;
        }
        self.sendseqinit();
        let d = self.keepinit(cx.conf);
        self.timer_activate(Tt::Keep, d, cx.now);
    }

    /// `tcp_usr_listen`'s state change.
    pub fn listen(&mut self, stats: &crate::Stats) {
        self.set_state(State::Listen, stats);
    }

    /// `tcp_usr_send` after the data went into the send buffer: a FIN
    /// after it with `eof`, then output (`more`: more is coming at once,
    /// `PRUS_MORETOCOME`).
    pub fn send<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, eof: bool, more: bool) -> Fate {
        if self.acktime == 0 {
            self.acktime = cx.now;
        }
        if eof && self.usrclosed(so, cx) == Fate::Closed {
            return Fate::Closed;
        }
        if more {
            self.flags |= tf::MORETOCOME;
        }
        let f = self.output(so, cx);
        self.flags &= !tf::MORETOCOME;
        f
    }

    /// `tcp_usr_send` with `PRUS_OOB`, the urgent data already in the
    /// send buffer: the urgent pointer at its end, forced out.
    pub fn send_oob<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        if self.acktime == 0 {
            self.acktime = cx.now;
        }
        self.snd_up = self.snd_una.wrapping_add(so.snd_cc());
        self.flags |= tf::FORCEDATA;
        let f = self.output(so, cx);
        self.flags &= !tf::FORCEDATA;
        f
    }

    /// `tcp_usr_rcvd`: the user read; a window update may be due.
    pub fn rcvd<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        self.output(so, cx)
    }

    /// `tcp_usr_shutdown`'s `SHUT_WR` (the caller has marked the send
    /// buffer `SBS_CANTSENDMORE`): a FIN after the data.
    pub fn shutdown<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        if self.usrclosed(so, cx) == Fate::Closed {
            return Fate::Closed;
        }
        self.output(so, cx)
    }

    /// `tcp_usrclosed`: the user will send no more.
    pub(crate) fn usrclosed<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        match self.state {
            State::Listen | State::Closed => {
                if self.state == State::Listen {
                    self.set_state(State::Closed, cx.stats);
                }
                return self.close(so, cx);
            }
            State::SynSent | State::SynReceived => self.flags |= tf::NEEDFIN,
            State::Established => self.set_state(State::FinWait1, cx.stats),
            State::CloseWait => self.set_state(State::LastAck, cx.stats),
            _ => {}
        }
        if self.acktime == 0 {
            self.acktime = cx.now;
        }
        if self.state >= State::FinWait2 {
            self.free_sackholes(cx.stats);
            so.disconnected();
            if self.state == State::FinWait2 {
                let d = if on(&cx.conf.fast_finwait2_recycle) {
                    get(&cx.conf.finwait2_timeout) as u32
                } else {
                    self.maxidle(cx.conf)
                };
                self.timer_activate(Tt::TwoMsl, d, cx.now);
            }
        }
        Fate::Alive
    }

    /// `tcp_disconnect`: before the handshake completes, the connection
    /// just goes; with `SO_LINGER` of zero, a reset; otherwise a FIN after
    /// the data, the unread data thrown away (the caller flushes the
    /// receive buffer, `sbflush`).
    pub fn disconnect<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, linger_zero: bool) -> Fate {
        if self.state < State::Established {
            return self.close(so, cx);
        }
        if so.options() & so::LINGER != 0 && linger_zero {
            return self.drop_with(so, cx, None);
        }
        so.disconnecting();
        if self.usrclosed(so, cx) == Fate::Closed {
            return Fate::Closed;
        }
        self.output(so, cx)
    }

    /// `tcp_usr_abort`.
    pub fn abort<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx) -> Fate {
        self.drop(so, cx, Errno::ECONNABORTED)
    }

    /// `tcp_usr_close`: the descriptor's last close.
    pub fn usr_close<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, linger_zero: bool) -> Fate {
        if self.state == State::TimeWait {
            return Fate::Alive;
        }
        self.flags |= tf::CLOSED;
        self.disconnect(so, cx, linger_zero)
    }
}

/// What an ICMP error about one of the connection's segments asks
/// (`tcp_ctlinput`'s notify functions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    /// `tcp_notify`: a soft error, or the end of a connection that never
    /// got going.
    Soft(Errno),
    /// `tcp_drop_syn_sent` (`net.inet.tcp.icmp_may_rst`): a connect
    /// refused or unreachable fails at once.
    DropSynSent(Errno),
    /// `tcp_mtudisc`: the path's MTU is `mtu` (its headers included).
    Mtu(u32),
}

impl Tcpcb {
    /// `tcp_ctlinput`'s work on this connection, for an error about the
    /// segment with sequence number `th_seq`.
    pub fn ctlinput<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, th_seq: u32, n: Notify) -> Fate {
        if !(crate::seq::geq(th_seq, self.snd_una) && crate::seq::lt(th_seq, self.snd_max)) {
            return Fate::Alive;
        }
        match n {
            Notify::Mtu(mtu) => {
                if mtu < self.t_maxseg + self.inc.min_protoh() {
                    cx.hc.update_mtu(&self.inc.peer(), mtu, cx.stats);
                    return self.mtudisc(so, cx, Some(mtu));
                }
                Fate::Alive
            }
            Notify::DropSynSent(e) => {
                if self.state == State::SynSent {
                    return self.drop(so, cx, e);
                }
                Fate::Alive
            }
            Notify::Soft(e) => {
                if self.state == State::Established
                    && matches!(
                        e,
                        Errno::EHOSTUNREACH | Errno::ENETUNREACH | Errno::EHOSTDOWN
                    )
                {
                    // The route is dropped; the next send finds another.
                    self.losing = true;
                    Fate::Alive
                } else if self.state < State::Established
                    && self.rxtshift > 3
                    && self.softerror.is_some()
                {
                    self.drop(so, cx, e)
                } else {
                    self.softerror = Some(e);
                    so.wake_writers();
                    Fate::Alive
                }
            }
        }
    }

    /// `tcp_mtudisc`: the segment size for a smaller path MTU, and what
    /// was sent resent at it.
    pub fn mtudisc<S: Sock>(&mut self, so: &mut S, cx: &mut Ctx, mtu: Option<u32>) -> Fate {
        let path = (cx.path)(&self.inc);
        let hc = cx.hc;
        let _ = self.mss_update(cx, hc, &path, -1, mtu);
        if so.snd_hiwat() < self.t_maxseg {
            self.t_maxseg = so.snd_hiwat();
            if self.t_maxseg < get(&cx.conf.mssdflt) as u32 {
                self.flags2 |= crate::tcb::tf2::PROC_SACK_PROHIBIT;
            } else {
                self.flags2 &= !crate::tcb::tf2::PROC_SACK_PROHIBIT;
            }
        }
        cx.stats.inc(tcps::MTURESENT);
        self.rtttime = 0;
        self.snd_nxt = self.snd_una;
        self.free_sackholes(cx.stats);
        self.snd_recover = self.snd_max;
        if self.flags & tf::SACK_PERMIT != 0 {
            self.flags &= !tf::FASTRECOVERY;
        }
        self.output(so, cx)
    }
}

/// `ip_next_mtu` going down: the plateau below `mtu` (RFC 1191).
#[must_use]
pub fn next_mtu_down(mtu: u32) -> u32 {
    const TAB: [u32; 13] = [
        65535, 32000, 17914, 8166, 4352, 2002, 1492, 1280, 1006, 508, 296, 68, 0,
    ];
    TAB.into_iter().find(|&t| mtu > t).unwrap_or(0)
}
