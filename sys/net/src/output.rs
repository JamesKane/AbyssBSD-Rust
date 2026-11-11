// SPDX-License-Identifier: BSD-2-Clause
//! An interface's output for a network-layer packet (`if_output`):
//! `ether_output` on an Ethernet interface, which finds the next hop's
//! link-layer address and puts the header on, and `looutput` on a
//! loopback one. Called under the interfaces' lock; the frame leaves when
//! it is let go.
//!
//! Behavior reference: sys/net/if_ethersubr.c (`ether_output`,
//! `ether_resolve_addr`), sys/net/if_loop.c (`looutput`),
//! sys/net/if.c (`if_simloop`).

use abi::errno::Errno;
use abi::net::{IFT_LOOP, IFT_PPP, iff};
use mbuf::{Packet, csum, flags};

use crate::Family;
use crate::addr::{In6, v4_of};
use crate::arp::{self, Resolved};
use crate::ether::{self, Header};
use crate::ifnet::Net;
use crate::netisr::Proto;

/// Sends packet `m` of `family` out of interface `ifindex` towards
/// `nexthop` (a gateway if `is_gw`, else the destination); `now` is the
/// uptime clock ARP's entries run on. A packet waiting for ARP is held
/// and counts as sent.
///
/// # Errors
///
/// `ENETDOWN` if the interface is not up and running, `ENXIO` if it is
/// gone, ARP's errors (`EHOSTDOWN`, `EHOSTUNREACH`, ...), `EAFNOSUPPORT`
/// for an interface that carries neither.
pub fn output(
    n: &mut Net,
    ifindex: u16,
    family: Family,
    nexthop: &In6,
    is_gw: bool,
    m: Packet,
    now: u64,
) -> Result<(), Errno> {
    let ifp = n.by_index(ifindex).ok_or(Errno::ENXIO)?;
    if ifp.flags & (iff::UP | iff::RUNNING) != iff::UP | iff::RUNNING {
        return Err(Errno::ENETDOWN);
    }
    let (ty, ifflags, lladdr) = (ifp.ty, ifp.flags, ifp.lladdr);
    match ty {
        IFT_LOOP => {
            n.txq.push((ifindex, Some(family), m));
            Ok(())
        }
        // A point-to-point link with no link layer (`tunoutput`): the
        // packet as it is, with its family.
        IFT_PPP => {
            if let Some(l) = &ifp.link.0 {
                l.ready()?;
            }
            n.txq.push((ifindex, Some(family), m));
            Ok(())
        }
        t if ether::is_ether(t) => {
            let src = lladdr.ok_or(Errno::EAFNOSUPPORT)?;
            let mut slot = Some(m);
            let dst = match family {
                Family::Inet => {
                    match arp::resolve(n, ifindex, is_gw, &mut slot, v4_of(nexthop), now)? {
                        Resolved::Addr(a) => a,
                        // `ether_output` turns `EWOULDBLOCK` into success.
                        Resolved::Held => return Ok(()),
                    }
                }
                Family::Inet6 => match resolve6(n, ifindex, nexthop, &mut slot)? {
                    Resolved::Addr(a) => a,
                    Resolved::Held => return Ok(()),
                },
            };
            let Some(mut m) = slot else {
                return Ok(());
            };
            let ty = match family {
                Family::Inet => ether::ty::IP,
                Family::Inet6 => ether::ty::IPV6,
            };
            // A broadcast on a simplex interface does not come back from
            // the wire: a copy goes up as if it had (`if_simloop`).
            if m.hdr.flags & flags::BCAST != 0
                && ifflags & iff::SIMPLEX != 0
                && let Ok(mut c) = m.copy()
            {
                looped_csum(&mut c);
                c.hdr.rcvif = ifindex;
                let proto = match family {
                    Family::Inet => Proto::Ip,
                    Family::Inet6 => {
                        c.hdr.flags |= flags::LOOP;
                        Proto::Ipv6
                    }
                };
                n.isrq.push((proto, c));
            }
            ether::encap(&mut m, &Header { dst, src, ty })?;
            n.txq.push((ifindex, None, m));
            Ok(())
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

/// An IPv6 next hop's link-layer address (`nd6_resolve`): a multicast
/// group's mapped address, else what neighbor discovery finds, which may
/// hold the packet while it solicits the neighbor.
fn resolve6(
    n: &mut Net,
    ifindex: u16,
    nexthop: &In6,
    m: &mut Option<Packet>,
) -> Result<Resolved, Errno> {
    if n.by_index(ifindex)
        .is_some_and(|i| i.nd6_flags & abi::net::nd6_iff::IFDISABLED != 0)
    {
        return Err(Errno::ENETDOWN);
    }
    if m.as_ref().is_some_and(|m| m.hdr.flags & flags::MCAST != 0) || nexthop[0] == 0xff {
        // `ETHER_MAP_IPV6_MULTICAST`.
        return Ok(Resolved::Addr([
            0x33,
            0x33,
            nexthop[12],
            nexthop[13],
            nexthop[14],
            nexthop[15],
        ]));
    }
    match crate::in6_hooks() {
        Some(h) => (h.resolve)(n, ifindex, nexthop, m),
        None => n
            .lle(ifindex, Family::Inet6, nexthop)
            .filter(|e| e.flags & crate::lltable::lle::VALID != 0)
            .map(|e| Resolved::Addr(e.lladdr))
            .ok_or(Errno::EHOSTUNREACH),
    }
}

/// `update_mbuf_csumflags`: what a looped copy's sender left for the
/// device to do is taken as done.
fn looped_csum(m: &mut Packet) {
    if m.hdr.csum_flags & (csum::IP_UDP | csum::IP_TCP) != 0 {
        m.hdr.csum_flags |= csum::DATA_VALID | csum::PSEUDO_HDR;
        m.hdr.csum_data = 0xffff;
    }
    if m.hdr.csum_flags & csum::IP != 0 {
        m.hdr.csum_flags |= csum::IP_CHECKED | csum::IP_VALID;
    }
}
