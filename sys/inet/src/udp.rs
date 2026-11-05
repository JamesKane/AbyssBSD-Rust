// SPDX-License-Identifier: BSD-2-Clause
//! UDP: datagrams from one socket straight to the one its destination's
//! lookup finds, with what the receiver asked to learn about each
//! (`IP_RECVDSTADDR`, `IPV6_RECVPKTINFO`, ...), and the ICMP port
//! unreachable a connected sender sees as `ECONNREFUSED`.
//!
//! Behavior reference: sys/netinet/udp_usrreq.c (`udp_send`,
//! `udp_append`, `udp_connect`, `udp_disconnect`, `udp_shutdown`,
//! `udp_notify`), sys/netinet6/udp6_usrreq.c, sys/kern/uipc_socket.c
//! (`soreceive_dgram`, `sosend_dgram`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::{inp, ss};
use abi::socket::{MSG_TRUNC, SHUT_RD, SHUT_RDWR, SHUT_WR, SO_BINTIME, SO_BROADCAST, SO_TIMESTAMP};

use mbuf::{Packet, csum};
use net::Vnet;

use crate::addr::{self, Sockaddr, is_broadcast4, is_unspecified, is_v4mapped, v4_of};
use crate::ip::{self, Ip};
use crate::pcb::UDB;
use crate::state::udps;
use crate::{Cred, Family, MSIZE, Socket, cksum, icmp, mbuf_cost};

/// What a receiver asked to be told about a datagram, decided when it
/// arrives (`udp_append`'s `ip_savecontrol`, `ip6_savecontrol`).
pub mod want {
    pub const DSTADDR: u32 = 1 << 0;
    pub const TTL: u32 = 1 << 1;
    pub const TOS: u32 = 1 << 2;
    pub const IF: u32 = 1 << 3;
    pub const ORIGDSTADDR: u32 = 1 << 4;
    pub const PKTINFO: u32 = 1 << 5;
    pub const HOPLIMIT: u32 = 1 << 6;
    pub const TCLASS: u32 = 1 << 7;
    pub const TIMESTAMP: u32 = 1 << 8;
    pub const BINTIME: u32 = 1 << 9;
    /// `SO_TIMESTAMP` with `SO_TS_REALTIME` (`SCM_REALTIME`) and
    /// `SO_TS_MONOTONIC` (`SCM_MONOTONIC`).
    pub const REALTIME: u32 = 1 << 10;
    pub const MONOTONIC: u32 = 1 << 11;
}

/// A datagram's facts, for the control messages its receiver wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// [`want`] bits.
    pub want: u32,
    /// Its destination, as the receiver names addresses.
    pub dst: Sockaddr,
    /// TTL or hop limit, TOS or traffic class.
    pub ttl: u8,
    pub tos: u8,
    /// The interface it came in on.
    pub ifindex: u16,
    /// When it arrived (the hooks' clock).
    pub stamp: u64,
}

/// A queued datagram (`MT_SONAME` mbuf, control, and data).
#[derive(Debug)]
pub(crate) struct Dgram {
    pub(crate) from: Sockaddr,
    pub(crate) data: Vec<u8>,
    pub(crate) meta: Meta,
    /// What it is charged (`sb_ccc`, `sb_mbcnt`).
    cc: u32,
    mb: u32,
}

/// What a send's control messages ask of its one datagram (`udp_send`'s
/// `IP_SENDSRCADDR` and `IP_TOS`; `ip6_setpktopts`' `IPV6_PKTINFO`,
/// `IPV6_HOPLIMIT`, `IPV6_TCLASS`, and `IPV6_NEXTHOP`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendCtl {
    pub src: Option<[u8; 4]>,
    pub tos: Option<u8>,
    pub pktinfo6: Option<([u8; 16], u32)>,
    pub hlim6: Option<i32>,
    pub tclass6: Option<i32>,
    pub nexthop6: Option<[u8; 16]>,
    /// `TLS_SET_RECORD_TYPE`.
    pub tls_type: Option<u8>,
}

/// What a receive got.
#[derive(Debug, Default)]
pub struct Received {
    pub data: Vec<u8>,
    /// The sender, for a datagram.
    pub from: Option<Sockaddr>,
    /// `MSG_TRUNC`, `MSG_OOB`.
    pub flags: i32,
    /// With `MSG_TRUNC`, a datagram's whole length.
    pub full: usize,
    pub meta: Option<Meta>,
    /// A TLS record's `TLS_GET_RECORD`.
    pub tls: Option<crate::tls::RecordInfo>,
}

impl Socket {
    /// `udp_connect` and `udp6_connect` (after `soconnectat` has
    /// disconnected a connected socket, which is how `AF_UNSPEC`
    /// disconnects).
    pub(crate) fn udp_connect(&self, raw: &[u8], cred: &Cred) -> Result<(), Errno> {
        if self.inner.lock().state & ss::ISCONNECTED != 0 {
            self.udp_disconnect();
        }
        let sa = match self.family {
            Family::Inet => addr::parse_in(raw, false)?,
            Family::Inet6 => addr::parse_in6(raw)?,
        };
        let mut t = UDB.lock();
        if t.get(self.id).is_some_and(|p| !is_unspecified(&p.faddr)) {
            return Err(Errno::EISCONN);
        }
        t.connect(self, &sa, cred)?;
        drop(t);
        let mut i = self.inner.lock();
        i.state = (i.state | ss::ISCONNECTED) & !ss::ISDISCONNECTED;
        i.error = None;
        drop(i);
        self.wake();
        Ok(())
    }

    /// `udp_disconnect`.
    fn udp_disconnect(&self) {
        UDB.lock().disconnect(self.id);
        self.inner.lock().state &= !ss::ISCONNECTED;
    }

    /// `udp_shutdown`: `ENOTCONN` unconnected, but the buffers shut all
    /// the same.
    pub(crate) fn udp_shutdown(&self, how: i32) -> Result<(), Errno> {
        let mut i = self.inner.lock();
        let r = if i.state & ss::ISCONNECTED == 0 {
            Err(Errno::ENOTCONN)
        } else {
            Ok(())
        };
        if how == SHUT_RD || how == SHUT_RDWR {
            // `sorflush`.
            i.rcv.cant = true;
            i.dgrams.clear();
            i.rcv.cc = 0;
            i.rcv.mbcnt = 0;
        }
        if how == SHUT_WR || how == SHUT_RDWR {
            i.snd.cant = true;
        }
        drop(i);
        self.wake();
        r
    }

    /// `udp_close` and `udp_detach`.
    pub(crate) fn udp_close(&self) {
        UDB.lock().free(self.id);
        let mut i = self.inner.lock();
        i.dgrams.clear();
        i.rcv.cc = 0;
        i.rcv.mbcnt = 0;
        i.state &= !ss::ISCONNECTED;
    }

    /// `sosend_dgram` and `udp_send`: one datagram, to the connected
    /// peer or `to` (raw), binding an ephemeral port first if it has
    /// none. A datagram nobody is bound to is dropped; a connected
    /// sender then sees `ECONNREFUSED` on its next call (the ICMP port
    /// unreachable). Returns the bytes sent.
    ///
    /// # Errors
    ///
    /// `EPIPE` after `shutdown`; its pending error; `EMSGSIZE` over
    /// `net.inet.udp.maxdgram`; `EISCONN` with `to` while connected;
    /// `EDESTADDRREQ` with neither; address errors (`EAFNOSUPPORT`,
    /// `EINVAL`, `EADDRNOTAVAIL`); `EACCES` to a broadcast address
    /// without `SO_BROADCAST`; `ENETUNREACH` or `EHOSTUNREACH`.
    pub fn send_dgram(&self, data: &[u8], to: Option<&[u8]>, cred: &Cred) -> Result<usize, Errno> {
        self.send_dgram_ctl(data, to, cred, SendCtl::default())
    }

    /// [`Socket::send_dgram`] with the control messages' choices.
    ///
    /// # Errors
    ///
    /// As [`Socket::send_dgram`]; `EINVAL` for a source address on a
    /// socket with no port, or neither it nor the socket with an
    /// address; `EADDRNOTAVAIL` for a source not this host's.
    pub fn send_dgram_ctl(
        &self,
        data: &[u8],
        to: Option<&[u8]>,
        cred: &Cred,
        ctl: SendCtl,
    ) -> Result<usize, Errno> {
        if let crate::Proto::Raw(_) = self.proto {
            if self.family == Family::Inet6 {
                return self.raw6_send(data, to, &ctl);
            }
            return self.raw_send(data, to, cred);
        }
        let connected = {
            let mut i = self.inner.lock();
            if i.snd.cant {
                return Err(Errno::EPIPE);
            }
            if let Some(e) = i.error.take() {
                return Err(e);
            }
            if data.len() > i.snd.hiwat as usize {
                return Err(Errno::EMSGSIZE);
            }
            i.state & ss::ISCONNECTED != 0
        };
        // `IP_MULTICAST_IF`'s interface, looked at before the table's lock.
        let mif = self.mopts().map_or(0, |m| m.ifindex);
        let mut flow = 0;
        let (src, dst, family) = {
            let mut t = UDB.lock();
            // `IP_SENDSRCADDR`: `in_pcbbind_setup` of the address with
            // the socket's port, for this datagram, checked before the
            // destination is.
            if let Some(a) = ctl.src {
                let p = t.get(self.id).ok_or(Errno::EINVAL)?;
                if p.lport == 0 || (is_unspecified(&p.laddr) && a == [0; 4]) {
                    return Err(Errno::EINVAL);
                }
                if a != [0; 4]
                    && p.flags & inp::BINDANY == 0
                    && !self
                        .vnet
                        .net
                        .lock()
                        .is_local(Family::Inet, &addr::from_v4(a), 0)
                {
                    return Err(Errno::EADDRNOTAVAIL);
                }
            }
            let dst = match to {
                Some(raw) => {
                    if connected {
                        return Err(Errno::EISCONN);
                    }
                    let sa = match self.family {
                        Family::Inet => addr::parse_in(raw, false)?,
                        Family::Inet6 => addr::parse_in6(raw)?,
                    };
                    let p = t.get(self.id).ok_or(Errno::EINVAL)?;
                    let sa = if self.family == Family::Inet6 && is_v4mapped(&sa.addr) {
                        // `udp6_send` hands a mapped destination to IPv4,
                        // unless the socket is IPv6-only.
                        if p.flags & inp::IPV6_V6ONLY != 0 {
                            return Err(Errno::EINVAL);
                        }
                        sa.unmapped()
                    } else if self.family == Family::Inet6 && p.vflag & abi::inet::INP_IPV6 == 0 {
                        // Bound to a mapped address: IPv4 only.
                        return Err(Errno::EAFNOSUPPORT);
                    } else {
                        sa
                    };
                    if sa.port == 0 {
                        return Err(Errno::EADDRNOTAVAIL);
                    }
                    sa
                }
                None => {
                    if !connected {
                        return Err(Errno::EDESTADDRREQ);
                    }
                    let p = t.get(self.id).ok_or(Errno::ENOTCONN)?;
                    if p.vflag & abi::inet::INP_IPV6 == 0 {
                        Sockaddr::v4(v4_of(&p.faddr), p.fport)
                    } else {
                        Sockaddr::v6(p.faddr, p.fport, p.zone)
                    }
                }
            };
            if dst.family == Family::Inet {
                let a = v4_of(&dst.addr);
                if is_broadcast4(&a) && self.so_options() & SO_BROADCAST == 0 {
                    return Err(Errno::EACCES);
                }
            }
            if t.get(self.id).is_some_and(|p| p.lport == 0) {
                t.bind_anon(self, cred)?;
            }
            // `in_pcbladdr`: to a group, from the address of the interface
            // `IP_MULTICAST_IF` chose, no route needed.
            let mcast_src = (dst.family == Family::Inet
                && crate::addr::is_multicast4(&v4_of(&dst.addr))
                && mif != 0)
                .then(|| {
                    let p = t.get(self.id)?;
                    let bound = v4_of(&p.laddr);
                    if bound != [0; 4] {
                        return Some(Ok(Sockaddr::v4(bound, p.lport)));
                    }
                    let n = self.vnet.net.lock();
                    let a = n.by_index(mif).and_then(|i| {
                        i.addrs
                            .iter()
                            .find(|a| a.family == Family::Inet)
                            .map(|a| v4_of(&a.addr))
                    });
                    Some(
                        a.map(|a| Sockaddr::v4(a, p.lport))
                            .ok_or(Errno::EADDRNOTAVAIL),
                    )
                })
                .flatten();
            let mut src = match mcast_src {
                Some(r) => r?,
                // IPv6's source is chosen below, with the table let go.
                None if dst.family == Family::Inet6 => {
                    let p = t.get(self.id).ok_or(Errno::EINVAL)?;
                    flow = p.flow;
                    Sockaddr::v6(p.laddr, p.lport, p.zone)
                }
                None => t.source_for(self.id, &dst)?,
            };
            if let Some(a) = ctl
                .src
                .filter(|a| *a != [0; 4] && dst.family == Family::Inet)
            {
                src = Sockaddr::v4(a, src.port);
            }
            let family = dst.family;
            (src, dst, family)
        };
        if family == Family::Inet {
            return self
                .udp_output4(data, &src, &dst, ctl.tos)
                .map(|()| data.len());
        }
        self.udp_output6(data, src, dst, connected, flow, &ctl)
            .map(|()| data.len())
    }

    /// `udp_append` and `rip_append`: queues a datagram from `src` to
    /// `dst` that came in on interface `ifindex`, if there is room (else
    /// it is dropped, as `udps_fullsock`), with the facts its PCB flags
    /// ask for; whether it was.
    #[allow(clippy::too_many_arguments, reason = "udp_append's")]
    pub(crate) fn dgram_append(
        self: &Arc<Self>,
        src: &Sockaddr,
        dst: &Sockaddr,
        data: &[u8],
        flags: i32,
        orig: bool,
        family: Family,
        ttl: u8,
        tos: u8,
        ifindex: u16,
    ) -> bool {
        let mut wanted = if orig { want::ORIGDSTADDR } else { 0 };
        let v4 = src.family == Family::Inet;
        let bit = |f: i32, w: u32| if flags & f != 0 { w } else { 0 };
        if v4 {
            wanted |= bit(inp::RECVDSTADDR, want::DSTADDR)
                | bit(inp::RECVTTL, want::TTL)
                | bit(inp::RECVTOS, want::TOS)
                | bit(inp::RECVIF, want::IF);
        } else {
            wanted |= bit(inp::IN6P_PKTINFO, want::PKTINFO)
                | bit(inp::IN6P_HOPLIMIT, want::HOPLIMIT)
                | bit(inp::IN6P_TCLASS, want::TCLASS);
        }
        let so = self.so_options();
        // `ip_savecontrol`'s `CHECK_SO_CT`: `SO_TIMESTAMP` in the clock
        // `SO_TS_CLOCK` names.
        if so & SO_TIMESTAMP != 0 {
            wanted |= match self.inner.lock().ts_clock {
                SO_TS_BINTIME => want::BINTIME,
                SO_TS_REALTIME => want::REALTIME,
                SO_TS_MONOTONIC => want::MONOTONIC,
                _ => want::TIMESTAMP,
            };
        }
        if so & SO_BINTIME != 0 {
            wanted |= want::BINTIME;
        }
        // A mapped receiver sees IPv4 peers as `::ffff:a.b.c.d`.
        let (from, to) = if family == Family::Inet6 && v4 {
            (src.mapped(), dst.mapped())
        } else {
            (*src, *dst)
        };
        let meta = Meta {
            want: wanted,
            dst: to,
            ttl,
            tos,
            ifindex,
            stamp: crate::now(),
        };
        let salen: u32 = match from.family {
            Family::Inet => 16,
            Family::Inet6 => 28,
        };
        let cc = salen + u32::try_from(data.len()).unwrap_or(u32::MAX);
        let mb = MSIZE + mbuf_cost(data.len());
        {
            let mut i = self.inner.lock();
            if cc > i.rcv.space() {
                if self.proto == crate::Proto::Udp {
                    crate::state::of(&self.vnet)
                        .udpstat
                        .inc(crate::state::udps::FULLSOCK);
                }
                return false;
            }
            i.rcv.cc += cc;
            i.rcv.mbcnt += mb;
            i.dgrams.push_back(Dgram {
                from,
                data: data.to_vec(),
                meta,
                cc,
                mb,
            });
        }
        self.rd.notify(0);
        true
    }

    /// `soreceive_dgram`, one look: the next datagram, its data to
    /// `max` bytes (`MSG_TRUNC` if cut short).
    ///
    /// # Errors
    ///
    /// With nothing queued: its pending error, else `EWOULDBLOCK`; after
    /// `shutdown` an empty result.
    pub fn recv_dgram(&self, max: usize, peek: bool) -> Result<Received, Errno> {
        let mut i = self.inner.lock();
        if peek {
            let Some(d) = i.dgrams.front() else {
                return empty(&mut i, peek);
            };
            let mut r = cut(
                d.from,
                d.data[..d.data.len().min(max)].to_vec(),
                d.meta,
                max,
            );
            r.full = d.data.len();
            if r.full > max {
                r.flags |= MSG_TRUNC;
            }
            return Ok(r);
        }
        let Some(d) = i.dgrams.pop_front() else {
            return empty(&mut i, peek);
        };
        i.rcv.cc -= d.cc;
        i.rcv.mbcnt -= d.mb;
        drop(i);
        Ok(cut(d.from, d.data, d.meta, max))
    }
}

/// A receive's result with nothing queued.
fn empty(i: &mut crate::Inner, peek: bool) -> Result<Received, Errno> {
    if let Some(e) = i.error {
        if !peek {
            i.error = None;
        }
        return Err(e);
    }
    if i.rcv.cant {
        return Ok(Received::default());
    }
    Err(Errno::EAGAIN)
}

fn cut(from: Sockaddr, mut data: Vec<u8>, meta: Meta, max: usize) -> Received {
    let full = data.len();
    let mut flags = 0;
    if data.len() > max {
        data.truncate(max);
        flags |= MSG_TRUNC;
    }
    Received {
        data,
        from: Some(from),
        flags,
        full,
        meta: Some(meta),
        tls: None,
    }
}

/// The hop limit a send uses: the socket's, or the default
/// (`in6_selecthlim`).
pub(crate) fn hops(h: i16) -> u8 {
    if h < 0 {
        u8::try_from(
            crate::tune::IP6_HLIM
                .load(core::sync::atomic::Ordering::Relaxed)
                .clamp(0, 255),
        )
        .unwrap_or(64)
    } else {
        u8::try_from(h).unwrap_or(64)
    }
}

/// The UDP header's length.
const HDR: usize = 8;

/// `M_HASHTYPE_OPAQUE`.
pub(crate) const HASHTYPE_OPAQUE: u32 = 63;

/// `udp_flowid`: each new UDP socket's flow (`udp_attach`).
static FLOWID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub(crate) fn next_flowid() -> u32 {
    // Relaxed: a counter.
    FLOWID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/// `SO_TS_BINTIME`, `SO_TS_REALTIME`, `SO_TS_MONOTONIC`.
const SO_TS_BINTIME: i32 = 1;
const SO_TS_REALTIME: i32 = 2;
const SO_TS_MONOTONIC: i32 = 3;

impl Socket {
    /// `udp_send`'s IPv4 half: `data` from `src` to `dst` in a datagram,
    /// its checksum's pseudo-header part in the header for whoever
    /// finishes it (`CSUM_UDP`), through `ip_output`.
    fn udp_output4(
        &self,
        data: &[u8],
        src: &Sockaddr,
        dst: &Sockaddr,
        tos: Option<u8>,
    ) -> Result<(), Errno> {
        let st = crate::state::of(&self.vnet);
        let ulen = HDR + data.len();
        let len = u16::try_from(ip::HDR_LEN + ulen).map_err(|_| Errno::EMSGSIZE)?;
        let (s4, d4) = (v4_of(&src.addr), v4_of(&dst.addr));
        let ipo = self.inner.lock().ip;
        let flags = UDB.lock().get(self.id).map_or(0, |p| p.flags);
        let mut m = Packet::for_output(ulen + ip::HDR_LEN).map_err(|_| Errno::ENOBUFS)?;
        m.data.extend_from_slice(data).map_err(|_| Errno::ENOBUFS)?;
        let mut uh = [0u8; HDR];
        uh[0..2].copy_from_slice(&src.port.to_be_bytes());
        uh[2..4].copy_from_slice(&dst.port.to_be_bytes());
        #[allow(clippy::cast_possible_truncation, reason = "under 64 KiB, checked")]
        uh[4..6].copy_from_slice(&(ulen as u16).to_be_bytes());
        if crate::state::on(&st.conf.udp_cksum) {
            let p = cksum::fold(cksum::pseudo4(s4, d4, ip::proto::UDP, ulen));
            uh[6..8].copy_from_slice(&p.to_be_bytes());
            m.hdr.csum_flags = csum::IP_UDP;
            m.hdr.csum_data = 6;
        }
        m.prepend(HDR)
            .map_err(|_| Errno::ENOBUFS)?
            .copy_from_slice(&uh);
        let mut h = Ip::new(ip::proto::UDP, s4, d4, len, ipo.ttl, tos.unwrap_or(ipo.tos));
        if flags & inp::DONTFRAG != 0 {
            h.off = ip::DF;
        }
        m.prepend(ip::HDR_LEN)
            .map_err(|_| Errno::ENOBUFS)?
            .copy_from_slice(&h.bytes());
        let so = self.so_options();
        let mut o = ip::Opts {
            flags: 0,
            mcast: self.mopts(),
            fib: 0,
            pcp: self.pcp(),
            router_alert: false,
            flowid: self.inner.lock().ip.flowid,
        };
        // `udp_send`'s `CALC_FLOWID_OUTBOUND_SENDTO`.
        if ip::hash_outbound(&self.vnet) {
            o.flowid = ip::flow_hash(s4, d4, src.port, dst.port, ip::proto::UDP);
        }
        if so & SO_BROADCAST != 0 {
            o.flags |= ip::out::ALLOWBROADCAST;
        }
        if so & abi::socket::SO_DONTROUTE != 0 {
            o.flags |= ip::out::ROUTETOIF;
        }
        if flags & inp::ONESBCAST != 0 {
            o.flags |= ip::out::SENDONES;
        }
        st.udpstat.inc(udps::OPACKETS);
        ip::output(&self.vnet, m, &o)
    }

    /// Its multicast choices (`inp_moptions`): `IP_MULTICAST_IF`'s
    /// interface, the TTL, and the loopback.
    pub(crate) fn mopts(&self) -> Option<ip::Mopts> {
        let o = self.inner.lock().ip;
        let ifindex = if o.mif == [0; 4] {
            0
        } else {
            self.vnet
                .net
                .lock()
                .ifaddr(Family::Inet, &addr::from_v4(o.mif))
                .map_or(0, |(i, _)| i.index)
        };
        Some(ip::Mopts {
            ifindex,
            ttl: o.mttl,
            loop_: o.mloop != 0,
        })
    }

    /// Its `IP_VLAN_PCP` priority, if set.
    pub(crate) fn pcp(&self) -> Option<u8> {
        u8::try_from(self.inner.lock().ip.pcp).ok()
    }

    /// Whether it joined `group` on interface `ifindex` (`imo_multi_filter`
    /// passing it).
    pub(crate) fn member4(&self, group: [u8; 4], ifindex: u16) -> bool {
        self.inner
            .lock()
            .groups
            .iter()
            .any(|&(g, i)| g == group && i == ifindex)
    }

    /// Whether it joined any group (has `inp_moptions` memberships).
    pub(crate) fn has_groups(&self) -> bool {
        !self.inner.lock().groups.is_empty()
    }
}

/// `udp_input`: a datagram for this host in IPv4 packet `m` (header `ip`),
/// checked, to the socket bound to its destination, to every socket
/// that may hear it if it is broadcast or multicast; a port no one is
/// bound to is answered with ICMP port unreachable.
pub(crate) fn input4(vnet: &Arc<Vnet>, mut m: Packet, ip: &Ip) {
    let st = crate::state::of(vnet);
    st.udpstat.inc(udps::IPACKETS);
    let hlen = usize::from(ip.hl);
    if m.len() < hlen + HDR {
        st.udpstat.inc(udps::HDROPS);
        return;
    }
    let mut uh = [0u8; HDR];
    m.data.copy_out(hlen, &mut uh);
    let sport = u16::from_be_bytes([uh[0], uh[1]]);
    let dport = u16::from_be_bytes([uh[2], uh[3]]);
    let ulen = usize::from(u16::from_be_bytes([uh[4], uh[5]]));
    let sum = u16::from_be_bytes([uh[6], uh[7]]);
    if dport == 0 {
        return;
    }
    let ip_len = usize::from(ip.len) - hlen;
    if ip_len != ulen {
        if ulen > ip_len || ulen < HDR {
            st.udpstat.inc(udps::BADLEN);
            return;
        }
        if m.data.split_off(hlen + ulen).is_err() {
            return;
        }
    }
    if sum != 0 {
        let f = m.hdr.csum_flags;
        let bad = if f & csum::DATA_VALID != 0 {
            let s = if f & csum::PSEUDO_HDR != 0 {
                #[allow(clippy::cast_possible_truncation, reason = "a 16-bit sum")]
                {
                    m.hdr.csum_data as u16
                }
            } else {
                cksum::fold(
                    cksum::pseudo4(ip.src, ip.dst, ip::proto::UDP, ulen)
                        + u64::from(m.hdr.csum_data),
                )
            };
            s ^ 0xffff != 0
        } else if f & csum::IP_UDP != 0 {
            false
        } else {
            let s = cksum::pseudo4(ip.src, ip.dst, ip::proto::UDP, ulen)
                + cksum::sum_chain(&m.data, hlen, ulen);
            !cksum::fold(s) != 0
        };
        if bad {
            st.udpstat.inc(udps::BADSUM);
            return;
        }
    } else {
        st.udpstat.inc(udps::NOSUM);
    }
    let from = Sockaddr::v4(ip.src, sport);
    let to = Sockaddr::v4(ip.dst, dport);
    let rcvif = m.hdr.rcvif;
    let bcast = ip.dst == [0xff; 4] || ip.dst == [0; 4] || {
        let n = vnet.net.lock();
        n.by_index(rcvif).is_some_and(|i| {
            i.addrs.iter().any(|a| {
                a.family == Family::Inet
                    && a.plen < 32
                    && a.broadcast.is_some_and(|b| v4_of(&b) == ip.dst)
            })
        })
    };
    let mut data = alloc::vec![0u8; ulen - HDR];
    m.data.copy_out(hlen + HDR, &mut data);
    if crate::addr::is_multicast4(&ip.dst) || bcast {
        multi_input(vnet, &st, &from, &to, &data, ip, rcvif);
        return;
    }
    let target = UDB.lock().deliver(vnet.id, Family::Inet, &from, &to);
    let Some((so, flags, flags2, family)) = target else {
        if crate::state::on(&st.conf.udp_log_in_vain) {
            crate::log(&alloc::format!(
                "Connection attempt to UDP {}:{} from {}:{}\n",
                net::addr::fmt4(ip.dst),
                dport,
                net::addr::fmt4(ip.src),
                sport
            ));
        }
        st.udpstat.inc(udps::NOPORT);
        if m.hdr.flags & mbuf::flags::MCAST != 0 {
            st.udpstat.inc(udps::NOPORTMCAST);
            return;
        }
        if m.hdr.flags & mbuf::flags::BCAST != 0 {
            st.udpstat.inc(udps::NOPORTBCAST);
            return;
        }
        if crate::state::on(&st.conf.udp_blackhole)
            && (crate::state::on(&st.conf.udp_blackhole_local)
                || !ip::is_local4(&vnet.net.lock(), &ip.src))
        {
            return;
        }
        if !icmp::bandlim(&st, crate::state::bandlim::UNREACH) {
            return;
        }
        icmp::error(
            vnet,
            m,
            icmp::ty::UNREACH,
            icmp::code::UNREACH_PORT,
            [0; 4],
            0,
        );
        return;
    };
    let minttl = so.inner.lock().ip.minttl;
    if minttl != 0 && minttl > ip.ttl {
        return;
    }
    let orig = flags2 & inp::ORIGDSTADDR != 0;
    so.dgram_append(
        &from, &to, &data, flags, orig, family, ip.ttl, ip.tos, rcvif,
    );
}

/// `udp_multi_input`: a broadcast or multicast datagram to each socket
/// bound to its port that may hear it (a multicast one only by members
/// of its group), until one without `SO_REUSEADDR` or `SO_REUSEPORT`.
fn multi_input(
    vnet: &Arc<Vnet>,
    st: &crate::state::State,
    from: &Sockaddr,
    to: &Sockaddr,
    data: &[u8],
    ip: &Ip,
    rcvif: u16,
) {
    let candidates: Vec<(Arc<Socket>, i32, i32, Family)> = {
        let t = UDB.lock();
        t.pcbs
            .values()
            .filter(|p| {
                let (l, f) = (v4_of(&p.laddr), v4_of(&p.faddr));
                p.vnet.id == vnet.id
                    && p.hashed
                    && p.lport == to.port
                    && p.vflag & abi::inet::INP_IPV4 != 0
                    && (l == [0; 4] || l == ip.dst)
                    && (f == [0; 4] || f == ip.src)
                    && (p.fport == 0 || p.fport == from.port)
            })
            .filter_map(|p| p.socket().map(|s| (s, p.flags, p.flags2, p.family)))
            .collect()
    };
    let mut appends = 0;
    for (so, flags, flags2, family) in candidates {
        if crate::addr::is_multicast4(&ip.dst) {
            if !so.has_groups() {
                continue;
            }
            if !so.member4(ip.dst, rcvif) {
                st.ipstat.inc(crate::state::ips::NOTMEMBER);
                continue;
            }
        }
        let orig = flags2 & inp::ORIGDSTADDR != 0;
        if so.dgram_append(from, to, data, flags, orig, family, ip.ttl, ip.tos, rcvif) {
            appends += 1;
        }
        if so.so_options()
            & (abi::socket::SO_REUSEPORT | abi::socket::SO_REUSEPORT_LB | abi::socket::SO_REUSEADDR)
            == 0
        {
            break;
        }
    }
    if appends == 0 {
        st.udpstat.inc(udps::NOPORT);
        if crate::addr::is_multicast4(&ip.dst) {
            st.udpstat.inc(udps::NOPORTMCAST);
        } else {
            st.udpstat.inc(udps::NOPORTBCAST);
        }
    }
}

/// `udp_common_ctlinput`: an ICMP error of type `t`, code `c` about a
/// datagram this host sent (inner header `oip`, its first 8 bytes after
/// in `ports`) is the error of the connected socket that sent it.
pub(crate) fn ctlinput4(vnet: &Arc<Vnet>, t: u8, c: u8, oip: &Ip, ports: &[u8; 8]) {
    let Some(e) = icmp::errmap(t, c) else {
        return;
    };
    let sport = u16::from_be_bytes([ports[0], ports[1]]);
    let dport = u16::from_be_bytes([ports[2], ports[3]]);
    let so = UDB
        .lock()
        .lookup4(vnet.id, oip.dst, dport, oip.src, sport, false)
        .and_then(crate::pcb::Pcb::socket);
    if let Some(so) = so {
        so.inner.lock().error = Some(e);
        so.wake();
    }
}

impl Socket {
    /// `udp6_send`'s output: `data` from `src` (chosen here if the
    /// socket is not bound, or by `IPV6_PKTINFO`) to `dst` in a datagram
    /// whose checksum's pseudo-header part is in the header for whoever
    /// finishes it (`CSUM_UDP_IPV6`), through `ip6_output`, with the
    /// PCB's flow label `flow`.
    fn udp_output6(
        &self,
        data: &[u8],
        src: Sockaddr,
        mut dst: Sockaddr,
        connected: bool,
        flow: u32,
        ctl: &SendCtl,
    ) -> Result<(), Errno> {
        let st = crate::state::of(&self.vnet);
        let ulen = HDR + data.len();
        let plen = u16::try_from(ulen).map_err(|_| Errno::EMSGSIZE)?;
        let req = {
            let mut r = self.srcreq(&dst, Some(ctl));
            // `udp6_send`: a connected socket sends from its address.
            if connected {
                r.pktinfo = None;
            }
            r
        };
        // The table's lock before the interfaces', never under it.
        let pflags = self.pcb_flags();
        let mut n = self.vnet.net.lock();
        if dst.scope != 0 && n.by_index(u16::try_from(dst.scope).unwrap_or(0)).is_none() {
            return Err(Errno::ENXIO);
        }
        let (saddr, ifp) = if connected && !is_unspecified(&src.addr) {
            (src.addr, u16::try_from(src.scope).unwrap_or(0))
        } else {
            crate::in6::selectsrc(&n, &st, &req)?
        };
        if dst.scope == 0 && crate::in6::is_zoned(&dst.addr) {
            dst.scope = u32::from(ifp);
        }
        let hops = self.inner.lock().ip.hops;
        let faddr = connected.then_some((&dst.addr, u16::try_from(dst.scope).unwrap_or(0)));
        let hlim = crate::in6::selecthlim(&n, hops, 0, faddr);
        let mut m = Packet::for_output(ulen + crate::ip6::HDR_LEN).map_err(|_| Errno::ENOBUFS)?;
        m.data.extend_from_slice(data).map_err(|_| Errno::ENOBUFS)?;
        let mut uh = [0u8; HDR];
        uh[0..2].copy_from_slice(&src.port.to_be_bytes());
        uh[2..4].copy_from_slice(&dst.port.to_be_bytes());
        uh[4..6].copy_from_slice(&plen.to_be_bytes());
        let p = cksum::fold(crate::ip6::pseudo(&saddr, &dst.addr, ip::proto::UDP, ulen));
        uh[6..8].copy_from_slice(&p.to_be_bytes());
        m.hdr.csum_flags = csum::IP6_UDP;
        m.hdr.csum_data = 6;
        m.prepend(HDR)
            .map_err(|_| Errno::ENOBUFS)?
            .copy_from_slice(&uh);
        let mut h = crate::ip6::Ip6::new(ip::proto::UDP, saddr, dst.addr, plen, hlim, 0);
        h.flow |= flow & crate::ip6::FLOWLABEL_MASK;
        m.prepend(crate::ip6::HDR_LEN)
            .map_err(|_| Errno::ENOBUFS)?
            .copy_from_slice(&h.bytes());
        let mut o = self.opts6(&dst, Some(ctl), &saddr, ifp, pflags);
        if connected {
            o.ifindex = 0;
        }
        // `udp6_send`'s `CALC_FLOWID_OUTBOUND_SENDTO`.
        if n.hash_outbound {
            o.flowid = flow_hash6(&saddr, &dst.addr, src.port, dst.port, ip::proto::UDP);
        }
        st.udpstat.inc(udps::OPACKETS);
        crate::ip6::output_in(&mut n, &st, m, &o)
    }

    /// Whether it joined IPv6 `group` on interface `ifindex`.
    pub(crate) fn member6(&self, group: &[u8; 16], ifindex: u16) -> bool {
        self.inner
            .lock()
            .groups6
            .iter()
            .any(|(g, i)| g == group && *i == ifindex)
    }

    /// Whether it joined any IPv6 group (has `in6p_moptions` memberships).
    pub(crate) fn has_groups6(&self) -> bool {
        !self.inner.lock().groups6.is_empty()
    }
}

/// `fib6_calc_software_hash`: a flow for a packet of this host's, from
/// its addresses, ports, and protocol (`struct _hash_5tuple_ipv6`).
#[must_use]
pub(crate) fn flow_hash6(src: &[u8; 16], dst: &[u8; 16], sport: u16, dport: u16, proto: u8) -> u32 {
    let mut d = [0u8; 40];
    d[..16].copy_from_slice(src);
    d[16..32].copy_from_slice(dst);
    d[32..34].copy_from_slice(&sport.to_be_bytes());
    d[34..36].copy_from_slice(&dport.to_be_bytes());
    d[36] = proto;
    ip::toeplitz(&ip::MPATH_KEY, &d)
}

/// `udp6_input`: a datagram for this host at `off` of IPv6 packet `m`,
/// checked (its checksum is not optional), to the socket bound to its
/// destination, or to each that may hear a multicast one; a port no one
/// is bound to is answered with ICMPv6 port unreachable.
pub(crate) fn input6(vnet: &Arc<Vnet>, mut m: Packet, off: usize) {
    let st = crate::state::of(vnet);
    st.udpstat.inc(udps::IPACKETS);
    let Ok(ip6) = m.header::<crate::ip6::Ip6>(0) else {
        return;
    };
    if m.len() < off + HDR {
        st.udpstat.inc(udps::HDROPS);
        return;
    }
    let mut uh = [0u8; HDR];
    m.data.copy_out(off, &mut uh);
    let sport = u16::from_be_bytes([uh[0], uh[1]]);
    let dport = u16::from_be_bytes([uh[2], uh[3]]);
    let ulen = usize::from(u16::from_be_bytes([uh[4], uh[5]]));
    let sum = u16::from_be_bytes([uh[6], uh[7]]);
    let plen = usize::from(ip6.plen) + crate::ip6::HDR_LEN - off;
    if plen != ulen {
        st.udpstat.inc(udps::BADLEN);
        return;
    }
    if sum == 0 {
        st.udpstat.inc(udps::NOSUM);
        return;
    }
    let f = m.hdr.csum_flags;
    let bad = if f & csum::DATA_VALID != 0 {
        let s = if f & csum::PSEUDO_HDR != 0 {
            #[allow(clippy::cast_possible_truncation, reason = "a 16-bit sum")]
            {
                m.hdr.csum_data as u16
            }
        } else {
            cksum::fold(
                crate::ip6::pseudo(&ip6.src, &ip6.dst, ip::proto::UDP, ulen)
                    + u64::from(m.hdr.csum_data),
            )
        };
        s ^ 0xffff != 0
    } else {
        crate::ip6::cksum6(&m, ip::proto::UDP, off, ulen) != 0
    };
    if bad {
        st.udpstat.inc(udps::BADSUM);
        return;
    }
    let rcvif = m.hdr.rcvif;
    let zone = |a: &[u8; 16]| {
        if crate::in6::is_zoned(a) {
            u32::from(rcvif)
        } else {
            0
        }
    };
    let from = Sockaddr::v6(ip6.src, sport, zone(&ip6.src));
    let to = Sockaddr::v6(ip6.dst, dport, zone(&ip6.dst));
    let mut data = alloc::vec![0u8; ulen - HDR];
    m.data.copy_out(off + HDR, &mut data);
    if net::addr::is_multicast6(&ip6.dst) {
        multi_input6(vnet, &st, &from, &to, &data, &ip6, rcvif);
        return;
    }
    let target = UDB.lock().deliver(vnet.id, Family::Inet6, &from, &to);
    let Some((so, flags, flags2, family)) = target else {
        st.udpstat.inc(udps::NOPORT);
        if m.hdr.flags & mbuf::flags::MCAST != 0 {
            st.udpstat.inc(udps::NOPORTMCAST);
            return;
        }
        if crate::state::on(&st.conf.udp_blackhole)
            && (crate::state::on(&st.conf.udp_blackhole_local)
                || !crate::in6::is_local(&vnet.net.lock(), &ip6.src))
        {
            return;
        }
        crate::icmp6::error(
            vnet,
            m,
            crate::icmp6::ty::DST_UNREACH,
            crate::icmp6::code::DST_UNREACH_NOPORT,
            0,
        );
        return;
    };
    let orig = flags2 & inp::ORIGDSTADDR != 0;
    so.dgram_append(
        &from,
        &to,
        &data,
        flags,
        orig,
        family,
        ip6.hlim,
        ip6.tclass(),
        rcvif,
    );
}

/// `udp6_multi_input`: a multicast datagram to each socket bound to its
/// port that joined its group (or has no memberships), until one without
/// `SO_REUSEADDR` or `SO_REUSEPORT`.
fn multi_input6(
    vnet: &Arc<Vnet>,
    st: &crate::state::State,
    from: &Sockaddr,
    to: &Sockaddr,
    data: &[u8],
    ip6: &crate::ip6::Ip6,
    rcvif: u16,
) {
    let candidates: Vec<(Arc<Socket>, i32, i32, Family)> = {
        let t = UDB.lock();
        t.pcbs
            .values()
            .filter(|p| {
                p.vnet.id == vnet.id
                    && p.lport == to.port
                    && p.vflag & abi::inet::INP_IPV6 != 0
                    && (is_unspecified(&p.laddr) || p.laddr == ip6.dst)
                    && (is_unspecified(&p.faddr) || (p.faddr == ip6.src && p.fport == from.port))
            })
            .filter_map(|p| p.socket().map(|s| (s, p.flags, p.flags2, p.family)))
            .collect()
    };
    let mut appends = 0;
    for (so, flags, flags2, family) in candidates {
        if so.has_groups6() && !so.member6(&ip6.dst, rcvif) {
            st.ip6stat.inc(crate::state::ip6s::NOTMEMBER);
            continue;
        }
        let orig = flags2 & inp::ORIGDSTADDR != 0;
        if so.dgram_append(
            from,
            to,
            data,
            flags,
            orig,
            family,
            ip6.hlim,
            ip6.tclass(),
            rcvif,
        ) {
            appends += 1;
        }
        if so.so_options() & (abi::socket::SO_REUSEPORT | abi::socket::SO_REUSEADDR) == 0 {
            break;
        }
    }
    if appends == 0 {
        st.udpstat.inc(udps::NOPORT);
        st.udpstat.inc(udps::NOPORTMCAST);
    }
}

/// `udp6_common_ctlinput`: an ICMPv6 error about a datagram this host
/// sent (its UDP header at `ctl.off` of `m`) is the error of the
/// connected socket that sent it.
pub(crate) fn ctlinput6(vnet: &Arc<Vnet>, m: &Packet, ctl: &crate::icmp6::Ctl) {
    let Some(e) = crate::icmp6::errmap(ctl.t, ctl.c) else {
        return;
    };
    let mut ports = [0u8; 4];
    if m.data.copy_out(ctl.off, &mut ports) != 4 {
        return;
    }
    let sport = u16::from_be_bytes([ports[0], ports[1]]);
    let dport = u16::from_be_bytes([ports[2], ports[3]]);
    // `in6_pcbnotify`: the sockets connected to that destination port
    // from that source.
    let targets: Vec<Arc<Socket>> = UDB
        .lock()
        .pcbs
        .values()
        .filter(|p| {
            p.vnet.id == vnet.id
                && p.vflag & abi::inet::INP_IPV6 != 0
                && p.faddr == ctl.dst
                && p.fport == dport
                && p.lport == sport
                && (is_unspecified(&ctl.src) || p.laddr == ctl.src)
        })
        .filter_map(crate::pcb::Pcb::socket)
        .collect();
    for so in targets {
        so.inner.lock().error = Some(e);
        so.wake();
    }
}
