// SPDX-License-Identifier: BSD-2-Clause
//! Ethernet, ARP, and the link-layer table between two VNETs joined by
//! an `epair`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::net::{RTM_VERSION, RtMsghdr, rta, rtf, rtm};
use mbuf::{Chain, Packet};

use crate::addr::{In6, from_v4};
use crate::arp::{self, Resolved};
use crate::clone::{self, Ctx};
use crate::ifnet::Ifaddr;
use crate::lltable::lle;
use crate::{Family, Vnet, netisr};

static NOW: AtomicU64 = AtomicU64::new(100);

fn clock() -> u64 {
    NOW.load(Ordering::Relaxed)
}

fn ctx() -> Ctx {
    Ctx {
        now: 0,
        hostuuid: "6b0c3f6e-1111-2222-3333-444455556666".into(),
        jail: "0".into(),
        random: [7; 12],
    }
}

fn v4(v: &Vnet, name: &[u8], last: u8) {
    let a = Ifaddr {
        family: Family::Inet,
        addr: from_v4([192, 0, 2, last]),
        plen: 24,
        dst: None,
        broadcast: Some(from_v4([192, 0, 2, 255])),
        flags6: 0,
        vltime: u32::MAX,
        pltime: u32::MAX,
        vhid: 0,
    };
    v.net.lock().add_addr(name, a).unwrap();
}

/// `epair0a` with 192.0.2.1 in one VNET, `epair0b` with 192.0.2.2 in
/// another, and what each queued for the other drained.
fn wire() -> (Arc<Vnet>, Arc<Vnet>, u16, u16) {
    crate::init();
    crate::set_uptime(clock);
    let v = Vnet::new(0);
    let j = Vnet::new(0);
    clone::create(&v, b"epair", &ctx()).unwrap();
    clone::vmove(&v, b"epair0b", &j).unwrap();
    v4(&v, b"epair0a", 1);
    v4(&j, b"epair0b", 2);
    let a = v.net.lock().by_name(b"epair0a").unwrap().index;
    let b = j.net.lock().by_name(b"epair0b").unwrap().index;
    settle(&v, &j);
    (v, j, a, b)
}

/// Delivers what is in flight until nothing is.
fn settle(v: &Arc<Vnet>, j: &Arc<Vnet>) {
    while netisr::drain(v) + netisr::drain(j) > 0 {}
}

fn lladdr(v: &Vnet, name: &[u8]) -> [u8; 6] {
    v.net.lock().by_name(name).unwrap().lladdr.unwrap()
}

fn ip4(a: [u8; 4]) -> In6 {
    from_v4(a)
}

#[test]
fn addresses_announce_and_are_permanent() {
    let (v, j, a, b) = wire();
    let la = lladdr(&v, b"epair0a");
    let n = v.net.lock();
    let e = n.lle(a, Family::Inet, &ip4([192, 0, 2, 1])).unwrap();
    assert_eq!(
        e.flags & (lle::IFADDR | lle::STATIC),
        lle::IFADDR | lle::STATIC
    );
    assert_eq!(e.lladdr, la);
    // Each end's gratuitous request reached the other: counted, learned
    // from nothing (it was not for the receiver's address).
    assert_eq!(n.arpstat.txrequests, 1);
    assert_eq!(n.arpstat.received, 1);
    drop(n);
    assert!(
        j.net
            .lock()
            .lle(b, Family::Inet, &ip4([192, 0, 2, 1]))
            .is_none()
    );
}

#[test]
fn resolution_asks_and_the_answer_releases_the_held_packet() {
    let (v, j, a, b) = wire();
    let pkt = Packet {
        hdr: mbuf::PktHdr::default(),
        data: Chain::from_slice(&[0x45; 20]).unwrap(),
    };
    let r = arp::resolve(
        &mut v.net.lock(),
        a,
        false,
        Some(pkt),
        [192, 0, 2, 2],
        clock(),
    );
    assert_eq!(r, Ok(Resolved::Held));
    let held_before = j.netisr.handled.load(Ordering::Relaxed);
    settle(&v, &j);
    // b learned a from the request for its address, and a learned b from
    // the reply.
    let lb = lladdr(&j, b"epair0b");
    let la = lladdr(&v, b"epair0a");
    {
        let n = v.net.lock();
        let e = n.lle(a, Family::Inet, &ip4([192, 0, 2, 2])).unwrap();
        assert_ne!(e.flags & lle::VALID, 0);
        assert_eq!(e.lladdr, lb);
        assert!(e.hold.is_empty());
        assert_eq!(n.arpstat.rxreplies, 1);
    }
    assert_eq!(
        j.net
            .lock()
            .lle(b, Family::Inet, &ip4([192, 0, 2, 1]))
            .unwrap()
            .lladdr,
        la
    );
    // The held IPv4 packet went: request, reply, and it.
    assert!(j.netisr.handled.load(Ordering::Relaxed) >= held_before + 2);
    // Now it resolves at once.
    let r = arp::resolve(&mut v.net.lock(), a, false, None, [192, 0, 2, 2], clock());
    assert_eq!(r, Ok(Resolved::Addr(lb)));
    // Off the network, nothing to make.
    let r = arp::resolve(
        &mut v.net.lock(),
        a,
        false,
        None,
        [198, 51, 100, 1],
        clock(),
    );
    assert_eq!(r, Err(Errno::EINVAL));
}

#[test]
fn unanswered_entries_give_up_and_age_out() {
    let (v, _j, a, _b) = wire();
    let dst = [192, 0, 2, 99];
    let t0 = clock();
    for k in 0..5u64 {
        let r = arp::resolve(&mut v.net.lock(), a, false, None, dst, t0 + k);
        assert_eq!(r, Ok(Resolved::Held));
    }
    // A sixth try within the same second sends nothing more.
    assert_eq!(
        arp::resolve(&mut v.net.lock(), a, true, None, dst, t0 + 4),
        Err(Errno::EHOSTUNREACH)
    );
    assert_eq!(
        arp::resolve(&mut v.net.lock(), a, false, None, dst, t0 + 5),
        Err(Errno::EHOSTDOWN)
    );
    arp::tick(&mut v.net.lock(), t0 + 5 + 20);
    assert!(v.net.lock().lle(a, Family::Inet, &ip4(dst)).is_none());
    assert_eq!(v.net.lock().arpstat.timeouts, 1);
}

#[test]
fn resolved_entries_verify_then_expire() {
    let (v, j, a, _b) = wire();
    let t0 = clock();
    let _ = arp::resolve(&mut v.net.lock(), a, false, None, [192, 0, 2, 2], t0);
    settle(&v, &j);
    let dst = ip4([192, 0, 2, 2]);
    let expire = v.net.lock().lle(a, Family::Inet, &dst).unwrap().expire;
    assert_eq!(expire, t0 + 1200);
    // Before expiry it asks whether it is used; unused, it lapses.
    arp::tick(&mut v.net.lock(), t0 + 1195);
    assert_eq!(v.net.lock().lle(a, Family::Inet, &dst).unwrap().state, 2);
    arp::tick(&mut v.net.lock(), t0 + 1196);
    assert!(v.net.lock().lle(a, Family::Inet, &dst).is_some());
    arp::tick(&mut v.net.lock(), t0 + 1200);
    assert!(v.net.lock().lle(a, Family::Inet, &dst).is_none());
}

fn lla_msg(ty: u8, dst: [u8; 4], index: u16, ll: [u8; 6]) -> Vec<u8> {
    let mut sin = [0u8; 16];
    sin[0] = 16;
    sin[1] = abi::inet::AF_INET;
    sin[4..8].copy_from_slice(&dst);
    let mut sdl = [0u8; 56];
    sdl[0] = 54;
    sdl[1] = abi::inet::AF_LINK;
    sdl[2..4].copy_from_slice(&index.to_ne_bytes());
    sdl[6] = 6;
    sdl[8..14].copy_from_slice(&ll);
    let mut h = RtMsghdr {
        rtm_version: RTM_VERSION,
        rtm_type: ty,
        rtm_flags: rtf::HOST | rtf::STATIC | rtf::LLDATA,
        rtm_addrs: rta::DST | rta::GATEWAY,
        rtm_seq: 42,
        ..RtMsghdr::default()
    };
    h.rtm_msglen = u16::try_from(size_of::<RtMsghdr>() + 16 + 56).unwrap();
    let mut v = Vec::new();
    crate::rtmsg::put(&mut v, &h);
    v.extend_from_slice(&sin);
    v.extend_from_slice(&sdl);
    v
}

fn reply_hdr(v: &[u8]) -> RtMsghdr {
    // SAFETY: a reply holds a whole header.
    unsafe { core::ptr::read_unaligned(v.as_ptr().cast()) }
}

#[test]
fn static_entries_through_the_routing_socket() {
    let (v, _j, a, _b) = wire();
    let ll = [0x52, 0x54, 0, 0x14, 0xe3, 0x10];
    let m = lla_msg(rtm::ADD, [192, 0, 2, 3], a, ll);
    let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
    assert_eq!(r, Ok(()));
    let h = reply_hdr(&reply);
    assert_eq!(h.rtm_flags & rtf::DONE, rtf::DONE);
    assert_eq!(&reply[size_of::<RtMsghdr>()..], &m[size_of::<RtMsghdr>()..]);
    {
        let n = v.net.lock();
        let e = n.lle(a, Family::Inet, &ip4([192, 0, 2, 3])).unwrap();
        assert_eq!((e.lladdr, e.flags & lle::STATIC), (ll, lle::STATIC));
        // `arp -a`: the interface's own and the static one.
        let d = n.lle_dump(Family::Inet, 0, 0);
        let one = size_of::<RtMsghdr>() + 16 + 54;
        assert_eq!(d.len(), 2 * one);
        let h = reply_hdr(&d[one..]);
        assert_eq!(h.rtm_type, rtm::GET);
        assert_eq!(h.rtm_flags, rtf::UP | rtf::HOST | rtf::LLDATA | rtf::STATIC);
        assert_eq!(&d[one + size_of::<RtMsghdr>() + 16 + 8..][..6], &ll);
    }
    // Off the interface's networks: no entry.
    let m = lla_msg(rtm::ADD, [198, 51, 100, 3], a, ll);
    let (_, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
    assert_eq!(r, Err(Errno::ENOMEM));
    // The interface's own may not be replaced or deleted.
    let m = lla_msg(rtm::ADD, [192, 0, 2, 1], a, ll);
    let (_, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
    assert_eq!(r, Err(Errno::EPERM));
    let m = lla_msg(rtm::DELETE, [192, 0, 2, 1], a, ll);
    let (_, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
    assert_eq!(r, Err(Errno::EPERM));
    let m = lla_msg(rtm::DELETE, [192, 0, 2, 3], a, ll);
    let (reply, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
    assert_eq!(r, Ok(()));
    assert_eq!(reply_hdr(&reply).rtm_type, rtm::DELETE);
    let (_, r) = crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0);
    assert_eq!(r, Err(Errno::ENOENT));
}

#[test]
fn deleting_the_address_clears_its_network() {
    let (v, j, a, _b) = wire();
    let _ = arp::resolve(&mut v.net.lock(), a, false, None, [192, 0, 2, 2], clock());
    settle(&v, &j);
    let m = lla_msg(rtm::ADD, [192, 0, 2, 3], a, [2, 0, 0, 0, 0, 3]);
    crate::rtsock::request(&mut v.net.lock(), &m, 7, true, 0)
        .1
        .unwrap();
    assert_eq!(v.net.lock().lle.len(), 3);
    v.net
        .lock()
        .delete_addr(b"epair0a", Family::Inet, Some(&ip4([192, 0, 2, 1])))
        .unwrap();
    assert!(v.net.lock().lle.is_empty());
}

#[test]
fn frames_not_for_us_stop_at_the_tap() {
    let (v, j, _a, _b) = wire();
    let before = j.net.lock().arpstat.received;
    // An ARP request to another station's address, unicast.
    let mut f = std::vec![0u8; 60];
    f[..6].copy_from_slice(&[2, 9, 9, 9, 9, 9]);
    f[6..12].copy_from_slice(&lladdr(&v, b"epair0a"));
    f[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
    let (index, link) = {
        let n = v.net.lock();
        let i = n.by_name(b"epair0a").unwrap();
        (i.index, i.link.0.clone().unwrap())
    };
    let m = Packet {
        hdr: mbuf::PktHdr::default(),
        data: Chain::from_slice(&f).unwrap(),
    };
    link.transmit(&v, index, None, m).unwrap();
    settle(&v, &j);
    assert_eq!(j.net.lock().arpstat.received, before);
}

/// A driver's device that counts what it is told.
struct Dev {
    sent: core::sync::atomic::AtomicUsize,
    flags: core::sync::atomic::AtomicU32,
    port: Arc<crate::netif::Port>,
}

impl crate::link::Link for Dev {
    fn transmit(
        &self,
        _: &Arc<Vnet>,
        _: u16,
        _: Option<Family>,
        m: Packet,
    ) -> Result<(), mbuf::NoBufs> {
        self.port.tap.mtap(&m, crate::bpf::Way::Out);
        self.sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn moved(&self, vnet: &Arc<Vnet>, index: u16) {
        self.port.moved(vnet, index);
    }

    fn runs_while_up(&self) -> bool {
        true
    }

    fn link_status(&self) -> Option<bool> {
        Some(true)
    }

    fn flags_changed(&self, flags: u32) {
        self.flags.store(flags, Ordering::Relaxed);
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[test]
fn a_driver_attaches_runs_while_up_and_follows_moves() {
    use abi::net::{LINK_STATE_DOWN, iff};
    crate::init();
    let port = crate::netif::Port::new();
    let dev = Arc::new(Dev {
        sent: core::sync::atomic::AtomicUsize::new(0),
        flags: core::sync::atomic::AtomicU32::new(0),
        port: Arc::clone(&port),
    });
    let e = crate::netif::Ether {
        dname: "tnet",
        unit: 7,
        lladdr: [2, 0, 0, 0, 0, 7],
        mtu: 1500,
        capabilities: 0,
        capenable: 0,
        baudrate: 10_000_000_000,
    };
    let name = crate::netif::attach_ether(&e, &port, dev.clone());
    assert_eq!(name, "tnet7");
    let v = crate::vnet0();
    let f = v.net.lock().by_name(b"tnet7").unwrap().flags;
    assert_eq!(f & (iff::UP | iff::RUNNING), 0);
    assert_eq!(v.net.lock().by_name(b"tnet7").unwrap().link_state, 0);
    v.net.lock().set_flags(b"tnet7", f | iff::UP).unwrap();
    // Up, the driver reports its link.
    assert_eq!(
        v.net.lock().by_name(b"tnet7").unwrap().link_state,
        abi::net::LINK_STATE_UP
    );
    let f = v.net.lock().by_name(b"tnet7").unwrap().flags;
    assert_eq!(f & (iff::UP | iff::RUNNING), iff::UP | iff::RUNNING);
    assert_eq!(dev.flags.load(Ordering::Relaxed), f);
    // A frame comes in on the port and reaches ARP in its VNET.
    let before = v.net.lock().arpstat.received;
    let mut fr = std::vec![0u8; 42];
    fr[..6].copy_from_slice(&[0xff; 6]);
    fr[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 9]);
    fr[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
    fr[14..16].copy_from_slice(&1u16.to_be_bytes());
    fr[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
    fr[18] = 6;
    fr[19] = 4;
    port.input(Packet {
        hdr: mbuf::PktHdr::default(),
        data: Chain::from_slice(&fr).unwrap(),
    });
    assert_eq!(v.net.lock().arpstat.received, before + 1);
    port.link_state(false);
    assert_eq!(
        v.net.lock().by_name(b"tnet7").unwrap().link_state,
        LINK_STATE_DOWN
    );
    // Moved into a VNET jail, it delivers there.
    let j = Vnet::new(0);
    clone::vmove(v, b"tnet7", &j).unwrap();
    assert!(Arc::ptr_eq(&port.home().0.unwrap(), &j));
    crate::netif::detach(&port);
    assert!(j.net.lock().by_name(b"tnet7").is_none());
}

#[test]
fn multicast_memberships_count_references() {
    let v = Vnet::new(0);
    clone::create(&v, b"epair", &ctx()).unwrap();
    let a = v.net.lock().by_name(b"epair0a").unwrap().index;
    let g = [1, 0, 0x5e, 0, 0, 1];
    let mut n = v.net.lock();
    n.add_multi(a, g, true).unwrap();
    assert_eq!(n.add_multi(a, g, true), Err(Errno::EADDRINUSE));
    n.add_multi(a, g, false).unwrap();
    let d = crate::rtmsg::ifmalist(&n, 0, 0);
    assert_eq!(usize::from(u16::from_ne_bytes([d[0], d[1]])), d.len());
    assert_eq!(d[3], abi::net::rtm::NEWMADDR);
    n.del_multi(a, g).unwrap();
    assert_eq!(n.by_index(a).unwrap().maddrs.len(), 1);
    n.del_multi(a, g).unwrap();
    assert!(n.by_index(a).unwrap().maddrs.is_empty());
    assert_eq!(n.del_multi(a, g), Err(Errno::EADDRNOTAVAIL));
    assert!(crate::rtmsg::ifmalist(&n, 0, 0).is_empty());
}
