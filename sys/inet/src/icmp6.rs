// SPDX-License-Identifier: BSD-2-Clause
//! ICMPv6 (docs/NETWORKING.md §8): echo replies, errors about this host's
//! packets passed to their protocols (path MTU from packet too big),
//! neighbor discovery's and MLD's messages to them, every message to the
//! raw ICMPv6 sockets whose filters pass it, and the errors this host
//! sends, under `net.inet6.icmp6.errppslimit` with its jitter.
//!
//! Behavior reference: sys/netinet6/icmp6.c (`icmp6_input`,
//! `icmp6_error`, `icmp6_errcount`, `icmp6_notify_error`,
//! `icmp6_mtudisc_update`, `icmp6_reflect`, `icmp6_ratelimit`,
//! `icmp6_errmap`), sys/netinet/icmp6.h (`icmp6_ifoutstat_inc`).

use alloc::sync::Arc;

use abi::errno::Errno;
use mbuf::{Packet, flags};
use net::Vnet;
use net::addr::{In6, is_multicast6, is_unspecified};
use net::ifnet::{Net, icmp6_ifs, in6_ifs};

use crate::in6::{self, is_zoned};
use crate::ip6::{self, HDR_LEN, Ip6, Opts6, PLD_MAXLEN, proto};
use crate::state::{self, Rate, State, icp6s, ip6s, val};

/// ICMPv6 types (`ICMP6_*`, `ND_*`, `MLD_*`).
pub mod ty {
    pub const DST_UNREACH: u8 = 1;
    pub const PACKET_TOO_BIG: u8 = 2;
    pub const TIME_EXCEEDED: u8 = 3;
    pub const PARAM_PROB: u8 = 4;
    pub const ECHO_REQUEST: u8 = 128;
    pub const ECHO_REPLY: u8 = 129;
    pub const MLD_LISTENER_QUERY: u8 = 130;
    pub const MLD_LISTENER_REPORT: u8 = 131;
    pub const MLD_LISTENER_DONE: u8 = 132;
    pub const ROUTER_SOLICIT: u8 = 133;
    pub const ROUTER_ADVERT: u8 = 134;
    pub const NEIGHBOR_SOLICIT: u8 = 135;
    pub const NEIGHBOR_ADVERT: u8 = 136;
    pub const REDIRECT: u8 = 137;
    pub const ROUTER_RENUMBERING: u8 = 138;
    pub const WRUREQUEST: u8 = 139;
    pub const WRUREPLY: u8 = 140;
    pub const MLDV2_LISTENER_REPORT: u8 = 143;
    /// `ICMP6_INFOMSG_MASK`.
    pub const INFOMSG_MASK: u8 = 0x80;
}

/// Their codes.
pub mod code {
    pub const DST_UNREACH_NOROUTE: u8 = 0;
    pub const DST_UNREACH_ADMIN: u8 = 1;
    pub const DST_UNREACH_BEYONDSCOPE: u8 = 2;
    pub const DST_UNREACH_ADDR: u8 = 3;
    pub const DST_UNREACH_NOPORT: u8 = 4;
    pub const DST_UNREACH_POLICY: u8 = 5;
    pub const DST_UNREACH_REJECT: u8 = 6;
    pub const TIME_EXCEED_TRANSIT: u8 = 0;
    pub const TIME_EXCEED_REASSEMBLY: u8 = 1;
    pub const PARAMPROB_HEADER: u8 = 0;
    pub const PARAMPROB_NEXTHEADER: u8 = 1;
    pub const PARAMPROB_OPTION: u8 = 2;
}

/// `sizeof(struct icmp6_hdr)`.
pub const HDR: usize = 8;

/// The rate limits (`ratelim_which`).
pub const RATELIM_PARAM_PROB: usize = 0;
pub const RATELIM_TOO_BIG: usize = 1;
pub const RATELIM_UNREACH: usize = 2;
pub const RATELIM_TEXCEED: usize = 3;
pub const RATELIM_REDIR: usize = 4;
pub const RATELIM_REPLY: usize = 5;
pub const RATELIM_OTHER: usize = 6;
pub const RATELIM_MAX: usize = 7;

/// `icmp6_rate_descrs`.
const RATE_DESCRS: [&str; RATELIM_MAX] = [
    "bad IPv6 header",
    "packet too big",
    "destination unreachable",
    "time exceeded",
    "neighbor discovery redirect",
    "echo reply",
    "(other)",
];

/// `icmp6lim_new_jitter`: a new jitter in `[-jitter, jitter]`.
fn new_jitter(st: &State, r: &mut Rate) {
    // Relaxed: a setting.
    let j = st
        .conf6
        .icmp6lim_jitter
        .load(core::sync::atomic::Ordering::Relaxed);
    if j > 0 {
        let span = j * 2 + 1;
        r.jitter =
            i32::try_from(crate::random() % span).unwrap_or(0) - i32::try_from(j).unwrap_or(0);
    }
}

/// `icmp6_ratelimit`: whether a message of type `t` may not go now.
pub(crate) fn ratelimit(st: &State, t: u8) -> bool {
    // Relaxed: a setting.
    let lim = st
        .conf6
        .errppslimit
        .load(core::sync::atomic::Ordering::Relaxed);
    if lim == 0 {
        return false;
    }
    let which = match t {
        ty::PARAM_PROB => RATELIM_PARAM_PROB,
        ty::PACKET_TOO_BIG => RATELIM_TOO_BIG,
        ty::DST_UNREACH => RATELIM_UNREACH,
        ty::TIME_EXCEEDED => RATELIM_TEXCEED,
        ty::REDIRECT => RATELIM_REDIR,
        ty::ECHO_REPLY => RATELIM_REPLY,
        _ => RATELIM_OTHER,
    };
    let (pps, limit) = {
        let mut rates = st.rates6.lock();
        let r = &mut rates[which];
        let limit = i64::from(lim) + i64::from(r.jitter);
        let pps = r.check(crate::now(), u64::try_from(limit).unwrap_or(0));
        if pps > 0 {
            new_jitter(st, r);
        }
        (pps, limit)
    };
    if pps > 0 && val(&st.conf6.icmp6lim_output) != 0 {
        crate::log(&alloc::format!(
            "Limiting ICMPv6 {} output from {pps} to {limit} packets/sec\n",
            RATE_DESCRS[which]
        ));
    }
    if pps == -1 {
        st.icmp6stat.inc(icp6s::TOOFREQ);
        return true;
    }
    false
}

/// `icmp6_errmap`: the error a socket sees for an ICMPv6 error.
#[must_use]
pub const fn errmap(t: u8, c: u8) -> Option<Errno> {
    match (t, c) {
        (ty::DST_UNREACH, code::DST_UNREACH_NOROUTE | code::DST_UNREACH_ADDR) => {
            Some(Errno::EHOSTUNREACH)
        }
        (ty::DST_UNREACH, code::DST_UNREACH_NOPORT | code::DST_UNREACH_ADMIN) => {
            Some(Errno::ECONNREFUSED)
        }
        (ty::DST_UNREACH, code::DST_UNREACH_BEYONDSCOPE) => Some(Errno::ENOPROTOOPT),
        (ty::PACKET_TOO_BIG, _) => Some(Errno::EMSGSIZE),
        (ty::TIME_EXCEEDED, code::TIME_EXCEED_TRANSIT) => Some(Errno::EHOSTUNREACH),
        (ty::PARAM_PROB, code::PARAMPROB_NEXTHEADER) => Some(Errno::ECONNREFUSED),
        (ty::PARAM_PROB, code::PARAMPROB_HEADER | code::PARAMPROB_OPTION) => {
            Some(Errno::ENOPROTOOPT)
        }
        _ => None,
    }
}

/// `icmp6_errcount`: an error's type and code, counted as sent.
fn errcount(st: &State, t: u8, c: u8) {
    let i = match (t, c) {
        (ty::DST_UNREACH, code::DST_UNREACH_NOROUTE) => icp6s::ODST_UNREACH_NOROUTE,
        (ty::DST_UNREACH, code::DST_UNREACH_ADMIN) => icp6s::ODST_UNREACH_ADMIN,
        (ty::DST_UNREACH, code::DST_UNREACH_BEYONDSCOPE) => icp6s::ODST_UNREACH_BEYONDSCOPE,
        (ty::DST_UNREACH, code::DST_UNREACH_ADDR) => icp6s::ODST_UNREACH_ADDR,
        (ty::DST_UNREACH, code::DST_UNREACH_NOPORT) => icp6s::ODST_UNREACH_NOPORT,
        (ty::PACKET_TOO_BIG, _) => icp6s::OPACKET_TOO_BIG,
        (ty::TIME_EXCEEDED, code::TIME_EXCEED_TRANSIT) => icp6s::OTIME_EXCEED_TRANSIT,
        (ty::TIME_EXCEEDED, code::TIME_EXCEED_REASSEMBLY) => icp6s::OTIME_EXCEED_REASSEMBLY,
        (ty::PARAM_PROB, code::PARAMPROB_HEADER) => icp6s::OPARAMPROB_HEADER,
        (ty::PARAM_PROB, code::PARAMPROB_NEXTHEADER) => icp6s::OPARAMPROB_NEXTHEADER,
        (ty::PARAM_PROB, code::PARAMPROB_OPTION) => icp6s::OPARAMPROB_OPTION,
        (ty::REDIRECT, _) => icp6s::OREDIRECT,
        _ => icp6s::OUNKNOWN,
    };
    st.icmp6stat.inc(i);
}

/// `icmp6_ifoutstat_inc`: a message of type `t` sent out of interface
/// `ifindex`.
pub(crate) fn ifoutstat(n: &Net, ifindex: u16, t: u8, c: u8) {
    let Some(i) = n.by_index(ifindex) else {
        return;
    };
    let s = &i.stats;
    s.icmp6_inc(icmp6_ifs::OUT_MSG);
    if t < ty::INFOMSG_MASK {
        s.icmp6_inc(icmp6_ifs::OUT_ERROR);
    }
    let x = match t {
        ty::DST_UNREACH => {
            if c == code::DST_UNREACH_ADMIN {
                s.icmp6_inc(icmp6_ifs::OUT_ADMINPROHIB);
            }
            icmp6_ifs::OUT_DSTUNREACH
        }
        ty::PACKET_TOO_BIG => icmp6_ifs::OUT_PKTTOOBIG,
        ty::TIME_EXCEEDED => icmp6_ifs::OUT_TIMEEXCEED,
        ty::PARAM_PROB => icmp6_ifs::OUT_PARAMPROB,
        ty::ECHO_REQUEST => icmp6_ifs::OUT_ECHO,
        ty::ECHO_REPLY => icmp6_ifs::OUT_ECHOREPLY,
        ty::MLD_LISTENER_QUERY => icmp6_ifs::OUT_MLDQUERY,
        ty::MLD_LISTENER_REPORT => icmp6_ifs::OUT_MLDREPORT,
        ty::MLD_LISTENER_DONE => icmp6_ifs::OUT_MLDDONE,
        ty::ROUTER_SOLICIT => icmp6_ifs::OUT_ROUTERSOLICIT,
        ty::ROUTER_ADVERT => icmp6_ifs::OUT_ROUTERADVERT,
        ty::NEIGHBOR_SOLICIT => icmp6_ifs::OUT_NEIGHBORSOLICIT,
        ty::NEIGHBOR_ADVERT => icmp6_ifs::OUT_NEIGHBORADVERT,
        ty::REDIRECT => icmp6_ifs::OUT_REDIRECT,
        _ => return,
    };
    s.icmp6_inc(x);
}

/// Counts against interface `ifindex` one of IPv6's or ICMPv6's.
pub(crate) fn ifstat(vnet: &Vnet, ifindex: u16, in6: Option<usize>, icmp6: Option<usize>) {
    let n = vnet.net.lock();
    if let Some(i) = n.by_index(ifindex) {
        if let Some(x) = in6 {
            i.stats.in6_inc(x);
        }
        if let Some(x) = icmp6 {
            i.stats.icmp6_inc(x);
        }
    }
}

/// `icmp6_error`: an error of type `t` and code `c` about packet `m`
/// (`param` its pointer or MTU), back to its source unless it may not
/// have one.
pub(crate) fn error(vnet: &Arc<Vnet>, mut m: Packet, t: u8, c: u8, param: usize) {
    let st = state::of(vnet);
    st.icmp6stat.inc(icp6s::ERROR);
    errcount(st, t, c);
    let Ok(oip6) = m.header::<Ip6>(0) else {
        st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
        return;
    };
    // Not about multicast (RFC 4443 2.4 (e.3)), but for path MTU and an
    // option the sender was told to report.
    if (m.hdr.flags & (flags::BCAST | flags::MCAST) != 0 || is_multicast6(&oip6.dst))
        && t != ty::PACKET_TOO_BIG
        && !(t == ty::PARAM_PROB && c == code::PARAMPROB_OPTION)
    {
        return;
    }
    // (e.5): no source to answer.
    if is_unspecified(&oip6.src) || is_multicast6(&oip6.src) {
        return;
    }
    // Not about an ICMPv6 error or redirect.
    if let Some((off, proto::ICMPV6)) = ip6::lasthdr(&m) {
        let mut b = [0u8; 1];
        if m.data.copy_out(off, &mut b) != 1 {
            st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
            return;
        }
        if b[0] < ty::ECHO_REQUEST || b[0] == ty::REDIRECT {
            st.icmp6stat.inc(icp6s::CANTERROR);
            return;
        }
    }
    if ratelimit(st, t) {
        return;
    }
    if m.len() >= PLD_MAXLEN {
        let _ = m.data.split_off(PLD_MAXLEN);
    }
    let mut h = [0u8; HDR_LEN + HDR];
    h[..HDR_LEN].copy_from_slice(&oip6.bytes());
    h[HDR_LEN] = t;
    h[HDR_LEN + 1] = c;
    #[allow(clippy::cast_possible_truncation, reason = "a 32-bit pointer or MTU")]
    h[HDR_LEN + 4..].copy_from_slice(&(param as u32).to_be_bytes());
    let Ok(b) = m.prepend(HDR_LEN + HDR) else {
        return;
    };
    b.copy_from_slice(&h);
    st.icmp6stat.inc(icp6s::OUTHIST + usize::from(t));
    reflect(vnet, st, m, HDR_LEN);
}

/// `icmp6_reflect`: ICMPv6 packet `m` (its message at `off`) back to its
/// source: from the destination if that was this host's, else from the
/// source selection would choose; the checksum computed.
pub(crate) fn reflect(vnet: &Arc<Vnet>, st: &State, mut m: Packet, off: usize) {
    if off < HDR_LEN {
        return;
    }
    let Ok(mut ip6) = m.header::<Ip6>(0) else {
        return;
    };
    // Headers between IPv6 and ICMPv6 go.
    if off > HDR_LEN {
        m.data.trim_front(off - HDR_LEN);
        if m.data.copy_in(0, &ip6.bytes()).is_err() {
            return;
        }
    }
    if is_unspecified(&ip6.src) {
        return;
    }
    let mut t = [0u8; 2];
    m.data.copy_out(HDR_LEN, &mut t);
    let rcvif = m.hdr.rcvif;
    let mut n = vnet.net.lock();
    let mut src = None;
    let mut hlim = crate::udp::hops(-1);
    if !is_multicast6(&ip6.dst)
        && let Some((_, f)) = in6::ifa_with(&n, &ip6.dst, rcvif)
        && f & (net::ifnet::in6_iff::ANYCAST | net::ifnet::in6_iff::NOTREADY) == 0
    {
        src = Some(ip6.dst);
        if let Some(i) = n.by_index(rcvif) {
            hlim = i.nd.chlim;
        }
    }
    let zone = if is_zoned(&ip6.src) { rcvif } else { 0 };
    let src = match src {
        Some(s) => s,
        None => {
            let req = in6::SrcReq {
                dst: ip6.src,
                zone,
                fib: m.hdr.fib,
                prefer_tempaddr: -1,
                ..in6::SrcReq::default()
            };
            match in6::selectsrc(&n, st, &req) {
                Ok((s, ifp)) => {
                    hlim = in6::selecthlim(&n, -1, ifp, None);
                    s
                }
                Err(_) => return,
            }
        }
    };
    let src_zone = if is_zoned(&src) { rcvif } else { 0 };
    ip6.dst = ip6.src;
    ip6.src = src;
    ip6.flow = u32::from(ip6::VERSION) << 28;
    ip6.nxt = proto::ICMPV6;
    ip6.hlim = hlim;
    ip6.plen = u16::try_from(m.len() - HDR_LEN).unwrap_or(u16::MAX);
    if m.data.copy_in(0, &ip6.bytes()).is_err() {
        return;
    }
    let _ = m.data.copy_in(HDR_LEN + 2, &[0, 0]);
    let c = ip6::cksum6(&m, proto::ICMPV6, HDR_LEN, m.len() - HDR_LEN);
    let _ = m.data.copy_in(HDR_LEN + 2, &c.to_be_bytes());
    m.hdr.flags &= !(flags::BCAST | flags::MCAST | flags::LOOP | flags::RTALERT_MLD);
    m.hdr.rcvif = 0;
    m.hdr.csum_flags = 0;
    let o = Opts6 {
        zone,
        src_zone,
        fib: m.hdr.fib,
        minmtu: ip6::minmtu::MCASTONLY,
        ..Opts6::default()
    };
    if let Ok(outif) = ip6::output_if(&mut n, st, m, &o) {
        ifoutstat(&n, outif, t[0], t[1]);
    }
}

/// `icmp6_input`: an ICMPv6 message at `off` of `m`, for this host.
pub(crate) fn input(vnet: &Arc<Vnet>, mut m: Packet, off: usize) {
    let st = state::of(vnet);
    let icmp6len = m.len().saturating_sub(off);
    if icmp6len < HDR {
        st.icmp6stat.inc(icp6s::TOOSHORT);
        return;
    }
    let Ok(ip6) = m.header::<Ip6>(0) else {
        return;
    };
    let ifp = m.hdr.rcvif;
    // Only groups the interface joined (no source filters for ICMPv6).
    if is_multicast6(&ip6.dst) && !crate::mld6::member(st, ifp, &ip6.dst) {
        st.ip6stat.inc(ip6s::NOTMEMBER);
        ifstat(vnet, ifp, Some(in6_ifs::IN_DISCARD), None);
        return;
    }
    let mut h = [0u8; HDR];
    m.data.copy_out(off, &mut h);
    let (t, c) = (h[0], h[1]);
    if ip6::cksum6(&m, proto::ICMPV6, off, icmp6len) != 0 {
        st.icmp6stat.inc(icp6s::CHECKSUM);
        return;
    }
    st.icmp6stat.inc(icp6s::INHIST + usize::from(t));
    ifstat(vnet, ifp, None, Some(icmp6_ifs::IN_MSG));
    if t < ty::INFOMSG_MASK {
        ifstat(vnet, ifp, None, Some(icmp6_ifs::IN_ERROR));
    }
    let icount = |x: usize| ifstat(vnet, ifp, None, Some(x));
    let badcode = || st.icmp6stat.inc(icp6s::BADCODE);
    let badlen = || st.icmp6stat.inc(icp6s::BADLEN);
    let mut deliver = false;
    match t {
        ty::DST_UNREACH => {
            icount(icmp6_ifs::IN_DSTUNREACH);
            if c == code::DST_UNREACH_ADMIN {
                icount(icmp6_ifs::IN_ADMINPROHIB);
            }
            match c {
                code::DST_UNREACH_ADMIN
                | code::DST_UNREACH_NOROUTE
                | code::DST_UNREACH_ADDR
                | code::DST_UNREACH_BEYONDSCOPE
                | code::DST_UNREACH_NOPORT => deliver = true,
                _ => badcode(),
            }
        }
        ty::PACKET_TOO_BIG => {
            icount(icmp6_ifs::IN_PKTTOOBIG);
            deliver = true;
        }
        ty::TIME_EXCEEDED => {
            icount(icmp6_ifs::IN_TIMEEXCEED);
            match c {
                code::TIME_EXCEED_TRANSIT | code::TIME_EXCEED_REASSEMBLY => deliver = true,
                _ => badcode(),
            }
        }
        ty::PARAM_PROB => {
            icount(icmp6_ifs::IN_PARAMPROB);
            match c {
                code::PARAMPROB_NEXTHEADER | code::PARAMPROB_HEADER | code::PARAMPROB_OPTION => {
                    deliver = true;
                }
                _ => badcode(),
            }
        }
        ty::ECHO_REQUEST => {
            icount(icmp6_ifs::IN_ECHO);
            if c != 0 {
                badcode();
            } else if !ratelimit(st, ty::ECHO_REPLY)
                && let Ok(mut n) = m.copy()
            {
                // Only the IPv6 and ICMPv6 headers and the data, any
                // headers between them gone.
                if off > HDR_LEN {
                    n.data.trim_front(off - HDR_LEN);
                    let _ = n.data.copy_in(0, &ip6.bytes());
                }
                let _ = n.data.copy_in(HDR_LEN, &[ty::ECHO_REPLY, 0]);
                st.icmp6stat.inc(icp6s::REFLECT);
                st.icmp6stat
                    .inc(icp6s::OUTHIST + usize::from(ty::ECHO_REPLY));
                reflect(vnet, st, n, HDR_LEN);
            }
        }
        ty::ECHO_REPLY => {
            icount(icmp6_ifs::IN_ECHOREPLY);
            if c != 0 {
                badcode();
            }
        }
        ty::MLD_LISTENER_QUERY
        | ty::MLD_LISTENER_REPORT
        | ty::MLD_LISTENER_DONE
        | ty::MLDV2_LISTENER_REPORT => {
            // Link-local, one hop, with a Router Alert.
            if ip6.hlim != 1 || m.hdr.flags & flags::RTALERT_MLD == 0 {
                return;
            }
            if !crate::mld6::input(vnet, st, &m, off, icmp6len) {
                return;
            }
        }
        ty::WRUREQUEST => {
            // Node information queries are off (`net.inet6.icmp6.nodeinfo`).
        }
        ty::WRUREPLY => {
            if c != 0 {
                badcode();
            }
        }
        ty::ROUTER_SOLICIT => {
            icount(icmp6_ifs::IN_ROUTERSOLICIT);
            if c != 0 {
                badcode();
            } else if icmp6len < 8 {
                badlen();
            } else {
                crate::nd6_rtr::rs_input(vnet, st, &m, off, icmp6len);
            }
        }
        ty::ROUTER_ADVERT => {
            icount(icmp6_ifs::IN_ROUTERADVERT);
            if c != 0 {
                badcode();
            } else if icmp6len < 16 {
                badlen();
            } else {
                crate::nd6_rtr::ra_input(vnet, st, &m, off, icmp6len);
            }
        }
        ty::NEIGHBOR_SOLICIT => {
            icount(icmp6_ifs::IN_NEIGHBORSOLICIT);
            if c != 0 {
                badcode();
            } else if icmp6len < 24 {
                badlen();
            } else {
                crate::nd6::ns_input(vnet, st, &m, off, icmp6len);
            }
        }
        ty::NEIGHBOR_ADVERT => {
            icount(icmp6_ifs::IN_NEIGHBORADVERT);
            if c != 0 {
                badcode();
            } else if icmp6len < 24 {
                badlen();
            } else {
                crate::nd6::na_input(vnet, st, &m, off, icmp6len);
            }
        }
        ty::REDIRECT => {
            icount(icmp6_ifs::IN_REDIRECT);
            if c != 0 {
                badcode();
            } else if icmp6len < 40 {
                badlen();
            } else {
                crate::nd6::redirect_input(vnet, st, &m, off, icmp6len);
            }
        }
        ty::ROUTER_RENUMBERING => {
            if c != 0 && c != 1 {
                badcode();
            } else if icmp6len < 16 {
                badlen();
            }
        }
        _ => {
            // An unknown error must reach its protocol; an unknown
            // informational message must not.
            if t < ty::ECHO_REQUEST {
                deliver = true;
            }
        }
    }
    if deliver && !notify_error(vnet, st, &m, off, icmp6len) {
        return;
    }
    crate::raw6::icmp6_input(vnet, st, m, off, t);
}

/// `icmp6_notify_error`: the packet an error quotes, followed through its
/// extension headers to its protocol, which is told (`ip6_ctlprotox`);
/// packet too big updates the path MTU first. False if the message was
/// unusable (it then goes no further).
fn notify_error(vnet: &Arc<Vnet>, st: &State, m: &Packet, off: usize, icmp6len: usize) -> bool {
    if icmp6len < HDR + HDR_LEN {
        st.icmp6stat.inc(icp6s::TOOSHORT);
        return false;
    }
    let mut h = [0u8; HDR];
    m.data.copy_out(off, &mut h);
    let eoff0 = off + HDR;
    let mut e = [0u8; HDR_LEN];
    if m.data.copy_out(eoff0, &mut e) != HDR_LEN {
        st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
        return false;
    }
    let Some(eip6) = <Ip6 as mbuf::Header>::parse(&e) else {
        return false;
    };
    let mut nxt = eip6.nxt;
    let mut eoff = eoff0 + HDR_LEN;
    let mut finaldst = eip6.dst;
    loop {
        let mut b = [0u8; 8];
        match nxt {
            proto::HOPOPTS | proto::DSTOPTS | proto::AH => {
                if m.data.copy_out(eoff, &mut b[..2]) != 2 {
                    st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
                    return false;
                }
                eoff += if nxt == proto::AH {
                    (usize::from(b[1]) + 2) << 2
                } else {
                    (usize::from(b[1]) + 1) << 3
                };
                nxt = b[0];
            }
            proto::ROUTING => {
                if m.data.copy_out(eoff, &mut b[..4]) != 4 {
                    st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
                    return false;
                }
                let rthlen = (usize::from(b[1]) + 1) << 3;
                // A type 0 header names the final destination last.
                if b[3] != 0 && b[2] == 0 && b[1] % 2 == 0 && b[1] / 2 > 0 {
                    let hops = usize::from(b[1] / 2);
                    let mut d = [0u8; 16];
                    if m.data.copy_out(eoff + 8 + (hops - 1) * 16, &mut d) == 16 {
                        finaldst = d;
                    }
                }
                eoff += rthlen;
                nxt = b[0];
            }
            proto::FRAGMENT => {
                if m.data.copy_out(eoff, &mut b) != 8 {
                    st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
                    return false;
                }
                // Past a later fragment's header nothing is known.
                if u16::from_be_bytes([b[2], b[3]]) & 0xfff8 != 0 {
                    break;
                }
                eoff += 8;
                nxt = b[0];
            }
            _ => break,
        }
    }
    if is_unspecified(&eip6.src) || is_unspecified(&eip6.dst) {
        return false;
    }
    let rcvif = m.hdr.rcvif;
    let dzone = if is_zoned(&finaldst) { rcvif } else { 0 };
    if h[0] == ty::PACKET_TOO_BIG {
        let mtu = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
        mtudisc_update(vnet, st, &finaldst, dzone, mtu);
    }
    let ctl = Ctl {
        t: h[0],
        c: h[1],
        src: eip6.src,
        dst: finaldst,
        off: eoff,
    };
    match nxt {
        proto::UDP => crate::udp::ctlinput6(vnet, m, &ctl),
        proto::TCP => {
            let mtu = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
            crate::tcp::ctlinput6(vnet, m, &ctl, mtu);
        }
        _ => crate::raw6::ctlinput(vnet, &ctl),
    }
    true
}

/// What an error says to the protocol (`struct ip6ctlparam`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ctl {
    pub t: u8,
    pub c: u8,
    /// The quoted packet's source and final destination.
    pub src: In6,
    pub dst: In6,
    /// Where its protocol's header is in the message.
    pub off: usize,
}

/// `icmp6_mtudisc_update`: a smaller MTU toward `dst` than what the host
/// cache or the link says is remembered.
fn mtudisc_update(vnet: &Arc<Vnet>, st: &State, dst: &In6, zone: u16, mtu: u32) {
    if mtu < ip6::MMTU {
        return;
    }
    let mut max = crate::nd6::pmtu_get(st, dst);
    if max == 0 {
        let n = vnet.net.lock();
        max = n
            .route6(0, dst, zone)
            .and_then(|r| n.by_index(r.ifindex))
            .map_or(0, net::ifnet::Ifnet::linkmtu6);
    }
    if mtu < max {
        crate::nd6::pmtu_set(st, dst, mtu);
        st.icmp6stat.inc(icp6s::PMTUCHG);
    }
}
