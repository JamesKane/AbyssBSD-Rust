// SPDX-License-Identifier: BSD-2-Clause
//! Host tests of IPv4 on the wire: hosts in VNETs joined by `epair`s,
//! with ARP, UDP, raw ICMP sockets, fragments, and a router between two
//! networks.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::{IPPROTO_IP, ip as ipopt};
use net::addr::{from_v4, sockaddr_in};
use net::clone::{self, Ctx};
use net::ifnet::{Gateway, Ifaddr, Route};
use net::{Family, Vnet};

use crate::cksum;
use crate::state::{self, icps, ips, udps};
use crate::{Cred, Proto, Socket};

fn ctx() -> Ctx {
    Ctx {
        now: 0,
        hostuuid: "6b0c3f6e-1111-2222-3333-444455556666".into(),
        jail: "0".into(),
        random: [9; 12],
    }
}

fn addr(v: &Vnet, name: &[u8], a: [u8; 4]) {
    let b = [a[0], a[1], a[2], 255];
    let x = Ifaddr {
        family: Family::Inet,
        addr: from_v4(a),
        plen: 24,
        dst: None,
        broadcast: Some(from_v4(b)),
        flags6: 0,
        vltime: u32::MAX,
        pltime: u32::MAX,
        vhid: 0,
        created: 0,
        updated: 0,
    };
    v.net.lock().add_addr(name, x).unwrap();
}

fn index(v: &Vnet, name: &[u8]) -> u16 {
    v.net.lock().by_name(name).unwrap().index
}

fn default_via(v: &Vnet, name: &[u8], gw: [u8; 4], src: [u8; 4]) {
    let r = Route {
        family: Family::Inet,
        dst: from_v4([0; 4]),
        plen: 0,
        gateway: Gateway::Addr(from_v4(gw)),
        flags: abi::net::rtf::UP | abi::net::rtf::GATEWAY | abi::net::rtf::STATIC,
        ifindex: index(v, name),
        ifa: from_v4(src),
        mtu: 0,
        weight: 1,
        expire: 0,
        nhidx: 0,
        fib: 0,
    };
    v.net.lock().add_route(r).unwrap();
}

/// Two hosts on 192.0.2.0/24: `epair*a` in the first, `epair*b` in the
/// second, with the epair's own names.
fn pair(a: u8, b: u8) -> (Arc<Vnet>, Arc<Vnet>, alloc::string::String) {
    let h1 = Vnet::new(0);
    let h2 = Vnet::new(0);
    let name = clone::create(&h1, b"epair", &ctx()).unwrap();
    let base = alloc::string::String::from(name.trim_end_matches('a'));
    let (na, nb) = (alloc::format!("{base}a"), alloc::format!("{base}b"));
    clone::vmove(&h1, nb.as_bytes(), &h2).unwrap();
    addr(&h1, na.as_bytes(), [192, 0, 2, a]);
    addr(&h2, nb.as_bytes(), [192, 0, 2, b]);
    (h1, h2, base)
}

fn udp_in(v: &Arc<Vnet>) -> Arc<Socket> {
    Socket::new_in(v, Family::Inet, Proto::Udp, &Cred::root())
}

fn raw_in(v: &Arc<Vnet>, p: u8) -> Arc<Socket> {
    Socket::new_in(v, Family::Inet, Proto::Raw(p), &Cred::root())
}

/// An ICMP echo request with identifier `id` and sequence `seq`.
fn echo(id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = alloc::vec![8, 0, 0, 0];
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(payload);
    let c = cksum::of(&b);
    b[2..4].copy_from_slice(&c.to_be_bytes());
    b
}

#[test]
fn udp_crosses_the_wire_after_arp() {
    let _g = crate::tests::setup();
    let (h1, h2, base) = pair(1, 2);
    let r = udp_in(&h2);
    r.bind(&sockaddr_in([0; 4], 31001), &Cred::root()).unwrap();
    let s = udp_in(&h1);
    let n = s
        .send_dgram(
            b"hello",
            Some(&sockaddr_in([192, 0, 2, 2], 31001)),
            &Cred::root(),
        )
        .unwrap();
    assert_eq!(n, 5);
    let got = r.recv_dgram(100, false).unwrap();
    assert_eq!(got.data, b"hello");
    let from = got.from.unwrap();
    assert_eq!(net::addr::v4_of(&from.addr), [192, 0, 2, 1]);
    assert_eq!(from.port, s.sockname().port);
    let meta = got.meta.unwrap();
    assert_eq!(meta.ttl, 64);
    assert_eq!(
        meta.ifindex,
        index(&h2, alloc::format!("{base}b").as_bytes())
    );
    // The sender learned the receiver's address on the way.
    let a = index(&h1, alloc::format!("{base}a").as_bytes());
    assert!(
        h1.net
            .lock()
            .lle(a, Family::Inet, &from_v4([192, 0, 2, 2]))
            .is_some_and(|e| e.flags & net::lltable::lle::VALID != 0)
    );
    let st = state::of(&h2);
    assert_eq!(st.udpstat.get(udps::IPACKETS), 1);
    assert_eq!(state::of(&h1).udpstat.get(udps::OPACKETS), 1);
    // A connected socket's datagram to a closed port: ICMP port
    // unreachable comes back, and its next call fails with it.
    let c = udp_in(&h1);
    c.connect(&sockaddr_in([192, 0, 2, 2], 31002), &Cred::root(), false)
        .unwrap();
    c.send_dgram(b"x", None, &Cred::root()).unwrap();
    assert_eq!(c.recv_dgram(10, false).unwrap_err(), Errno::ECONNREFUSED);
    assert_eq!(st.udpstat.get(udps::NOPORT), 1);
    assert_eq!(st.icmpstat.get(icps::OUTHIST + 3), 1);
    assert_eq!(state::of(&h1).icmpstat.get(icps::INHIST + 3), 1);
    for x in [r, s, c] {
        x.close();
    }
}

#[test]
fn raw_icmp_echo_and_the_reply() {
    let _g = crate::tests::setup();
    let (h1, h2, _) = pair(1, 2);
    let p = raw_in(&h1, 1);
    let req = echo(0x1234, 7, b"abcdefgh");
    p.send_dgram(&req, Some(&sockaddr_in([192, 0, 2, 2], 0)), &Cred::root())
        .unwrap();
    let got = p.recv_dgram(200, false).unwrap();
    // The whole datagram, its header first.
    let d = &got.data;
    assert_eq!(d[0], 0x45);
    assert_eq!(usize::from(u16::from_be_bytes([d[2], d[3]])), d.len());
    assert_eq!(d[9], 1);
    assert_eq!(&d[12..16], &[192, 0, 2, 2]);
    assert_eq!(&d[16..20], &[192, 0, 2, 1]);
    assert_eq!(cksum::of(&d[..20]), 0);
    let icmp = &d[20..];
    assert_eq!(icmp[0], 0, "an echo reply");
    assert_eq!(&icmp[4..8], &req[4..8]);
    assert_eq!(&icmp[8..], b"abcdefgh");
    assert_eq!(cksum::of(icmp), 0);
    assert_eq!(state::of(&h2).icmpstat.get(icps::REFLECT), 1);
    // A protocol no one speaks: protocol unreachable.
    let q = raw_in(&h1, 253);
    q.send_dgram(
        b"sixteen bytes!!!",
        Some(&sockaddr_in([192, 0, 2, 2], 0)),
        &Cred::root(),
    )
    .unwrap();
    let got = p.recv_dgram(200, false).unwrap();
    assert_eq!((got.data[20], got.data[21]), (3, 2));
    assert_eq!(state::of(&h2).ipstat.get(ips::NOPROTO), 1);
    p.close();
    q.close();
}

#[test]
fn large_datagrams_fragment_and_reassemble() {
    let _g = crate::tests::setup();
    let (h1, h2, _) = pair(1, 2);
    let r = udp_in(&h2);
    r.bind(&sockaddr_in([0; 4], 31003), &Cred::root()).unwrap();
    r.setopt(
        abi::socket::SOL_SOCKET,
        abi::socket::SO_RCVBUF,
        &65536i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    let s = udp_in(&h1);
    s.setopt(
        abi::socket::SOL_SOCKET,
        abi::socket::SO_SNDBUF,
        &65536i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    let data: Vec<u8> = (0..4000u32).map(|i| (i * 7 % 251) as u8).collect();
    let to = sockaddr_in([192, 0, 2, 2], 31003);
    s.send_dgram(&data, Some(&to), &Cred::root()).unwrap();
    let got = r.recv_dgram(8000, false).unwrap();
    assert_eq!(got.data, data);
    assert_eq!(state::of(&h1).ipstat.get(ips::FRAGMENTED), 1);
    assert_eq!(state::of(&h1).ipstat.get(ips::OFRAGMENTS), 3);
    assert_eq!(state::of(&h2).ipstat.get(ips::FRAGMENTS), 3);
    assert_eq!(state::of(&h2).ipstat.get(ips::REASSEMBLED), 1);
    // Don't fragment: refused here.
    s.setopt(
        IPPROTO_IP,
        ipopt::IP_DONTFRAG,
        &1i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    assert_eq!(
        s.send_dgram(&data, Some(&to), &Cred::root()),
        Err(Errno::EMSGSIZE)
    );
    r.close();
    s.close();
}

#[test]
fn header_included_and_broadcast() {
    let _g = crate::tests::setup();
    let (h1, h2, _) = pair(1, 2);
    let r = udp_in(&h2);
    r.bind(&sockaddr_in([0; 4], 31004), &Cred::root()).unwrap();
    let raw = raw_in(&h1, 255);
    raw.setopt(
        IPPROTO_IP,
        ipopt::IP_HDRINCL,
        &1i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    assert_eq!(
        raw.getopt(IPPROTO_IP, ipopt::IP_HDRINCL).unwrap(),
        1i32.to_ne_bytes()
    );
    // A UDP datagram written whole, as `dhclient` writes its renewals:
    // no checksum, identification 0, source the wildcard.
    let mut p = alloc::vec![
        0x45, 0, 0, 31, 0, 0, 0, 0, 64, 17, 0, 0, 0, 0, 0, 0, 192, 0, 2, 2
    ];
    p.extend_from_slice(&[0x1f, 0x90, 0x79, 0x1c, 0, 11, 0, 0, b'h', b'e', b'y']);
    raw.send_dgram(&p, Some(&sockaddr_in([192, 0, 2, 2], 0)), &Cred::root())
        .unwrap();
    let got = r.recv_dgram(10, false).unwrap();
    assert_eq!(got.data, b"hey");
    assert_eq!(net::addr::v4_of(&got.from.unwrap().addr), [192, 0, 2, 1]);
    // A length that is not the datagram's is refused.
    p[3] = 30;
    assert_eq!(
        raw.send_dgram(&p, Some(&sockaddr_in([192, 0, 2, 2], 0)), &Cred::root()),
        Err(Errno::EINVAL)
    );
    // Broadcast needs `SO_BROADCAST`; then every host on the network
    // hears it, the sender too.
    let b = udp_in(&h1);
    b.bind(&sockaddr_in([0; 4], 31004), &Cred::root()).unwrap();
    let to = sockaddr_in([192, 0, 2, 255], 31004);
    assert_eq!(
        b.send_dgram(b"all", Some(&to), &Cred::root()),
        Err(Errno::EACCES)
    );
    b.setopt(
        abi::socket::SOL_SOCKET,
        abi::socket::SO_BROADCAST,
        &1i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    b.send_dgram(b"all", Some(&to), &Cred::root()).unwrap();
    assert_eq!(r.recv_dgram(10, false).unwrap().data, b"all");
    assert_eq!(b.recv_dgram(10, false).unwrap().data, b"all");
    for x in [r, raw, b] {
        x.close();
    }
}

#[test]
fn a_router_forwards_and_answers_for_the_dead() {
    let _g = crate::tests::setup();
    let h1 = Vnet::new(0);
    let rt = Vnet::new(0);
    let h2 = Vnet::new(0);
    let e0 = clone::create(&rt, b"epair", &ctx()).unwrap();
    let e1 = clone::create(&rt, b"epair", &ctx()).unwrap();
    let (b0, b1) = (e0.trim_end_matches('a'), e1.trim_end_matches('a'));
    let (h1if, r0, r1, h2if) = (
        alloc::format!("{b0}a"),
        alloc::format!("{b0}b"),
        alloc::format!("{b1}a"),
        alloc::format!("{b1}b"),
    );
    clone::vmove(&rt, h1if.as_bytes(), &h1).unwrap();
    clone::vmove(&rt, h2if.as_bytes(), &h2).unwrap();
    addr(&h1, h1if.as_bytes(), [10, 1, 0, 1]);
    addr(&rt, r0.as_bytes(), [10, 1, 0, 254]);
    addr(&rt, r1.as_bytes(), [10, 2, 0, 254]);
    addr(&h2, h2if.as_bytes(), [10, 2, 0, 1]);
    default_via(&h1, h1if.as_bytes(), [10, 1, 0, 254], [10, 1, 0, 1]);
    default_via(&h2, h2if.as_bytes(), [10, 2, 0, 254], [10, 2, 0, 1]);
    let rs = state::of(&rt);
    let r = udp_in(&h2);
    r.bind(&sockaddr_in([0; 4], 31005), &Cred::root()).unwrap();
    let s = udp_in(&h1);
    let to = sockaddr_in([10, 2, 0, 1], 31005);
    // Not a router yet: dropped.
    s.send_dgram(b"one", Some(&to), &Cred::root()).unwrap();
    assert_eq!(r.recv_dgram(10, false).unwrap_err(), Errno::EAGAIN);
    assert_eq!(rs.ipstat.get(ips::CANTFORWARD), 1);
    rs.conf
        .forwarding
        .store(1, core::sync::atomic::Ordering::Relaxed);
    s.send_dgram(b"two", Some(&to), &Cred::root()).unwrap();
    let got = r.recv_dgram(10, false).unwrap();
    assert_eq!(got.data, b"two");
    assert_eq!(got.meta.unwrap().ttl, 63);
    assert_eq!(rs.ipstat.get(ips::FORWARD), 1);
    // TTL 1: the router answers time exceeded to a raw ICMP socket.
    let icmp = raw_in(&h1, 1);
    s.setopt(
        IPPROTO_IP,
        ipopt::IP_TTL,
        &1i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    s.send_dgram(b"three", Some(&to), &Cred::root()).unwrap();
    let got = icmp.recv_dgram(200, false).unwrap();
    assert_eq!(&got.data[12..16], &[10, 1, 0, 254]);
    assert_eq!((got.data[20], got.data[21]), (11, 0));
    // No route onward: host unreachable.
    s.setopt(
        IPPROTO_IP,
        ipopt::IP_TTL,
        &64i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    s.send_dgram(b"four", Some(&sockaddr_in([10, 9, 0, 1], 9)), &Cred::root())
        .unwrap();
    let got = icmp.recv_dgram(200, false).unwrap();
    assert_eq!((got.data[20], got.data[21]), (3, 1));
    for x in [r, s, icmp] {
        x.close();
    }
}

#[test]
fn arp_reads_and_changes_neighbors_by_netlink() {
    use crate::netlink::{Ctx, handle, nlm, ty};
    use crate::tests::{nlreplies, nlreq};
    let _g = crate::tests::setup();
    let (h1, _h2, base) = pair(1, 2);
    let a = index(&h1, alloc::format!("{base}a").as_bytes());
    // Something to resolve the peer.
    let s = udp_in(&h1);
    s.send_dgram(b"x", Some(&sockaddr_in([192, 0, 2, 2], 9)), &Cred::root())
        .unwrap();
    let ctx = Ctx {
        port: 7,
        may_route: true,
        ..Ctx::default()
    };
    // `arp -an`: a dump of the IPv4 entries, the interface's own and the
    // peer's.
    let mut ndm = [0u8; 12];
    ndm[0] = abi::inet::AF_INET;
    let r = handle(
        &ctx,
        &h1.net,
        &nlreq(ty::GETNEIGH, nlm::DUMP, 1, &ndm, &[]),
        0,
    );
    let kinds = nlreplies(&r);
    let entries = kinds.iter().filter(|k| k.0 == ty::NEWNEIGH).count();
    assert_eq!(entries, 2, "{kinds:?}");
    assert_eq!(kinds.last().map(|k| (k.0, k.2)), Some((ty::DONE, 0)));
    // `arp -d 192.0.2.2`.
    ndm[4..8].copy_from_slice(&u32::from(a).to_ne_bytes());
    let dst: &[u8] = &[192, 0, 2, 2];
    let r = handle(
        &ctx,
        &h1.net,
        &nlreq(ty::DELNEIGH, nlm::ACK, 2, &ndm, &[(1, dst)]),
        0,
    );
    assert_eq!(nlreplies(&r), [(ty::ERROR, 0x100, 0)]);
    assert!(
        h1.net
            .lock()
            .lle(a, Family::Inet, &from_v4([192, 0, 2, 2]))
            .is_none()
    );
    // `arp -d` of the interface's own: refused.
    let own: &[u8] = &[192, 0, 2, 1];
    let r = handle(
        &ctx,
        &h1.net,
        &nlreq(ty::DELNEIGH, nlm::ACK, 3, &ndm, &[(1, own)]),
        0,
    );
    assert_eq!(nlreplies(&r)[0].2, Errno::EPERM.get());
    // `arp -s 192.0.2.9 2:0:0:0:0:9`: a permanent entry.
    ndm[10] = 0x40;
    let dst: &[u8] = &[192, 0, 2, 9];
    let ll: &[u8] = &[2, 0, 0, 0, 0, 9];
    let r = handle(
        &ctx,
        &h1.net,
        &nlreq(
            ty::NEWNEIGH,
            nlm::ACK | nlm::CREATE | nlm::REPLACE,
            4,
            &ndm,
            &[(1, dst), (2, ll)],
        ),
        0,
    );
    assert_eq!(nlreplies(&r)[0].2, 0);
    let n = h1.net.lock();
    let e = n.lle(a, Family::Inet, &from_v4([192, 0, 2, 9])).unwrap();
    assert_eq!(e.lladdr, [2, 0, 0, 0, 0, 9]);
    assert_ne!(e.flags & net::lltable::lle::STATIC, 0);
    drop(n);
    s.close();
}

#[test]
fn priority_tags_on_the_interface_and_the_socket() {
    let _g = crate::tests::setup();
    let (h1, h2, base) = pair(1, 2);
    let a = alloc::format!("{base}a");
    // `ifconfig epair0a pcp 3`: tagged on VLAN 0, which the other end
    // takes as untagged.
    h1.net
        .lock()
        .with_iface(a.as_bytes(), |i| i.pcp = 3)
        .unwrap();
    let r = udp_in(&h2);
    r.bind(&sockaddr_in([0; 4], 31006), &Cred::root()).unwrap();
    let s = udp_in(&h1);
    let to = sockaddr_in([192, 0, 2, 2], 31006);
    s.send_dgram(b"pcp", Some(&to), &Cred::root()).unwrap();
    assert_eq!(r.recv_dgram(10, false).unwrap().data, b"pcp");
    // `ping -C5`: the socket's priority instead.
    s.setopt(
        IPPROTO_IP,
        ipopt::IP_VLAN_PCP,
        &5i32.to_ne_bytes(),
        &Cred::root(),
    )
    .unwrap();
    assert_eq!(
        s.getopt(IPPROTO_IP, ipopt::IP_VLAN_PCP).unwrap(),
        5i32.to_ne_bytes()
    );
    s.send_dgram(b"five", Some(&to), &Cred::root()).unwrap();
    assert_eq!(r.recv_dgram(10, false).unwrap().data, b"five");
    assert_eq!(
        s.setopt(
            IPPROTO_IP,
            ipopt::IP_VLAN_PCP,
            &8i32.to_ne_bytes(),
            &Cred::root()
        ),
        Err(Errno::EINVAL)
    );
    r.close();
    s.close();
}

#[test]
fn software_vlan_header_for_priority() {
    let mut m = mbuf::Packet {
        hdr: mbuf::PktHdr::default(),
        data: mbuf::Chain::from_slice(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0x08, 0x00, 0x45, 0,
        ])
        .unwrap(),
    };
    net::ether::set_pcp(&mut m, 3, false).unwrap();
    let mut b = [0u8; 20];
    m.data.copy_out(0, &mut b);
    assert_eq!(
        b,
        [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0x81, 0x00, 0x60, 0x00, 0x08, 0x00, 0x45, 0
        ]
    );
}

#[test]
fn group_joins_and_leaves_are_reported() {
    let _g = crate::tests::setup();
    let (h1, h2, _) = pair(1, 2);
    let rtr = raw_in(&h2, 2);
    let mut mreq = [0u8; 8];
    mreq[..4].copy_from_slice(&[230, 0, 0, 1]);
    mreq[4..].copy_from_slice(&[192, 0, 2, 1]);
    // IGMPv3: a state-change report to 224.0.0.22, TTL 1, Router Alert.
    let s = udp_in(&h1);
    s.setopt(IPPROTO_IP, ipopt::IP_ADD_MEMBERSHIP, &mreq, &Cred::root())
        .unwrap();
    let d = rtr.recv_dgram(100, false).unwrap().data;
    assert_eq!((d[0], d[1], d[8]), (0x46, 0xc0, 1));
    assert_eq!(&d[16..20], &[224, 0, 0, 22]);
    assert_eq!(&d[20..24], &[0x94, 4, 0, 0]);
    assert_eq!(cksum::of(&d[..24]), 0);
    let r = &d[24..];
    assert_eq!((r[0], &r[6..8], r[8]), (0x22, &[0, 1][..], 4));
    assert_eq!(&r[12..16], &[230, 0, 0, 1]);
    assert_eq!(cksum::of(r), 0);
    // A second member is not reported; the last to leave is.
    let t = udp_in(&h1);
    t.setopt(IPPROTO_IP, ipopt::IP_ADD_MEMBERSHIP, &mreq, &Cred::root())
        .unwrap();
    assert_eq!(rtr.recv_dgram(100, false).unwrap_err(), Errno::EAGAIN);
    t.close();
    assert_eq!(rtr.recv_dgram(100, false).unwrap_err(), Errno::EAGAIN);
    s.close();
    let d = rtr.recv_dgram(100, false).unwrap().data;
    assert_eq!(d[24 + 8], 3);
    // IGMPv2: a report to the group, a leave to 224.0.0.2.
    state::of(&h1)
        .conf
        .igmp_version
        .store(2, core::sync::atomic::Ordering::Relaxed);
    let s = udp_in(&h1);
    s.setopt(IPPROTO_IP, ipopt::IP_ADD_MEMBERSHIP, &mreq, &Cred::root())
        .unwrap();
    let d = rtr.recv_dgram(100, false).unwrap().data;
    assert_eq!((&d[16..20], d[24]), (&[230, 0, 0, 1][..], 0x16));
    s.setopt(IPPROTO_IP, ipopt::IP_DROP_MEMBERSHIP, &mreq, &Cred::root())
        .unwrap();
    let d = rtr.recv_dgram(100, false).unwrap().data;
    assert_eq!((&d[16..20], d[24]), (&[224, 0, 0, 2][..], 0x17));
    s.close();
    rtr.close();
}

#[test]
fn flows_spread_over_a_multipath_route() {
    let mut n = net::ifnet::Net::new();
    let path = |gw: u8, weight: u32, nhidx: u32| Route {
        family: Family::Inet,
        dst: from_v4([198, 51, 100, 0]),
        plen: 24,
        gateway: Gateway::Addr(from_v4([192, 0, 2, gw])),
        flags: abi::net::rtf::UP | abi::net::rtf::GATEWAY,
        ifindex: 2,
        ifa: from_v4([192, 0, 2, 1]),
        mtu: 0,
        weight,
        expire: 0,
        nhidx,
        fib: 0,
    };
    n.routes.push(path(10, 1, 7));
    n.routes.push(path(20, 1, 3));
    let first = n.routes[0].clone();
    let gw = |flow: u32| match crate::ip::mpath_select(&n, &first, flow).gateway {
        Gateway::Addr(a) => a[15],
        Gateway::Link(_) => 0,
    };
    // By next-hop index: flow 0 the lower one's, flow 1 the other's.
    assert_eq!((gw(0), gw(1), gw(2), gw(3)), (20, 10, 20, 10));
    // Weights 2:1 get two slots and one.
    n.routes[1].weight = 2;
    let first = n.routes[0].clone();
    let picks: Vec<u8> = (0..6)
        .map(|f| match crate::ip::mpath_select(&n, &first, f).gateway {
            Gateway::Addr(a) => a[15],
            Gateway::Link(_) => 0,
        })
        .collect();
    assert_eq!(picks, [20, 20, 10, 20, 20, 10]);
}

#[test]
fn toeplitz_matches_the_rss_verification_suite() {
    // FreeBSD's multipath key is the RSS one; Microsoft's published
    // IPv4 vectors (addresses, then with the ports).
    let k = &crate::ip::MPATH_KEY;
    let v4 = |s: [u8; 4], d: [u8; 4]| [s, d].concat();
    assert_eq!(
        crate::ip::toeplitz(k, &v4([66, 9, 149, 187], [161, 142, 100, 80])),
        0x323e_8fc2
    );
    let mut t = v4([66, 9, 149, 187], [161, 142, 100, 80]);
    t.extend_from_slice(&2794u16.to_be_bytes());
    t.extend_from_slice(&1766u16.to_be_bytes());
    assert_eq!(crate::ip::toeplitz(k, &t), 0x51cc_c178);
    let mut t = v4([199, 92, 111, 2], [65, 69, 140, 83]);
    t.extend_from_slice(&14230u16.to_be_bytes());
    t.extend_from_slice(&4739u16.to_be_bytes());
    assert_eq!(crate::ip::toeplitz(k, &t), 0xc626_b0ea);
}

fn tcp_in(v: &Arc<Vnet>) -> Arc<Socket> {
    Socket::new_in(v, Family::Inet, Proto::Tcp, &Cred::root())
}

/// Moves `data` from `c` to `s`, reading as it comes, with time passing
/// for the timers.
fn stream(c: &Arc<Socket>, s: &Arc<Socket>, data: &[u8]) -> Vec<u8> {
    let (mut sent, mut got) = (0, Vec::new());
    let mut rounds = 0;
    while got.len() < data.len() {
        if sent < data.len() {
            match c.send_stream(&data[sent..], false) {
                Ok(n) => sent += n,
                Err(Errno::EAGAIN) => {}
                Err(e) => panic!("{e:?}"),
            }
        }
        crate::advance(10_000_000);
        loop {
            match s.recv_stream(1 << 20, false, false) {
                Ok(r) if !r.data.is_empty() => got.extend(r.data),
                Ok(_) | Err(Errno::EAGAIN) => break,
                Err(e) => panic!("{e:?}"),
            }
        }
        rounds += 1;
        assert!(rounds < 10_000, "stuck at {} of {}", got.len(), data.len());
    }
    got
}

#[test]
fn tcp_crosses_the_wire_and_closes() {
    let _g = crate::tests::setup();
    let (h1, h2, _) = pair(1, 2);
    let l = tcp_in(&h2);
    l.bind(&sockaddr_in([0; 4], 32100), &Cred::root()).unwrap();
    l.listen(5, &Cred::root()).unwrap();
    let c = tcp_in(&h1);
    c.connect(&sockaddr_in([192, 0, 2, 2], 32100), &Cred::root(), false)
        .unwrap();
    assert_eq!(c.connect_done(), Ok(()));
    let s = l.accept().unwrap();
    assert_eq!(s.tcp_state(), crate::Tstate::Established);
    // The SYN asked for an MSS of the epair's MTU less the headers.
    let mss = |so: &Socket| crate::tcp::snapshot(so).maxseg;
    assert_eq!(mss(&c), 1460);
    assert_eq!(mss(&s), 1460);
    let data: Vec<u8> = (0..1_000_000u32)
        .map(|i| (i * 13 + i / 255).to_le_bytes()[0])
        .collect();
    assert_eq!(stream(&c, &s, &data), data);
    let st1 = state::of(&h1);
    assert!(st1.tcp.stats.get(::tcp::tcps::SNDPACK) > 600);
    assert_eq!(st1.tcp.stats.get(::tcp::tcps::SNDREXMITPACK), 0);
    // An orderly close from the client: TIME_WAIT there, CLOSED here.
    c.shutdown(abi::socket::SHUT_WR).unwrap();
    crate::advance(100_000_000);
    assert_eq!(s.recv_stream(10, false, false).unwrap().data.len(), 0);
    s.close();
    crate::advance(100_000_000);
    assert_eq!(c.tcp_state(), crate::Tstate::TimeWait);
    c.close();
    // Twice the MSL later the port is free again.
    crate::advance(61_000_000_000);
    l.close();
}
