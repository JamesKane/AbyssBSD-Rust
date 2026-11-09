// SPDX-License-Identifier: BSD-2-Clause
//! `setsockopt` and `getsockopt` on internet sockets: the socket level's
//! options, `IPPROTO_IP` (`AF_INET`), `IPPROTO_IPV6` (`AF_INET6`), and
//! `IPPROTO_TCP`. The other IP level is `EINVAL`, as each family's
//! `*_ctloutput` answers; an unknown option at the right level is
//! `ENOPROTOOPT`.
//!
//! Behavior reference: sys/kern/uipc_socket.c (`sosetopt`, `sogetopt`),
//! sys/netinet/ip_output.c (`ip_ctloutput`), sys/netinet/in_mcast.c
//! (`inp_setmoptions`, `inp_getmoptions`), sys/netinet6/ip6_output.c
//! (`ip6_ctloutput`), sys/netinet/tcp_usrreq.c (`tcp_ctloutput_set`,
//! `tcp_default_ctloutput`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::inet::{INP_IPV4, IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, IPPROTO_UDP, inp, ip, ipv6, tcp};

/// `IPPROTO_ICMPV6`.
const IPPROTO_ICMPV6: i32 = 58;
/// `IPV6_NEXTHOP`.
const IPV6_NEXTHOP: i32 = 48;
use abi::socket::{
    SO_ACCEPTCONN, SO_ACCEPTFILTER, SO_BINTIME, SO_BROADCAST, SO_DEBUG, SO_DOMAIN, SO_DONTROUTE,
    SO_ERROR, SO_FIB, SO_KEEPALIVE, SO_LINGER, SO_LISTENINCQLEN, SO_LISTENQLEN, SO_LISTENQLIMIT,
    SO_MAX_PACING_RATE, SO_NO_DDP, SO_NO_OFFLOAD, SO_NOSIGPIPE, SO_OOBINLINE, SO_PROTOCOL,
    SO_RCVBUF, SO_RCVLOWAT, SO_RCVTIMEO, SO_RERROR, SO_REUSEADDR, SO_REUSEPORT, SO_REUSEPORT_LB,
    SO_SNDBUF, SO_SNDLOWAT, SO_SNDTIMEO, SO_TIMESTAMP, SO_TS_CLOCK, SO_TS_CLOCK_MAX, SO_TYPE,
    SO_USELOOPBACK, SO_USER_COOKIE, SOL_SOCKET,
};

use crate::pcb::{self, table};
use crate::{Cred, Family, Proto, Socket, udp};

/// The boolean options, each its own `so_options` bit.
const BOOL_OPTS: &[i32] = &[
    SO_DEBUG,
    SO_KEEPALIVE,
    SO_DONTROUTE,
    SO_USELOOPBACK,
    SO_BROADCAST,
    SO_REUSEADDR,
    SO_REUSEPORT,
    SO_REUSEPORT_LB,
    SO_OOBINLINE,
    SO_TIMESTAMP,
    SO_BINTIME,
    SO_NOSIGPIPE,
    SO_NO_DDP,
    SO_NO_OFFLOAD,
    SO_RERROR,
];

/// `IPPROTO_IP` booleans and their `inp_flags` bits.
const IP_FLAGS: &[(i32, i32)] = &[
    (ip::IP_RECVOPTS, inp::RECVOPTS),
    (ip::IP_RECVRETOPTS, inp::RECVRETOPTS),
    (ip::IP_RECVDSTADDR, inp::RECVDSTADDR),
    (ip::IP_RECVTTL, inp::RECVTTL),
    (ip::IP_RECVIF, inp::RECVIF),
    (ip::IP_ONESBCAST, inp::ONESBCAST),
    (ip::IP_DONTFRAG, inp::DONTFRAG),
    (ip::IP_BINDANY, inp::BINDANY),
    (ip::IP_RECVTOS, inp::RECVTOS),
];

/// `IPPROTO_IPV6` booleans and their `inp_flags` bits.
const IP6_FLAGS: &[(i32, i32)] = &[
    (ipv6::IPV6_RECVPKTINFO, inp::IN6P_PKTINFO),
    (ipv6::IPV6_RECVHOPLIMIT, inp::IN6P_HOPLIMIT),
    (ipv6::IPV6_RECVTCLASS, inp::IN6P_TCLASS),
    (ipv6::IPV6_RECVRTHDR, 0x0010_0000),
    (ipv6::IPV6_RECVHOPOPTS, 0x0004_0000),
    (ipv6::IPV6_RECVDSTOPTS, 0x0008_0000),
    (ipv6::IPV6_AUTOFLOWLABEL, inp::IN6P_AUTOFLOWLABEL),
    (ipv6::IPV6_BINDANY, inp::BINDANY),
    (ipv6::IPV6_DONTFRAG, 0x0000_0800),
];

/// `sooptcopyin` of an `int`: `EINVAL` if shorter.
fn int(val: &[u8]) -> Result<i32, Errno> {
    let b: [u8; 4] = val
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .ok_or(Errno::EINVAL)?;
    Ok(i32::from_ne_bytes(b))
}

/// An `int` or a `u_char` (`inp_setmoptions`' multicast TTL and loop).
fn int_or_byte(val: &[u8]) -> Result<i32, Errno> {
    match val.len() {
        1 => Ok(i32::from(val[0])),
        _ => int(val),
    }
}

fn i64_at(val: &[u8], at: usize) -> i64 {
    val.get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i64::from_ne_bytes)
}

fn i32v(v: i32) -> Vec<u8> {
    v.to_ne_bytes().to_vec()
}

const NANOS: u64 = 1_000_000_000;

impl Socket {
    /// `sosetopt`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a short or bad value or the wrong IP level, `EDOM`
    /// for an out-of-range time, `ENOBUFS` for a buffer too big,
    /// `EACCES` for `IP_BINDANY` without privilege, `ENOPROTOOPT` for an
    /// unknown option.
    pub fn setopt(
        self: &Arc<Self>,
        level: i32,
        name: i32,
        val: &[u8],
        cred: &Cred,
    ) -> Result<(), Errno> {
        match level {
            SOL_SOCKET => self.sol_setopt(name, val),
            IPPROTO_TCP if self.proto == Proto::Tcp => self.tcp_setopt(name, val),
            IPPROTO_UDP if self.proto == Proto::Udp => Err(Errno::ENOPROTOOPT),
            IPPROTO_IP if self.family == Family::Inet => self.ip_setopt(name, val, cred),
            IPPROTO_IPV6 if self.family == Family::Inet6 => self.ip6_setopt(name, val, cred),
            // `rip6_ctloutput` to `icmp6_ctloutput`.
            IPPROTO_ICMPV6
                if self.family == Family::Inet6 && matches!(self.proto, Proto::Raw(_)) =>
            {
                match name {
                    crate::raw6::ICMP6_FILTER => self.set_icmp6_filter(val),
                    _ => Err(Errno::ENOPROTOOPT),
                }
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn sol_setopt(&self, name: i32, val: &[u8]) -> Result<(), Errno> {
        let mut i = self.inner.lock();
        match name {
            SO_LINGER => {
                if val.len() < 8 {
                    return Err(Errno::EINVAL);
                }
                let onoff = int(&val[..4])?;
                let linger = int(&val[4..8])?;
                if linger < 0 || linger > i32::from(u16::MAX) || linger > i32::MAX / 1000 {
                    return Err(Errno::EDOM);
                }
                i.linger = linger;
                if onoff != 0 {
                    self.options.fetch_or(SO_LINGER, Ordering::Relaxed);
                } else {
                    self.options.fetch_and(!SO_LINGER, Ordering::Relaxed);
                }
            }
            n if BOOL_OPTS.contains(&n) => {
                // Relaxed: binds read the word as FreeBSD's atomic load.
                if int(val)? != 0 {
                    self.options.fetch_or(n, Ordering::Relaxed);
                } else {
                    self.options.fetch_and(!n, Ordering::Relaxed);
                }
            }
            SO_USER_COOKIE => i.user_cookie = int(val)?.cast_unsigned(),
            SO_SNDBUF | SO_RCVBUF | SO_SNDLOWAT | SO_RCVLOWAT => {
                let v = int(val)?;
                if v < 1 {
                    return Err(Errno::EINVAL);
                }
                let cc = v.cast_unsigned();
                let sb = if name == SO_SNDBUF || name == SO_SNDLOWAT {
                    &mut i.snd
                } else {
                    &mut i.rcv
                };
                if name == SO_SNDBUF || name == SO_RCVBUF {
                    if !sb.reserve(cc) {
                        return Err(Errno::ENOBUFS);
                    }
                    // `sbsetopt`: a size set by hand is kept.
                    sb.auto = false;
                } else {
                    sb.lowat = cc.min(sb.hiwat);
                    sb.autolowat = false;
                }
            }
            SO_SNDTIMEO | SO_RCVTIMEO => {
                if val.len() < 16 {
                    return Err(Errno::EINVAL);
                }
                let (sec, usec) = (i64_at(val, 0), i64_at(val, 8));
                if sec < 0 || !(0..1_000_000).contains(&usec) {
                    return Err(Errno::EDOM);
                }
                let ns = if sec > i64::from(i32::MAX) {
                    u64::MAX
                } else {
                    sec.cast_unsigned() * NANOS + usec.cast_unsigned() * 1000
                };
                if name == SO_SNDTIMEO {
                    i.snd.timeo = ns;
                } else {
                    i.rcv.timeo = ns;
                }
            }
            SO_TS_CLOCK => {
                let v = int(val)?;
                if !(0..=SO_TS_CLOCK_MAX).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                i.ts_clock = v;
            }
            // No rate limiting on loopback (`RATELIMIT` is not in
            // GENERIC either).
            SO_MAX_PACING_RATE => return Err(Errno::EOPNOTSUPP),
            SO_FIB => {
                if int(val)? != 0 {
                    return Err(Errno::EINVAL);
                }
            }
            // No accept filters are loaded (`accept_filt_setopt`).
            // From the state held here: `is_listening` would take the
            // lock again.
            SO_ACCEPTFILTER => {
                let listening = i.tcp.as_ref().is_some_and(|t| t.listen.is_some());
                return Err(if listening {
                    Errno::ENOENT
                } else {
                    Errno::EINVAL
                });
            }
            _ => return Err(Errno::ENOPROTOOPT),
        }
        Ok(())
    }

    fn ip_setopt(&self, name: i32, val: &[u8], cred: &Cred) -> Result<(), Errno> {
        if let Some(&(_, bit)) = IP_FLAGS.iter().find(|(n, _)| *n == name) {
            if name == ip::IP_BINDANY && !cred.bindany {
                return Err(Errno::EPERM);
            }
            pcb::set_flag(self.proto, self.id, bit, int(val)? != 0);
            return Ok(());
        }
        match name {
            ip::IP_OPTIONS | ip::IP_RETOPTS => {
                if val.len() > 40 {
                    return Err(Errno::EINVAL);
                }
                Ok(())
            }
            ip::IP_TOS => {
                let v = int(val)?;
                self.inner.lock().ip.tos = u8::try_from(v & 0xff).unwrap_or(0);
                Ok(())
            }
            ip::IP_TTL => {
                let v = int(val)?;
                self.inner.lock().ip.ttl = u8::try_from(v & 0xff).unwrap_or(0);
                Ok(())
            }
            ip::IP_MINTTL => {
                let v = int(val)?;
                if !(0..=255).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.minttl = u8::try_from(v).unwrap_or(0);
                Ok(())
            }
            ip::IP_ORIGDSTADDR => {
                let on = int(val)? != 0;
                if let Some(mut p) = table(self.proto).lock().get_mut(self.id) {
                    if on {
                        p.flags2 |= inp::ORIGDSTADDR;
                    } else {
                        p.flags2 &= !inp::ORIGDSTADDR;
                    }
                }
                Ok(())
            }
            ip::IP_PORTRANGE => {
                let (low, high) = match int(val)? {
                    ip::IP_PORTRANGE_DEFAULT => (false, false),
                    ip::IP_PORTRANGE_HIGH => (false, true),
                    ip::IP_PORTRANGE_LOW => (true, false),
                    _ => return Err(Errno::EINVAL),
                };
                pcb::set_flag(self.proto, self.id, inp::LOWPORT, low);
                pcb::set_flag(self.proto, self.id, inp::HIGHPORT, high);
                Ok(())
            }
            ip::IP_MULTICAST_TTL => {
                let v = int_or_byte(val)?;
                if !(0..=255).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.mttl = u8::try_from(v).unwrap_or(1);
                Ok(())
            }
            ip::IP_MULTICAST_LOOP => {
                let v = int_or_byte(val)?;
                self.inner.lock().ip.mloop = u8::from(v != 0);
                Ok(())
            }
            ip::IP_MULTICAST_IF => {
                // An `in_addr` or an `ip_mreqn` (its interface index at
                // byte 8); the interface must exist.
                let a: [u8; 4] = val
                    .get(..4)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(Errno::EINVAL)?;
                if a != [0; 4]
                    && !self
                        .vnet
                        .net
                        .lock()
                        .is_local(Family::Inet, &crate::addr::from_v4(a), 0)
                {
                    return Err(Errno::EADDRNOTAVAIL);
                }
                self.inner.lock().ip.mif = a;
                Ok(())
            }
            ip::IP_ADD_MEMBERSHIP | ip::IP_DROP_MEMBERSHIP => {
                self.membership(name == ip::IP_ADD_MEMBERSHIP, val)
            }
            ip::IP_FLOWID | ip::IP_FLOWTYPE => {
                let v = int(val)?.cast_unsigned();
                let mut i = self.inner.lock();
                if name == ip::IP_FLOWID {
                    i.ip.flowid = v;
                } else {
                    i.ip.flowtype = v;
                }
                Ok(())
            }
            ip::IP_VLAN_PCP => {
                let v = int(val)?;
                if !(-1..=7).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.pcp = i8::try_from(v).unwrap_or(-1);
                Ok(())
            }
            ip::IP_HDRINCL if matches!(self.proto, Proto::Raw(_)) => {
                pcb::set_flag(self.proto, self.id, inp::HDRINCL, int(val)? != 0);
                Ok(())
            }
            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    fn ip6_setopt(&self, name: i32, val: &[u8], cred: &Cred) -> Result<(), Errno> {
        if let Some(&(_, bit)) = IP6_FLAGS.iter().find(|(n, _)| *n == name) {
            if val.len() != 4 {
                return Err(Errno::EINVAL);
            }
            if name == ipv6::IPV6_BINDANY && !cred.bindany {
                return Err(Errno::EPERM);
            }
            pcb::set_flag(self.proto, self.id, bit, int(val)? != 0);
            return Ok(());
        }
        match name {
            ipv6::IPV6_V6ONLY => {
                if val.len() != 4 {
                    return Err(Errno::EINVAL);
                }
                let on = int(val)? != 0;
                let mut t = table(self.proto).lock();
                let mut p = t.get_mut(self.id).ok_or(Errno::EINVAL)?;
                if p.lport != 0 || !crate::addr::is_unspecified(&p.laddr) {
                    return Err(Errno::EINVAL);
                }
                if on {
                    p.flags |= inp::IPV6_V6ONLY;
                    p.vflag &= !INP_IPV4;
                } else {
                    p.flags &= !inp::IPV6_V6ONLY;
                    p.vflag |= INP_IPV4;
                }
                Ok(())
            }
            ipv6::IPV6_UNICAST_HOPS => {
                if val.len() != 4 {
                    return Err(Errno::EINVAL);
                }
                let v = int(val)?;
                if !(-1..256).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                let mut i = self.inner.lock();
                i.ip.hops = i16::try_from(v).unwrap_or(-1);
                Ok(())
            }
            ipv6::IPV6_TCLASS => {
                let v = int(val)?;
                if !(-1..256).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.tclass = i16::try_from(v).unwrap_or(-1);
                Ok(())
            }
            ipv6::IPV6_MULTICAST_HOPS => {
                let v = int(val)?;
                if !(-1..256).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.mhops = if v == -1 {
                    1
                } else {
                    i16::try_from(v).unwrap_or(1)
                };
                Ok(())
            }
            ipv6::IPV6_MULTICAST_LOOP => {
                let v = int(val)?;
                if !(0..=1).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.mloop6 = u8::from(v != 0);
                Ok(())
            }
            ipv6::IPV6_MULTICAST_IF => {
                let v = int(val)?.cast_unsigned();
                if v != 0
                    && self
                        .vnet
                        .net
                        .lock()
                        .by_index(u16::try_from(v).unwrap_or(0))
                        .is_none()
                {
                    return Err(Errno::EADDRNOTAVAIL);
                }
                self.inner.lock().ip.mif6 = v;
                Ok(())
            }
            ipv6::IPV6_PORTRANGE => {
                let (low, high) = match int(val)? {
                    ip::IP_PORTRANGE_DEFAULT => (false, false),
                    ip::IP_PORTRANGE_HIGH => (false, true),
                    ip::IP_PORTRANGE_LOW => (true, false),
                    _ => return Err(Errno::EINVAL),
                };
                pcb::set_flag(self.proto, self.id, inp::LOWPORT, low);
                pcb::set_flag(self.proto, self.id, inp::HIGHPORT, high);
                Ok(())
            }
            ipv6::IPV6_ORIGDSTADDR => {
                let on = int(val)? != 0;
                if let Some(mut p) = table(self.proto).lock().get_mut(self.id) {
                    if on {
                        p.flags2 |= inp::ORIGDSTADDR;
                    } else {
                        p.flags2 &= !inp::ORIGDSTADDR;
                    }
                }
                Ok(())
            }
            ipv6::IPV6_PREFER_TEMPADDR => {
                let v = int(val)?;
                if !(-1..=1).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.prefer_tempaddr = v;
                Ok(())
            }
            ipv6::IPV6_USE_MIN_MTU => {
                if val.len() != 4 {
                    return Err(Errno::EINVAL);
                }
                let v = int(val)?;
                if !(-1..=1).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.minmtu6 = i8::try_from(v).unwrap_or(-1);
                Ok(())
            }
            ipv6::IPV6_RECVPATHMTU => {
                int(val)?;
                Ok(())
            }
            ipv6::IPV6_CHECKSUM if matches!(self.proto, Proto::Raw(_)) => {
                if val.len() != 4 {
                    return Err(Errno::EINVAL);
                }
                let v = int(val)?;
                if v < -1 || v % 2 != 0 {
                    return Err(Errno::EINVAL);
                }
                if self.proto == Proto::Raw(58) {
                    return if v == 2 { Ok(()) } else { Err(Errno::EINVAL) };
                }
                self.set_cksum6(v)
            }
            ipv6::IPV6_CHECKSUM => Err(Errno::EINVAL),
            ipv6::IPV6_JOIN_GROUP | ipv6::IPV6_LEAVE_GROUP => {
                self.membership6(name == ipv6::IPV6_JOIN_GROUP, val)
            }
            // The sticky packet options (`ip6_pcbopt`).
            ipv6::IPV6_PKTINFO => {
                if val.is_empty() {
                    self.inner.lock().ip.pktinfo6 = None;
                    return Ok(());
                }
                let pi = crate::raw6::check_pktinfo(&self.vnet, val)?;
                self.inner.lock().ip.pktinfo6 = (pi != ([0; 16], 0)).then_some(pi);
                Ok(())
            }
            ipv6::IPV6_HOPLIMIT => {
                if val.len() != 4 {
                    return Err(Errno::EINVAL);
                }
                let v = int(val)?;
                if !(-1..256).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                self.inner.lock().ip.hlim6 = i16::try_from(v).unwrap_or(-1);
                Ok(())
            }
            IPV6_NEXTHOP => {
                if val.is_empty() {
                    self.inner.lock().ip.nexthop6 = None;
                    return Ok(());
                }
                let a = crate::raw6::check_nexthop(val)?;
                self.inner.lock().ip.nexthop6 = Some(a);
                Ok(())
            }
            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    /// `in6p_join_group` and `in6p_leave_group` (`IPV6_JOIN_GROUP`,
    /// `IPV6_LEAVE_GROUP`): a `struct ipv6_mreq`, its interface 0 for the
    /// one the group's route goes out of.
    fn membership6(&self, join: bool, val: &[u8]) -> Result<(), Errno> {
        if val.len() < 20 {
            return Err(Errno::EINVAL);
        }
        let g: [u8; 16] = val[..16].try_into().map_err(|_| Errno::EINVAL)?;
        let idx = u32::from_ne_bytes(val[16..20].try_into().map_err(|_| Errno::EINVAL)?);
        if !crate::addr::is_multicast6(&g) {
            return Err(Errno::EINVAL);
        }
        let st = crate::state::of(&self.vnet);
        let mut n = self.vnet.net.lock();
        let ifindex = if idx == 0 {
            // `in6p_lookup_mcast_ifp`: the group's route's interface.
            match n.route6(0, &g, 0) {
                Some(r) => r.ifindex,
                None => 0,
            }
        } else {
            u16::try_from(idx).map_err(|_| Errno::EADDRNOTAVAIL)?
        };
        if !join {
            let mut i = self.inner.lock();
            let at = i
                .groups6
                .iter()
                .position(|&(x, k)| x == g && (idx == 0 || k == ifindex))
                .ok_or(Errno::EADDRNOTAVAIL)?;
            let (_, k) = i.groups6.remove(at);
            drop(i);
            crate::mld6::leave(&mut n, st, k, &g);
            return Ok(());
        }
        let ok = n
            .by_index(ifindex)
            .is_some_and(|i| i.flags & abi::net::iff::MULTICAST != 0);
        if !ok {
            return Err(Errno::EADDRNOTAVAIL);
        }
        {
            let mut i = self.inner.lock();
            if i.groups6.iter().any(|&(x, k)| x == g && k == ifindex) {
                return Err(Errno::EADDRINUSE);
            }
            i.groups6.push((g, ifindex));
        }
        crate::mld6::join(&mut n, st, ifindex, &g, 0);
        Ok(())
    }

    fn tcp_setopt(self: &Arc<Self>, name: i32, val: &[u8]) -> Result<(), Errno> {
        let st = crate::state::of(&self.vnet);
        let mut push = false;
        {
            let mut i = self.inner.lock();
            let t = i.tcp.as_mut().ok_or(Errno::EINVAL)?;
            if t.dropped {
                return Err(Errno::ECONNRESET);
            }
            let tp = &mut t.tp;
            let bit = |tp: &mut ::tcp::Tcpcb, b: u32, on: bool| {
                if on {
                    tp.flags |= b;
                } else {
                    tp.flags &= !b;
                }
            };
            match name {
                tcp::TCP_NODELAY => bit(tp, ::tcp::tf::NODELAY, int(val)? != 0),
                tcp::TCP_NOOPT => bit(tp, ::tcp::tf::NOOPT, int(val)? != 0),
                tcp::TCP_NOPUSH => {
                    let on = int(val)? != 0;
                    // Clearing it sends what it held back.
                    push = !on && tp.flags & ::tcp::tf::NOPUSH != 0 && tp.state.have_established();
                    bit(tp, ::tcp::tf::NOPUSH, on);
                }
                tcp::TCP_MAXSEG => {
                    let v = int(val)?;
                    let minmss = ::tcp::get(&st.tcp.conf.minmss);
                    if v > 0 && v.cast_unsigned() <= tp.t_maxseg && v + 40 >= minmss {
                        tp.t_maxseg = v.cast_unsigned();
                        tp.user_maxseg = v.cast_unsigned();
                    } else {
                        return Err(Errno::EINVAL);
                    }
                }
                tcp::TCP_KEEPIDLE | tcp::TCP_KEEPINTVL | tcp::TCP_KEEPINIT => {
                    let v = u32::from_ne_bytes(
                        val.get(..4)
                            .ok_or(Errno::EINVAL)?
                            .try_into()
                            .map_err(|_| Errno::EINVAL)?,
                    );
                    // Seconds, at most `UINT_MAX / hz`.
                    if v > u32::MAX / 1000 {
                        return Err(Errno::EINVAL);
                    }
                    let ms = v * 1000;
                    match name {
                        tcp::TCP_KEEPIDLE => tp.keepidle = ms,
                        tcp::TCP_KEEPINTVL => tp.keepintvl = ms,
                        _ => tp.keepinit = ms,
                    }
                }
                tcp::TCP_KEEPCNT => {
                    let v = int(val)?;
                    if v < 0 {
                        return Err(Errno::EINVAL);
                    }
                    tp.keepcnt = v.cast_unsigned();
                }
                tcp::TCP_CONGESTION => {
                    let n = val.iter().position(|&b| b == 0).unwrap_or(val.len());
                    let a = ::tcp::cc::Algo::by_name(&val[..n]).ok_or(Errno::ESRCH)?;
                    if tp.cc.algo() != a {
                        tp.cc = ::tcp::cc::Cc::new(a);
                    }
                }
                tcp::TCP_FASTOPEN => return Err(Errno::EINVAL),
                tcp::TCP_INFO => return Err(Errno::EINVAL),
                tcp::TCP_TXTLS_MODE => {
                    let v = int(val)?;
                    drop(i);
                    return Self::tls_set_tx_mode(&self.inner.lock(), v);
                }
                _ => return Err(Errno::ENOPROTOOPT),
            }
        }
        if push {
            self.tcp_output();
        }
        Ok(())
    }

    /// `sogetopt`: the value, which the caller copies out to the size the
    /// user gave.
    ///
    /// # Errors
    ///
    /// `EINVAL` for the wrong IP level, `ENOPROTOOPT` for an unknown
    /// option.
    pub fn getopt(&self, level: i32, name: i32) -> Result<Vec<u8>, Errno> {
        match level {
            SOL_SOCKET => self.sol_getopt(name),
            IPPROTO_TCP if self.proto == Proto::Tcp => self.tcp_getopt(name),
            IPPROTO_UDP if self.proto == Proto::Udp => Err(Errno::ENOPROTOOPT),
            IPPROTO_IP if self.family == Family::Inet => self.ip_getopt(name),
            IPPROTO_IPV6 if self.family == Family::Inet6 => self.ip6_getopt(name),
            IPPROTO_ICMPV6
                if self.family == Family::Inet6 && matches!(self.proto, Proto::Raw(_)) =>
            {
                match name {
                    crate::raw6::ICMP6_FILTER => Ok(self.icmp6_filter()),
                    _ => Err(Errno::ENOPROTOOPT),
                }
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn sol_getopt(&self, name: i32) -> Result<Vec<u8>, Errno> {
        let opts = self.so_options();
        let mut i = self.inner.lock();
        let tv = |ns: u64| {
            let mut b = Vec::new();
            let (s, us) = if ns == u64::MAX {
                (i64::MAX, 0)
            } else {
                (
                    (ns / NANOS).cast_signed(),
                    (ns % NANOS / 1000).cast_signed(),
                )
            };
            b.extend_from_slice(&s.to_ne_bytes());
            b.extend_from_slice(&us.to_ne_bytes());
            b
        };
        let (qlimit, qlen) = i
            .tcp
            .as_ref()
            .and_then(|t| t.listen.as_ref())
            .map_or((0, 0), |l| {
                (l.qlimit, u32::try_from(l.comp.len()).unwrap_or(0))
            });
        Ok(match name {
            SO_LINGER => {
                let mut b = i32v(opts & SO_LINGER);
                b.extend_from_slice(&i.linger.to_ne_bytes());
                b
            }
            n if BOOL_OPTS.contains(&n) || n == SO_ACCEPTCONN => i32v(opts & n),
            SO_FIB => i32v(0),
            SO_DOMAIN => i32v(i32::from(self.family.af())),
            SO_TYPE => i32v(self.proto.sock_type().cast_signed()),
            SO_PROTOCOL => i32v(self.proto.number()),
            SO_ERROR => i32v(i.error.take().map_or(0, Errno::get)),
            SO_SNDBUF => i32v(i.snd.hiwat.cast_signed()),
            SO_RCVBUF => i32v(i.rcv.hiwat.cast_signed()),
            SO_SNDLOWAT => i32v(i.snd.lowat.cast_signed()),
            SO_RCVLOWAT => i32v(i.rcv.lowat.cast_signed()),
            SO_SNDTIMEO => tv(i.snd.timeo),
            SO_RCVTIMEO => tv(i.rcv.timeo),
            SO_LISTENQLIMIT => i32v(qlimit.cast_signed()),
            SO_LISTENQLEN => i32v(qlen.cast_signed()),
            SO_LISTENINCQLEN => i32v(0),
            SO_USER_COOKIE => i32v(i.user_cookie.cast_signed()),
            SO_TS_CLOCK => i32v(i.ts_clock),
            SO_ACCEPTFILTER => return Err(Errno::EINVAL),
            _ => return Err(Errno::ENOPROTOOPT),
        })
    }

    fn ip_getopt(&self, name: i32) -> Result<Vec<u8>, Errno> {
        let (flags, flags2) = table(self.proto)
            .lock()
            .get(self.id)
            .map_or((0, 0), |p| (p.flags, p.flags2));
        if let Some(&(_, bit)) = IP_FLAGS.iter().find(|(n, _)| *n == name) {
            return Ok(i32v(i32::from(flags & bit != 0)));
        }
        let i = self.inner.lock();
        Ok(match name {
            ip::IP_OPTIONS | ip::IP_RETOPTS => Vec::new(),
            ip::IP_TOS => i32v(i32::from(i.ip.tos)),
            ip::IP_TTL => i32v(i32::from(i.ip.ttl)),
            ip::IP_MINTTL => i32v(i32::from(i.ip.minttl)),
            ip::IP_ORIGDSTADDR => i32v(i32::from(flags2 & inp::ORIGDSTADDR != 0)),
            ip::IP_PORTRANGE => i32v(if flags & inp::HIGHPORT != 0 {
                ip::IP_PORTRANGE_HIGH
            } else if flags & inp::LOWPORT != 0 {
                ip::IP_PORTRANGE_LOW
            } else {
                ip::IP_PORTRANGE_DEFAULT
            }),
            ip::IP_MULTICAST_TTL => i32v(i32::from(i.ip.mttl)),
            ip::IP_MULTICAST_LOOP => i32v(i32::from(i.ip.mloop)),
            ip::IP_MULTICAST_IF => i.ip.mif.to_vec(),
            ip::IP_FLOWID => i32v(i.ip.flowid.cast_signed()),
            ip::IP_FLOWTYPE => i32v(i.ip.flowtype.cast_signed()),
            ip::IP_VLAN_PCP => i32v(i32::from(i.ip.pcp)),
            ip::IP_HDRINCL if matches!(self.proto, Proto::Raw(_)) => {
                i32v(i32::from(flags & inp::HDRINCL != 0))
            }
            _ => return Err(Errno::ENOPROTOOPT),
        })
    }

    /// `inp_join_group` and `inp_leave_group` with an `ip_mreq` or
    /// `ip_mreqn`: the group on the interface named by index, by address,
    /// or (neither) by the route to the group; the interface's
    /// link-layer filter takes the group's address.
    fn membership(&self, join: bool, val: &[u8]) -> Result<(), Errno> {
        if val.len() < 8 {
            return Err(Errno::EINVAL);
        }
        let group: [u8; 4] = val[0..4].try_into().map_err(|_| Errno::EINVAL)?;
        let ifaddr: [u8; 4] = val[4..8].try_into().map_err(|_| Errno::EINVAL)?;
        let ifindex = if val.len() >= 12 {
            i32::from_ne_bytes(val[8..12].try_into().map_err(|_| Errno::EINVAL)?)
        } else {
            0
        };
        if !crate::addr::is_multicast4(&group) {
            return Err(Errno::EINVAL);
        }
        let (index, ether) = {
            let n = self.vnet.net.lock();
            let i = if ifindex > 0 {
                u16::try_from(ifindex).ok().and_then(|x| n.by_index(x))
            } else if ifaddr != [0; 4] {
                n.ifaddr(Family::Inet, &crate::addr::from_v4(ifaddr))
                    .map(|(i, _)| i)
            } else {
                // `inp_lookup_mcast_ifp`: the route to the group, else
                // the first interface with an IPv4 address that is a
                // multicast one and not a loopback.
                n.route_in(0, Family::Inet, &crate::addr::from_v4(group))
                    .and_then(|r| n.by_index(r.ifindex))
                    .or_else(|| {
                        n.ifaces.iter().find(|i| {
                            i.flags & abi::net::iff::MULTICAST != 0
                                && i.flags & abi::net::iff::LOOPBACK == 0
                                && i.addrs.iter().any(|a| a.family == Family::Inet)
                        })
                    })
            };
            let i = i
                .filter(|i| i.flags & abi::net::iff::MULTICAST != 0)
                .ok_or(Errno::EADDRNOTAVAIL)?;
            (i.index, i.lladdr.is_some())
        };
        {
            let mut i = self.inner.lock();
            let at = i.groups.iter().position(|&(g, x)| g == group && x == index);
            match (join, at) {
                (true, Some(_)) => return Err(Errno::EADDRINUSE),
                (true, None) => i.groups.push((group, index)),
                (false, Some(at)) => {
                    i.groups.remove(at);
                }
                (false, None) => return Err(Errno::EADDRNOTAVAIL),
            }
        }
        if ether {
            let ll = crate::igmp::ether_group(group);
            let mut n = self.vnet.net.lock();
            let _ = if join {
                n.add_multi(index, ll, false)
            } else {
                n.del_multi(index, ll)
            };
        }
        if join {
            crate::igmp::join(&self.vnet, index, group);
        } else {
            crate::igmp::leave(&self.vnet, index, group);
        }
        Ok(())
    }

    /// Leaves every group (`inp_freemoptions`), as its last close does.
    pub(crate) fn leave_groups(&self) {
        let groups6 = core::mem::take(&mut self.inner.lock().groups6);
        if !groups6.is_empty() {
            let st = crate::state::of(&self.vnet);
            let mut n = self.vnet.net.lock();
            for (g, k) in groups6 {
                crate::mld6::leave(&mut n, st, k, &g);
            }
        }
        let groups = core::mem::take(&mut self.inner.lock().groups);
        if groups.is_empty() {
            return;
        }
        {
            let mut n = self.vnet.net.lock();
            for &(g, index) in &groups {
                if n.by_index(index).is_some_and(|i| i.lladdr.is_some()) {
                    let _ = n.del_multi(index, crate::igmp::ether_group(g));
                }
            }
        }
        for (g, index) in groups {
            crate::igmp::leave(&self.vnet, index, g);
        }
    }

    fn ip6_getopt(&self, name: i32) -> Result<Vec<u8>, Errno> {
        let (flags, flags2) = table(self.proto)
            .lock()
            .get(self.id)
            .map_or((0, 0), |p| (p.flags, p.flags2));
        if let Some(&(_, bit)) = IP6_FLAGS.iter().find(|(n, _)| *n == name) {
            return Ok(i32v(i32::from(flags & bit != 0)));
        }
        let i = self.inner.lock();
        Ok(match name {
            ipv6::IPV6_V6ONLY => i32v(i32::from(flags & inp::IPV6_V6ONLY != 0)),
            ipv6::IPV6_UNICAST_HOPS => i32v(i32::from(i.ip.hops)),
            ipv6::IPV6_TCLASS => i32v(if i.ip.tclass < 0 {
                0
            } else {
                i32::from(i.ip.tclass)
            }),
            ipv6::IPV6_MULTICAST_HOPS => i32v(i32::from(i.ip.mhops)),
            ipv6::IPV6_MULTICAST_LOOP => i32v(i32::from(i.ip.mloop6)),
            ipv6::IPV6_MULTICAST_IF => i32v(i.ip.mif6.cast_signed()),
            ipv6::IPV6_PORTRANGE => i32v(if flags & inp::HIGHPORT != 0 {
                ip::IP_PORTRANGE_HIGH
            } else if flags & inp::LOWPORT != 0 {
                ip::IP_PORTRANGE_LOW
            } else {
                ip::IP_PORTRANGE_DEFAULT
            }),
            ipv6::IPV6_ORIGDSTADDR => i32v(i32::from(flags2 & inp::ORIGDSTADDR != 0)),
            ipv6::IPV6_PREFER_TEMPADDR => i32v(i.ip.prefer_tempaddr),
            ipv6::IPV6_USE_MIN_MTU => i32v(i32::from(i.ip.minmtu6)),
            ipv6::IPV6_RECVPATHMTU => i32v(0),
            ipv6::IPV6_CHECKSUM if self.proto == Proto::Raw(58) => i32v(2),
            ipv6::IPV6_CHECKSUM => i32v(i.ip.cksum6),
            ipv6::IPV6_PKTINFO => {
                let (a, k) = i.ip.pktinfo6.unwrap_or(([0; 16], 0));
                let mut b = a.to_vec();
                b.extend_from_slice(&k.to_ne_bytes());
                b
            }
            ipv6::IPV6_HOPLIMIT => i32v(i32::from(i.ip.hlim6)),
            IPV6_NEXTHOP => {
                i.ip.nexthop6
                    .map(|a| net::addr::sockaddr_in6(&a, 0, 0))
                    .unwrap_or_default()
            }
            _ => return Err(Errno::ENOPROTOOPT),
        })
    }

    fn tcp_getopt(&self, name: i32) -> Result<Vec<u8>, Errno> {
        let st = crate::state::of(&self.vnet);
        let conf = &st.tcp.conf;
        let i = self.inner.lock();
        let t = i.tcp.as_ref().ok_or(Errno::EINVAL)?;
        if t.dropped {
            return Err(Errno::ECONNRESET);
        }
        let tp = &t.tp;
        let or = |v: u32, d: &core::sync::atomic::AtomicI32| {
            if v != 0 {
                v
            } else {
                ::tcp::get(d).cast_unsigned()
            }
        };
        Ok(match name {
            tcp::TCP_NODELAY => i32v(i32::from(tp.flags & ::tcp::tf::NODELAY != 0)),
            tcp::TCP_NOPUSH => i32v(i32::from(tp.flags & ::tcp::tf::NOPUSH != 0)),
            tcp::TCP_NOOPT => i32v(i32::from(tp.flags & ::tcp::tf::NOOPT != 0)),
            tcp::TCP_MAXSEG => i32v(tp.t_maxseg.cast_signed()),
            tcp::TCP_KEEPIDLE => i32v((or(tp.keepidle, &conf.keepidle) / 1000).cast_signed()),
            tcp::TCP_KEEPINTVL => i32v((or(tp.keepintvl, &conf.keepintvl) / 1000).cast_signed()),
            tcp::TCP_KEEPINIT => i32v((or(tp.keepinit, &conf.keepinit) / 1000).cast_signed()),
            tcp::TCP_KEEPCNT => i32v(or(tp.keepcnt, &conf.keepcnt).cast_signed()),
            tcp::TCP_CONGESTION => {
                let mut b = alloc::vec![0u8; tcp::TCP_CA_NAME_MAX];
                let n = tp.cc.algo().name().as_bytes();
                b[..n.len()].copy_from_slice(n);
                b
            }
            tcp::TCP_FASTOPEN => i32v(0),
            tcp::TCP_FUNCTION_BLK => {
                // `struct tcp_function_set`: the stack's name, and a
                // `uint32_t` count.
                let mut b = alloc::vec![0u8; tcp::TCP_FUNCTION_NAME_LEN_MAX + 4];
                b[..7].copy_from_slice(b"freebsd");
                b
            }
            tcp::TCP_INFO => tcp_info(tp),
            tcp::TCP_TXTLS_MODE | tcp::TCP_RXTLS_MODE => i32v(Self::tls_mode(&i, name)?),
            _ => return Err(Errno::ENOPROTOOPT),
        })
    }

    /// Whether a received datagram's facts include `want` (for the
    /// kernel's control messages).
    #[must_use]
    pub fn wants(meta: &udp::Meta, want: u32) -> bool {
        meta.want & want != 0
    }
}

/// `tcp_fill_info`: `struct tcp_info`.
fn tcp_info(tp: &::tcp::Tcpcb) -> Vec<u8> {
    use ::tcp::{tf, tf2};
    let mut b = alloc::vec![0u8; 256];
    let put = |b: &mut [u8], off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    b[0] = u8::try_from(tp.state.number()).unwrap_or(0);
    let mut opts = 0u8;
    if tp.flags & tf::REQ_TSTMP != 0 && tp.flags & tf::RCVD_TSTMP != 0 {
        opts |= 0x01;
    }
    if tp.flags & tf::SACK_PERMIT != 0 {
        opts |= 0x02;
    }
    if tp.flags & tf::REQ_SCALE != 0 && tp.flags & tf::RCVD_SCALE != 0 {
        opts |= 0x04;
        b[6] = (tp.snd_scale & 0xf) | (tp.rcv_scale << 4);
    }
    match tp.flags2 & (tf2::ECN_PERMIT | tf2::ACE_PERMIT) {
        tf2::ECN_PERMIT => opts |= 0x08,
        0 => {}
        _ => opts |= 0x40,
    }
    b[5] = opts;
    let now = crate::now() / 1_000_000;
    put(&mut b, 8, tp.rxtcur * 1000);
    put(&mut b, 16, tp.t_maxseg);
    put(&mut b, 20, tp.t_maxseg);
    put(
        &mut b,
        52,
        u32::try_from(now.saturating_sub(tp.rcvtime) * 1000).unwrap_or(u32::MAX),
    );
    put(
        &mut b,
        68,
        u32::try_from((i64::from(tp.srtt) * 1000) >> 5).unwrap_or(0),
    );
    put(
        &mut b,
        72,
        u32::try_from((i64::from(tp.rttvar) * 1000) >> 4).unwrap_or(0),
    );
    put(&mut b, 76, tp.snd_ssthresh);
    put(&mut b, 80, tp.snd_cwnd);
    put(&mut b, 96, tp.rcv_wnd);
    put(&mut b, 100, tp.snd_wnd);
    put(&mut b, 108, tp.snd_nxt);
    put(&mut b, 112, tp.rcv_nxt);
    put(&mut b, 120, tp.sndrexmitpack);
    put(&mut b, 124, tp.rcvoopack);
    put(&mut b, 128, tp.sndzerowin);
    let ace = tf2::ECN_PERMIT | tf2::ACE_PERMIT;
    put(
        &mut b,
        132,
        if tp.flags2 & ace == ace {
            tp.scep.wrapping_sub(5)
        } else {
            tp.scep
        },
    );
    put(&mut b, 136, tp.rcep);
    put(&mut b, 176, tp.snd_una);
    put(&mut b, 180, tp.snd_max);
    put(&mut b, 184, u32::try_from(tp.rcv_numsacks).unwrap_or(0));
    put(&mut b, 188, tp.rcv_adv);
    put(&mut b, 192, tp.dupacks);
    put(&mut b, 196, tp.rttlow.cast_unsigned());
    b
}
