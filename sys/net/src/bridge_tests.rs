// SPDX-License-Identifier: BSD-2-Clause
//! The bridge between VNETs joined by `epair`s, its forwarding table and
//! requests; and the `tun` and `tap` devices' queues.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::bridge::{ifbaf, ifbif};
use abi::errno::Errno;
use abi::net::{IFT_BRIDGE, IFT_PPP, iff};
use mbuf::{Chain, Packet};

use crate::addr::from_v4;
use crate::arp::{self, Resolved};
use crate::bridge::{self, Bridge, VlanSet};
use crate::clone::{self, Ctx};
use crate::ifnet::Ifaddr;
use crate::link::Link;
use crate::tuntap::{Kind, WriteLen};
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
        broadcast: None,
        flags6: 0,
        vltime: u32::MAX,
        pltime: u32::MAX,
        vhid: 0,
        created: 0,
        updated: 0,
    };
    v.net.lock().add_addr(name, a).unwrap();
}

fn up(v: &Vnet, name: &[u8]) {
    let f = v.net.lock().by_name(name).unwrap().flags;
    v.net.lock().set_flags(name, f | iff::UP).unwrap();
}

fn index(v: &Vnet, name: &str) -> u16 {
    v.net.lock().by_name(name.as_bytes()).unwrap().index
}

fn lladdr(v: &Vnet, name: &str) -> [u8; 6] {
    v.net
        .lock()
        .by_name(name.as_bytes())
        .unwrap()
        .lladdr
        .unwrap()
}

fn the_bridge(v: &Vnet, name: &str) -> Arc<Bridge> {
    let n = v.net.lock();
    bridge::of(n.by_name(name.as_bytes()).unwrap())
        .and_then(Bridge::arc)
        .unwrap()
}

fn settle(vs: &[&Arc<Vnet>]) {
    while vs.iter().map(|v| netisr::drain(v)).sum::<usize>() > 0 {}
}

/// Two hosts, each in a VNET of its own behind an `epair`, the other
/// ends members of a bridge in a third: `h1 (a0) -- (b0) bridge (b1) --
/// (a1) h2`.
struct Lan {
    h1: Arc<Vnet>,
    h2: Arc<Vnet>,
    br: Arc<Vnet>,
    bname: alloc::string::String,
    a0: alloc::string::String,
    b0: alloc::string::String,
    a1: alloc::string::String,
    b1: alloc::string::String,
}

fn lan() -> Lan {
    crate::init();
    crate::set_uptime(clock);
    let (h1, h2, br) = (Vnet::new(0), Vnet::new(0), Vnet::new(0));
    let e0 = clone::create(&br, b"epair", &ctx()).unwrap();
    let e1 = clone::create(&br, b"epair", &ctx()).unwrap();
    let (a0, a1) = (e0.clone(), e1.clone());
    let b0 = alloc::format!("{}b", &e0[..e0.len() - 1]);
    let b1 = alloc::format!("{}b", &e1[..e1.len() - 1]);
    clone::vmove(&br, a0.as_bytes(), &h1).unwrap();
    clone::vmove(&br, a1.as_bytes(), &h2).unwrap();
    let bname = clone::create(&br, b"bridge", &ctx()).unwrap();
    let b = the_bridge(&br, &bname);
    b.add(&br, &mut br.net.lock(), b0.as_bytes()).unwrap();
    b.add(&br, &mut br.net.lock(), b1.as_bytes()).unwrap();
    for n in [&b0, &b1, &bname] {
        up(&br, n.as_bytes());
    }
    v4(&h1, a0.as_bytes(), 1);
    v4(&h2, a1.as_bytes(), 2);
    let l = Lan {
        h1,
        h2,
        br,
        bname,
        a0,
        b0,
        a1,
        b1,
    };
    settle(&[&l.h1, &l.h2, &l.br]);
    l
}

#[test]
fn a_bridge_is_made_down_with_its_own_address() {
    crate::init();
    let v = Vnet::new(0);
    let name = clone::create(&v, b"bridge", &ctx()).unwrap();
    assert_eq!(name, "bridge0");
    let n = v.net.lock();
    let i = n.by_name(b"bridge0").unwrap();
    assert_eq!(i.ty, IFT_BRIDGE);
    assert_eq!(i.flags, iff::BROADCAST | iff::SIMPLEX | iff::MULTICAST);
    assert_eq!(i.mtu, 1500);
    assert_eq!(i.groups, ["bridge"]);
    assert_eq!(i.lladdr.unwrap()[..4], [0x58, 0x9c, 0xfc, 0x10]);
    drop(n);
    assert_eq!(clone::create(&v, b"bridge", &ctx()).unwrap(), "bridge1");
    assert_eq!(clone::create(&v, b"bridge0", &ctx()), Err(Errno::EEXIST));
    clone::destroy(&v, b"bridge0").unwrap();
    assert_eq!(clone::create(&v, b"bridge", &ctx()).unwrap(), "bridge0");
}

#[test]
fn members_are_listed_newest_first_with_freebsds_defaults() {
    let l = lan();
    let b = the_bridge(&l.br, &l.bname);
    let n = l.br.net.lock();
    let m = b.members(&n);
    assert_eq!(m.len(), 2);
    let name = |r: &abi::bridge::Ifbreq| {
        let end = r.ifbr_ifsname.iter().position(|&c| c == 0).unwrap();
        alloc::string::String::from_utf8(r.ifbr_ifsname[..end].to_vec()).unwrap()
    };
    assert_eq!(name(&m[0]), l.b1);
    assert_eq!(name(&m[1]), l.b0);
    // LEARNING, DISCOVER, AUTOEDGE, AUTOPTP.
    assert_eq!(m[0].ifbr_ifsflags, 0x143);
    assert_eq!(m[0].ifbr_priority, 128);
    // 20 Gb/s over an epair's 10 Gb/s.
    assert_eq!(m[0].ifbr_path_cost, 2000);
    assert_eq!(m[0].ifbr_state, 5);
    assert_eq!(m[0].ifbr_proto, 2);
    // Members are promiscuous and bridged.
    let i = n.by_name(l.b0.as_bytes()).unwrap();
    assert_ne!(i.flags & iff::PROMISC, 0);
    assert!(i.bridge.is_some());
    // The bridge's link follows its members'.
    assert_eq!(
        n.by_name(l.bname.as_bytes()).unwrap().link_state,
        abi::net::LINK_STATE_UP
    );
    drop(n);
    // Adding one twice, or a bridge's member to another, is refused.
    assert_eq!(
        b.add(&l.br, &mut l.br.net.lock(), l.b0.as_bytes()),
        Err(Errno::EEXIST)
    );
    let other = clone::create(&l.br, b"bridge", &ctx()).unwrap();
    let o = the_bridge(&l.br, &other);
    assert_eq!(
        o.add(&l.br, &mut l.br.net.lock(), l.b0.as_bytes()),
        Err(Errno::EBUSY)
    );
    assert_eq!(
        o.add(&l.br, &mut l.br.net.lock(), b"nonesuch"),
        Err(Errno::ENOENT)
    );
    // Leaving undoes promiscuous mode.
    b.del(&mut l.br.net.lock(), l.b0.as_bytes()).unwrap();
    let n = l.br.net.lock();
    let i = n.by_name(l.b0.as_bytes()).unwrap();
    assert_eq!(i.flags & iff::PROMISC, 0);
    assert!(i.bridge.is_none());
    assert_eq!(b.members(&n).len(), 1);
}

#[test]
fn frames_cross_the_bridge_both_ways_and_it_learns() {
    let l = lan();
    let a0 = index(&l.h1, &l.a0);
    let counted = |l: &Lan| {
        let n = l.br.net.lock();
        let s = &n.by_name(l.bname.as_bytes()).unwrap().stats;
        (s.ipackets.get(), s.opackets.get())
    };
    let before = counted(&l);
    // h1 asks for h2: the broadcast floods, the reply comes back unicast.
    let r = arp::resolve(
        &mut l.h1.net.lock(),
        a0,
        false,
        &mut None,
        [192, 0, 2, 2],
        clock(),
    );
    assert_eq!(r, Ok(Resolved::Held));
    settle(&[&l.h1, &l.h2, &l.br]);
    let ll2 = lladdr(&l.h2, &l.a1);
    let ll1 = lladdr(&l.h1, &l.a0);
    {
        let n = l.h1.net.lock();
        let e = n
            .lle(a0, Family::Inet, &from_v4([192, 0, 2, 2]))
            .expect("h1 learned h2");
        assert_eq!(e.lladdr, ll2);
    }
    // The bridge knows each host behind its own member.
    let b = the_bridge(&l.br, &l.bname);
    let n = l.br.net.lock();
    let t = b.addresses(&n, clock());
    let at = |mac: [u8; 6]| {
        t.iter()
            .find(|a| a.ifba_dst == mac)
            .map(|a| {
                let end = a.ifba_ifsname.iter().position(|&c| c == 0).unwrap();
                alloc::string::String::from_utf8(a.ifba_ifsname[..end].to_vec()).unwrap()
            })
            .unwrap()
    };
    assert_eq!(at(ll1), l.b0);
    assert_eq!(at(ll2), l.b1);
    assert!(t.iter().all(|a| a.ifba_flags == ifbaf::DYNAMIC));
    assert!(t.iter().all(|a| a.ifba_expire == 1200));
    // The newest first: h2 answered after h1 asked.
    assert_eq!(t[0].ifba_dst, ll2);
    // The bridge counted both frames it forwarded.
    drop(n);
    let after = counted(&l);
    assert_eq!((after.0 - before.0, after.1 - before.1), (2, 2));
}

#[test]
fn the_bridge_answers_for_its_own_address() {
    let l = lan();
    v4(&l.br, l.bname.as_bytes(), 3);
    settle(&[&l.h1, &l.h2, &l.br]);
    let a0 = index(&l.h1, &l.a0);
    let _ = arp::resolve(
        &mut l.h1.net.lock(),
        a0,
        false,
        &mut None,
        [192, 0, 2, 3],
        clock(),
    );
    settle(&[&l.h1, &l.h2, &l.br]);
    let n = l.h1.net.lock();
    let e = n
        .lle(a0, Family::Inet, &from_v4([192, 0, 2, 3]))
        .expect("the bridge answered");
    assert_eq!(e.lladdr, lladdr(&l.br, &l.bname));
}

#[test]
fn a_bridge_that_is_down_does_not_forward() {
    let l = lan();
    let f = l.br.net.lock().by_name(l.bname.as_bytes()).unwrap().flags;
    l.br.net
        .lock()
        .set_flags(l.bname.as_bytes(), f & !iff::UP)
        .unwrap();
    let a0 = index(&l.h1, &l.a0);
    let _ = arp::resolve(
        &mut l.h1.net.lock(),
        a0,
        false,
        &mut None,
        [192, 0, 2, 2],
        clock(),
    );
    settle(&[&l.h1, &l.h2, &l.br]);
    assert!(
        l.h1.net
            .lock()
            .lle(a0, Family::Inet, &from_v4([192, 0, 2, 2]))
            .is_none_or(|e| e.flags & crate::lltable::lle::VALID == 0)
    );
}

#[test]
fn the_table_takes_static_entries_limits_and_flushes() {
    let l = lan();
    let b = the_bridge(&l.br, &l.bname);
    // What the hosts' announcements taught it goes first.
    b.flush(abi::bridge::IFBF_FLUSHALL);
    let n = l.br.net.lock();
    let mac = [2, 0, 0, 0, 0, 9];
    b.set_address(&n, l.b0.as_bytes(), &mac, 0, ifbaf::STATIC)
        .unwrap();
    assert_eq!(
        b.set_address(&n, l.b0.as_bytes(), &[1, 0, 0, 0, 0, 1], 0, ifbaf::STATIC),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        b.set_address(&n, l.b0.as_bytes(), &[0; 6], 0, ifbaf::STATIC),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        b.set_address(&n, b"nonesuch", &mac, 0, ifbaf::STATIC),
        Err(Errno::ENOENT)
    );
    b.set_address(&n, l.b1.as_bytes(), &[2, 0, 0, 0, 0, 8], 0, ifbaf::DYNAMIC)
        .unwrap();
    let t = b.addresses(&n, clock());
    assert_eq!(t.len(), 2);
    let s = t.iter().find(|a| a.ifba_dst == mac).unwrap();
    assert_eq!((s.ifba_flags, s.ifba_expire), (ifbaf::STATIC, 0));
    // A flush keeps static entries; flushing all does not.
    b.flush(0);
    assert_eq!(b.addresses(&n, clock()).len(), 1);
    b.flush(abi::bridge::IFBF_FLUSHALL);
    assert!(b.addresses(&n, clock()).is_empty());
    // Delete by address, any VLAN.
    b.set_address(&n, l.b0.as_bytes(), &mac, 5, ifbaf::STATIC)
        .unwrap();
    b.delete_address(&mac, 0).unwrap();
    assert_eq!(b.delete_address(&mac, 0), Err(Errno::ENOENT));
    // The table's size bounds it.
    b.set_cache(1);
    b.set_address(&n, l.b0.as_bytes(), &[2, 0, 0, 0, 0, 1], 0, ifbaf::DYNAMIC)
        .unwrap();
    assert_eq!(
        b.set_address(&n, l.b0.as_bytes(), &[2, 0, 0, 0, 0, 2], 0, ifbaf::DYNAMIC),
        Err(Errno::ENOSPC)
    );
    assert_eq!(b.exceeded(), 1);
    // A member's own limit.
    b.set_cache(100);
    b.set_addrmax(&n, l.b1.as_bytes(), 1).unwrap();
    b.set_address(&n, l.b1.as_bytes(), &[2, 0, 0, 0, 0, 3], 0, ifbaf::DYNAMIC)
        .unwrap();
    assert_eq!(
        b.set_address(&n, l.b1.as_bytes(), &[2, 0, 0, 0, 0, 4], 0, ifbaf::DYNAMIC),
        Err(Errno::ENOSPC)
    );
    let m = b.member(&n, l.b1.as_bytes()).unwrap();
    assert_eq!(
        (m.ifbr_addrcnt, m.ifbr_addrmax, m.ifbr_addrexceeded),
        (1, 1, 1)
    );
}

#[test]
fn learned_entries_age_out_at_the_prune() {
    let l = lan();
    let b = the_bridge(&l.br, &l.bname);
    b.flush(abi::bridge::IFBF_FLUSHALL);
    {
        let n = l.br.net.lock();
        b.set_timeout(10);
        b.set_address(&n, l.b0.as_bytes(), &[2, 0, 0, 0, 0, 1], 0, ifbaf::DYNAMIC)
            .unwrap();
    }
    let t0 = clock();
    // Expired, but kept until the bridge prunes its table.
    bridge::tick(t0 + 20);
    assert_eq!(b.addresses(&l.br.net.lock(), t0 + 20).len(), 1);
    bridge::tick(t0 + bridge::PRUNE_PERIOD + 1);
    assert!(b.addresses(&l.br.net.lock(), t0).is_empty());
}

#[test]
fn member_requests_check_what_freebsd_checks() {
    let l = lan();
    let b = the_bridge(&l.br, &l.bname);
    let n = l.br.net.lock();
    let m = l.b0.as_bytes();
    assert_eq!(b.set_member_flags(&n, m, ifbif::SPAN), Err(Errno::EINVAL));
    b.set_member_flags(&n, m, ifbif::LEARNING | ifbif::PRIVATE)
        .unwrap();
    assert_eq!(
        b.member(&n, m).unwrap().ifbr_ifsflags,
        ifbif::LEARNING | ifbif::PRIVATE
    );
    b.set_port_priority(&n, m, 77).unwrap();
    assert_eq!(b.member(&n, m).unwrap().ifbr_priority, 64);
    assert_eq!(b.set_port_priority(&n, m, 241), Err(Errno::EINVAL));
    b.set_path_cost(&n, m, 1234).unwrap();
    let r = b.member(&n, m).unwrap();
    assert_eq!(r.ifbr_path_cost, 1234);
    assert_ne!(r.ifbr_ifsflags & ifbif::BSTP_ADMCOST, 0);
    b.set_path_cost(&n, m, 0).unwrap();
    assert_eq!(b.member(&n, m).unwrap().ifbr_path_cost, 2000);
    assert_eq!(b.set_pvid(&n, m, 4095), Err(Errno::EINVAL));
    b.set_pvid(&n, m, 10).unwrap();
    let mut set = VlanSet([0; abi::bridge::BRVLAN_WORDS]);
    set.0[0] = 1;
    assert_eq!(
        b.set_vlans(&n, m, abi::bridge::BRDG_VLAN_OP_SET, &set),
        Err(Errno::EINVAL)
    );
    set.0[0] = 1 << 20;
    b.set_vlans(&n, m, abi::bridge::BRDG_VLAN_OP_SET, &set)
        .unwrap();
    assert!(b.vlans(&n, m).unwrap().test(20));
    b.set_vlans(&n, m, abi::bridge::BRDG_VLAN_OP_DEL, &set)
        .unwrap();
    assert!(!b.vlans(&n, m).unwrap().test(20));
    assert_eq!(b.set_vlanproto(&n, m, 0x0800), Err(Errno::EINVAL));
    // STP's parameters, as `bstp_set_*` limit them.
    assert_eq!(b.set_hellotime(1), Err(Errno::EPERM));
    b.set_protocol(0).unwrap();
    b.set_hellotime(1).unwrap();
    assert_eq!(b.set_maxage(41), Err(Errno::EINVAL));
    b.set_priority(4097).unwrap();
    assert_eq!(b.priority(), 4096);
    assert_eq!(b.params().ifbop_bridgeid, 4096 << 48);
    assert_eq!(b.set_protocol(1), Err(Errno::EINVAL));
}

#[test]
fn a_destroyed_bridge_lets_its_members_go() {
    let l = lan();
    clone::destroy(&l.br, l.bname.as_bytes()).unwrap();
    let n = l.br.net.lock();
    for m in [&l.b0, &l.b1] {
        let i = n.by_name(m.as_bytes()).unwrap();
        assert!(i.bridge.is_none());
        assert_eq!(i.flags & iff::PROMISC, 0);
    }
}

#[test]
fn a_member_that_goes_leaves_its_bridge() {
    let l = lan();
    let b = the_bridge(&l.br, &l.bname);
    clone::destroy(&l.br, l.b0.as_bytes()).unwrap();
    assert_eq!(b.members(&l.br.net.lock()).len(), 1);
}

fn ip_packet(dst: [u8; 4], len: usize) -> Packet {
    let mut b = alloc::vec![0u8; len];
    b[0] = 0x45;
    b[16..20].copy_from_slice(&dst);
    Packet {
        hdr: mbuf::PktHdr::default(),
        data: Chain::from_slice(&b).unwrap(),
    }
}

/// The device for interface `name`, as the kernel's hook recorded it.
fn tun_of(v: &Vnet, name: &str) -> Arc<crate::tuntap::Tun> {
    TUNS.lock()
        .iter()
        .find(|t| {
            let (h, i) = t.home();
            h.is_some_and(|h| core::ptr::eq(Arc::as_ptr(&h), core::ptr::from_ref(v)))
                && v.net.lock().by_index(i).is_some_and(|x| x.name == name)
        })
        .cloned()
        .unwrap()
}

static TUNS: sync::SpinLock<Vec<Arc<crate::tuntap::Tun>>> = sync::SpinLock::new(Vec::new());

fn made(t: &Arc<crate::tuntap::Tun>) {
    TUNS.lock().push(Arc::clone(t));
}

fn gone(t: &crate::tuntap::Tun) {
    TUNS.lock().retain(|x| !core::ptr::eq(Arc::as_ptr(x), t));
}

fn renamed(_t: &crate::tuntap::Tun, _n: &str) {}

fn tun_setup() -> Arc<Vnet> {
    crate::init();
    crate::tuntap::set_hooks(crate::tuntap::Hooks {
        made,
        gone,
        renamed,
    });
    Vnet::new(0)
}

#[test]
fn a_tun_is_point_to_point_and_queues_for_its_reader() {
    let v = tun_setup();
    let name = clone::create(&v, b"tun", &ctx()).unwrap();
    let t = tun_of(&v, &name);
    let i = index(&v, &name);
    {
        let n = v.net.lock();
        let ifp = n.by_index(i).unwrap();
        assert_eq!(ifp.ty, IFT_PPP);
        assert_eq!(ifp.flags, iff::POINTOPOINT | iff::MULTICAST);
        assert_eq!(
            ifp.capenable,
            abi::net::ifcap::LINKSTATE | abi::net::ifcap::MEXTPG
        );
    }
    // Closed, nothing goes out.
    let n = v.net.lock();
    assert_eq!(
        n.by_index(i).unwrap().link.0.as_ref().unwrap().ready(),
        Err(Errno::EHOSTDOWN)
    );
    drop(n);
    t.open(42).unwrap();
    assert_eq!(t.open(43), Err(Errno::EBUSY));
    assert_eq!(
        v.net.lock().by_index(i).unwrap().link_state,
        abi::net::LINK_STATE_UP
    );
    up(&v, name.as_bytes());
    v.net.lock().with_index(i, |x| x.flags |= iff::RUNNING);
    // Plain mode: IPv4 alone, as it is.
    clone::output(&v, i, Family::Inet, ip_packet([10, 0, 0, 2], 28)).unwrap();
    let m = t.dequeue().unwrap().unwrap();
    assert_eq!(m.len(), 28);
    assert!(clone::output(&v, i, Family::Inet6, ip_packet([0; 4], 40)).is_err());
    assert!(t.dequeue().unwrap().is_none());
    // `TUNSIFHEAD`: the family first, in network order.
    let mut one = 1i32.to_ne_bytes();
    assert_eq!(t.ioctl(abi::tun::TUNSIFHEAD, &mut one, 42), Ok(true));
    clone::output(&v, i, Family::Inet6, ip_packet([0; 4], 40)).unwrap();
    let m = t.dequeue().unwrap().unwrap();
    let mut h = [0u8; 4];
    m.data.copy_out(0, &mut h);
    assert_eq!(u32::from_be_bytes(h), 28);
    assert_eq!(t.first_len(), 0);
    // The queue holds 50.
    for _ in 0..60 {
        let _ = clone::output(&v, i, Family::Inet, ip_packet([10, 0, 0, 2], 20));
    }
    assert_eq!(t.queued(), crate::tuntap::QLEN);
    // Writes: too long is EIO; down, ignored.
    assert_eq!(t.write_len(70_000), Err(Errno::EIO));
    assert_eq!(t.write_len(20), Ok(WriteLen::Packet { vhdrlen: 0 }));
    // Closing drops the queue, the link, and the addresses.
    assert!(!t.close(42, "test"));
    assert_eq!(t.dequeue().err(), Some(Errno::EHOSTDOWN));
    let n = v.net.lock();
    let ifp = n.by_index(i).unwrap();
    assert_eq!(ifp.flags & (iff::UP | iff::RUNNING), 0);
    assert_eq!(ifp.link_state, abi::net::LINK_STATE_DOWN);
    drop(n);
    assert_eq!(t.write_len(20), Ok(WriteLen::Ignore));
    assert_eq!(t.status().as_deref(), Some(""));
    // Destroyed, its unit is free again.
    clone::destroy(&v, name.as_bytes()).unwrap();
    assert!(!crate::tuntap::unit_taken(Kind::Tun, t.unit));
}

#[test]
fn tun_requests() {
    let v = tun_setup();
    let name = clone::create(&v, b"tun", &ctx()).unwrap();
    let t = tun_of(&v, &name);
    let mut b = (iff::BROADCAST as i32).to_ne_bytes();
    assert_eq!(t.ioctl(abi::tun::TUNSIFMODE, &mut b, 1), Ok(true));
    assert_eq!(
        v.net.lock().by_name(name.as_bytes()).unwrap().flags,
        iff::BROADCAST
    );
    let mut bad = (iff::LOOPBACK as i32).to_ne_bytes();
    assert_eq!(
        t.ioctl(abi::tun::TUNSIFMODE, &mut bad, 1),
        Err(Errno::EINVAL)
    );
    let mut info = [0u8; 8];
    assert_eq!(t.ioctl(abi::tun::TUNGIFINFO, &mut info, 1), Ok(true));
    assert_eq!(u16::from_ne_bytes([info[4], info[5]]), 1500);
    assert_eq!(info[6], IFT_PPP);
    let mut ifr = [0u8; 32];
    assert_eq!(t.ioctl(abi::tun::TUNGIFNAME, &mut ifr, 1), Ok(true));
    assert_eq!(&ifr[..name.len()], name.as_bytes());
    // Tap requests are not a tun's.
    let mut x = [0u8; 4];
    assert_eq!(t.ioctl(abi::tun::TAPGVNETHDR, &mut x, 1), Ok(false));
    t.ioctl(abi::tun::TUNSIFPID, &mut [], 77).unwrap();
    assert_eq!(t.status().unwrap(), "\tOpened by PID 77\n");
    let mut on = 1i32.to_ne_bytes();
    t.ioctl(abi::tun::TUNSTRANSIENT, &mut on, 1).unwrap();
    t.open(77).unwrap();
    assert!(t.close(77, "t"));
    assert!(t.dying());
}

#[test]
fn a_tap_is_ethernet_and_its_writes_arrive() {
    let v = tun_setup();
    let name = clone::create(&v, b"tap", &ctx()).unwrap();
    let t = tun_of(&v, &name);
    let i = index(&v, &name);
    let ll = lladdr(&v, &name);
    {
        let n = v.net.lock();
        let ifp = n.by_index(i).unwrap();
        assert_eq!(ifp.ty, abi::net::IFT_ETHER);
        assert_eq!(ifp.groups, ["tap"]);
        let md = ifp.link.0.as_ref().unwrap().media().unwrap();
        assert_eq!(md.status, crate::link::ifm::AVALID);
    }
    t.open(5).unwrap();
    // Open, it runs and its remote address is its own.
    assert_ne!(v.net.lock().by_index(i).unwrap().flags & iff::RUNNING, 0);
    let mut ga = [0u8; 32];
    t.ioctl(abi::net::sio::SIOCGIFADDR, &mut ga, 5).unwrap();
    assert_eq!(ga[..6], ll);
    // A gratuitous request for an address on it goes to the reader.
    v4(&v, name.as_bytes(), 1);
    let m = t.dequeue().unwrap().expect("the announcement");
    let mut h = [0u8; 14];
    m.data.copy_out(0, &mut h);
    assert_eq!(h[..6], [0xff; 6]);
    assert_eq!(u16::from_be_bytes([h[12], h[13]]), 0x0806);
    // A request written for its address is answered.
    let mut f = Vec::new();
    f.extend_from_slice(&ll);
    f.extend_from_slice(&[2, 0, 0, 0, 0, 7]);
    f.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 1]);
    f.extend_from_slice(&[2, 0, 0, 0, 0, 7, 192, 0, 2, 7]);
    f.extend_from_slice(&[0; 6]);
    f.extend_from_slice(&[192, 0, 2, 1]);
    t.write(None, Chain::from_slice(&f).unwrap()).unwrap();
    netisr::drain(&v);
    let r = t.dequeue().unwrap().expect("the reply");
    let mut rh = [0u8; 22];
    r.data.copy_out(0, &mut rh);
    assert_eq!(rh[..6], [2, 0, 0, 0, 0, 7]);
    assert_eq!(u16::from_be_bytes([rh[20], rh[21]]), 2);
    // A frame for another station is not received.
    let before = v.net.lock().arpstat.received;
    let mut g = f.clone();
    g[..6].copy_from_slice(&[2, 0, 0, 0, 0, 9]);
    t.write(None, Chain::from_slice(&g).unwrap()).unwrap();
    netisr::drain(&v);
    assert_eq!(v.net.lock().arpstat.received, before);
    // The virtio-net header's lengths.
    let mut bad = 11i32.to_ne_bytes();
    assert_eq!(
        t.ioctl(abi::tun::TAPSVNETHDR, &mut bad, 5),
        Err(Errno::EINVAL)
    );
    let mut ok = 12i32.to_ne_bytes();
    t.ioctl(abi::tun::TAPSVNETHDR, &mut ok, 5).unwrap();
    assert_eq!(
        t.write_len(65535 + 12),
        Ok(WriteLen::Packet { vhdrlen: 12 })
    );
    assert_ne!(
        v.net.lock().by_index(i).unwrap().capabilities & abi::net::ifcap::TSO4,
        0
    );
    // A tap is a bridge member like any Ethernet interface.
    let br = clone::create(&v, b"bridge", &ctx()).unwrap();
    let b = the_bridge(&v, &br);
    b.add(&v, &mut v.net.lock(), name.as_bytes()).unwrap();
    let r = b.member(&v.net.lock(), name.as_bytes()).unwrap();
    // No speed: the default cost.
    assert_eq!(r.ifbr_path_cost, 55);
}

#[test]
fn a_renamed_interface_keeps_its_bridge_and_is_announced() {
    let l = lan();
    l.br.net.lock().rename(l.b0.as_bytes(), "uplink").unwrap();
    let b = the_bridge(&l.br, &l.bname);
    assert!(b.member(&l.br.net.lock(), b"uplink").is_ok());
    assert_eq!(l.br.net.lock().rename(b"uplink", &l.b1), Err(Errno::EEXIST));
    assert_eq!(l.br.net.lock().rename(b"uplink", ""), Err(Errno::EINVAL));
    assert_eq!(l.br.net.lock().rename(b"nonesuch", "x"), Err(Errno::ENXIO));
}
