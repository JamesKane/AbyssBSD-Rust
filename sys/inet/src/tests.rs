// SPDX-License-Identifier: BSD-2-Clause
//! Host tests: FreeBSD's loopback UDP and TCP behavior, case by case.
//!
//! The tables are global, as the kernel's are; each test takes a lock
//! and uses its own ports, so tests run one at a time.

use alloc::sync::Arc;
use std::sync::Mutex;

use abi::errno::Errno;
use abi::inet::{IPPROTO_IPV6, ipv6};
use abi::socket::{SHUT_WR, SO_LINGER, SO_OOBINLINE, SO_REUSEADDR, SO_REUSEPORT, SOL_SOCKET};

use crate::addr::{self, LOOPBACK6, Sockaddr, sockaddr_in, sockaddr_in6, v4mapped};
use crate::ifnet::{self, NET};
use crate::pcb::{TCB, UDB};
use crate::{Cred, Family, Proto, Socket, Tstate, tune};

static LOCK: Mutex<()> = Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ifnet::attach_loopback(0);
    g
}

fn user() -> Cred {
    Cred {
        uid: 1001,
        ..Cred::default()
    }
}

fn root() -> Cred {
    Cred::root()
}

const LO: [u8; 4] = [127, 0, 0, 1];

fn udp4() -> Arc<Socket> {
    Socket::new(Family::Inet, Proto::Udp, &user())
}

fn tcp4() -> Arc<Socket> {
    Socket::new(Family::Inet, Proto::Tcp, &user())
}

fn set_int(s: &Socket, level: i32, name: i32, v: i32) {
    s.setopt(level, name, &v.to_ne_bytes(), &user()).unwrap();
}

#[test]
fn lo0_has_its_addresses_and_routes() {
    let _g = setup();
    let n = NET.lock();
    let lo = n.by_name(b"lo0").unwrap();
    assert_eq!(lo.index, 1);
    assert!(lo.flags & abi::net::iff::UP != 0);
    let addrs: std::vec::Vec<_> = lo.addrs.iter().map(|a| (a.family, a.plen)).collect();
    assert_eq!(
        addrs,
        [(Family::Inet, 8), (Family::Inet6, 128), (Family::Inet6, 64)]
    );
    assert!(n.is_local(Family::Inet, &addr::from_v4(LO), 0));
    assert!(!n.is_local(Family::Inet, &addr::from_v4([127, 0, 0, 2]), 0));
    // 127.0.0.1 is a host route; 127.0.0.2 has none.
    assert!(n.lookup(Family::Inet, &addr::from_v4(LO)).is_ok());
    assert!(
        n.lookup(Family::Inet, &addr::from_v4([127, 0, 0, 2]))
            .is_err()
    );
    assert!(n.lookup(Family::Inet6, &LOOPBACK6).is_ok());
}

#[test]
fn udp_bind_send_receive() {
    let _g = setup();
    let a = udp4();
    let b = udp4();
    b.bind(&sockaddr_in(LO, 20001), &user()).unwrap();
    assert_eq!(b.sockname(), Sockaddr::v4(LO, 20001));
    let n = a
        .send_dgram(b"hello", Some(&sockaddr_in(LO, 20001)), &user())
        .unwrap();
    assert_eq!(n, 5);
    // The sender got an ephemeral port from the default range.
    let from = a.sockname();
    assert!((10000..=65535).contains(&from.port));
    let r = b.recv_dgram(100, false).unwrap();
    assert_eq!(r.data, b"hello");
    assert_eq!(r.from, Some(Sockaddr::v4(LO, from.port)));
    assert_eq!(b.recv_dgram(100, false).unwrap_err(), Errno::EAGAIN);
    // Truncation.
    a.send_dgram(b"0123456789", Some(&sockaddr_in(LO, 20001)), &user())
        .unwrap();
    let r = b.recv_dgram(4, false).unwrap();
    assert_eq!(r.data, b"0123");
    assert_eq!(r.full, 10);
    assert!(r.flags & abi::socket::MSG_TRUNC != 0);
    a.close();
    b.close();
}

#[test]
fn udp_bind_conflicts_and_reuse() {
    let _g = setup();
    let a = udp4();
    let b = udp4();
    a.bind(&sockaddr_in([0; 4], 20010), &user()).unwrap();
    assert_eq!(
        b.bind(&sockaddr_in(LO, 20010), &user()),
        Err(Errno::EADDRINUSE)
    );
    // Bound twice is EINVAL.
    assert_eq!(a.bind(&sockaddr_in(LO, 20011), &user()), Err(Errno::EINVAL));
    // Not a local address.
    assert_eq!(
        b.bind(&sockaddr_in([10, 0, 0, 1], 20012), &user()),
        Err(Errno::EADDRNOTAVAIL)
    );
    // A reserved port needs privilege.
    assert_eq!(b.bind(&sockaddr_in(LO, 514), &user()), Err(Errno::EACCES));
    let c = Socket::new(Family::Inet, Proto::Udp, &root());
    c.bind(&sockaddr_in([0; 4], 514), &root()).unwrap();
    // SO_REUSEPORT on both shares a port.
    let d = udp4();
    let e = udp4();
    set_int(&d, SOL_SOCKET, SO_REUSEPORT, 1);
    set_int(&e, SOL_SOCKET, SO_REUSEPORT, 1);
    d.bind(&sockaddr_in(LO, 20013), &user()).unwrap();
    e.bind(&sockaddr_in(LO, 20013), &user()).unwrap();
    for s in [a, b, c, d, e] {
        s.close();
    }
}

#[test]
fn udp_connected_gets_econnrefused() {
    let _g = setup();
    let a = udp4();
    a.connect(&sockaddr_in(LO, 20020), &user(), false).unwrap();
    assert!(a.is_connected());
    assert_eq!(a.send_dgram(b"x", None, &user()), Ok(1));
    // The ICMP port unreachable shows on the next call.
    assert_eq!(a.recv_dgram(10, false).unwrap_err(), Errno::ECONNREFUSED);
    assert_eq!(a.recv_dgram(10, false).unwrap_err(), Errno::EAGAIN);
    // Sending to an address while connected.
    assert_eq!(
        a.send_dgram(b"x", Some(&sockaddr_in(LO, 1)), &user()),
        Err(Errno::EISCONN)
    );
    // Unconnected without an address.
    let b = udp4();
    assert_eq!(b.send_dgram(b"x", None, &user()), Err(Errno::EDESTADDRREQ));
    // No route.
    assert_eq!(
        b.send_dgram(b"x", Some(&sockaddr_in([10, 1, 1, 1], 9)), &user()),
        Err(Errno::ENETUNREACH)
    );
    a.close();
    b.close();
}

#[test]
fn udp_v6_and_mapped() {
    let _g = setup();
    let six = Socket::new(Family::Inet6, Proto::Udp, &user());
    // v6only by default: the IPv4 socket gets nothing.
    six.bind(&sockaddr_in6(&[0; 16], 20030, 0), &user())
        .unwrap();
    let four = udp4();
    four.send_dgram(b"v4", Some(&sockaddr_in(LO, 20030)), &user())
        .unwrap();
    assert_eq!(six.recv_dgram(10, false).unwrap_err(), Errno::EAGAIN);
    // And IPv4 can bind the same port.
    let four2 = udp4();
    four2.bind(&sockaddr_in([0; 4], 20030), &user()).unwrap();
    // With IPV6_V6ONLY off, a dual socket gets IPv4, from a mapped name.
    let dual = Socket::new(Family::Inet6, Proto::Udp, &user());
    set_int(&dual, IPPROTO_IPV6, ipv6::IPV6_V6ONLY, 0);
    dual.bind(&sockaddr_in6(&[0; 16], 20031, 0), &user())
        .unwrap();
    four.send_dgram(b"m", Some(&sockaddr_in(LO, 20031)), &user())
        .unwrap();
    let r = dual.recv_dgram(10, false).unwrap();
    assert_eq!(r.data, b"m");
    let f = r.from.unwrap();
    assert_eq!(f.family, Family::Inet6);
    assert_eq!(f.addr, v4mapped(LO));
    // ::1 to ::1.
    let a6 = Socket::new(Family::Inet6, Proto::Udp, &user());
    a6.send_dgram(b"six", Some(&sockaddr_in6(&LOOPBACK6, 20030, 0)), &user())
        .unwrap();
    let r = six.recv_dgram(10, false).unwrap();
    assert_eq!(r.data, b"six");
    assert_eq!(r.from.unwrap().addr, LOOPBACK6);
    for s in [six, four, four2, dual, a6] {
        s.close();
    }
}

fn listener(port: u16) -> Arc<Socket> {
    let l = tcp4();
    set_int(&l, SOL_SOCKET, SO_REUSEADDR, 1);
    l.bind(&sockaddr_in(LO, port), &user()).unwrap();
    l.listen(5, &user()).unwrap();
    l
}

fn connected(port: u16) -> (Arc<Socket>, Arc<Socket>, Arc<Socket>) {
    let l = listener(port);
    let c = tcp4();
    c.connect(&sockaddr_in(LO, port), &user(), false).unwrap();
    let s = l.accept().unwrap();
    (l, c, s)
}

#[test]
fn tcp_connect_refused_and_established() {
    let _g = setup();
    let c = tcp4();
    assert_eq!(
        c.connect(&sockaddr_in(LO, 20040), &user(), false),
        Err(Errno::ECONNREFUSED)
    );
    // Reset: it connects no more.
    assert_eq!(
        c.connect(&sockaddr_in(LO, 20040), &user(), false),
        Err(Errno::ECONNREFUSED)
    );
    let (l, c2, s) = connected(20041);
    assert_eq!(c2.tcp_state(), Tstate::Established);
    assert_eq!(s.tcp_state(), Tstate::Established);
    assert_eq!(s.sockname(), Sockaddr::v4(LO, 20041));
    assert_eq!(s.peername().unwrap(), c2.sockname());
    assert_eq!(c2.peername().unwrap(), Sockaddr::v4(LO, 20041));
    assert_eq!(
        c2.connect(&sockaddr_in(LO, 20041), &user(), false),
        Err(Errno::EISCONN)
    );
    assert_eq!(l.accept().unwrap_err(), Errno::EAGAIN);
    // Nonblocking: EINPROGRESS, then connected.
    let c3 = tcp4();
    assert_eq!(
        c3.connect(&sockaddr_in(LO, 20041), &user(), true),
        Err(Errno::EINPROGRESS)
    );
    assert_eq!(c3.connect_done(), Ok(()));
    let s3 = l.accept().unwrap();
    for x in [c, c2, s, c3, s3, l] {
        x.close();
    }
}

#[test]
fn tcp_data_half_close_and_time_wait() {
    let _g = setup();
    let (l, c, s) = connected(20050);
    assert_eq!(c.send_stream(b"hello world", false), Ok(11));
    let r = s.recv_stream(5, false, false).unwrap();
    assert_eq!(r.data, b"hello");
    assert_eq!(s.nread(), 6);
    // Half-close: the server reads the rest, then the end.
    c.shutdown(SHUT_WR).unwrap();
    assert_eq!(c.send_stream(b"x", false), Err(Errno::EPIPE));
    assert_eq!(s.recv_stream(100, false, false).unwrap().data, b" world");
    assert!(s.recv_stream(100, false, false).unwrap().data.is_empty());
    assert_eq!(s.tcp_state(), Tstate::CloseWait);
    assert_eq!(c.tcp_state(), Tstate::FinWait2);
    // The other way still works.
    assert_eq!(s.send_stream(b"back", false), Ok(4));
    assert_eq!(c.recv_stream(100, false, false).unwrap().data, b"back");
    let cport = c.sockname().port;
    s.close();
    // The active closer waits in TIME_WAIT, holding its port.
    assert_eq!(c.tcp_state(), Tstate::TimeWait);
    assert!(c.recv_stream(100, false, false).unwrap().data.is_empty());
    c.close();
    let d = tcp4();
    assert_eq!(
        d.bind(&sockaddr_in(LO, cport), &user()),
        Err(Errno::EADDRINUSE)
    );
    let e = tcp4();
    set_int(&e, SOL_SOCKET, SO_REUSEADDR, 1);
    e.bind(&sockaddr_in(LO, cport), &user()).unwrap();
    d.close();
    e.close();
    l.close();
}

#[test]
fn tcp_flow_control_and_window() {
    let _g = setup();
    let (l, c, s) = connected(20060);
    let big = alloc::vec![7u8; 200_000];
    let mut sent = 0;
    loop {
        match c.send_stream(&big[sent..], false) {
            Ok(n) => sent += n,
            Err(Errno::EAGAIN) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    // The receive buffer and the send buffer, full.
    let rcv =
        usize::try_from(tune::TCP_RECVSPACE.load(core::sync::atomic::Ordering::Relaxed)).unwrap();
    let snd =
        usize::try_from(tune::TCP_SENDSPACE.load(core::sync::atomic::Ordering::Relaxed)).unwrap();
    assert_eq!(sent, rcv + snd);
    assert_eq!(c.nwrite(), snd);
    let mut got = 0;
    while got < sent {
        let r = s.recv_stream(10_000, false, false).unwrap();
        assert!(!r.data.is_empty());
        got += r.data.len();
    }
    assert_eq!(c.nwrite(), 0);
    for x in [c, s, l] {
        x.close();
    }
}

#[test]
fn tcp_close_with_linger_zero_resets() {
    let _g = setup();
    let (l, c, s) = connected(20070);
    let lv = [1i32.to_ne_bytes(), 0i32.to_ne_bytes()].concat();
    c.setopt(SOL_SOCKET, SO_LINGER, &lv, &user()).unwrap();
    c.close();
    assert_eq!(
        s.recv_stream(10, false, false).unwrap_err(),
        Errno::ECONNRESET
    );
    assert_eq!(s.send_stream(b"x", false), Err(Errno::EPIPE));
    s.close();
    l.close();
}

#[test]
fn tcp_write_after_peer_close_resets() {
    let _g = setup();
    let (l, c, s) = connected(20080);
    s.close();
    // The first write goes; the RST it draws ends the connection.
    assert_eq!(c.send_stream(b"a", false), Ok(1));
    assert_eq!(c.send_stream(b"b", false), Err(Errno::EPIPE));
    assert_eq!(
        c.recv_stream(10, false, false).unwrap_err(),
        Errno::ECONNRESET
    );
    c.close();
    l.close();
}

#[test]
fn tcp_listen_backlog_and_close_aborts_queue() {
    let _g = setup();
    let l = tcp4();
    l.bind(&sockaddr_in(LO, 20090), &user()).unwrap();
    l.listen(0, &user()).unwrap();
    // Backlog 0: one complete connection (`3 * 0 / 2`), then waiting.
    let a = tcp4();
    a.connect(&sockaddr_in(LO, 20090), &user(), false).unwrap();
    let b = tcp4();
    assert_eq!(
        b.connect(&sockaddr_in(LO, 20090), &user(), true),
        Err(Errno::EINPROGRESS)
    );
    assert_eq!(b.connect_done(), Err(Errno::EAGAIN));
    assert_eq!(b.tcp_state(), Tstate::SynSent);
    // Accepting lets the waiting one in.
    let sa = l.accept().unwrap();
    assert_eq!(b.connect_done(), Ok(()));
    l.close();
    // The queued connection is reset.
    assert_eq!(
        b.recv_stream(10, false, false).unwrap_err(),
        Errno::ECONNRESET
    );
    for x in [a, b, sa] {
        x.close();
    }
}

#[test]
fn tcp_v6_v4mapped_and_oob() {
    let _g = setup();
    let l = Socket::new(Family::Inet6, Proto::Tcp, &user());
    set_int(&l, IPPROTO_IPV6, ipv6::IPV6_V6ONLY, 0);
    l.bind(&sockaddr_in6(&[0; 16], 20100, 0), &user()).unwrap();
    l.listen(-1, &user()).unwrap();
    let c = tcp4();
    c.connect(&sockaddr_in(LO, 20100), &user(), false).unwrap();
    let s = l.accept().unwrap();
    assert_eq!(s.peername().unwrap().addr, v4mapped(LO));
    assert_eq!(s.sockname().addr, v4mapped(LO));
    // Urgent data: the byte leaves the stream, the mark before it.
    c.send_stream(b"ab", false).unwrap();
    c.send_stream(b"!", true).unwrap();
    assert!(!s.at_mark());
    assert_eq!(s.recv_stream(10, false, false).unwrap().data, b"ab");
    assert!(s.at_mark());
    assert_eq!(s.recv_stream(1, false, true).unwrap().data, b"!");
    assert_eq!(s.recv_stream(1, false, true).unwrap_err(), Errno::EINVAL);
    set_int(&s, SOL_SOCKET, SO_OOBINLINE, 1);
    for x in [c, s, l] {
        x.close();
    }
}

#[test]
fn pcblist_shapes() {
    let _g = setup();
    let (l, c, s) = connected(20110);
    let all = |_| true;
    let tcp = crate::export::tcp_pcblist(&all);
    let gen_ = size_of::<abi::inet::Xinpgen>();
    let rec = size_of::<abi::inet::Xtcpcb>();
    assert_eq!((tcp.len() - 2 * gen_) % rec, 0);
    let n = (tcp.len() - 2 * gen_) / rec;
    assert!(n >= 3);
    let udp = crate::export::udp_pcblist(&all);
    assert_eq!((udp.len() - 2 * gen_) % size_of::<abi::inet::Xinpcb>(), 0);
    for x in [c, s, l] {
        x.close();
    }
    let _ = (&TCB, &UDB);
}

/// A netlink request: header, fixed part, attributes.
fn nlreq(ty: u16, flags: u16, seq: u32, fixed: &[u8], attrs: &[(u16, &[u8])]) -> std::vec::Vec<u8> {
    let mut m = std::vec::Vec::new();
    m.extend_from_slice(&0u32.to_ne_bytes());
    m.extend_from_slice(&ty.to_ne_bytes());
    m.extend_from_slice(&(flags | crate::netlink::nlm::REQUEST).to_ne_bytes());
    m.extend_from_slice(&seq.to_ne_bytes());
    m.extend_from_slice(&0u32.to_ne_bytes());
    m.extend_from_slice(fixed);
    for (t, d) in attrs {
        let len = u16::try_from(4 + d.len()).unwrap();
        m.extend_from_slice(&len.to_ne_bytes());
        m.extend_from_slice(&t.to_ne_bytes());
        m.extend_from_slice(d);
        m.resize(m.len().div_ceil(4) * 4, 0);
    }
    let len = u32::try_from(m.len()).unwrap();
    m[0..4].copy_from_slice(&len.to_ne_bytes());
    m
}

/// The (type, flags, error-or-0) of each reply message.
fn nlreplies(b: &[u8]) -> std::vec::Vec<(u16, u16, i32)> {
    let mut out = std::vec::Vec::new();
    let mut at = 0;
    while at + 16 <= b.len() {
        let len = u32::from_ne_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        let ty = u16::from_ne_bytes(b[at + 4..at + 6].try_into().unwrap());
        let fl = u16::from_ne_bytes(b[at + 6..at + 8].try_into().unwrap());
        let e = if ty == crate::netlink::ty::ERROR || ty == crate::netlink::ty::DONE {
            i32::from_ne_bytes(b[at + 16..at + 20].try_into().unwrap())
        } else {
            0
        };
        out.push((ty, fl, e));
        at += len.div_ceil(4) * 4;
    }
    out
}

#[test]
fn netlink_links_addresses_and_routes() {
    use crate::netlink::{Ctx, handle, nlm, ty};
    let _g = setup();
    let ctx = Ctx {
        port: 99,
        ext_ack: true,
        may_addaddr: true,
        may_deladdr: true,
        may_route: true,
        ..Ctx::default()
    };
    // ifconfig's link dump: lo0, then the end (no separate ack).
    let r = handle(
        &ctx,
        &nlreq(ty::GETLINK, nlm::DUMP | nlm::ACK, 1, &[0; 16], &[]),
        0,
    );
    let kinds = nlreplies(&r);
    assert_eq!(kinds.first().map(|k| k.0), Some(ty::NEWLINK));
    assert_eq!(
        kinds.last(),
        Some(&(
            ty::DONE,
            nlm::DUMP | nlm::ACK | nlm::REQUEST | nlm::MULTI,
            0
        ))
    );
    assert!(r.windows(4).any(|w| w == b"lo0\0"));
    // One link by name, acknowledged.
    let r = handle(
        &ctx,
        &nlreq(
            ty::GETLINK,
            nlm::ACK,
            2,
            &[0; 16],
            &[(abi::netlink::IFLA_IFNAME, b"lo0\0")],
        ),
        0,
    );
    assert_eq!(
        nlreplies(&r),
        [
            (ty::NEWLINK, nlm::ACK | nlm::REQUEST, 0),
            (ty::ERROR, nlm::CAPPED, 0)
        ]
    );
    let r = handle(
        &ctx,
        &nlreq(
            ty::GETLINK,
            nlm::ACK,
            3,
            &[0; 16],
            &[(abi::netlink::IFLA_IFNAME, b"em0\0")],
        ),
        0,
    );
    assert_eq!(nlreplies(&r), [(ty::ERROR, 0, Errno::ENODEV.get())]);
    // The IPv4 addresses of lo0.
    let mut ifa = [0u8; 8];
    ifa[0] = abi::inet::AF_INET;
    ifa[4..8].copy_from_slice(&1u32.to_ne_bytes());
    let r = handle(&ctx, &nlreq(ty::GETADDR, nlm::DUMP, 4, &ifa, &[]), 0);
    assert_eq!(
        nlreplies(&r).iter().filter(|k| k.0 == ty::NEWADDR).count(),
        1
    );
    assert_eq!(r[17], 8, "prefix length");
    // ifconfig lo0 inet 127.0.0.2/32 alias, then -alias.
    let mut ifa2 = ifa;
    ifa2[1] = 32;
    let add = nlreq(
        ty::NEWADDR,
        nlm::ACK,
        5,
        &ifa2,
        &[(abi::netlink::IFA_LOCAL, &[127, 0, 0, 2])],
    );
    assert_eq!(
        nlreplies(&handle(&ctx, &add, 0)),
        [(ty::ERROR, nlm::CAPPED, 0)]
    );
    assert!(
        NET.lock()
            .is_local(Family::Inet, &addr::from_v4([127, 0, 0, 2]), 0)
    );
    let del = nlreq(
        ty::DELADDR,
        nlm::ACK,
        6,
        &ifa2,
        &[(abi::netlink::IFA_LOCAL, &[127, 0, 0, 2])],
    );
    assert_eq!(
        nlreplies(&handle(&ctx, &del, 0)),
        [(ty::ERROR, nlm::CAPPED, 0)]
    );
    assert!(
        !NET.lock()
            .is_local(Family::Inet, &addr::from_v4([127, 0, 0, 2]), 0)
    );
    // Without the privilege.
    let user = Ctx {
        port: 98,
        ..Ctx::default()
    };
    assert_eq!(nlreplies(&handle(&user, &add, 0))[0].2, Errno::EPERM.get());
    // route add -inet6 ::ffff:0.0.0.0 -prefixlen 96 ::1 -reject: once.
    let mut rtm = [0u8; 12];
    rtm[0] = abi::inet::AF_INET6;
    rtm[1] = 96;
    rtm[7] = abi::netlink::RTN_PROHIBIT;
    let dst = v4mapped([0; 4]);
    let one = LOOPBACK6;
    let route = nlreq(
        ty::NEWROUTE,
        nlm::ACK | nlm::CREATE,
        7,
        &rtm,
        &[
            (abi::netlink::NL_RTA_DST, &dst),
            (abi::netlink::NL_RTA_GATEWAY, &one),
        ],
    );
    assert_eq!(
        nlreplies(&handle(&ctx, &route, 0)),
        [(ty::ERROR, nlm::CAPPED, 0)]
    );
    assert_eq!(
        nlreplies(&handle(&ctx, &route, 0))[0].2,
        Errno::EEXIST.get()
    );
    assert_eq!(
        NET.lock().lookup(Family::Inet6, &v4mapped([10, 0, 0, 1])),
        Err(crate::ifnet::NoRoute::Rejected)
    );
    // route get 127.0.0.1.
    let mut get = [0u8; 12];
    get[0] = abi::inet::AF_INET;
    let r = handle(
        &ctx,
        &nlreq(
            ty::GETROUTE,
            0,
            8,
            &get,
            &[(abi::netlink::NL_RTA_DST, &[127, 0, 0, 1])],
        ),
        0,
    );
    assert_eq!(nlreplies(&r)[0].0, ty::NEWROUTE);
    // An unknown type.
    assert_eq!(
        nlreplies(&handle(&ctx, &nlreq(99, 0, 9, &[], &[]), 0))[0].2,
        Errno::EOPNOTSUPP.get()
    );
    let mut del = rtm;
    del[7] = 0;
    let unroute = nlreq(
        ty::DELROUTE,
        nlm::ACK,
        10,
        &del,
        &[(abi::netlink::NL_RTA_DST, &dst)],
    );
    assert_eq!(
        nlreplies(&handle(&ctx, &unroute, 0)),
        [(ty::ERROR, nlm::CAPPED, 0)]
    );
}

#[test]
fn iflist_records() {
    let _g = setup();
    let n = NET.lock();
    let b = crate::rtmsg::iflist(&n, 0, 0, true);
    drop(n);
    // Each record: its length (aligned), version 5, a type.
    let mut at = 0;
    let mut types = std::vec::Vec::new();
    while at < b.len() {
        let len = usize::from(u16::from_ne_bytes([b[at], b[at + 1]]));
        assert_eq!(len % 8, 0);
        assert_eq!(b[at + 2], abi::net::RTM_VERSION);
        types.push(b[at + 3]);
        at += len;
    }
    assert_eq!(at, b.len());
    assert_eq!(types[0], abi::net::rtm::IFINFO);
    assert!(types[1..].iter().all(|&t| t == abi::net::rtm::NEWADDR));
    assert_eq!(types.len(), 4, "the link and three addresses");
    // The routing table: the host route to 127.0.0.1 among them.
    let n = NET.lock();
    let d = crate::rtmsg::dump(&n, abi::inet::AF_INET);
    assert!(!d.is_empty());
    assert_eq!(d[3], abi::net::rtm::GET);
}
