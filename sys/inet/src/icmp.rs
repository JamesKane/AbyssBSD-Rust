// SPDX-License-Identifier: BSD-2-Clause
//! ICMP for IPv4: echo and timestamp replies, errors about this host's
//! datagrams handed to their protocols (a UDP socket's
//! `ECONNREFUSED`), redirects, and the errors this host sends, under
//! `net.inet.icmp.icmplim`'s rate limit with its jitter. Every message
//! also goes to the raw sockets that want it.
//!
//! Behavior reference: sys/netinet/ip_icmp.c (`icmp_input`,
//! `icmp_error`, `icmp_reflect`, `icmp_send`, `badport_bandlim`,
//! `icmp_errmap`, `iptime`, `icmp_verify_redirect_gateway`).

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::net::iff;
use mbuf::{Packet, flags};
use net::addr::{from_v4, is_multicast4, prefix_match, v4_of};
use net::{Family, Vnet};

use crate::cksum;
use crate::ip::{self, HDR_LEN, Ip, MF, OFFMASK, proto};
use crate::state::{self, State, bandlim, icps, on};

/// Message types (`ICMP_*`).
pub mod ty {
    pub const ECHOREPLY: u8 = 0;
    pub const UNREACH: u8 = 3;
    pub const SOURCEQUENCH: u8 = 4;
    pub const REDIRECT: u8 = 5;
    pub const ECHO: u8 = 8;
    pub const ROUTERADVERT: u8 = 9;
    pub const ROUTERSOLICIT: u8 = 10;
    pub const TIMXCEED: u8 = 11;
    pub const PARAMPROB: u8 = 12;
    pub const TSTAMP: u8 = 13;
    pub const TSTAMPREPLY: u8 = 14;
    pub const IREQ: u8 = 15;
    pub const IREQREPLY: u8 = 16;
    pub const MASKREQ: u8 = 17;
    pub const MASKREPLY: u8 = 18;
    pub const MAX: u8 = 40;
}

/// Codes (`ICMP_UNREACH_*`, `ICMP_TIMXCEED_*`, `ICMP_REDIRECT_*`,
/// `ICMP_PARAMPROB_*`).
pub mod code {
    pub const UNREACH_NET: u8 = 0;
    pub const UNREACH_HOST: u8 = 1;
    pub const UNREACH_PROTOCOL: u8 = 2;
    pub const UNREACH_PORT: u8 = 3;
    pub const UNREACH_NEEDFRAG: u8 = 4;
    pub const UNREACH_SRCFAIL: u8 = 5;
    pub const UNREACH_NET_PROHIB: u8 = 9;
    pub const UNREACH_HOST_PROHIB: u8 = 10;
    pub const UNREACH_FILTER_PROHIB: u8 = 13;
    pub const UNREACH_PRECEDENCE_CUTOFF: u8 = 15;
    pub const REDIRECT_HOST: u8 = 1;
    pub const TIMXCEED_INTRANS: u8 = 0;
    pub const TIMXCEED_REASS: u8 = 1;
    pub const PARAMPROB_ERRATPTR: u8 = 0;
    pub const PARAMPROB_OPTABSENT: u8 = 1;
    pub const PARAMPROB_LENGTH: u8 = 2;
}

/// `ICMP_MINLEN`, `ICMP_TSLEN`, `ICMP_MASKLEN`, `ICMP_ADVLENMIN`.
const MINLEN: usize = 8;
const TSLEN: usize = 20;
const ADVLENMIN: usize = 8 + HDR_LEN + 8;

/// `ICMP_INFOTYPE`.
const fn info_type(t: u8) -> bool {
    matches!(
        t,
        ty::ECHOREPLY
            | ty::ECHO
            | ty::ROUTERADVERT
            | ty::ROUTERSOLICIT
            | ty::TSTAMP
            | ty::TSTAMPREPLY
            | ty::IREQ
            | ty::IREQREPLY
            | ty::MASKREQ
            | ty::MASKREPLY
    )
}

/// `icmp_errmap`: the error an ICMP error means to a socket, if any.
#[must_use]
pub const fn errmap(t: u8, c: u8) -> Option<Errno> {
    match (t, c) {
        (ty::UNREACH, 0 | 1 | 5..=8 | 11 | 12 | 14 | 15) | (ty::TIMXCEED, 0) => {
            Some(Errno::EHOSTUNREACH)
        }
        (ty::UNREACH, 4) => Some(Errno::EMSGSIZE),
        (ty::UNREACH, 2 | 3 | 9 | 10 | 13) => Some(Errno::ECONNREFUSED),
        (ty::PARAMPROB, 0 | 1) => Some(Errno::ENOPROTOOPT),
        _ => None,
    }
}

/// `badport_bandlim`: whether a response of kind `which` may go now,
/// logging once a second when some were held back.
pub(crate) fn bandlim(st: &State, which: usize) -> bool {
    // Relaxed: settings.
    let lim = st.conf.icmplim.load(Ordering::Relaxed);
    if lim == 0 {
        return true;
    }
    let jitter_max = st.conf.icmplim_jitter.load(Ordering::Relaxed);
    let mut rates = st.rates.lock();
    let Some(r) = rates.get_mut(which) else {
        return true;
    };
    let limit = u64::from(lim).saturating_add_signed(i64::from(r.jitter));
    let pps = r.check(crate::now(), limit);
    if pps > 0 {
        if on(&st.conf.icmplim_output) {
            crate::log(&format!(
                "Limiting {} response from {} to {} packets/sec\n",
                bandlim::NAMES[which],
                pps,
                limit
            ));
        }
        // `icmplim_new_jitter`.
        r.jitter = if jitter_max > 0 {
            let span = jitter_max * 2 + 1;
            i32::try_from(crate::random() % span).unwrap_or(0)
                - i32::try_from(jitter_max).unwrap_or(0)
        } else {
            0
        };
    }
    pps != -1
}

/// `iptime`: milliseconds since midnight UT, in network order's value.
fn iptime() -> u32 {
    let ns = crate::realtime();
    let s = (ns / 1_000_000_000) % 86_400;
    u32::try_from(s * 1000 + (ns % 1_000_000_000) / 1_000_000).unwrap_or(0)
}

/// A received ICMP message (`icmp_input`): `m` holds the whole packet,
/// header `ip`.
pub(crate) fn input(vnet: &Arc<Vnet>, m: Packet, ip: &Ip) {
    let st = state::of(vnet);
    let hlen = usize::from(ip.hl);
    let icmplen = usize::from(ip.len) - hlen;
    if icmplen < MINLEN {
        st.icmpstat.inc(icps::TOOSHORT);
        return;
    }
    if cksum::of_chain(&m.data, hlen, icmplen) != 0 {
        st.icmpstat.inc(icps::CHECKSUM);
        return;
    }
    let mut head = [0u8; MINLEN];
    m.data.copy_out(hlen, &mut head);
    let (t, c) = (head[0], head[1]);
    if t > ty::MAX {
        crate::raw::input4(vnet, m, ip, true);
        return;
    }
    st.icmpstat.inc(icps::INHIST + usize::from(t));
    match t {
        ty::UNREACH | ty::TIMXCEED | ty::PARAMPROB => {
            let max = match t {
                ty::UNREACH => code::UNREACH_PRECEDENCE_CUTOFF,
                ty::TIMXCEED => code::TIMXCEED_REASS,
                _ => code::PARAMPROB_LENGTH,
            };
            if c > max {
                st.icmpstat.inc(icps::BADCODE);
            } else if !notify(vnet, st, &m, hlen, icmplen, t, c) {
                return;
            }
        }
        ty::ECHO => {
            if !on(&st.conf.bmcastecho) && m.hdr.flags & (flags::MCAST | flags::BCAST) != 0 {
                st.icmpstat.inc(icps::BMCASTECHO);
            } else {
                if !bandlim(st, bandlim::ECHO) {
                    return;
                }
                reply(vnet, st, m, ip, ty::ECHOREPLY, None);
                return;
            }
        }
        ty::TSTAMP => {
            if !on(&st.conf.tstamprepl) {
            } else if !on(&st.conf.bmcastecho) && m.hdr.flags & (flags::MCAST | flags::BCAST) != 0 {
                st.icmpstat.inc(icps::BMCASTTSTAMP);
            } else if icmplen < TSLEN {
                st.icmpstat.inc(icps::BADLEN);
            } else {
                if !bandlim(st, bandlim::TSTAMP) {
                    return;
                }
                let t = iptime().to_be_bytes();
                reply(vnet, st, m, ip, ty::TSTAMPREPLY, Some(t));
                return;
            }
        }
        ty::MASKREQ => {
            // `net.inet.icmp.maskrepl` is 0 by default; a reply would
            // carry the receiving address's mask.
            if on(&st.conf.maskrepl) {
                maskreply(vnet, st, m, ip, icmplen);
                return;
            }
        }
        ty::REDIRECT => redirect(vnet, st, &m, ip, hlen, icmplen, c),
        _ => {}
    }
    crate::raw::input4(vnet, m, ip, true);
}

/// The inner header of an error about one of this host's datagrams,
/// handed to its protocol (`ip_ctlprotox`); false if the message is
/// dropped instead of going on to raw sockets.
fn notify(
    vnet: &Arc<Vnet>,
    st: &State,
    m: &Packet,
    hlen: usize,
    icmplen: usize,
    t: u8,
    c: u8,
) -> bool {
    if icmplen < ADVLENMIN {
        st.icmpstat.inc(icps::BADLEN);
        return false;
    }
    let mut inner = [0u8; HDR_LEN];
    m.data.copy_out(hlen + 8, &mut inner);
    let Some(oip) = <Ip as mbuf::Header>::parse(&inner) else {
        return false;
    };
    let ohl = usize::from(oip.hl);
    // `ICMP_ADVLEN`: the inner header and 8 bytes after it.
    if icmplen < 8 + ohl + 8 || ohl < HDR_LEN {
        st.icmpstat.inc(icps::BADLEN);
        return false;
    }
    if is_multicast4(&oip.dst) {
        st.icmpstat.inc(icps::BADCODE);
        return true;
    }
    if oip.dst == [0; 4] || oip.src == [0; 4] {
        return false;
    }
    let mut ports = [0u8; 8];
    m.data.copy_out(hlen + 8 + ohl, &mut ports);
    if oip.p == proto::UDP {
        crate::udp::ctlinput4(vnet, t, c, &oip, &ports);
    }
    if oip.p == proto::TCP {
        let mut nm = [0u8; 2];
        m.data.copy_out(hlen + 6, &mut nm);
        crate::tcp::ctlinput4(vnet, t, c, &oip, &ports, u16::from_be_bytes(nm));
    }
    true
}

/// `icmp_input`'s redirect: logged if asked; unless this host routes or
/// ignores them, a host route through the new gateway.
fn redirect(vnet: &Arc<Vnet>, st: &State, m: &Packet, ip: &Ip, hlen: usize, icmplen: usize, c: u8) {
    let mut b = [0u8; 8 + HDR_LEN];
    m.data.copy_out(hlen, &mut b);
    let gw: [u8; 4] = b[4..8].try_into().unwrap_or([0; 4]);
    let odst: [u8; 4] = b[8 + 16..8 + 20].try_into().unwrap_or([0; 4]);
    if on(&st.conf.log_redirect) {
        crate::log(&format!(
            "icmp redirect from {}: {} => {}\n",
            net::addr::fmt4(ip.src),
            net::addr::fmt4(odst),
            net::addr::fmt4(gw)
        ));
    }
    if on(&st.conf.drop_redirect) || on(&st.conf.forwarding) {
        return;
    }
    if c > 3 {
        st.icmpstat.inc(icps::BADCODE);
        return;
    }
    let ohl = usize::from(b[8] & 0xf) << 2;
    if icmplen < ADVLENMIN || icmplen < 8 + ohl + 8 || ohl < HDR_LEN {
        st.icmpstat.inc(icps::BADLEN);
        return;
    }
    // `icmp_verify_redirect_gateway`: from the current gateway for the
    // destination, to a gateway on a network of the receiving interface
    // that is not this host.
    let rcvif = m.hdr.rcvif;
    let mut n = vnet.net.lock();
    let ok = n
        .route_in(m.hdr.fib, Family::Inet, &from_v4(odst))
        .is_some_and(|r| {
            r.flags & abi::net::rtf::GATEWAY != 0
                && matches!(r.gateway, net::ifnet::Gateway::Addr(g) if v4_of(&g) == ip.src)
        })
        && !ip::is_local4(&n, &gw)
        && n.by_index(rcvif).is_some_and(|i| {
            i.addrs.iter().any(|a| {
                a.family == Family::Inet
                    && prefix_match(Family::Inet, &a.addr, &from_v4(gw), a.plen)
            })
        });
    if !ok {
        return;
    }
    let src = n
        .by_index(rcvif)
        .and_then(|i| {
            i.addrs.iter().find(|a| {
                a.family == Family::Inet
                    && prefix_match(Family::Inet, &a.addr, &from_v4(gw), a.plen)
            })
        })
        .map_or([0; 4], |a| v4_of(&a.addr));
    // `net.inet.icmp.redirtimeout`: ten minutes.
    let now = u64::try_from(net::realtime()).unwrap_or(0);
    ip::add_redirect(&mut n, odst, gw, rcvif, src, 600, now);
}

/// Replies to a request in `m` (header `ip`) with type `t`, the rest of
/// the message as it came; a timestamp reply gets its receive and
/// transmit times (`icmp_reflect` by way of `icmp_input`).
fn reply(vnet: &Arc<Vnet>, st: &State, mut m: Packet, ip: &Ip, t: u8, stamp: Option<[u8; 4]>) {
    let hlen = usize::from(ip.hl);
    m.data.trim_front(hlen);
    if m.data.copy_in(0, &[t]).is_err() {
        return;
    }
    if let Some(s) = stamp {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&s);
        b[4..].copy_from_slice(&s);
        if m.data.copy_in(12, &b).is_err() {
            return;
        }
    }
    st.icmpstat.inc(icps::REFLECT);
    st.icmpstat.inc(icps::OUTHIST + usize::from(t));
    reflect(vnet, st, m, ip);
}

/// The mask reply `net.inet.icmp.maskrepl` allows.
fn maskreply(vnet: &Arc<Vnet>, st: &State, mut m: Packet, ip: &Ip, icmplen: usize) {
    if icmplen < 12 {
        return;
    }
    let target = if ip.dst == [0xff; 4] || ip.dst == [0; 4] {
        ip.src
    } else {
        ip.dst
    };
    let mask = {
        let n = vnet.net.lock();
        n.by_index(m.hdr.rcvif).and_then(|i| {
            i.addrs
                .iter()
                .filter(|a| a.family == Family::Inet)
                .find(|a| prefix_match(Family::Inet, &a.addr, &from_v4(target), a.plen))
                .map(|a| (u32::MAX.checked_shl(32 - u32::from(a.plen)).unwrap_or(0)).to_be_bytes())
        })
    };
    let Some(mask) = mask else {
        return;
    };
    let hlen = usize::from(ip.hl);
    if m.data.copy_in(hlen + 8, &mask).is_err() {
        return;
    }
    reply(vnet, st, m, ip, ty::MASKREPLY, None);
}

/// `icmp_reflect`: ICMP message `m` (no IP header) back to the source of
/// the packet whose header was `ip`, from the address it was sent to if
/// that is this host's, else the receiving interface's, else the route
/// back's.
fn reflect(vnet: &Arc<Vnet>, st: &State, mut m: Packet, ip: &Ip) {
    let src = ip.src;
    if is_multicast4(&src) || src[0] >= 240 || src[0] == 0 {
        st.icmpstat.inc(icps::BADADDR);
        return;
    }
    let t = ip.dst;
    let rcvif = m.hdr.rcvif;
    let from = {
        let n = vnet.net.lock();
        let inet = |i: &net::ifnet::Ifnet| -> Vec<net::ifnet::Ifaddr> {
            i.addrs
                .iter()
                .filter(|a| a.family == Family::Inet)
                .cloned()
                .collect()
        };
        let rif = n.by_index(rcvif);
        if ip::is_local4(&n, &t) {
            Some(t)
        } else if let Some(a) = rif.filter(|i| i.flags & iff::BROADCAST != 0).and_then(|i| {
            inet(i)
                .into_iter()
                .find(|a| a.broadcast.is_some_and(|b| v4_of(&b) == t))
        }) {
            Some(v4_of(&a.addr))
        } else if let Some(a) = rif
            .filter(|_| on(&st.conf.rfi))
            .and_then(|i| inet(i).into_iter().next())
        {
            Some(v4_of(&a.addr))
        } else {
            n.route_in(m.hdr.fib, Family::Inet, &from_v4(src))
                .map(|r| v4_of(&r.ifa))
        }
    };
    let Some(from) = from else {
        st.icmpstat.inc(icps::NOROUTE);
        return;
    };
    // `icmp_send`.
    if m.data.copy_in(2, &[0, 0]).is_err() {
        return;
    }
    let c = cksum::of_chain(&m.data, 0, m.len());
    if m.data.copy_in(2, &c.to_be_bytes()).is_err() {
        return;
    }
    let Ok(len) = u16::try_from(HDR_LEN + m.len()) else {
        return;
    };
    let ttl = u8::try_from(crate::tune::IP_TTL.load(Ordering::Relaxed).clamp(0, 255)).unwrap_or(64);
    let h = Ip::new(proto::ICMP, from, src, len, ttl, ip.tos);
    let Ok(b) = m.prepend(HDR_LEN) else {
        return;
    };
    b.copy_from_slice(&h.bytes());
    m.hdr.flags &= !(flags::BCAST | flags::MCAST);
    m.hdr.rcvif = 0;
    m.hdr.csum_flags = 0;
    let fib = m.hdr.fib;
    let _ = ip::output(
        vnet,
        m,
        &ip::Opts {
            fib,
            ..ip::Opts::default()
        },
    );
}

/// `icmp_error`: an error of type `t` and code `c` about packet `n` (its
/// IP header first) to its source, quoting its header and up to
/// `net.inet.icmp.quotelen` bytes after; `gw` for a redirect, the next
/// hop's MTU for fragmentation needed. Nothing is sent about a
/// broadcast or multicast packet, a fragment after the first, or an
/// ICMP error.
pub(crate) fn error(vnet: &Arc<Vnet>, n: Packet, t: u8, mut c: u8, gw: [u8; 4], mtu: u32) {
    let st = state::of(vnet);
    if t != ty::REDIRECT {
        st.icmpstat.inc(icps::ERROR);
    }
    if n.hdr.flags & (flags::BCAST | flags::MCAST) != 0 {
        return;
    }
    let mut n = n;
    let Ok(oip) = n.header::<Ip>(0) else {
        return;
    };
    let ohl = usize::from(oip.hl);
    if n.len() < ohl + MINLEN {
        return;
    }
    if oip.off & !(MF | ip::DF) & (OFFMASK | ip::RF) != 0 {
        return;
    }
    if oip.p == proto::ICMP && t != ty::REDIRECT {
        let mut it = [0u8; 1];
        n.data.copy_out(ohl, &mut it);
        if !info_type(it[0]) {
            st.icmpstat.inc(icps::OLDICMP);
            return;
        }
    }
    let quotelen = usize::try_from(st.conf.quotelen.load(Ordering::Relaxed).max(0)).unwrap_or(0);
    let olen = usize::from(oip.len).saturating_sub(ohl);
    let elen = if oip.p == proto::TCP && n.len() >= ohl + 20 {
        let mut off = [0u8; 1];
        n.data.copy_out(ohl + 12, &mut off);
        let thl = usize::from(off[0] >> 4) << 2;
        if thl < 20 || olen < thl {
            return;
        }
        thl.max(quotelen.min(olen))
    } else {
        8.max(quotelen.min(olen))
    };
    let icmplen = (ohl + elen).min(n.len());
    if icmplen < HDR_LEN {
        return;
    }
    st.icmpstat.inc(icps::OUTHIST + usize::from(t));
    let mut msg = [0u8; 8];
    msg[0] = t;
    if t == ty::REDIRECT {
        msg[4..8].copy_from_slice(&gw);
    } else if t == ty::PARAMPROB {
        msg[4] = c;
        c = 0;
    } else if t == ty::UNREACH && c == code::UNREACH_NEEDFRAG && mtu != 0 {
        msg[6..8].copy_from_slice(&u16::try_from(mtu).unwrap_or(u16::MAX).to_be_bytes());
    }
    msg[1] = c;
    let Ok(mut m) = Packet::for_output(8 + icmplen) else {
        return;
    };
    m.hdr.fib = n.hdr.fib;
    m.hdr.rcvif = n.hdr.rcvif;
    let mut quote = alloc::vec![0u8; icmplen];
    n.data.copy_out(0, &mut quote);
    if m.data.extend_from_slice(&msg).is_err() || m.data.extend_from_slice(&quote).is_err() {
        return;
    }
    // Reflected as if it came from the offending packet's source to its
    // destination.
    let mut back = oip;
    back.hl = 20;
    back.tos = 0;
    reflect(vnet, st, m, &back);
}
