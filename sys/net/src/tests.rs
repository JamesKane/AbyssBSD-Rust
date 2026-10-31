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
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true);
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
        let (reply, _) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true);
        assert_eq!(hdr(&reply).rtm_index, index);
        let m = msg(rtm::GET, 0, &[(rta::DST, sin([198, 51, 100, 1]))]);
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true);
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
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &m, 1, true);
        r.unwrap();
        let h = hdr(&reply);
        assert_eq!(
            h.rtm_flags,
            rtf::UP | rtf::DONE | rtf::GATEWAY | rtf::STATIC
        );
        assert_eq!(h.rtm_index, index);
        let (_, r) = crate::rtsock::request(&mut v.net.lock(), &m, 1, true);
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
            crate::rtsock::request(&mut v.net.lock(), &bad, 1, true).1,
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            crate::rtsock::request(&mut v.net.lock(), &m, 1, false).1,
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
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &host, 1, true);
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
        crate::rtsock::request(&mut v.net.lock(), &ch, 1, true)
            .1
            .unwrap();
        let del = msg(rtm::DELETE, 0, &[(rta::DST, net), (rta::NETMASK, mask)]);
        let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &del, 1, true);
        r.unwrap();
        assert_eq!(hdr(&reply).rtm_type, rtm::DELETE);
        assert_eq!(
            crate::rtsock::request(&mut v.net.lock(), &del, 1, true).1,
            Err(Errno::ESRCH)
        );
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
