// SPDX-License-Identifier: BSD-2-Clause

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::net::iff;
use mbuf::{Chain, Packet};

use crate::clone::{self, Ctx, name2unit};
use crate::link::gen_addr;
use crate::netisr::{self, Proto};
use crate::{Family, Vnet};

fn ctx() -> Ctx {
    Ctx {
        now: 0,
        hostuuid: "6b0c3f6e-1111-2222-3333-444455556666".into(),
        jail: "0".into(),
        random: [7; 12],
    }
}

fn packet(b: &[u8]) -> Packet {
    Packet {
        hdr: mbuf::PktHdr::default(),
        data: Chain::from_slice(b).unwrap(),
    }
}

fn up(v: &Vnet, name: &[u8]) {
    let f = v.net.lock().by_name(name).unwrap().flags;
    v.net.lock().set_flags(name, f | iff::UP).unwrap();
}

#[test]
fn units_parse_as_freebsd() {
    assert_eq!(name2unit(b"lo"), Ok(None));
    assert_eq!(name2unit(b"lo12"), Ok(Some(12)));
    assert_eq!(name2unit(b"lo0"), Ok(Some(0)));
    assert_eq!(name2unit(b"lo01"), Err(Errno::EINVAL));
    assert_eq!(name2unit(b"lo1x"), Err(Errno::EINVAL));
    assert_eq!(name2unit(b"lo99999999999"), Err(Errno::EINVAL));
}

#[test]
fn lo_clones_take_the_lowest_unit() {
    let v = Vnet::new(0);
    assert_eq!(clone::create(&v, b"lo", &ctx()).unwrap(), "lo1");
    assert_eq!(clone::create(&v, b"lo5", &ctx()).unwrap(), "lo5");
    assert_eq!(clone::create(&v, b"lo", &ctx()).unwrap(), "lo2");
    assert_eq!(clone::create(&v, b"lo5", &ctx()), Err(Errno::EEXIST));
    assert_eq!(clone::create(&v, b"lo0", &ctx()), Err(Errno::EEXIST));
    assert_eq!(clone::create(&v, b"bogus", &ctx()), Err(Errno::EINVAL));
    assert_eq!(clone::create(&v, b"lo40000", &ctx()), Err(Errno::ENOSPC));
    assert_eq!(clone::destroy(&v, b"lo0").unwrap_err(), Errno::EINVAL);
    assert_eq!(clone::destroy(&v, b"lo9").unwrap_err(), Errno::ENXIO);
    clone::destroy(&v, b"lo1").unwrap();
    assert_eq!(clone::create(&v, b"lo", &ctx()).unwrap(), "lo1");
    let n = v.net.lock();
    let lo2 = n.by_name(b"lo2").unwrap();
    assert_eq!(
        (lo2.dname, lo2.dunit, lo2.flags),
        ("lo", 2, iff::LOOPBACK | iff::MULTICAST)
    );
}

#[test]
fn epairs_come_in_pairs() {
    let v = Vnet::new(0);
    assert_eq!(clone::create(&v, b"epair", &ctx()).unwrap(), "epair0a");
    assert_eq!(clone::create(&v, b"epair", &ctx()).unwrap(), "epair1a");
    assert_eq!(clone::create(&v, b"epair0a", &ctx()), Err(Errno::EEXIST));
    assert_eq!(clone::create(&v, b"epair7b", &ctx()), Err(Errno::EINVAL));
    {
        let n = v.net.lock();
        let (a, b) = (
            n.by_name(b"epair0a").unwrap(),
            n.by_name(b"epair0b").unwrap(),
        );
        assert_eq!(a.lladdr, gen_addr(&ctx().hostuuid, "epair0a", "0"));
        assert_eq!(a.lladdr.unwrap()[..4], [0x58, 0x9c, 0xfc, 0x10]);
        assert_ne!(a.lladdr, b.lladdr);
        assert_eq!(a.mtu, 1500);
        assert!(a.flags & iff::RUNNING != 0 && a.flags & iff::UP == 0);
    }
    // Destroying either end destroys both.
    let gone = clone::destroy(&v, b"epair0b").unwrap();
    assert_eq!(gone.len(), 2);
    assert!(v.net.lock().by_name(b"epair0a").is_none());
    assert_eq!(clone::create(&v, b"epair", &ctx()).unwrap(), "epair0a");
    // With the default host UUID the address is random.
    let mut c = ctx();
    c.hostuuid = "00000000-0000-0000-0000-000000000000".into();
    clone::create(&v, b"epair9", &c).unwrap();
    let a = v.net.lock().by_name(b"epair9a").unwrap().lladdr.unwrap();
    assert_eq!(a, [0x06, 7, 7, 7, 7, 7]);
}

#[test]
fn frames_cross_an_epair() {
    let v = Vnet::new(0);
    clone::create(&v, b"epair", &ctx()).unwrap();
    let (ia, ib) = {
        let n = v.net.lock();
        (
            n.by_name(b"epair0a").unwrap().index,
            n.by_name(b"epair0b").unwrap().index,
        )
    };
    // Down: refused.
    assert_eq!(
        clone::output(&v, ia, Family::Inet, packet(&[1; 60])),
        Err(Errno::ENETDOWN)
    );
    up(&v, b"epair0a");
    // The other end down: dropped.
    assert_eq!(
        clone::output(&v, ia, Family::Inet, packet(&[1; 60])),
        Err(Errno::ENOBUFS)
    );
    up(&v, b"epair0b");
    clone::output(&v, ia, Family::Inet, packet(&[2; 60])).unwrap();
    assert_eq!(v.netisr.len(), 1);
    // No Ethernet input yet: drained and dropped.
    assert_eq!(netisr::drain(&v), 1);
    let n = v.net.lock();
    let (a, b) = (n.by_index(ia).unwrap(), n.by_index(ib).unwrap());
    assert_eq!(a.stats.opackets.load(Ordering::Relaxed), 1);
    assert_eq!(a.stats.oerrors.load(Ordering::Relaxed), 1);
    assert_eq!(b.stats.ibytes.load(Ordering::Relaxed), 60);
}

#[test]
fn loopback_output_comes_back_through_netisr() {
    static SEEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    fn ip6(_: &Arc<Vnet>, m: Packet) {
        assert_eq!(m.hdr.rcvif, 1);
        SEEN.fetch_add(m.len(), Ordering::Relaxed);
    }
    netisr::register(Proto::Ipv6, ip6);
    let v = Vnet::new(0);
    up(&v, b"lo0");
    clone::output(&v, 1, Family::Inet6, packet(&[0; 40])).unwrap();
    assert_eq!(netisr::drain(&v), 1);
    assert_eq!(SEEN.load(Ordering::Relaxed), 40);
    assert_eq!(v.lo.opackets.load(Ordering::Relaxed), 1);
    assert_eq!(v.lo.ipackets.load(Ordering::Relaxed), 1);
}

#[test]
fn interfaces_move_between_vnets_and_go_home() {
    let host = Vnet::new(0);
    let jail = Vnet::new(0);
    clone::create(&host, b"epair", &ctx()).unwrap();
    up(&host, b"epair0a");
    up(&host, b"epair0b");
    clone::vmove(&host, b"epair0b", &jail).unwrap();
    assert!(host.net.lock().by_name(b"epair0b").is_none());
    let (ib, flags) = {
        let n = jail.net.lock();
        let b = n.by_name(b"epair0b").unwrap();
        (b.index, b.flags)
    };
    // lo0 is 1 there; it came in down.
    assert_eq!((ib, flags & iff::UP), (2, 0));
    assert_eq!(clone::vmove(&host, b"epair0b", &jail), Err(Errno::ENXIO));
    // Frames follow it: the jail's end gets them once up.
    up(&jail, b"epair0b");
    let ia = host.net.lock().by_name(b"epair0a").unwrap().index;
    clone::output(&host, ia, Family::Inet, packet(&[3; 64])).unwrap();
    assert_eq!((host.netisr.len(), jail.netisr.len()), (0, 1));
    // A clone made inside, and the jail's lo0, go with it; epair0b goes
    // home.
    clone::create(&jail, b"epair", &ctx()).unwrap();
    clone::shutdown(&jail);
    assert!(jail.net.lock().ifaces.is_empty());
    assert!(host.net.lock().by_name(b"epair0b").is_some());
    assert!(host.net.lock().by_name(b"epair1a").is_none());
}

mod rtsock {
    use alloc::vec::Vec;

    use abi::net::{RTM_VERSION, RtMsghdr, rta, rtf, rtm};

    use super::*;
    use crate::addr::from_v4;
    use crate::ifnet::{INFINITE, Ifaddr};

    fn sin(a: [u8; 4]) -> Vec<u8> {
        let mut v = alloc::vec![0u8; 16];
        v[0] = 16;
        v[1] = abi::inet::AF_INET;
        v[4..8].copy_from_slice(&a);
        v
    }

    fn msg(ty: u8, flags: i32, sas: &[(i32, Vec<u8>)]) -> Vec<u8> {
        let mut h = RtMsghdr {
            rtm_version: RTM_VERSION,
            rtm_type: ty,
            rtm_flags: flags,
            rtm_seq: 42,
            ..RtMsghdr::default()
        };
        let mut body = Vec::new();
        for (bit, sa) in sas {
            h.rtm_addrs |= bit;
            body.extend_from_slice(sa);
            body.resize(body.len().div_ceil(8) * 8, 0);
        }
        h.rtm_msglen = u16::try_from(size_of::<RtMsghdr>() + body.len()).unwrap();
        let mut v = Vec::new();
        crate::rtmsg::put(&mut v, &h);
        v.extend(body);
        v
    }

    fn hdr(v: &[u8]) -> RtMsghdr {
        // SAFETY: a reply holds a whole header.
        unsafe { core::ptr::read_unaligned(v.as_ptr().cast()) }
    }

    fn setup() -> (Arc<Vnet>, u16) {
        let v = Vnet::new(0);
        clone::create(&v, b"epair", &ctx()).unwrap();
        let a = Ifaddr {
            family: Family::Inet,
            addr: from_v4([192, 0, 2, 1]),
            plen: 24,
            dst: None,
            broadcast: Some(from_v4([192, 0, 2, 255])),
            flags6: 0,
            vltime: INFINITE,
            pltime: INFINITE,
            vhid: 0,
        };
        v.net.lock().add_addr(b"epair0a", a).unwrap();
        let i = v.net.lock().by_name(b"epair0a").unwrap().index;
        (v, i)
    }

    #[test]
    fn get_an_interface_prefix() {
        let (v, index) = setup();
        let m = msg(
            rtm::GET,
            0,
            &[
                (rta::DST, sin([192, 0, 2, 0])),
                (rta::NETMASK, sin([255, 255, 255, 0])),
            ],
        );
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
        r.unwrap();
        let h = hdr(&reply);
        assert_eq!(
            (h.rtm_type, h.rtm_errno, h.rtm_pid, h.rtm_seq),
            (rtm::GET, 0, 7, 42)
        );
        assert_eq!(h.rtm_flags, rtf::UP | rtf::DONE | rtf::PINNED);
        assert_eq!(h.rtm_index, index);
        assert_eq!(h.rtm_addrs, rta::DST | rta::GATEWAY | rta::NETMASK);
        // The gateway is `link#N`: a bare `sockaddr_dl`.
        let gw = &reply[size_of::<RtMsghdr>() + 16..][..54];
        assert_eq!((gw[0], gw[1], gw[4], gw[5]), (54, abi::inet::AF_LINK, 6, 0));
        assert_eq!(u16::from_ne_bytes([gw[2], gw[3]]), index);
        // Longest prefix, and none.
        let m = msg(rtm::GET, 0, &[(rta::DST, sin([192, 0, 2, 77]))]);
        let (reply, _) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
        assert_eq!(hdr(&reply).rtm_index, index);
        let m = msg(rtm::GET, 0, &[(rta::DST, sin([198, 51, 100, 1]))]);
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
        assert_eq!(r, Err(Errno::ESRCH));
        assert_eq!(hdr(&reply).rtm_errno, Errno::ESRCH.get());
    }

    #[test]
    fn add_change_and_delete_a_gateway_route() {
        let (v, index) = setup();
        let net = sin([198, 51, 100, 0]);
        let mask = sin([255, 255, 255, 128]);
        let m = msg(
            rtm::ADD,
            rtf::GATEWAY | rtf::STATIC,
            &[
                (rta::DST, net.clone()),
                (rta::GATEWAY, sin([192, 0, 2, 254])),
                (rta::NETMASK, mask.clone()),
            ],
        );
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &m, 1, true, 0);
        r.unwrap();
        let h = hdr(&reply);
        assert_eq!(
            h.rtm_flags,
            rtf::UP | rtf::DONE | rtf::GATEWAY | rtf::STATIC
        );
        assert_eq!(h.rtm_index, index);
        let (_, r) = crate::rtsock::request(&mut v.net.lock(), &m, 1, true, 0);
        assert_eq!(r, Err(Errno::EEXIST));
        // Unreachable gateway; and no privilege.
        let bad = msg(
            rtm::ADD,
            rtf::GATEWAY,
            &[
                (rta::DST, sin([203, 0, 113, 0])),
                (rta::GATEWAY, sin([10, 9, 9, 9])),
                (rta::NETMASK, mask.clone()),
            ],
        );
        assert_eq!(
            crate::rtsock::request(&mut v.net.lock(), &bad, 1, true, 0).1,
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            crate::rtsock::request(&mut v.net.lock(), &m, 1, false, 0).1,
            Err(Errno::EPERM)
        );
        // A host route without RTF_HOST.
        let host = msg(
            rtm::ADD,
            rtf::GATEWAY,
            &[
                (rta::DST, sin([203, 0, 113, 5])),
                (rta::GATEWAY, sin([192, 0, 2, 254])),
            ],
        );
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &host, 1, true, 0);
        r.unwrap();
        assert_eq!(
            hdr(&reply).rtm_flags,
            rtf::UP | rtf::DONE | rtf::GATEWAY | rtf::HOST
        );
        // Change the gateway, then delete.
        let ch = msg(
            rtm::CHANGE,
            rtf::GATEWAY,
            &[
                (rta::DST, net.clone()),
                (rta::GATEWAY, sin([192, 0, 2, 253])),
                (rta::NETMASK, mask.clone()),
            ],
        );
        crate::rtsock::request(&mut v.net.lock(), &ch, 1, true, 0)
            .1
            .unwrap();
        let del = msg(rtm::DELETE, 0, &[(rta::DST, net), (rta::NETMASK, mask)]);
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &del, 1, true, 0);
        r.unwrap();
        assert_eq!(hdr(&reply).rtm_type, rtm::DELETE);
        assert_eq!(
            crate::rtsock::request(&mut v.net.lock(), &del, 1, true, 0).1,
            Err(Errno::ESRCH)
        );
    }

    #[test]
    fn multipath_groups() {
        let (v, _) = setup();
        let net = sin([128, 66, 0, 0]);
        let mask = sin([255, 255, 255, 0]);
        let add = |gw: u8, w: u64, ty: u8| {
            let mut m = msg(
                ty,
                rtf::GATEWAY,
                &[
                    (rta::DST, net.clone()),
                    (rta::GATEWAY, sin([192, 0, 2, gw])),
                    (rta::NETMASK, mask.clone()),
                ],
            );
            let at = core::mem::offset_of!(RtMsghdr, rtm_inits);
            m[at..at + 8].copy_from_slice(&abi::net::rtv::WEIGHT.to_ne_bytes());
            let at = core::mem::offset_of!(RtMsghdr, rtm_rmx)
                + core::mem::offset_of!(abi::net::RtMetrics, rmx_weight);
            m[at..at + 8].copy_from_slice(&w.to_ne_bytes());
            crate::rtsock::request(&mut v.net.lock(), &m, 1, true, 0).1
        };
        add(10, 2, rtm::ADD).unwrap();
        add(5, 3, rtm::ADD).unwrap();
        assert_eq!(add(5, 3, rtm::ADD), Err(Errno::EEXIST));
        add(5, 4, rtm::CHANGE).unwrap();
        assert_eq!(add(7, 4, rtm::CHANGE), Err(Errno::ESRCH));
        let paths = |v: &Vnet| {
            let mut p: Vec<(u8, u32)> = v
                .net
                .lock()
                .routes
                .iter()
                .filter(|r| r.dst == from_v4([128, 66, 0, 0]))
                .map(|r| match r.gateway {
                    crate::ifnet::Gateway::Addr(g) => (g[15], r.weight),
                    crate::ifnet::Gateway::Link(_) => (0, r.weight),
                })
                .collect();
            p.sort_unstable();
            p
        };
        assert_eq!(paths(&v), [(5, 4), (10, 2)]);
        add(10, 2, rtm::DELETE).unwrap();
        assert_eq!(paths(&v), [(5, 4)]);
        // The dump lists each path.
        add(11, 1, rtm::ADD).unwrap();
        let d = crate::rtmsg::dump(&v.net.lock(), abi::inet::AF_INET, Some(0));
        let mut at = 0;
        let mut seen = 0;
        while at < d.len() {
            let h = hdr(&d[at..]);
            if h.rtm_flags & rtf::GATEWAY != 0 {
                seen += 1;
            }
            at += usize::from(h.rtm_msglen);
        }
        assert_eq!(seen, 2);
    }

    #[test]
    fn changes_are_announced_after_the_lock() {
        struct Rec(std::sync::Mutex<Vec<u8>>);
        impl crate::announce::Listener for Rec {
            fn event(&self, e: &crate::announce::Event) {
                if let Some((_, m)) = crate::rtsock::announce(e) {
                    self.0.lock().unwrap().push(m[3]);
                }
            }
        }
        let v = Vnet::new(0);
        let rec = Arc::new(Rec(std::sync::Mutex::new(Vec::new())));
        let l: Arc<dyn crate::announce::Listener> = rec.clone();
        v.net.listen(&l);
        clone::create(&v, b"epair", &ctx()).unwrap();
        let a = Ifaddr {
            family: Family::Inet,
            addr: from_v4([192, 0, 2, 1]),
            plen: 24,
            dst: None,
            broadcast: None,
            flags6: 0,
            vltime: INFINITE,
            pltime: INFINITE,
            vhid: 0,
        };
        v.net.lock().add_addr(b"epair0a", a).unwrap();
        let got = rec.0.lock().unwrap().clone();
        // Two arrivals; then up, the address, its prefix and local routes.
        assert_eq!(
            got,
            [
                rtm::IFANNOUNCE,
                rtm::IFANNOUNCE,
                rtm::IFINFO,
                rtm::NEWADDR,
                rtm::ADD,
                rtm::ADD
            ]
        );
    }
}

#[test]
fn a_prefix_route_moves_to_the_next_address() {
    use crate::addr::from_v4;
    use crate::ifnet::{INFINITE, Ifaddr};
    let v = Vnet::new(0);
    clone::create(&v, b"epair", &ctx()).unwrap();
    let a = |last: u8| Ifaddr {
        family: Family::Inet,
        addr: from_v4([192, 0, 2, last]),
        plen: 24,
        dst: None,
        broadcast: None,
        flags6: 0,
        vltime: INFINITE,
        pltime: INFINITE,
        vhid: 0,
    };
    v.net.lock().add_addr(b"epair0a", a(1)).unwrap();
    v.net.lock().add_addr(b"epair0b", a(2)).unwrap();
    let ib = v.net.lock().by_name(b"epair0b").unwrap().index;
    let prefix = |n: &crate::ifnet::Net| {
        n.routes
            .iter()
            .filter(|r| r.plen == 24)
            .map(|r| (r.ifindex, r.ifa))
            .collect::<alloc::vec::Vec<_>>()
    };
    assert_eq!(prefix(&v.net.lock()).len(), 1);
    v.net
        .lock()
        .delete_addr(b"epair0a", Family::Inet, Some(&from_v4([192, 0, 2, 1])))
        .unwrap();
    assert_eq!(prefix(&v.net.lock()), [(ib, from_v4([192, 0, 2, 2]))]);
}

#[test]
fn generic_netlink_has_its_controller() {
    use crate::netlink::{Ctx, handle_generic};
    let req = |flags: u16, name: Option<&[u8]>| {
        let mut m = alloc::vec::Vec::new();
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(&16u16.to_ne_bytes());
        m.extend_from_slice(&flags.to_ne_bytes());
        m.extend_from_slice(&7u32.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(&[3, 1, 0, 0]);
        if let Some(n) = name {
            let len = u16::try_from(4 + n.len() + 1).unwrap();
            m.extend_from_slice(&len.to_ne_bytes());
            m.extend_from_slice(&2u16.to_ne_bytes());
            m.extend_from_slice(n);
            m.push(0);
            m.resize(m.len().div_ceil(4) * 4, 0);
        }
        let l = u32::try_from(m.len()).unwrap();
        m[0..4].copy_from_slice(&l.to_ne_bytes());
        m
    };
    let ty = |r: &[u8]| u16::from_ne_bytes([r[4], r[5]]);
    // By name: the family, version 2.
    let r = handle_generic(&Ctx::default(), &req(1, Some(b"nlctrl")));
    assert_eq!((ty(&r), r[16], r[17]), (16, 1, 2));
    // Unknown: ENOENT.
    let r = handle_generic(&Ctx::default(), &req(1, Some(b"carp")));
    assert_eq!(ty(&r), 2);
    assert_eq!(
        i32::from_ne_bytes([r[16], r[17], r[18], r[19]]),
        Errno::ENOENT.get()
    );
    // A dump: the family, then the end.
    let r = handle_generic(&Ctx::default(), &req(1 | 0x300, None));
    let first = u32::from_ne_bytes([r[0], r[1], r[2], r[3]]) as usize;
    assert_eq!((ty(&r), ty(&r[first.div_ceil(4) * 4..])), (16, 3));
}

#[test]
fn netlink_notifications_name_their_groups() {
    use crate::announce::Event;
    use crate::netlink::{group, notify};
    let v = Vnet::new(0);
    clone::create(&v, b"epair", &ctx()).unwrap();
    let ifp = v.net.lock().by_name(b"epair0a").unwrap().clone();
    let (g, m) = notify(&Event::Info(ifp.clone()), 0).unwrap();
    assert_eq!((g, u16::from_ne_bytes([m[4], m[5]])), (group::LINK, 16));
    let (g, m) = notify(
        &Event::Announce {
            ifp,
            arrival: false,
        },
        0,
    )
    .unwrap();
    assert_eq!((g, u16::from_ne_bytes([m[4], m[5]])), (group::LINK, 17));
}
