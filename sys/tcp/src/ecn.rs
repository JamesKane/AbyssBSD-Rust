// SPDX-License-Identifier: BSD-2-Clause
//! Explicit congestion notification (`tcp_ecn.c`): RFC 3168, and
//! Accurate ECN's handshake and counters (`net.inet.tcp.ecn.enable` 3
//! and 4). The default, 2, accepts ECN from peers that ask and does not
//! ask itself.
//!
//! Behavior reference: sys/netinet/tcp_ecn.c.

use crate::cc::ccf;
use crate::hdr::th;
use crate::seq;
use crate::tcb::{Tcpcb, tf, tf2};
use crate::{Ctx, get, tcps};

/// The IP header's ECN field (`IPTOS_ECN_*`).
pub const NOTECT: u8 = 0x00;
pub const ECT1: u8 = 0x01;
pub const ECT0: u8 = 0x02;
pub const CE: u8 = 0x03;
pub const MASK: u8 = 0x03;

/// `SCF_*` ECN codepoints of a SYN cache entry.
pub mod scf {
    pub const ECN_MASK: u32 = 0x700;
    pub const ECN: u32 = 0x100;
    pub const ACE_N: u32 = 0x400;
    pub const ACE_0: u32 = 0x500;
    pub const ACE_1: u32 = 0x600;
    pub const ACE_CE: u32 = 0x700;
}

const ACE: u16 = th::AE | th::CWR | th::ECE;
const ACE_SHIFT: u32 = 6;

/// `tcp_ecn_get_ace`.
const fn get_ace(f: u16) -> u32 {
    ((f & ACE) >> ACE_SHIFT) as u32
}

/// `tcp_ecn_set_ace`.
fn set_ace(f: &mut u16, rcep: u32) {
    *f &= !ACE;
    *f |= ((rcep << ACE_SHIFT) as u16) & ACE;
}

/// The receive counter's start for the IP ECN field of a SYN.
const fn rcep_for(iptos: u8) -> u32 {
    match iptos & MASK {
        NOTECT => 0b010,
        ECT0 => 0b100,
        ECT1 => 0b011,
        _ => 0b110,
    }
}

fn count_rcv(cx: &Ctx, iptos: u8) {
    match iptos & MASK {
        CE => cx.stats.inc(tcps::ECN_RCVCE),
        ECT0 => cx.stats.inc(tcps::ECN_RCVECT0),
        ECT1 => cx.stats.inc(tcps::ECN_RCVECT1),
        _ => {}
    }
}

/// `tcp_ecn_syncache_add`: the `SCF_*` a SYN with `thflags` and `iptos`
/// asks for.
#[must_use]
pub fn syncache_add(cx: &Ctx, thflags: u16, iptos: u8) -> u32 {
    count_rcv(cx, iptos);
    let ace = matches!(get(&cx.conf.ecn_enable), 3 | 4);
    let ace_flags = || match iptos & MASK {
        CE => scf::ACE_CE,
        ECT0 => scf::ACE_0,
        ECT1 => scf::ACE_1,
        _ => scf::ACE_N,
    };
    match thflags & ACE {
        0 => 0,
        f if f == th::CWR | th::ECE => scf::ECN,
        f if f == ACE => {
            if ace {
                ace_flags()
            } else {
                scf::ECN
            }
        }
        _ if ace => ace_flags(),
        _ => 0,
    }
}

/// `tcp_ecn_syncache_respond`: the SYN-ACK's ECN flags.
#[must_use]
pub fn syncache_respond(cx: &Ctx, thflags: u16, scflags: u32) -> u16 {
    if thflags & th::SYN == 0 || scflags & scf::ECN_MASK == 0 {
        return thflags;
    }
    let (f, ace) = match scflags & scf::ECN_MASK {
        scf::ECN => (th::ECE, None),
        scf::ACE_N => (th::CWR, Some(tcps::ACE_NECT)),
        scf::ACE_0 => (th::AE, Some(tcps::ACE_ECT0)),
        scf::ACE_1 => (th::ECE | th::CWR, Some(tcps::ACE_ECT1)),
        _ => (th::AE | th::CWR, Some(tcps::ACE_CE)),
    };
    cx.stats.inc(tcps::ECN_SHS);
    if let Some(s) = ace {
        cx.stats.inc(s);
    }
    thflags | f
}

impl Tcpcb {
    /// `tcp_ecn_syncache_socket`.
    pub(crate) fn ecn_syncache_socket(&mut self, scflags: u32) {
        match scflags & scf::ECN_MASK {
            0 => {}
            scf::ECN => self.flags2 |= tf2::ECN_PERMIT,
            scf::ACE_CE => {
                self.flags2 |= tf2::ACE_PERMIT;
                self.scep = 6;
                self.rcep = 6;
            }
            _ => {
                self.flags2 |= tf2::ACE_PERMIT;
                self.scep = 5;
                self.rcep = 5;
            }
        }
    }

    /// `tcp_ecn_input_syn_sent`: the SYN-ACK says what the peer agreed to.
    pub(crate) fn ecn_input_syn_sent(&mut self, cx: &Ctx, thflags: u16, iptos: u8) {
        match get(&cx.conf.ecn_enable) {
            1 | 2 => {
                if thflags & (th::CWR | th::ECE) == th::ECE {
                    self.flags2 |= tf2::ECN_PERMIT;
                    self.flags2 &= !tf2::ACE_PERMIT;
                    cx.stats.inc(tcps::ECN_SHS);
                }
            }
            3 | 4 => {
                let ace = |t: &mut Self, scep: u32, stat: usize| {
                    t.flags2 |= tf2::ACE_PERMIT;
                    t.flags2 &= !tf2::ECN_PERMIT;
                    t.scep = scep;
                    cx.stats.inc(tcps::ECN_SHS);
                    cx.stats.inc(stat);
                };
                match thflags & ACE {
                    f if f == th::ECE => {
                        self.flags2 |= tf2::ECN_PERMIT;
                        self.flags2 &= !tf2::ACE_PERMIT;
                        cx.stats.inc(tcps::ECN_SHS);
                    }
                    f if f == th::CWR => ace(self, 5, tcps::ACE_NECT),
                    f if f == th::AE => ace(self, 5, tcps::ACE_ECT0),
                    f if f == th::CWR | th::ECE => ace(self, 5, tcps::ACE_ECT1),
                    f if f == th::AE | th::CWR => {
                        ace(self, 6, tcps::ACE_NECT);
                        self.snd_cwnd = 2 * self.maxseg();
                    }
                    _ => self.flags2 &= !(tf2::ECN_PERMIT | tf2::ACE_PERMIT),
                }
                self.rcep = rcep_for(iptos);
            }
            _ => {}
        }
    }

    /// `tcp_ecn_input_parallel_syn`: a SYN crossed ours.
    pub(crate) fn ecn_input_parallel_syn(&mut self, cx: &Ctx, thflags: u16, iptos: u8) {
        if thflags & th::ACK != 0 {
            return;
        }
        match get(&cx.conf.ecn_enable) {
            1 | 2 => {
                if thflags & (th::CWR | th::ECE) == th::CWR | th::ECE {
                    self.flags2 |= tf2::ECN_PERMIT | tf2::ECN_SND_ECE;
                    self.flags2 &= !tf2::ACE_PERMIT;
                    cx.stats.inc(tcps::ECN_SHS);
                }
            }
            3 | 4 => match thflags & ACE {
                f if f == th::CWR | th::ECE => {
                    self.flags2 |= tf2::ECN_PERMIT | tf2::ECN_SND_ECE;
                    self.flags2 &= !tf2::ACE_PERMIT;
                    cx.stats.inc(tcps::ECN_SHS);
                }
                ACE => {
                    self.flags2 |= tf2::ACE_PERMIT;
                    self.flags2 &= !tf2::ECN_PERMIT;
                    cx.stats.inc(tcps::ECN_SHS);
                    self.rcep = rcep_for(iptos);
                }
                _ => self.flags2 &= !(tf2::ECN_PERMIT | tf2::ACE_PERMIT),
            },
            _ => {}
        }
    }

    /// `tcp_ecn_input_segment`: a segment's ECN marks; returns the
    /// newly reported congestion-experienced packets.
    pub(crate) fn ecn_input_segment(
        &mut self,
        cx: &Ctx,
        thflags: u16,
        pkts: u32,
        iptos: u8,
    ) -> u32 {
        let mut delta = 0;
        count_rcv(cx, iptos);
        if self.flags2 & (tf2::ECN_PERMIT | tf2::ACE_PERMIT) == 0 {
            return 0;
        }
        if self.flags2 & tf2::ACE_PERMIT != 0 {
            if iptos & MASK == CE {
                self.rcep += 1;
            }
            if self.flags2 & tf2::ECN_PERMIT != 0 {
                delta = (get_ace(thflags) + 8 - (self.scep & 7)) & 7;
                if delta < pkts {
                    delta = pkts - ((pkts - delta) & 7);
                }
                self.scep += delta;
            } else {
                match get_ace(thflags) {
                    0b110 => {
                        self.scep = 6;
                        self.snd_cwnd = 2 * self.maxseg();
                    }
                    _ => self.scep = 5,
                }
                self.flags2 |= tf2::ECN_PERMIT;
            }
        } else {
            if thflags & (th::SYN | th::ECE) == th::ECE {
                delta = 1;
                self.scep += 1;
            }
            if thflags & th::CWR != 0 {
                self.flags2 &= !tf2::ECN_SND_ECE;
                self.flags |= tf::ACKNOW;
            }
            if iptos & MASK == CE {
                self.flags2 |= tf2::ECN_SND_ECE;
            }
        }
        // `cc_ecnpkt_handler_flags`: NewReno and CUBIC have no handler.
        if iptos & MASK == CE {
            self.ccv.flags |= ccf::IPHDR_CE;
        } else {
            self.ccv.flags &= !ccf::IPHDR_CE;
        }
        delta
    }

    /// `tcp_ecn_output_syn_sent`: the flags a SYN asks for ECN with.
    pub(crate) fn ecn_output_syn_sent(&self, cx: &Ctx) -> u16 {
        let retries = get(&cx.conf.ecn_maxretries) as u32;
        let ask = self.rxtshift == 0 || self.rxtshift <= retries;
        match get(&cx.conf.ecn_enable) {
            1 if ask => th::ECE | th::CWR,
            3 if ask => th::ECE | th::CWR | th::AE,
            _ => 0,
        }
    }

    /// `tcp_ecn_output_established`: the IP ECN field for a segment of
    /// `len` bytes, and its ECN flags.
    pub(crate) fn ecn_output_established(
        &mut self,
        cx: &Ctx,
        thflags: &mut u16,
        len: u32,
        rxmit: bool,
    ) -> u8 {
        if self.flags2 & (tf2::ECN_PERMIT | tf2::ACE_PERMIT) == 0 {
            return NOTECT;
        }
        let newdata = len > 0
            && seq::geq(self.snd_nxt, self.snd_max)
            && !rxmit
            && !(self.flags & tf::FORCEDATA != 0 && len == 1);
        let mut ipecn = NOTECT;
        if newdata {
            if self.flags2 & tf2::ECN_USE_ECT1 != 0 {
                ipecn = ECT1;
                cx.stats.inc(tcps::ECN_SNDECT1);
            } else {
                ipecn = ECT0;
                cx.stats.inc(tcps::ECN_SNDECT0);
            }
        }
        if self.flags2 & tf2::ACE_PERMIT != 0 {
            set_ace(thflags, self.rcep);
            if self.flags2 & tf2::ECN_PERMIT == 0 {
                self.rcep = if self.rcep == 0b110 { 6 } else { 5 };
                self.flags2 |= tf2::ECN_PERMIT;
            }
        } else {
            if newdata && self.flags2 & tf2::ECN_SND_CWR != 0 {
                *thflags |= th::CWR;
                self.flags2 &= !tf2::ECN_SND_CWR;
            }
            if self.flags2 & tf2::ECN_SND_ECE != 0 {
                *thflags |= th::ECE;
            }
        }
        ipecn
    }
}
