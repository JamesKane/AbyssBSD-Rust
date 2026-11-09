// SPDX-License-Identifier: BSD-2-Clause
//! Raw IP sockets (`SOCK_RAW` in `AF_INET`): every datagram of a socket's
//! protocol for this host, its IP header included, and datagrams sent
//! with the header the kernel makes or (`IP_HDRINCL`) the program's.
//! `ping`, `traceroute`, and `dhclient` use them.
//!
//! Behavior reference: sys/netinet/raw_ip.c (`rip_attach`, `rip_input`,
//! `rip_append`, `rip_send`, `rip_bind`, `rip_connect`, `rip_shutdown`,
//! `rip_ctloutput`'s `IP_HDRINCL`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::{inp, ss};
use abi::socket::{SHUT_RD, SHUT_RDWR, SHUT_WR, SO_DONTROUTE};
use mbuf::Packet;
use net::addr::{from_v4, is_multicast4, v4_of};
use net::{Family, Vnet};

use crate::addr::Sockaddr;
use crate::ip::{self, HDR_LEN, Ip, MAXPACKET, out};
use crate::pcb::RIPCB;
use crate::state::{self, ips};
use crate::{Cred, Proto, Socket};

/// `net.inet.raw.maxdgram`, `net.inet.raw.recvspace`.
pub static SENDSPACE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(9216);
pub static RECVSPACE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(9216);

/// `rip_input`: copies of IPv4 packet `m` (header `ip`) to the raw
/// sockets of its protocol whose addresses match, those bound and
/// connected to its exact addresses first. A packet of a protocol no
/// one handles (`known` false) that no socket took is answered with an
/// ICMP protocol unreachable.
pub(crate) fn input4(vnet: &Arc<Vnet>, m: Packet, ip: &Ip, known: bool) {
    let st = state::of(vnet);
    let targets: Vec<(Arc<Socket>, i32)> = {
        let t = RIPCB.lock();
        let matches = |exact: bool| {
            t.values()
                .filter(move |p| {
                    let (l, f) = (v4_of(&p.laddr), v4_of(&p.faddr));
                    p.vnet.id == vnet.id
                        && p.vflag & abi::inet::INP_IPV4 != 0
                        && if exact {
                            p.ip_p == ip.p && l == ip.dst && f == ip.src
                        } else {
                            !(p.ip_p == ip.p && l == ip.dst && f == ip.src)
                                && (p.ip_p == 0 || p.ip_p == ip.p)
                                && (l == [0; 4] || l == ip.dst)
                                && (f == [0; 4] || f == ip.src)
                        }
                })
                .filter_map(|p| p.socket().map(|s| (s, p.flags)))
        };
        matches(true).chain(matches(false)).collect()
    };
    let mut appended = 0;
    if !targets.is_empty() {
        let mut data = alloc::vec![0u8; m.len()];
        m.data.copy_out(0, &mut data);
        let from = Sockaddr::v4(ip.src, 0);
        let to = Sockaddr::v4(ip.dst, 0);
        for (so, flags) in targets {
            let minttl = so.inner.lock().ip.minttl;
            if minttl != 0 && minttl > ip.ttl {
                continue;
            }
            // A socket with memberships hears only its groups' traffic,
            // IGMP excepted.
            if is_multicast4(&ip.dst)
                && ip.p != ip::proto::IGMP
                && !so.member4(ip.dst, m.hdr.rcvif)
                && so.has_groups()
            {
                st.ipstat.inc(ips::NOTMEMBER);
                continue;
            }
            if so.dgram_append(
                &from,
                &to,
                &data,
                flags,
                false,
                Family::Inet,
                ip.ttl,
                ip.tos,
                m.hdr.rcvif,
            ) {
                appended += 1;
            }
        }
    }
    if appended == 0 && !known {
        st.ipstat.inc(ips::NOPROTO);
        st.ipstat.dec(ips::DELIVERED);
        crate::icmp::error(
            vnet,
            m,
            crate::icmp::ty::UNREACH,
            crate::icmp::code::UNREACH_PROTOCOL,
            [0; 4],
            0,
        );
    }
}

/// Whether the structure of options `opts` is RFC 791's.
fn options_ok(opts: &[u8]) -> bool {
    let mut at = 0;
    while at < opts.len() {
        match opts[at] {
            0 => break,
            1 => at += 1,
            _ => {
                if opts.len() - at < 2 {
                    return false;
                }
                let len = usize::from(opts[at + 1]);
                if len < 2 || len > opts.len() - at {
                    return false;
                }
                at += len;
            }
        }
    }
    true
}

impl Socket {
    /// `rip_bind`: to the address in `raw`, this host's or the wildcard.
    pub(crate) fn raw_bind(&self, raw: &[u8]) -> Result<(), Errno> {
        if raw.get(1).copied() != Some(abi::inet::AF_INET) {
            return Err(Errno::EAFNOSUPPORT);
        }
        if raw.len() != 16 || raw[0] != 16 {
            return Err(Errno::EINVAL);
        }
        let a: [u8; 4] = raw[4..8].try_into().map_err(|_| Errno::EINVAL)?;
        let mut t = RIPCB.lock();
        let mut p = t.get_mut(self.id).ok_or(Errno::EINVAL)?;
        if a != [0; 4]
            && p.flags & inp::BINDANY == 0
            && !self.vnet.net.lock().is_local(Family::Inet, &from_v4(a), 0)
        {
            return Err(Errno::EADDRNOTAVAIL);
        }
        p.laddr = from_v4(a);
        Ok(())
    }

    /// `rip_connect`: datagrams go to and come only from the address in
    /// `raw`.
    pub(crate) fn raw_connect(&self, raw: &[u8]) -> Result<(), Errno> {
        if raw.len() != 16 || raw[0] != 16 {
            return Err(Errno::EINVAL);
        }
        if raw[1] != abi::inet::AF_INET && raw[1] != abi::inet::AF_IMPLINK {
            return Err(Errno::EAFNOSUPPORT);
        }
        let a: [u8; 4] = raw[4..8].try_into().map_err(|_| Errno::EINVAL)?;
        RIPCB.lock().get_mut(self.id).ok_or(Errno::EINVAL)?.faddr = from_v4(a);
        let mut i = self.inner.lock();
        i.state = (i.state | ss::ISCONNECTED) & !(ss::ISCONNECTING | ss::ISDISCONNECTING);
        drop(i);
        self.wake();
        Ok(())
    }

    /// `rip_shutdown`.
    pub(crate) fn raw_shutdown(&self, how: i32) -> Result<(), Errno> {
        let mut i = self.inner.lock();
        if i.state & ss::ISCONNECTED == 0 {
            return Err(Errno::ENOTCONN);
        }
        if how == SHUT_RD || how == SHUT_RDWR {
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
        Ok(())
    }

    /// `rip_detach`.
    pub(crate) fn raw_close(&self) {
        RIPCB.lock().free(self.id);
        let mut i = self.inner.lock();
        i.dgrams.clear();
        i.rcv.cc = 0;
        i.rcv.mbcnt = 0;
        i.state &= !ss::ISCONNECTED;
    }

    /// `sosend_dgram` and `rip_send`: one datagram to the connected
    /// address or `to`, behind the header the kernel makes, or (with
    /// `IP_HDRINCL`) as the program wrote it.
    pub(crate) fn raw_send(
        &self,
        data: &[u8],
        to: Option<&[u8]>,
        _cred: &Cred,
    ) -> Result<usize, Errno> {
        let Proto::Raw(p) = self.proto else {
            return Err(Errno::EOPNOTSUPP);
        };
        let (connected, ip_opts) = {
            let mut i = self.inner.lock();
            if i.snd.cant {
                return Err(Errno::EPIPE);
            }
            if let Some(e) = i.error.take() {
                return Err(e);
            }
            let connected = i.state & ss::ISCONNECTED != 0;
            if !connected && to.is_none() {
                return Err(Errno::EDESTADDRREQ);
            }
            if data.len() > i.snd.space() as usize {
                return Err(Errno::EMSGSIZE);
            }
            (connected, i.ip)
        };
        let (flags, laddr, faddr) = {
            let t = RIPCB.lock();
            let pcb = t.get(self.id).ok_or(Errno::EINVAL)?;
            (pcb.flags, v4_of(&pcb.laddr), v4_of(&pcb.faddr))
        };
        let dst = match (connected, to) {
            (true, Some(_)) => return Err(Errno::EISCONN),
            (true, None) => faddr,
            (false, Some(raw)) => {
                if raw.get(1).copied() != Some(abi::inet::AF_INET) {
                    return Err(Errno::EAFNOSUPPORT);
                }
                if raw.len() != 16 || raw[0] != 16 {
                    return Err(Errno::EINVAL);
                }
                raw[4..8].try_into().map_err(|_| Errno::EINVAL)?
            }
            (false, None) => return Err(Errno::ENOTCONN),
        };
        let mut o = ip::Opts {
            flags: out::ALLOWBROADCAST,
            mcast: self.mopts(),
            fib: 0,
            pcp: self.pcp(),
            router_alert: false,
            flowid: ip_opts.flowid,
        };
        let hashed = ip::hash_outbound(&self.vnet);
        if self.so_options() & SO_DONTROUTE != 0 {
            o.flags |= out::ROUTETOIF;
        }
        if flags & inp::ONESBCAST != 0 {
            o.flags |= out::SENDONES;
        }
        let st = state::of(&self.vnet);
        let m = if flags & inp::HDRINCL == 0 {
            if data.len() + HDR_LEN > MAXPACKET {
                return Err(Errno::EMSGSIZE);
            }
            let len = u16::try_from(data.len() + HDR_LEN).map_err(|_| Errno::EMSGSIZE)?;
            if hashed {
                o.flowid = ip::flow_hash(laddr, dst, 0, 0, p);
            }
            let mut h = Ip::new(p, laddr, dst, len, ip_opts.ttl, ip_opts.tos);
            if flags & inp::DONTFRAG != 0 {
                h.off = ip::DF;
            }
            let mut m = Packet::for_output(data.len()).map_err(|_| Errno::ENOBUFS)?;
            m.data.extend_from_slice(data).map_err(|_| Errno::ENOBUFS)?;
            m.prepend(HDR_LEN)
                .map_err(|_| Errno::ENOBUFS)?
                .copy_from_slice(&h.bytes());
            m
        } else {
            if data.len() > MAXPACKET {
                return Err(Errno::EMSGSIZE);
            }
            if data.len() < HDR_LEN {
                return Err(Errno::EINVAL);
            }
            let mut h = <Ip as mbuf::Header>::parse(&data[..HDR_LEN]).ok_or(Errno::EINVAL)?;
            let hlen = usize::from(h.hl);
            if hlen < HDR_LEN || hlen > data.len() || usize::from(h.len) != data.len() {
                return Err(Errno::EINVAL);
            }
            if !options_ok(&data[HDR_LEN..hlen]) {
                return Err(Errno::EINVAL);
            }
            if hashed {
                o.flowid = ip::flow_hash(h.dst, h.src, 0, 0, h.p);
            }
            let mut d = data.to_vec();
            // No program can ask for identification 0; it gets one.
            if h.id == 0 {
                ip::fill_id(st, &mut h);
                d[4..6].copy_from_slice(&h.id.to_be_bytes());
            }
            o.flags |= out::RAWOUTPUT;
            st.ipstat.inc(ips::RAWOUT);
            let mut m = Packet::for_output(d.len()).map_err(|_| Errno::ENOBUFS)?;
            m.data.extend_from_slice(&d).map_err(|_| Errno::ENOBUFS)?;
            m
        };
        ip::output(&self.vnet, m, &o)?;
        Ok(data.len())
    }
}
