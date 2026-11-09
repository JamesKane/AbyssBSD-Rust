// SPDX-License-Identifier: BSD-2-Clause
//! Raw IPv6 sockets (`SOCK_RAW` in `AF_INET6`): every packet of a
//! socket's protocol for this host, its data past the IPv6 and extension
//! headers (with the facts the socket asked for as control messages);
//! ICMPv6 sockets get every ICMPv6 message their `ICMP6_FILTER` passes,
//! their checksums made and checked by the kernel; others may ask for a
//! checksum at an offset (`IPV6_CHECKSUM`). `ping -6`, `rtsold`, and
//! `traceroute6` use them.
//!
//! Behavior reference: sys/netinet6/raw_ip6.c (`rip6_input`,
//! `rip6_send`, `rip6_attach`, `rip6_bind`, `rip6_connect`,
//! `rip6_shutdown`), sys/netinet6/icmp6.c (`icmp6_rip6_input`,
//! `icmp6_ctloutput`), sys/netinet6/ip6_output.c (`ip6_raw_ctloutput`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::{INP_IPV6, inp, ss};
use mbuf::{Packet, flags};
use net::Vnet;
use net::addr::{In6, is_multicast6, is_unspecified};
use net::ifnet::in6_iff;

use crate::addr::Sockaddr;
use crate::icmp6::{self, ty};
use crate::in6::{self, is_zoned};
use crate::ip6::{self, HDR_LEN, Ip6, Mopts6, Opts6, proto};
use crate::pcb::RIPCB;
use crate::state::{self, State, icp6s, ip6s, rip6s};
use crate::udp::SendCtl;
use crate::{Family, Proto, Socket};

/// `ICMP6_FILTER`'s number at `IPPROTO_ICMPV6`.
pub const ICMP6_FILTER: i32 = 18;

/// The raw IPv6 sockets of protocol `p` whose addresses match packet
/// `ip6` (`rip6_inp_match`, `icmp6_rip6_match` for exactly ICMPv6), with
/// their PCB flags.
fn matching(vnet: &Vnet, ip6: &Ip6, p: u8, exact_proto: bool) -> Vec<(Arc<Socket>, i32)> {
    let t = RIPCB.lock();
    t.values()
        .filter(|x| {
            x.vnet.id == vnet.id
                && x.vflag & INP_IPV6 != 0
                && x.family == Family::Inet6
                && if exact_proto {
                    x.ip_p == p
                } else {
                    x.ip_p == 0 || x.ip_p == p
                }
                && (is_unspecified(&x.laddr) || x.laddr == ip6.dst)
                && (is_unspecified(&x.faddr) || x.faddr == ip6.src)
        })
        .filter_map(|x| x.socket().map(|s| (s, x.flags)))
        .collect()
}

/// The sender as a raw socket names it: a link-local address with its
/// zone (`sa6_recoverscope`).
fn from_of(ip6: &Ip6, rcvif: u16) -> Sockaddr {
    let zone = if is_zoned(&ip6.src) {
        u32::from(rcvif)
    } else {
        0
    };
    Sockaddr::v6(ip6.src, 0, zone)
}

/// The destination, for `IPV6_PKTINFO`.
fn to_of(ip6: &Ip6, rcvif: u16) -> Sockaddr {
    let zone = if is_zoned(&ip6.dst) {
        u32::from(rcvif)
    } else {
        0
    };
    Sockaddr::v6(ip6.dst, 0, zone)
}

/// Whether socket `so` hears multicast `m` (`im6o_mc_filter`): one with
/// memberships only its groups', MLD's messages always.
fn mc_pass(so: &Socket, ip6: &Ip6, m: &Packet) -> bool {
    if !is_multicast6(&ip6.dst) || m.hdr.flags & flags::RTALERT_MLD != 0 {
        return true;
    }
    let i = so.inner.lock();
    i.groups6.is_empty()
        || i.groups6
            .iter()
            .any(|&(g, idx)| g == ip6.dst && idx == m.hdr.rcvif)
}

/// `rip6_input`: copies of packet `m` (its protocol `nxt`'s data at `off`)
/// to the raw sockets of the protocol; a protocol no one handles (`known`
/// false) that no socket took is a parameter problem (unrecognized next
/// header), but for no next header.
pub(crate) fn input(vnet: &Arc<Vnet>, mut m: Packet, off: usize, nxt: u8, known: bool) {
    let st = state::of(vnet);
    st.rip6stat.inc(rip6s::IPACKETS);
    let Ok(ip6) = m.header::<Ip6>(0) else {
        return;
    };
    let rcvif = m.hdr.rcvif;
    let targets = matching(vnet, &ip6, nxt, false);
    let mut delivered = 0;
    let mut proto_none = nxt == proto::NONE;
    let mut data = Vec::new();
    for (so, pflags) in targets {
        let ck = so.inner.lock().ip.cksum6;
        if ck != -1 {
            st.rip6stat.inc(rip6s::ISUM);
            let ck = usize::try_from(ck).unwrap_or(0);
            if m.len() < off + ck + 2 || ip6::cksum6(&m, nxt, off, m.len() - off) != 0 {
                st.rip6stat.inc(rip6s::BADSUM);
                // Dropped without an error.
                proto_none = true;
                break;
            }
        }
        if !mc_pass(&so, &ip6, &m) {
            st.ip6stat.inc(ip6s::NOTMEMBER);
            continue;
        }
        if data.is_empty() {
            data = alloc::vec![0u8; m.len() - off];
            m.data.copy_out(off, &mut data);
        }
        if so.dgram_append(
            &from_of(&ip6, rcvif),
            &to_of(&ip6, rcvif),
            &data,
            pflags,
            false,
            Family::Inet6,
            ip6.hlim,
            ip6.tclass(),
            rcvif,
        ) {
            delivered += 1;
        } else {
            st.rip6stat.inc(rip6s::FULLSOCK);
        }
    }
    if delivered == 0 {
        st.rip6stat.inc(rip6s::NOSOCK);
        if m.hdr.flags & flags::MCAST != 0 {
            st.rip6stat.inc(rip6s::NOSOCKMCAST);
        }
        if !proto_none && !known {
            let prev = ip6::prevhdr(&m, off);
            icmp6::error(
                vnet,
                m,
                ty::PARAM_PROB,
                icmp6::code::PARAMPROB_NEXTHEADER,
                prev,
            );
        }
        st.ip6stat.dec(ip6s::DELIVERED);
    }
}

/// `icmp6_rip6_input`: ICMPv6 message `m` (at `off`, of type `t`) to the
/// raw ICMPv6 sockets whose filters pass it.
pub(crate) fn icmp6_input(vnet: &Arc<Vnet>, st: &State, mut m: Packet, off: usize, t: u8) {
    let Ok(ip6) = m.header::<Ip6>(0) else {
        return;
    };
    let rcvif = m.hdr.rcvif;
    let targets = matching(vnet, &ip6, proto::ICMPV6, true);
    let mut delivered = 0;
    let mut data = Vec::new();
    for (so, pflags) in targets {
        let pass = {
            let f = so.inner.lock().ip.icmp6filt;
            f[usize::from(t >> 5)] & (1 << (t & 31)) != 0
        };
        if !pass {
            continue;
        }
        if data.is_empty() {
            data = alloc::vec![0u8; m.len() - off];
            m.data.copy_out(off, &mut data);
        }
        if so.dgram_append(
            &from_of(&ip6, rcvif),
            &to_of(&ip6, rcvif),
            &data,
            pflags,
            false,
            Family::Inet6,
            ip6.hlim,
            ip6.tclass(),
            rcvif,
        ) {
            delivered += 1;
        }
    }
    if delivered == 0 {
        st.ip6stat.dec(ip6s::DELIVERED);
    }
}

/// `rip6_ctlinput`: the error goes to no raw socket (`in6_rtchange`
/// only forgets a cached route); the ICMPv6 message itself reaches the
/// ICMPv6 sockets.
pub(crate) fn ctlinput(_vnet: &Arc<Vnet>, _ctl: &icmp6::Ctl) {}

/// A `sockaddr_in6` call argument as `rip6_bind` and `rip6_send` take it.
fn parse(raw: &[u8]) -> Result<Sockaddr, Errno> {
    if raw.get(1).copied() != Some(abi::inet::AF_INET6) {
        return Err(Errno::EAFNOSUPPORT);
    }
    if raw.len() != 28 || raw[0] != 28 {
        return Err(Errno::EINVAL);
    }
    crate::addr::parse_in6(raw)
}

impl Socket {
    /// `rip6_bind`: to an address of this host's that is ready, or the
    /// wildcard.
    pub(crate) fn raw6_bind(&self, raw: &[u8]) -> Result<(), Errno> {
        let sa = parse(raw)?;
        let n = self.vnet.net.lock();
        if n.ifaces.is_empty() {
            return Err(Errno::EADDRNOTAVAIL);
        }
        let zone = u16::try_from(sa.scope).unwrap_or(0);
        if sa.scope != 0 && n.by_index(zone).is_none() {
            return Err(Errno::ENXIO);
        }
        if !is_unspecified(&sa.addr) {
            let Some((_, f)) = in6::ifa_with(&n, &sa.addr, zone) else {
                return Err(Errno::EADDRNOTAVAIL);
            };
            if f & (in6_iff::NOTREADY | in6_iff::DETACHED | in6_iff::DEPRECATED) != 0 {
                return Err(Errno::EADDRNOTAVAIL);
            }
        }
        drop(n);
        let mut t = RIPCB.lock();
        let mut p = t.get_mut(self.id).ok_or(Errno::EINVAL)?;
        p.laddr = sa.addr;
        p.zone = sa.scope;
        Ok(())
    }

    /// `rip6_connect`: datagrams go to and come only from the address in
    /// `raw`, from the source selection picks.
    pub(crate) fn raw6_connect(&self, raw: &[u8]) -> Result<(), Errno> {
        if raw.len() != 28 || raw.first() != Some(&28) {
            return Err(Errno::EINVAL);
        }
        if raw.get(1).copied() != Some(abi::inet::AF_INET6) {
            return Err(Errno::EAFNOSUPPORT);
        }
        let sa = crate::addr::parse_in6(raw)?;
        let (src, zone) = {
            let st = state::of(&self.vnet);
            let n = self.vnet.net.lock();
            if n.ifaces.is_empty() {
                return Err(Errno::EADDRNOTAVAIL);
            }
            let req = self.srcreq(&sa, None);
            let (src, ifp) = in6::selectsrc(&n, st, &req)?;
            (
                src,
                if sa.scope == 0 && is_zoned(&sa.addr) {
                    u32::from(ifp)
                } else {
                    sa.scope
                },
            )
        };
        {
            let mut t = RIPCB.lock();
            let mut p = t.get_mut(self.id).ok_or(Errno::EINVAL)?;
            p.faddr = sa.addr;
            p.laddr = src;
            p.zone = zone;
        }
        let mut i = self.inner.lock();
        i.state = (i.state | ss::ISCONNECTED) & !(ss::ISCONNECTING | ss::ISDISCONNECTING);
        drop(i);
        self.wake();
        Ok(())
    }

    /// What source selection is told about this socket and a send to
    /// `dst`.
    pub(crate) fn srcreq(&self, dst: &Sockaddr, ctl: Option<&SendCtl>) -> in6::SrcReq {
        let (laddr, bindany) = crate::pcb::table(self.proto)
            .lock()
            .get(self.id)
            .map_or(([0; 16], false), |p| (p.laddr, p.flags & inp::BINDANY != 0));
        let o = self.inner.lock().ip;
        let pktinfo = ctl
            .and_then(|c| c.pktinfo6)
            .or(o.pktinfo6)
            .map(|(a, i)| (a, u16::try_from(i).unwrap_or(0)));
        in6::SrcReq {
            dst: dst.addr,
            zone: u16::try_from(dst.scope).unwrap_or(0),
            pktinfo,
            laddr,
            mif: u16::try_from(o.mif6).unwrap_or(0),
            oifp: 0,
            fib: 0,
            bindany,
            prefer_tempaddr: o.prefer_tempaddr,
        }
    }

    /// The PCB's `inp_flags`, read before the interfaces' lock: a table's
    /// lock is taken before that one, never under it.
    pub(crate) fn pcb_flags(&self) -> i32 {
        crate::pcb::table(self.proto)
            .lock()
            .get(self.id)
            .map_or(0, |p| p.flags)
    }

    /// The packet options for a send to `dst` (`ip6_setpktopts` over the
    /// sticky ones, and `in6p_moptions`), with the PCB's `pflags`.
    pub(crate) fn opts6(
        &self,
        dst: &Sockaddr,
        ctl: Option<&SendCtl>,
        src: &In6,
        ifp: u16,
        pflags: i32,
    ) -> Opts6 {
        let o = self.inner.lock().ip;
        let hlim = ctl
            .and_then(|c| c.hlim6)
            .map(i16::try_from)
            .and_then(Result::ok)
            .filter(|h| *h >= 0)
            .or((o.hlim6 >= 0).then_some(o.hlim6))
            .and_then(|h| u8::try_from(h).ok());
        let tclass = ctl
            .and_then(|c| c.tclass6)
            .filter(|t| *t >= 0)
            .or((o.tclass >= 0).then_some(i32::from(o.tclass)))
            .and_then(|t| u8::try_from(t).ok());
        let ifindex = ctl
            .and_then(|c| c.pktinfo6)
            .or(o.pktinfo6)
            .map_or(0, |(_, i)| u16::try_from(i).unwrap_or(0));
        let mut flags = 0;
        if pflags & inp::DONTFRAG != 0 {
            flags |= ip6::out::DONTFRAG;
        }
        Opts6 {
            flags,
            mcast: Some(Mopts6 {
                ifindex: u16::try_from(o.mif6).unwrap_or(0),
                hlim: u8::try_from(o.mhops.max(0)).unwrap_or(1),
                loop_: o.mloop6 != 0,
            }),
            fib: 0,
            pcp: self.pcp(),
            flowid: o.flowid,
            hlim,
            tclass,
            ifindex,
            nexthop: ctl.and_then(|c| c.nexthop6).or(o.nexthop6),
            zone: if is_zoned(&dst.addr) {
                u16::try_from(dst.scope).unwrap_or(ifp)
            } else {
                0
            },
            src_zone: if is_zoned(src) { ifp } else { 0 },
            router_alert: false,
            minmtu: o.minmtu6,
        }
    }

    /// `rip6_send`: one datagram to the connected address or `to`, behind
    /// the IPv6 header the kernel makes; an ICMPv6 socket's (or one with
    /// `IPV6_CHECKSUM`'s) checksum is computed.
    pub(crate) fn raw6_send(
        &self,
        data: &[u8],
        to: Option<&[u8]>,
        ctl: &SendCtl,
    ) -> Result<usize, Errno> {
        let Proto::Raw(p) = self.proto else {
            return Err(Errno::EOPNOTSUPP);
        };
        let connected = {
            let mut i = self.inner.lock();
            if i.snd.cant {
                return Err(Errno::EPIPE);
            }
            if let Some(e) = i.error.take() {
                return Err(e);
            }
            if data.len() > i.snd.space() as usize {
                return Err(Errno::EMSGSIZE);
            }
            i.state & ss::ISCONNECTED != 0
        };
        let mut dst = match (connected, to) {
            (true, Some(_)) => return Err(Errno::EISCONN),
            (true, None) => {
                let t = RIPCB.lock();
                let pcb = t.get(self.id).ok_or(Errno::EINVAL)?;
                Sockaddr::v6(pcb.faddr, 0, pcb.zone)
            }
            (false, Some(raw)) => parse(raw)?,
            (false, None) => return Err(Errno::ENOTCONN),
        };
        let st = state::of(&self.vnet);
        let (ck, cksum6) = {
            let o = self.inner.lock().ip;
            (
                p == proto::ICMPV6 || o.cksum6 != -1,
                if p == proto::ICMPV6 { 2 } else { o.cksum6 },
            )
        };
        let ckoff = usize::try_from(cksum6).unwrap_or(0);
        if ck && data.len() < ckoff + 2 {
            return Err(Errno::EINVAL);
        }
        if data.len() + HDR_LEN > ip6::MAXPACKET + HDR_LEN {
            return Err(Errno::EMSGSIZE);
        }
        // The table's lock before the interfaces', never under it.
        let pflags = self.pcb_flags();
        let laddr = RIPCB.lock().get(self.id).map_or([0; 16], |x| x.laddr);
        let mut n = self.vnet.net.lock();
        if dst.scope != 0 && n.by_index(u16::try_from(dst.scope).unwrap_or(0)).is_none() {
            return Err(Errno::ENXIO);
        }
        let req = self.srcreq(&dst, Some(ctl));
        let (src, ifp) = in6::selectsrc(&n, st, &req)?;
        if dst.scope == 0 && is_zoned(&dst.addr) {
            dst.scope = u32::from(ifp);
        }
        let hops = self.inner.lock().ip.hops;
        let hlim = in6::selecthlim(&n, hops, ifp, None);
        let mut m = Packet::for_output(data.len()).map_err(|_| Errno::ENOBUFS)?;
        m.data.extend_from_slice(data).map_err(|_| Errno::ENOBUFS)?;
        let plen = u16::try_from(data.len()).map_err(|_| Errno::EMSGSIZE)?;
        let h = Ip6::new(p, src, dst.addr, plen, hlim, 0);
        m.prepend(HDR_LEN)
            .map_err(|_| Errno::ENOBUFS)?
            .copy_from_slice(&h.bytes());
        if ck {
            let at = HDR_LEN + ckoff;
            m.data.copy_in(at, &[0, 0]).map_err(|_| Errno::ENOBUFS)?;
            let c = ip6::cksum6(&m, p, HDR_LEN, data.len());
            m.data
                .copy_in(at, &c.to_be_bytes())
                .map_err(|_| Errno::ENOBUFS)?;
        }
        let mut o = self.opts6(&dst, Some(ctl), &src, ifp, pflags);
        // `rip6_send`'s `CALC_FLOWID_OUTBOUND`: from the bound address.
        if n.hash_outbound {
            o.flowid = crate::udp::flow_hash6(&laddr, &dst.addr, 0, 0, p);
        }
        let r = ip6::output_if(&mut n, st, m, &o);
        if p == proto::ICMPV6 {
            let (t, c) = (data[0], data[1]);
            if let Ok(outif) = r {
                icmp6::ifoutstat(&n, outif, t, c);
            }
            st.icmp6stat.inc(icp6s::OUTHIST + usize::from(t));
        } else {
            st.rip6stat.inc(rip6s::OPACKETS);
        }
        drop(n);
        r.map(|_| data.len())
    }

    /// `ip6_raw_ctloutput`'s `IPV6_CHECKSUM` (`setsockopt`): an even
    /// offset, or -1 for none; not on an ICMPv6 socket, whose checksum is
    /// always made.
    pub(crate) fn set_cksum6(&self, v: i32) -> Result<(), Errno> {
        if self.proto == Proto::Raw(proto::ICMPV6) {
            return Err(Errno::EINVAL);
        }
        if !matches!(self.proto, Proto::Raw(_)) {
            return Err(Errno::ENOPROTOOPT);
        }
        if v < -1 || (v > 0 && v % 2 != 0) {
            return Err(Errno::EINVAL);
        }
        self.inner.lock().ip.cksum6 = v;
        Ok(())
    }

    /// `ICMP6_FILTER`'s value.
    pub(crate) fn icmp6_filter(&self) -> Vec<u8> {
        self.inner
            .lock()
            .ip
            .icmp6filt
            .iter()
            .flat_map(|w| w.to_ne_bytes())
            .collect()
    }

    /// Sets `ICMP6_FILTER` (`icmp6_ctloutput`): 32 bytes, at most (the
    /// rest left as is when shorter); a raw ICMPv6 socket's only.
    pub(crate) fn set_icmp6_filter(&self, val: &[u8]) -> Result<(), Errno> {
        if self.proto != Proto::Raw(proto::ICMPV6) {
            return Err(Errno::EOPNOTSUPP);
        }
        if val.len() != 32 {
            return Err(Errno::EMSGSIZE);
        }
        let mut i = self.inner.lock();
        let mut b: Vec<u8> =
            i.ip.icmp6filt
                .iter()
                .flat_map(|w| w.to_ne_bytes())
                .collect();
        b[..val.len()].copy_from_slice(val);
        for (w, c) in i.ip.icmp6filt.iter_mut().zip(b.as_chunks::<4>().0) {
            *w = u32::from_ne_bytes(*c);
        }
        Ok(())
    }
}

/// `ip6_setpktopt`'s `IPV6_PKTINFO`: a `struct in6_pktinfo` whose
/// interface exists (or is 0) and whose address is this host's (or
/// unspecified).
///
/// # Errors
///
/// `EINVAL` for the wrong length, `ENXIO` for no such interface,
/// `EADDRNOTAVAIL` for an address not this host's.
pub fn check_pktinfo(vnet: &Vnet, val: &[u8]) -> Result<(In6, u32), Errno> {
    if val.len() != 20 {
        return Err(Errno::EINVAL);
    }
    let a: In6 = val[..16].try_into().map_err(|_| Errno::EINVAL)?;
    let idx = u32::from_ne_bytes(val[16..20].try_into().map_err(|_| Errno::EINVAL)?);
    let n = vnet.net.lock();
    if idx != 0 && n.by_index(u16::try_from(idx).unwrap_or(0)).is_none() {
        return Err(Errno::ENXIO);
    }
    if !is_unspecified(&a) {
        let zone = u16::try_from(idx).unwrap_or(0);
        match in6::ifa_with(&n, &a, zone) {
            Some((_, f)) if f & (in6_iff::ANYCAST | in6_iff::NOTREADY) == 0 => {}
            _ => return Err(Errno::EADDRNOTAVAIL),
        }
    }
    Ok((a, idx))
}

/// `ip6_setpktopt`'s `IPV6_NEXTHOP`: a `sockaddr_in6`, unicast.
///
/// # Errors
///
/// `EINVAL` for a short one or a multicast address, `EAFNOSUPPORT` for
/// another family.
pub fn check_nexthop(val: &[u8]) -> Result<In6, Errno> {
    if val.len() < 2 || usize::from(val[0]) > val.len() {
        return Err(Errno::EINVAL);
    }
    if val[1] != abi::inet::AF_INET6 {
        return Err(Errno::EAFNOSUPPORT);
    }
    if val[0] != 28 || val.len() < 28 {
        return Err(Errno::EINVAL);
    }
    let a: In6 = val[8..24].try_into().map_err(|_| Errno::EINVAL)?;
    if is_multicast6(&a) || is_unspecified(&a) {
        return Err(Errno::EINVAL);
    }
    Ok(a)
}
