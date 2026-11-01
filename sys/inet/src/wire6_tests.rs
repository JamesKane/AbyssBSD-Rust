// SPDX-License-Identifier: BSD-2-Clause
//! Host tests of IPv6 on the wire: hosts in VNETs joined by an `epair`,
//! with link-local addresses and DAD, neighbor discovery, UDP, raw
//! ICMPv6 sockets, fragments, errors, and SLAAC from a router
//! advertisement.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use net::addr::{In6, sockaddr_in6};
use net::clone::{self, Ctx};
use net::ifnet::{Ifaddr, ifa6_update, in6_iff};
use net::lltable::{lle, state as lls};
use net::{Family, Vnet};

use crate::state::{self, icp6s, ip6s, udps};
use crate::{Cred, Proto, Socket};

const SEC: u64 = 1_000_000_000;

fn ctx() -> Ctx {
    Ctx {
        now: 0,
        hostuuid: "6b0c3f6e-1111-2222-3333-444455556666".into(),
        jail: "0".into(),
        random: [9; 12],
    }
}

fn index(v: &Vnet, name: &str) -> u16 {
    v.net.lock().by_name(name.as_bytes()).unwrap().index
}

fn a6(s: &[u16; 8]) -> In6 {
    let mut a = [0u8; 16];
    for (i, w) in s.iter().enumerate() {
        a[i * 2..i * 2 + 2].copy_from_slice(&w.to_be_bytes());
    }
    a
}

/// `ifconfig <if> inet6 -ifdisabled up`: IPv6 on, the interface up.
fn enable6(v: &Vnet, name: &str) {
    let mut n = v.net.lock();
    let i = n.by_name(name.as_bytes()).unwrap().index;
    n.with_iface(name.as_bytes(), |i| {
        i.nd6_flags &= !abi::net::nd6_iff::IFDISABLED;
    });
    let flags = n.by_index(i).unwrap().flags | abi::net::iff::UP;
    n.set_flags(name.as_bytes(), flags).unwrap();
    n.in6_if_up(i);
}

/// `ifconfig <if> inet6 <a>/64`.
fn addr6(v: &Vnet, name: &str, a: In6) {
    let x = Ifaddr {
        family: Family::Inet6,
        addr: a,
        plen: 64,
        dst: None,
        broadcast: None,
        flags6: 0,
        vltime: u32::MAX,
        pltime: u32::MAX,
        vhid: 0,
        created: 0,
        updated: 0,
    };
    v.net
        .lock()
        .add_addr_flags(name.as_bytes(), x, ifa6_update::AIFADDR)
        .unwrap();
}

/// An address's flags.
fn flags6(v: &Vnet, name: &str, a: &In6) -> i32 {
    let n = v.net.lock();
    let i = n.by_name(name.as_bytes()).unwrap();
    i.addrs
        .iter()
        .find(|x| x.family == Family::Inet6 && x.addr == *a)
        .map_or(-1, |x| x.flags6)
}

/// The interface's link-local address.
fn linklocal(v: &Vnet, name: &str) -> In6 {
    let n = v.net.lock();
    let i = n.by_name(name.as_bytes()).unwrap();
    crate::in6::linklocal_of(i, 0).unwrap()
}

/// Two IPv6 hosts on an epair, past DAD, with `2001:db8::a` and
/// `2001:db8::b`; the ends' names.
fn pair6(
    a: u16,
    b: u16,
) -> (
    Arc<Vnet>,
    Arc<Vnet>,
    alloc::string::String,
    alloc::string::String,
) {
    let h1 = Vnet::new(0);
    let h2 = Vnet::new(0);
    let name = clone::create(&h1, b"epair", &ctx()).unwrap();
    let base = alloc::string::String::from(name.trim_end_matches('a'));
    let (na, nb) = (alloc::format!("{base}a"), alloc::format!("{base}b"));
    clone::vmove(&h1, nb.as_bytes(), &h2).unwrap();
    enable6(&h1, &na);
    enable6(&h2, &nb);
    addr6(&h1, &na, a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, a]));
    addr6(&h2, &nb, a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, b]));
    // DAD's delay and its one solicitation.
    crate::advance(3 * SEC);
    (h1, h2, na, nb)
}

fn udp6(v: &Arc<Vnet>) -> Arc<Socket> {
    Socket::new_in(v, Family::Inet6, Proto::Udp, &Cred::root())
}

fn raw6(v: &Arc<Vnet>, p: u8) -> Arc<Socket> {
    Socket::new_in(v, Family::Inet6, Proto::Raw(p), &Cred::root())
}

#[test]
fn link_local_addresses_pass_dad() {
    let _g = crate::tests::setup();
    let (h1, h2, na, nb) = pair6(1, 2);
    let ll1 = linklocal(&h1, &na);
    let ll2 = linklocal(&h2, &nb);
    // EUI-64 from the MAC, the universal bit flipped.
    assert_eq!(&ll1[..8], &[0xfe, 0x80, 0, 0, 0, 0, 0, 0]);
    assert_eq!(ll1[11..13], [0xff, 0xfe]);
    assert_ne!(ll1, ll2);
    for (v, n, a) in [
        (&h1, &na, ll1),
        (&h2, &nb, ll2),
        (&h1, &na, a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, 1])),
    ] {
        assert_eq!(flags6(v, n, &a) & in6_iff::NOTREADY, 0, "{a:02x?} is ready");
    }
    // Each heard the other's DAD solicitations, and joined its groups.
    let st = state::of(&h1);
    assert!(st.icmp6stat.get(icp6s::OUTHIST + 135) >= 2);
    assert!(crate::mld6::member(
        &st,
        index(&h1, &na),
        &crate::in6::solicited_node(&ll1)
    ));
    assert!(crate::mld6::member(
        &st,
        index(&h1, &na),
        &crate::in6::ALLNODES
    ));
    // The link-local prefix is on the list.
    assert!(
        st.nd6
            .lock()
            .prefixes
            .iter()
            .any(|p| p.plen == 64 && p.prefix[0] == 0xfe)
    );
}

#[test]
fn a_duplicate_address_is_found() {
    let _g = crate::tests::setup();
    let (h1, h2, na, nb) = pair6(1, 2);
    let same = a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x77]);
    addr6(&h1, &na, same);
    crate::advance(3 * SEC);
    assert_eq!(flags6(&h1, &na, &same) & in6_iff::NOTREADY, 0);
    addr6(&h2, &nb, same);
    crate::advance(3 * SEC);
    assert_ne!(flags6(&h2, &nb, &same) & in6_iff::DUPLICATED, 0);
    assert_eq!(flags6(&h1, &na, &same) & in6_iff::DUPLICATED, 0);
}

#[test]
fn udp_over_ipv6_resolves_its_neighbor() {
    let _g = crate::tests::setup();
    let (h1, h2, na, nb) = pair6(1, 2);
    let r = udp6(&h2);
    r.bind(&sockaddr_in6(&[0; 16], 32001, 0), &Cred::root())
        .unwrap();
    let s = udp6(&h1);
    let dst = a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, 2]);
    assert_eq!(
        s.send_dgram(
            b"hello6",
            Some(&sockaddr_in6(&dst, 32001, 0)),
            &Cred::root()
        ),
        Ok(6)
    );
    let got = r.recv_dgram(100, false).unwrap();
    assert_eq!(got.data, b"hello6");
    let from = got.from.unwrap();
    assert_eq!(from.addr, a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, 1]));
    assert_eq!(got.meta.unwrap().ttl, 64);
    // The neighbor answered the solicitation: reachable.
    let (i1, i2) = (index(&h1, &na), index(&h2, &nb));
    let e = h1
        .net
        .lock()
        .lle(i1, Family::Inet6, &dst)
        .map(|e| (e.state, e.flags & lle::VALID != 0));
    assert_eq!(e, Some((lls::ND6_REACHABLE, true)));
    // And the receiver learned the sender from the solicitation.
    let e2 = h2
        .net
        .lock()
        .lle(i2, Family::Inet6, &from.addr)
        .map(|e| e.state);
    assert_eq!(e2, Some(lls::ND6_STALE));
    assert_eq!(state::of(&h2).udpstat.get(udps::IPACKETS), 1);
    // A connected socket's datagram to a closed port: port unreachable,
    // and its next call fails with it.
    let c = udp6(&h1);
    c.connect(&sockaddr_in6(&dst, 32002, 0), &Cred::root(), false)
        .unwrap();
    // `IN6P_AUTOFLOWLABEL` (on by default): connecting chose a label.
    let flow = crate::pcb::UDB.lock().get(c.id).map(|p| p.flow);
    assert!(
        flow.is_some_and(|f| f != 0 && f & !0xf_ffff == 0),
        "{flow:?}"
    );
    c.send_dgram(b"x", None, &Cred::root()).unwrap();
    assert_eq!(c.recv_dgram(10, false).unwrap_err(), Errno::ECONNREFUSED);
    assert_eq!(state::of(&h2).icmp6stat.get(icp6s::ODST_UNREACH_NOPORT), 1);
    for x in [r, s, c] {
        x.close();
    }
}

/// An ICMPv6 echo request with identifier `id` and sequence `seq` (the
/// kernel computes its checksum).
fn echo6(id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = alloc::vec![128, 0, 0, 0];
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(payload);
    b
}

#[test]
fn ping6_over_a_raw_icmpv6_socket() {
    let _g = crate::tests::setup();
    let (h1, h2, na, nb) = pair6(1, 2);
    let p = raw6(&h1, 58);
    let dst = a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, 2]);
    p.send_dgram(
        &echo6(0x4242, 1, b"ping6!"),
        Some(&sockaddr_in6(&dst, 0, 0)),
        &Cred::root(),
    )
    .unwrap();
    // Neighbor discovery's messages come too, unfiltered: find the reply.
    let mut reply = None;
    while let Ok(got) = p.recv_dgram(200, false) {
        if got.data[0] == 129 {
            reply = Some(got);
        }
    }
    let got = reply.expect("an echo reply");
    assert_eq!(&got.data[4..8], &[0x42, 0x42, 0, 1]);
    assert_eq!(&got.data[8..], b"ping6!");
    assert_eq!(got.from.unwrap().addr, dst);
    // To the link-local address, its zone named.
    let ll2 = linklocal(&h2, &nb);
    let zone = u32::from(index(&h1, &na));
    p.send_dgram(
        &echo6(0x4242, 2, b"ll"),
        Some(&sockaddr_in6(&ll2, 0, zone)),
        &Cred::root(),
    )
    .unwrap();
    let mut seq2 = false;
    while let Ok(got) = p.recv_dgram(200, false) {
        if got.data[0] == 129 && got.data[7] == 2 {
            seq2 = true;
            assert_eq!(got.from.unwrap().scope, zone);
        }
    }
    assert!(seq2);
    // A filter that blocks echo replies.
    let mut filt = [0xffu8; 32];
    filt[129 / 8] &= !(1 << (129 % 8));
    p.setopt(58, crate::raw6::ICMP6_FILTER, &filt, &Cred::root())
        .unwrap();
    p.send_dgram(
        &echo6(0x4242, 3, b"x"),
        Some(&sockaddr_in6(&dst, 0, 0)),
        &Cred::root(),
    )
    .unwrap();
    while let Ok(got) = p.recv_dgram(200, false) {
        assert_ne!(got.data[0], 129);
    }
    assert!(state::of(&h2).icmp6stat.get(icp6s::REFLECT) >= 3);
    p.close();
}

#[test]
fn large_ipv6_datagrams_fragment_and_reassemble() {
    let _g = crate::tests::setup();
    let (h1, h2, _, _) = pair6(1, 2);
    let r = udp6(&h2);
    r.bind(&sockaddr_in6(&[0; 16], 32003, 0), &Cred::root())
        .unwrap();
    let s = udp6(&h1);
    let big: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
    let dst = a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, 2]);
    s.send_dgram(&big, Some(&sockaddr_in6(&dst, 32003, 0)), &Cred::root())
        .unwrap();
    let got = r.recv_dgram(5000, false).unwrap();
    assert_eq!(got.data, big);
    let st1 = state::of(&h1);
    let st2 = state::of(&h2);
    assert_eq!(st1.ip6stat.get(ip6s::FRAGMENTED), 1);
    assert_eq!(st1.ip6stat.get(ip6s::OFRAGMENTS), 3);
    assert_eq!(st2.ip6stat.get(ip6s::FRAGMENTS), 3);
    assert_eq!(st2.ip6stat.get(ip6s::REASSEMBLED), 1);
    r.close();
    s.close();
}

/// A router advertisement from `src` (hop limit 255 to all nodes),
/// with prefix `2001:db8:1::/64` valid `vl` and preferred `pl` seconds,
/// and the router's link-layer address.
fn ra(lifetime: u16, vl: u32, pl: u32, mac: [u8; 6]) -> Vec<u8> {
    let mut b = alloc::vec![134, 0, 0, 0, 64, 0];
    b.extend_from_slice(&lifetime.to_be_bytes());
    b.extend_from_slice(&[0; 8]);
    // Source link-layer address.
    b.extend_from_slice(&[1, 1]);
    b.extend_from_slice(&mac);
    // Prefix information: on-link, autonomous.
    b.extend_from_slice(&[3, 4, 64, 0xc0]);
    b.extend_from_slice(&vl.to_be_bytes());
    b.extend_from_slice(&pl.to_be_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&a6(&[0x2001, 0xdb8, 1, 0, 0, 0, 0, 0]));
    b
}

#[test]
fn slaac_from_a_router_advertisement() {
    let _g = crate::tests::setup();
    let (h1, h2, na, nb) = pair6(1, 2);
    let i1 = index(&h1, &na);
    h1.net.lock().with_iface(na.as_bytes(), |i| {
        i.nd6_flags |= abi::net::nd6_iff::ACCEPT_RTADV;
    });
    // The router sends from its link-local address, hop limit 255.
    let rtr = raw6(&h2, 58);
    let i2 = index(&h2, &nb);
    rtr.setopt(
        41,
        abi::inet::ipv6::IPV6_MULTICAST_HOPS,
        &255i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    rtr.setopt(
        41,
        abi::inet::ipv6::IPV6_MULTICAST_IF,
        &u32::from(i2).to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    let mac2 = h2.net.lock().by_index(i2).unwrap().lladdr.unwrap();
    let msg = ra(1800, 3600, 1800, mac2);
    rtr.send_dgram(
        &msg,
        Some(&sockaddr_in6(&crate::in6::ALLNODES, 0, u32::from(i2))),
        &Cred::root(),
    )
    .unwrap();
    // The address from the prefix and the link-local identifier, once
    // its DAD is done.
    let ll1 = linklocal(&h1, &na);
    let mut slaac = a6(&[0x2001, 0xdb8, 1, 0, 0, 0, 0, 0]);
    slaac[8..].copy_from_slice(&ll1[8..]);
    assert_ne!(flags6(&h1, &na, &slaac), -1, "the SLAAC address");
    assert_ne!(flags6(&h1, &na, &slaac) & in6_iff::AUTOCONF, 0);
    crate::advance(3 * SEC);
    assert_eq!(flags6(&h1, &na, &slaac) & in6_iff::NOTREADY, 0);
    // The default route through the router's link-local address, and
    // the prefix on-link.
    let ll2 = linklocal(&h2, &nb);
    {
        let n = h1.net.lock();
        let def = n
            .route6(0, &a6(&[0x2001, 0xdb8, 0xffff, 0, 0, 0, 0, 1]), 0)
            .unwrap();
        assert_eq!(def.plen, 0);
        assert_eq!(def.gateway, net::ifnet::Gateway::Addr(ll2));
        assert_eq!(def.ifindex, i1);
        let p = n.route6(0, &slaac, 0).unwrap();
        assert!(p.plen >= 64);
    }
    let st = state::of(&h1);
    {
        let nd = st.nd6.lock();
        assert_eq!(nd.routers.len(), 1);
        assert!(nd.routers[0].installed);
        assert!(
            nd.prefixes
                .iter()
                .any(|p| p.plen == 64 && p.prefix[..6] == [0x20, 0x01, 0x0d, 0xb8, 0, 1])
        );
    }
    // A router lifetime of 0 takes it away.
    rtr.send_dgram(
        &ra(0, 3600, 1800, mac2),
        Some(&sockaddr_in6(&crate::in6::ALLNODES, 0, u32::from(i2))),
        &Cred::root(),
    )
    .unwrap();
    assert!(st.nd6.lock().routers.is_empty());
    assert!(
        h1.net
            .lock()
            .route6(0, &a6(&[0x2001, 0xdb8, 0xffff, 0, 0, 0, 0, 1]), 0)
            .is_none()
    );
    rtr.close();
}

/// `sys/netinet6/lpm6`: two links between two hosts, a host route over
/// each through the other's link-local address; each route's packets go
/// out of its own link.
#[test]
fn host_routes_through_link_local_gateways() {
    let _g = crate::tests::setup();
    let h1 = Vnet::new(0);
    let h2 = Vnet::new(0);
    let mut ends = Vec::new();
    for _ in 0..2 {
        let name = clone::create(&h1, b"epair", &ctx()).unwrap();
        let base = alloc::string::String::from(name.trim_end_matches('a'));
        let (na, nb) = (alloc::format!("{base}a"), alloc::format!("{base}b"));
        clone::vmove(&h1, nb.as_bytes(), &h2).unwrap();
        enable6(&h1, &na);
        enable6(&h2, &nb);
        ends.push((na, nb));
    }
    let lo = clone::create(&h2, b"lo", &ctx()).unwrap();
    {
        let mut n = h2.net.lock();
        let f = n.by_name(lo.as_bytes()).unwrap().flags | abi::net::iff::UP;
        n.set_flags(lo.as_bytes(), f).unwrap();
    }
    for k in 0..2u16 {
        let x = Ifaddr {
            family: Family::Inet6,
            addr: a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 2, k]),
            plen: 128,
            dst: None,
            broadcast: None,
            flags6: 0,
            vltime: u32::MAX,
            pltime: u32::MAX,
            vhid: 0,
            created: 0,
            updated: 0,
        };
        h2.net
            .lock()
            .add_addr_flags(lo.as_bytes(), x, ifa6_update::AIFADDR)
            .unwrap();
    }
    crate::advance(3 * SEC);
    for (k, (na, nb)) in ends.iter().enumerate() {
        let ll = linklocal(&h2, nb);
        let i = index(&h1, na);
        let r = net::ifnet::Route {
            family: Family::Inet6,
            dst: a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 2, u16::try_from(k).unwrap()]),
            plen: 128,
            gateway: net::ifnet::Gateway::Addr(ll),
            flags: abi::net::rtf::UP
                | abi::net::rtf::GATEWAY
                | abi::net::rtf::HOST
                | abi::net::rtf::STATIC,
            ifindex: i,
            ifa: linklocal(&h1, na),
            mtu: 0,
            weight: 1,
            expire: 0,
            nhidx: 0,
            fib: 0,
        };
        h1.net.lock().add_route(r).unwrap();
    }
    let p = raw6(&h1, 58);
    for k in 0..2u16 {
        let dst = a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 2, k]);
        p.send_dgram(
            &echo6(7, k, b"lpm"),
            Some(&sockaddr_in6(&dst, 0, 0)),
            &Cred::root(),
        )
        .unwrap();
        crate::advance(SEC);
        let mut got = false;
        while let Ok(r) = p.recv_dgram(200, false) {
            if r.data[0] == 129 && r.data[7] == u8::try_from(k).unwrap() {
                got = true;
            }
        }
        assert!(got, "a reply through link {k}");
    }
    p.close();
}

#[test]
fn tcp_over_ipv6_resolves_its_neighbor_and_carries_data() {
    let _g = crate::tests::setup();
    let (h1, h2, _, _) = pair6(1, 2);
    let l = Socket::new_in(&h2, Family::Inet6, Proto::Tcp, &Cred::root());
    l.bind(&sockaddr_in6(&[0; 16], 32200, 0), &Cred::root())
        .unwrap();
    l.listen(5, &Cred::root()).unwrap();
    let c = Socket::new_in(&h1, Family::Inet6, Proto::Tcp, &Cred::root());
    let dst = a6(&[0x2001, 0xdb8, 0, 0, 0, 0, 0, 2]);
    c.connect(&sockaddr_in6(&dst, 32200, 0), &Cred::root(), false)
        .unwrap();
    assert_eq!(c.connect_done(), Ok(()));
    let s = l.accept().unwrap();
    assert_eq!(crate::tcp::snapshot(&s).maxseg, 1440);
    let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7).to_le_bytes()[0]).collect();
    let mut sent = 0;
    let mut got = Vec::new();
    while got.len() < data.len() {
        if sent < data.len()
            && let Ok(n) = c.send_stream(&data[sent..], false)
        {
            sent += n;
        }
        crate::advance(10_000_000);
        while let Ok(r) = s.recv_stream(1 << 20, false, false) {
            if r.data.is_empty() {
                break;
            }
            got.extend(r.data);
        }
    }
    assert_eq!(got, data);
    for x in [c, s, l] {
        x.close();
    }
}
