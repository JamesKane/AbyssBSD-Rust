// SPDX-License-Identifier: BSD-2-Clause
//! Router advertisements and what they set up (docs/NETWORKING.md §8,
//! SLAAC): the default router list with the default route it installs,
//! the prefix list with its on-link routes, the addresses made from
//! advertised prefixes and the interface identifier, their valid and
//! preferred lifetimes, and `nd6_timer`, which ages them each second.
//! `rtsold` sends the solicitations; a host ignores others'.
//!
//! Behavior reference: sys/netinet6/nd6_rtr.c (`nd6_rs_input`,
//! `nd6_ra_input`, `defrtrlist_update`, `defrouter_select_fib`,
//! `defrouter_addreq`, `defrouter_delreq`, `defrouter_del`,
//! `prelist_update`, `nd6_prelist_add`, `nd6_prefix_onlink`,
//! `nd6_prefix_offlink`, `in6_ifadd`, `rt6_flush`, `nd6_sysctl_drlist`,
//! `nd6_defrouter_timer`), sys/netinet6/nd6.c (`nd6_timer`,
//! `nd6_sysctl_prlist`), sys/netinet6/in6.c (`in6_aifaddr`'s prefix).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::net::{IFT_ETHER, nd6_iff, rtf};
use mbuf::Packet;
use net::Family;
use net::Vnet;
use net::addr::{In6, prefix_match};
use net::ifnet::{Gateway, INFINITE, Ifaddr, Net, Route, ifa6_update, in6_iff, masked};
use net::lltable::state;

use crate::icmp6::ty;
use crate::in6::{self, is_linklocal};
use crate::ip6::{self, Ip6};
use crate::nd6::{self, bytes_at, lladdr_opt, message, options};
use crate::state::{State, icp6s, on, val};

/// `ND_RA_FLAG_RTPREF_*`.
const RTPREF_MASK: u8 = 0x18;
const RTPREF_HIGH: u8 = 0x08;
const RTPREF_LOW: u8 = 0x18;
const RTPREF_RSV: u8 = 0x10;
/// `ND_OPT_PI_FLAG_ONLINK`, `ND_OPT_PI_FLAG_AUTO`.
const PI_ONLINK: u8 = 0x80;
const PI_AUTO: u8 = 0x40;
/// `NDPRF_ONLINK`, `NDPRF_DETACHED`.
pub const NDPRF_ONLINK: u32 = 0x1;
pub const NDPRF_DETACHED: u32 = 0x2;
/// `MAX_REACHABLE_TIME`, milliseconds.
const MAX_REACHABLE_TIME: u32 = 3_600_000;
/// Two hours, RFC 4862 5.5.3 (e)'s floor on a valid lifetime.
const TWOHOUR: u32 = 120 * 60;
/// `RTF_DYNAMIC`.
const RTF_DYNAMIC: i32 = 0x10;

/// A default router (`struct nd_defrouter`).
#[derive(Clone, Copy, Debug)]
pub struct DefRouter {
    pub addr: In6,
    pub ifindex: u16,
    /// The advertisement's flags (`raflags`).
    pub raflags: u8,
    pub rtlifetime: u16,
    /// When it expires, uptime seconds.
    pub expire: u64,
    /// Its default route is installed.
    pub installed: bool,
}

/// A prefix (`struct nd_prefix`).
#[derive(Clone, Debug)]
pub struct Prefix {
    pub ifindex: u16,
    pub prefix: In6,
    pub plen: u8,
    pub vltime: u32,
    pub pltime: u32,
    /// The advertisement's on-link and autonomous flags.
    pub raf_onlink: bool,
    pub raf_auto: bool,
    /// When last advertised, uptime seconds.
    pub lastupdate: u64,
    /// `NDPRF_*`; [`Prefix::onlink`] mirrors `NDPRF_ONLINK`.
    pub stateflags: u32,
    pub onlink: bool,
    /// Addresses made from it (`ndpr_addrcnt`).
    pub addrcnt: u32,
    /// The routers advertising it (`ndpr_advrtrs`).
    pub advrtrs: Vec<In6>,
}

/// `rtpref`: a router's preference, high above medium above low.
const fn rtpref(raflags: u8) -> i8 {
    match raflags & RTPREF_MASK {
        RTPREF_HIGH => 1,
        RTPREF_LOW => -1,
        // Medium, and the reserved value as medium.
        _ => {
            let _ = RTPREF_RSV;
            0
        }
    }
}

/// `nd6_rs_input`: a host ignores solicitations (only a router answers).
pub(crate) fn rs_input(vnet: &Arc<Vnet>, st: &State, m: &Packet, off: usize, len: usize) {
    if !on(&st.conf6.forwarding) {
        return;
    }
    let mut c = match m.copy() {
        Ok(c) => c,
        Err(_) => return,
    };
    let Ok(ip6) = c.header::<Ip6>(0) else {
        return;
    };
    if ip6.hlim != 255 {
        st.icmp6stat.inc(icp6s::INVLHLIM);
        st.icmp6stat.inc(icp6s::BADRS);
        return;
    }
    if net::addr::is_unspecified(&ip6.src) {
        return;
    }
    let b = message(m, off, len);
    let Ok(o) = options(st, &b[8..]) else {
        return;
    };
    let Ok(ll) = lladdr_opt(&b[8..], o.src_ll) else {
        st.icmp6stat.inc(icp6s::BADRS);
        return;
    };
    let mut n = vnet.net.lock();
    nd6::cache_lladdr(&mut n, st, m.hdr.rcvif, &ip6.src, ll, ty::ROUTER_SOLICIT, 0);
}

/// `nd6_ra_input`: on an interface that accepts them, an advertisement
/// updates the router list, the interface's ND parameters, the prefix
/// list (making addresses), the link MTU, and the router's entry.
pub(crate) fn ra_input(vnet: &Arc<Vnet>, st: &State, m: &Packet, off: usize, len: usize) {
    let ifindex = m.hdr.rcvif;
    let mut n = vnet.net.lock();
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    if ifp.nd6_flags & nd6_iff::ACCEPT_RTADV == 0 {
        return;
    }
    if m.hdr.flags & mbuf::flags::FRAGMENTED != 0 {
        return;
    }
    let mut c = match m.copy() {
        Ok(c) => c,
        Err(_) => return,
    };
    let Ok(ip6) = c.header::<Ip6>(0) else {
        return;
    };
    let bad = || st.icmp6stat.inc(icp6s::BADRA);
    if ip6.hlim != 255 {
        st.icmp6stat.inc(icp6s::INVLHLIM);
        bad();
        return;
    }
    let saddr = ip6.src;
    if !is_linklocal(&saddr) {
        bad();
        return;
    }
    let b = message(m, off, len);
    let Ok(o) = options(st, &b[16..]) else {
        return;
    };
    let opts = &b[16..];
    let mcast = net::addr::is_multicast6(&ip6.dst);
    let now = net::uptime();
    // The router.
    let raflags = b[5];
    let rtlifetime = if ifp.nd6_flags & nd6_iff::NO_RADR != 0
        || (on(&st.conf6.forwarding) && !on(&st.conf6.rfc6204w3))
    {
        0
    } else {
        u16::from_be_bytes([b[6], b[7]])
    };
    let advreachable = u32::from_be_bytes(bytes_at(&b, 8));
    let retrans = u32::from_be_bytes(bytes_at(&b, 12));
    let curhoplimit = b[4];
    let name = ifp.name.clone();
    if let Some(i) = n.ifaces.iter_mut().find(|i| i.index == ifindex) {
        if advreachable != 0
            && advreachable <= MAX_REACHABLE_TIME
            && i.nd.basereachable != advreachable
        {
            i.nd.basereachable = advreachable;
            i.nd.reachable = nd6::compute_rtime(advreachable);
            i.nd.recalctm = 7200;
        }
        if retrans != 0 {
            i.nd.retrans = retrans;
        }
        if curhoplimit != 0 {
            if i.nd.chlim < curhoplimit {
                i.nd.chlim = curhoplimit;
            } else if i.nd.chlim != curhoplimit {
                crate::log(&alloc::format!(
                    "RA with a lower CurHopLimit sent from {} on {name} (current = {}, received = {}). Ignored.\n",
                    nd6::fmt6(&saddr),
                    i.nd.chlim,
                    curhoplimit
                ));
            }
        }
    }
    let dr = DefRouter {
        addr: saddr,
        ifindex,
        raflags,
        rtlifetime,
        expire: now + u64::from(rtlifetime),
        installed: false,
    };
    let have_dr = defrtrlist_update(&mut n, st, dr);
    // Its prefixes.
    for &(at, olen) in &o.pi {
        if olen != 32 {
            continue;
        }
        let pi = &opts[at..at + 32];
        let plen = pi[2];
        if plen > 128 {
            continue;
        }
        let prefix: In6 = bytes_at(pi, 16);
        if net::addr::is_multicast6(&prefix) || is_linklocal(&prefix) {
            continue;
        }
        let new = Prefix {
            ifindex,
            prefix,
            plen,
            vltime: u32::from_be_bytes(bytes_at(pi, 4)),
            pltime: u32::from_be_bytes(bytes_at(pi, 8)),
            raf_onlink: pi[3] & PI_ONLINK != 0,
            raf_auto: pi[3] & PI_AUTO != 0,
            lastupdate: now,
            stateflags: 0,
            onlink: false,
            addrcnt: 0,
            advrtrs: Vec::new(),
        };
        prelist_update(&mut n, st, new, have_dr.then_some(saddr), mcast);
    }
    // The MTU option.
    if let Some(at) = o.mtu
        && opts.get(at + 1) == Some(&1)
    {
        let mtu = u32::from_be_bytes(bytes_at(opts, at + 4));
        if let Some(i) = n.ifaces.iter_mut().find(|i| i.index == ifindex)
            && mtu >= ip6::MMTU
            && mtu <= i.mtu
        {
            i.nd.linkmtu = mtu;
        }
    }
    // The router's link-layer address.
    let Ok(ll) = lladdr_opt(opts, o.src_ll) else {
        bad();
        return;
    };
    nd6::cache_lladdr(&mut n, st, ifindex, &saddr, ll, ty::ROUTER_ADVERT, 0);
    pfxlist_onlink_check(&mut n, st);
}

/// `defrtrlist_update`: router `new` added or refreshed (or, with a
/// lifetime of 0, removed), the list kept in preference order; whether
/// it is on the list after.
fn defrtrlist_update(n: &mut Net, st: &State, new: DefRouter) -> bool {
    sync_installed(n, st);
    if new.rtlifetime == 0 {
        defrouter_remove(n, st, &new.addr, new.ifindex);
        return false;
    }
    {
        let mut nd = st.nd6.lock();
        if let Some(i) = nd
            .routers
            .iter()
            .position(|d| d.addr == new.addr && d.ifindex == new.ifindex)
        {
            let old = nd.routers[i];
            let mut d = old;
            d.raflags = new.raflags;
            d.rtlifetime = new.rtlifetime;
            d.expire = new.expire;
            if d.installed && rtpref(new.raflags) == rtpref(old.raflags) {
                nd.routers[i] = d;
                return true;
            }
            nd.routers.remove(i);
            let at = nd
                .routers
                .iter()
                .position(|x| rtpref(d.raflags) > rtpref(x.raflags))
                .unwrap_or(nd.routers.len());
            nd.routers.insert(at, d);
        } else {
            let at = nd
                .routers
                .iter()
                .position(|x| rtpref(new.raflags) > rtpref(x.raflags))
                .unwrap_or(nd.routers.len());
            nd.routers.insert(at, new);
        }
    }
    defrouter_select(n, st);
    true
}

/// `nd6_subscription_cb`'s `check_release_defrouter`: a router whose
/// default route someone deleted (`route flush`) is no longer installed.
fn sync_installed(n: &Net, st: &State) {
    let mut nd = st.nd6.lock();
    for d in nd.routers.iter_mut().filter(|d| d.installed) {
        let fib = n.by_index(d.ifindex).map_or(0, |i| i.fib);
        let there = n.routes.iter().any(|r| {
            r.family == Family::Inet6
                && r.fib == fib
                && r.plen == 0
                && r.gateway == Gateway::Addr(d.addr)
                && r.ifindex == d.ifindex
        });
        if !there {
            d.installed = false;
        }
    }
}

/// `is_dr_reachable`: its entry is past incomplete.
fn reachable(n: &Net, d: &DefRouter) -> bool {
    n.lle(d.ifindex, Family::Inet6, &d.addr)
        .is_some_and(|e| e.state > state::ND6_INCOMPLETE)
}

/// `defrouter_select_fib` for every table: the first reachable router
/// (or, with none, the one after the installed one, round robin) gets
/// the default route unless the installed one is reachable and no
/// worse.
pub(crate) fn defrouter_select(n: &mut Net, st: &State) {
    sync_installed(n, st);
    let fibs: Vec<u16> = (0..u16::try_from(net::numfibs()).unwrap_or(1)).collect();
    for fib in fibs {
        let routers: Vec<DefRouter> = st.nd6.lock().routers.clone();
        let fib_of = |n: &Net, d: &DefRouter| n.by_index(d.ifindex).map_or(0, |i| i.fib);
        let mine: Vec<usize> = (0..routers.len())
            .filter(|&i| fib_of(n, &routers[i]) == fib)
            .collect();
        if mine.is_empty() {
            continue;
        }
        let installed = mine.iter().copied().find(|&i| routers[i].installed);
        let mut selected = mine.iter().copied().find(|&i| reachable(n, &routers[i]));
        if selected.is_none() {
            let start = installed.map_or(0, |i| i + 1);
            selected = (start..routers.len())
                .chain(0..routers.len())
                .find(|&i| fib_of(n, &routers[i]) == fib);
        } else if let (Some(s), Some(ins)) = (selected, installed)
            && reachable(n, &routers[ins])
            && rtpref(routers[s].raflags) <= rtpref(routers[ins].raflags)
        {
            selected = Some(ins);
        }
        if installed != selected {
            if let Some(i) = installed {
                delreq(n, st, &routers[i]);
            }
            if let Some(i) = selected {
                addreq(n, st, &routers[i]);
            }
        }
    }
}

/// `defrouter_addreq`: the default route through `d`.
fn addreq(n: &mut Net, st: &State, d: &DefRouter) {
    let Some(ifp) = n.by_index(d.ifindex) else {
        return;
    };
    let fib = ifp.fib;
    let Some(ifa) = in6::linklocal_of(ifp, 0).or_else(|| {
        ifp.addrs
            .iter()
            .find(|a| a.family == Family::Inet6)
            .map(|a| a.addr)
    }) else {
        return;
    };
    let r = Route {
        family: Family::Inet6,
        dst: [0; 16],
        plen: 0,
        gateway: Gateway::Addr(d.addr),
        flags: rtf::UP | rtf::GATEWAY,
        ifindex: d.ifindex,
        ifa,
        mtu: 0,
        weight: 1,
        expire: 0,
        nhidx: 0,
        fib,
    };
    if n.add_route(r).is_ok()
        && let Some(x) = st
            .nd6
            .lock()
            .routers
            .iter_mut()
            .find(|x| x.addr == d.addr && x.ifindex == d.ifindex)
    {
        x.installed = true;
    }
}

/// `defrouter_delreq`: its default route goes.
fn delreq(n: &mut Net, st: &State, d: &DefRouter) {
    let fib = n.by_index(d.ifindex).map_or(0, |i| i.fib);
    let r = n
        .routes
        .iter()
        .find(|r| {
            r.family == Family::Inet6
                && r.fib == fib
                && r.plen == 0
                && r.gateway == Gateway::Addr(d.addr)
                && r.ifindex == d.ifindex
        })
        .cloned();
    if let Some(r) = r {
        let _ = n.delete_exact(&r);
    }
    if let Some(x) = st
        .nd6
        .lock()
        .routers
        .iter_mut()
        .find(|x| x.addr == d.addr && x.ifindex == d.ifindex)
    {
        x.installed = false;
    }
}

/// `defrouter_remove` with `defrouter_del`: router `addr` on `ifindex`
/// goes, with its routes and as an advertiser of prefixes; another is
/// chosen. Whether it was on the list.
pub(crate) fn defrouter_remove(n: &mut Net, st: &State, addr: &In6, ifindex: u16) -> bool {
    let d = {
        let mut nd = st.nd6.lock();
        let Some(i) = nd
            .routers
            .iter()
            .position(|d| d.addr == *addr && d.ifindex == ifindex)
        else {
            return false;
        };
        nd.routers.remove(i)
    };
    defrouter_del(n, st, &d);
    true
}

/// `defrouter_del`.
fn defrouter_del(n: &mut Net, st: &State, d: &DefRouter) {
    let accept = n
        .by_index(d.ifindex)
        .is_some_and(|i| i.nd6_flags & nd6_iff::ACCEPT_RTADV != 0);
    if accept {
        rt6_flush(n, &d.addr, d.ifindex);
    }
    if d.installed {
        delreq(n, st, d);
    }
    for p in &mut st.nd6.lock().prefixes {
        p.advrtrs.retain(|r| r != &d.addr);
    }
    pfxlist_onlink_check(n, st);
    if d.installed {
        defrouter_select(n, st);
    }
}

/// `rt6_flush`: the dynamic routes (redirects) through `gw` go.
pub(crate) fn rt6_flush(n: &mut Net, gw: &In6, ifindex: u16) {
    if !is_linklocal(gw) {
        return;
    }
    let gone: Vec<Route> = n
        .routes
        .iter()
        .filter(|r| {
            r.family == Family::Inet6
                && r.flags & RTF_DYNAMIC != 0
                && r.gateway == Gateway::Addr(*gw)
                && r.ifindex == ifindex
        })
        .cloned()
        .collect();
    for r in gone {
        let _ = n.delete_exact(&r);
    }
}

/// `nd6_prefix_onlink`: the prefix's route out of its interface, unless
/// another on-link prefix already has it.
fn prefix_onlink(n: &mut Net, st: &State, idx: usize) {
    let p = {
        let nd = st.nd6.lock();
        let Some(p) = nd.prefixes.get(idx) else {
            return;
        };
        if p.stateflags & NDPRF_ONLINK != 0 {
            return;
        }
        let taken = nd.prefixes.iter().enumerate().any(|(i, o)| {
            i != idx
                && (!is_linklocal(&p.prefix) || o.ifindex == p.ifindex)
                && o.stateflags & NDPRF_ONLINK != 0
                && o.plen == p.plen
                && masked(Family::Inet6, &o.prefix, o.plen)
                    == masked(Family::Inet6, &p.prefix, p.plen)
        });
        if taken {
            return;
        }
        p.clone()
    };
    let Some(ifp) = n.by_index(p.ifindex) else {
        return;
    };
    let ifa = in6::linklocal_of(ifp, in6_iff::NOTREADY | in6_iff::ANYCAST).or_else(|| {
        ifp.addrs
            .iter()
            .find(|a| a.family == Family::Inet6)
            .map(|a| a.addr)
    });
    let Some(ifa) = ifa else {
        return;
    };
    let fib = ifp.fib;
    let mtu = ifp.mtu;
    let dst = masked(Family::Inet6, &p.prefix, p.plen);
    let exists = n.routes.iter().any(|r| {
        r.family == Family::Inet6
            && r.fib == fib
            && r.plen == p.plen
            && r.dst == dst
            && r.ifindex == p.ifindex
    });
    let ok = exists
        || n.add_route(Route {
            family: Family::Inet6,
            dst,
            plen: p.plen,
            gateway: Gateway::Link(p.ifindex),
            flags: rtf::UP | rtf::PINNED | if p.plen == 128 { rtf::HOST } else { 0 },
            ifindex: p.ifindex,
            ifa,
            mtu,
            weight: 1,
            expire: 0,
            nhidx: 0,
            fib,
        })
        .is_ok();
    if ok && let Some(p) = st.nd6.lock().prefixes.get_mut(idx) {
        p.stateflags |= NDPRF_ONLINK;
        p.onlink = true;
    }
}

/// `nd6_prefix_offlink`: its route goes (another prefix like it takes
/// over); with none to take over, the neighbor entries inside go.
fn prefix_offlink(n: &mut Net, st: &State, p: &Prefix) {
    let fib = n.by_index(p.ifindex).map_or(0, |i| i.fib);
    let dst = masked(Family::Inet6, &p.prefix, p.plen);
    let r = n
        .routes
        .iter()
        .find(|r| {
            r.family == Family::Inet6
                && r.fib == fib
                && r.plen == p.plen
                && r.dst == dst
                && r.ifindex == p.ifindex
                && r.flags & rtf::GATEWAY == 0
        })
        .cloned();
    if let Some(r) = r {
        let _ = n.delete_exact(&r);
    }
    let heir = {
        let mut nd = st.nd6.lock();
        if let Some(x) = nd
            .prefixes
            .iter_mut()
            .find(|x| x.ifindex == p.ifindex && x.prefix == p.prefix && x.plen == p.plen)
        {
            x.stateflags &= !NDPRF_ONLINK;
            x.onlink = false;
        }
        nd.prefixes.iter().position(|o| {
            o.stateflags & (NDPRF_ONLINK | NDPRF_DETACHED) == 0
                && o.ifindex != p.ifindex
                && o.plen == p.plen
                && masked(Family::Inet6, &o.prefix, o.plen) == dst
        })
    };
    match heir {
        Some(i) => prefix_onlink(n, st, i),
        None => n.lle_prefix_free(Family::Inet6, &p.prefix, p.plen, &[0xff; 16], false),
    }
}

/// `nd6_prelist_add`: a new prefix, on-link if it says so, `router`
/// among its advertisers.
fn prelist_add(n: &mut Net, st: &State, mut p: Prefix, router: Option<In6>) -> usize {
    p.prefix = masked(Family::Inet6, &p.prefix, p.plen);
    if let Some(r) = router {
        p.advrtrs.push(r);
    }
    let onlink = p.raf_onlink;
    let idx = {
        let mut nd = st.nd6.lock();
        nd.prefixes.insert(0, p);
        0
    };
    if onlink {
        prefix_onlink(n, st, idx);
    }
    idx
}

/// `nd6_prefix_lookup`.
fn lookup(st: &State, ifindex: u16, prefix: &In6, plen: u8) -> Option<usize> {
    let key = masked(Family::Inet6, prefix, plen);
    st.nd6
        .lock()
        .prefixes
        .iter()
        .position(|p| p.ifindex == ifindex && p.plen == plen && p.prefix == key)
}

/// `prelist_update`: an advertised prefix updates (or adds) the prefix
/// list, and an autonomous one the addresses made from it (RFC 4862
/// 5.5.3), or makes the first.
fn prelist_update(n: &mut Net, st: &State, new: Prefix, router: Option<In6>, mcast: bool) {
    let now = net::uptime();
    let idx = if let Some(i) = lookup(st, new.ifindex, &new.prefix, new.plen) {
        let online = {
            let mut nd = st.nd6.lock();
            let p = &mut nd.prefixes[i];
            if new.raf_onlink {
                p.raf_onlink = true;
            }
            if new.raf_auto {
                p.raf_auto = true;
            }
            if new.raf_onlink {
                p.vltime = new.vltime;
                p.pltime = new.pltime;
                p.lastupdate = now;
            }
            if let Some(r) = router
                && !p.advrtrs.contains(&r)
            {
                p.advrtrs.push(r);
            }
            new.raf_onlink && p.stateflags & NDPRF_ONLINK == 0
        };
        if online {
            prefix_onlink(n, st, i);
        }
        i
    } else {
        if new.vltime == 0 || (!new.raf_onlink && !new.raf_auto) {
            return;
        }
        let i = prelist_add(n, st, new.clone(), router);
        let mut nd = st.nd6.lock();
        if let Some(p) = nd.prefixes.get_mut(i)
            && !p.raf_onlink
        {
            p.vltime = 0;
            p.pltime = 0;
        }
        i
    };
    if !new.raf_auto || new.pltime > new.vltime {
        return;
    }
    // The addresses from it, their lifetimes per RFC 4862 5.5.3 (e).
    let mut matched = false;
    let ifindex = new.ifindex;
    let prefix = masked(Family::Inet6, &new.prefix, new.plen);
    if let Some(ifp) = n.ifaces.iter_mut().find(|i| i.index == ifindex) {
        for a in ifp.addrs.iter_mut().filter(|a| {
            a.family == Family::Inet6
                && a.flags6 & in6_iff::AUTOCONF != 0
                && a.flags6 & in6_iff::ANYCAST == 0
                && a.plen == new.plen
                && masked(Family::Inet6, &a.addr, a.plen) == prefix
        }) {
            let remaining = if a.vltime == INFINITE {
                INFINITE
            } else if now.saturating_sub(a.updated) > u64::from(a.vltime) {
                0
            } else {
                a.vltime - u32::try_from(now.saturating_sub(a.updated)).unwrap_or(u32::MAX)
            };
            let vltime = if TWOHOUR < new.vltime || remaining < new.vltime {
                new.vltime
            } else if remaining <= TWOHOUR {
                remaining
            } else {
                TWOHOUR
            };
            a.vltime = vltime;
            a.pltime = new.pltime;
            a.updated = now;
            a.flags6 &= !in6_iff::DEPRECATED;
            if a.flags6 & (in6_iff::DUPLICATED | in6_iff::TEMPORARY) == 0 {
                matched = true;
            }
        }
    }
    if !matched && new.vltime != 0 {
        // `in6_if2idlen`: Ethernet interfaces' identifiers are 64 bits.
        let idlen = match n.by_index(ifindex).map(|i| i.ty) {
            Some(IFT_ETHER) => 64,
            _ => return,
        };
        if usize::from(new.plen) + idlen != 128 {
            return;
        }
        if in6_ifadd(n, st, &new, mcast) {
            if let Some(p) = st.nd6.lock().prefixes.get_mut(idx) {
                p.addrcnt += 1;
            }
            pfxlist_onlink_check(n, st);
        }
    }
}

/// `in6_ifadd`: the address from prefix `p` and the interface identifier
/// of its link-local address, autoconfigured with the prefix's
/// lifetimes, its DAD delayed if the advertisement was multicast.
fn in6_ifadd(n: &mut Net, st: &State, p: &Prefix, mcast: bool) -> bool {
    let _ = st;
    let Some(ifp) = n.by_index(p.ifindex) else {
        return false;
    };
    let Some(ll) = ifp
        .addrs
        .iter()
        .find(|a| a.family == Family::Inet6 && is_linklocal(&a.addr))
        .cloned()
    else {
        return false;
    };
    if ll.plen != p.plen {
        return false;
    }
    let name = ifp.name.clone();
    let mut addr = masked(Family::Inet6, &p.prefix, p.plen);
    for (i, b) in addr.iter_mut().enumerate() {
        let keep = masked(Family::Inet6, &[0xff; 16], p.plen)[i];
        *b |= ll.addr[i] & !keep;
    }
    if in6::ifa_flags(ifp, &addr).is_some() {
        crate::log(&alloc::format!(
            "in6_ifadd: {} is already configured\n",
            nd6::fmt6(&addr)
        ));
        return false;
    }
    let a = Ifaddr {
        family: Family::Inet6,
        addr,
        plen: p.plen,
        dst: None,
        broadcast: None,
        flags6: in6_iff::AUTOCONF,
        vltime: p.vltime,
        pltime: p.pltime,
        vhid: 0,
        created: 0,
        updated: 0,
    };
    let flags = if mcast { ifa6_update::DADDELAY } else { 0 };
    n.add_addr_flags(name.as_bytes(), a, flags).is_ok()
}

/// `pfxlist_onlink_check`, simplified: each prefix that should be
/// on-link (advertised so, not detached) has its route.
pub(crate) fn pfxlist_onlink_check(n: &mut Net, st: &State) {
    let want: Vec<usize> = st
        .nd6
        .lock()
        .prefixes
        .iter()
        .enumerate()
        .filter(|(_, p)| p.raf_onlink && p.stateflags & (NDPRF_ONLINK | NDPRF_DETACHED) == 0)
        .map(|(i, _)| i)
        .collect();
    for i in want {
        prefix_onlink(n, st, i);
    }
}

/// `in6_ifattach_linklocal`'s prefix: `fe80::/64` of the interface, on
/// the list and on-link.
pub(crate) fn linklocal_prefix(n: &mut Net, st: &State, ifindex: u16, addr: &In6) {
    if lookup(st, ifindex, addr, 64).is_some() {
        return;
    }
    let p = Prefix {
        ifindex,
        prefix: *addr,
        plen: 64,
        vltime: INFINITE,
        pltime: INFINITE,
        raf_onlink: true,
        raf_auto: true,
        lastupdate: net::uptime(),
        stateflags: 0,
        onlink: false,
        // The address it was made for.
        addrcnt: 1,
        advrtrs: Vec::new(),
    };
    prelist_add(n, st, p, None);
}

/// `in6_aifaddr`'s prefix: an address set by hand (not a host route's)
/// puts its prefix on the list with its lifetimes; a prefix already
/// there only has them lengthened. A new address counts against it.
pub(crate) fn aifaddr_prefix(n: &mut Net, st: &State, ifindex: u16, addr: &In6, new: bool) {
    let Some(a) = n.by_index(ifindex).and_then(|i| {
        i.addrs
            .iter()
            .find(|a| a.family == Family::Inet6 && a.addr == *addr)
            .cloned()
    }) else {
        return;
    };
    if a.plen == 128 {
        return;
    }
    let now = net::uptime();
    // `in6_expire_time`: 0 for an infinite lifetime.
    let expiry = |lt: u32| {
        if lt == INFINITE {
            0
        } else {
            now + u64::from(lt)
        }
    };
    let current = |last: u64, lt: u32| {
        if lt == INFINITE {
            0
        } else {
            last + u64::from(lt)
        }
    };
    if let Some(i) = lookup(st, ifindex, addr, a.plen) {
        let mut nd = st.nd6.lock();
        let p = &mut nd.prefixes[i];
        if p.raf_onlink {
            let (np, nv) = (expiry(a.pltime), expiry(a.vltime));
            let (cp, cv) = (
                current(p.lastupdate, p.pltime),
                current(p.lastupdate, p.vltime),
            );
            if cp != 0 && (cp < np || np == 0) {
                p.pltime = a.pltime;
            }
            if cv != 0 && (cv < nv || nv == 0) {
                p.vltime = a.vltime;
            }
            p.lastupdate = now;
        }
        if new {
            p.addrcnt += 1;
        }
        return;
    }
    let p = Prefix {
        ifindex,
        prefix: *addr,
        plen: a.plen,
        vltime: a.vltime,
        pltime: a.pltime,
        raf_onlink: true,
        raf_auto: a.flags6 & in6_iff::AUTOCONF != 0,
        lastupdate: now,
        stateflags: 0,
        onlink: false,
        addrcnt: u32::from(new),
        advrtrs: Vec::new(),
    };
    prelist_add(n, st, p, None);
}

/// An address left: its prefix has one fewer, and a prefix set by hand
/// with none left goes (`in6_unlink_ifa`'s `nd6_prefix_unref`).
pub(crate) fn ifa_gone(n: &mut Net, st: &State, ifindex: u16, a: &Ifaddr) {
    let Some(i) = lookup(st, ifindex, &a.addr, a.plen) else {
        return;
    };
    let gone = {
        let mut nd = st.nd6.lock();
        let p = &mut nd.prefixes[i];
        p.addrcnt = p.addrcnt.saturating_sub(1);
        if p.addrcnt == 0 && p.advrtrs.is_empty() && !is_linklocal(&p.prefix) {
            Some(nd.prefixes.remove(i))
        } else {
            None
        }
    };
    if let Some(p) = gone
        && p.stateflags & NDPRF_ONLINK != 0
    {
        prefix_offlink(n, st, &p);
    }
}

/// `nd6_purge`: an interface's routers and prefixes go.
pub(crate) fn purge(n: &mut Net, st: &State, ifindex: u16) {
    let routers: Vec<DefRouter> = {
        let mut nd = st.nd6.lock();
        let r = nd
            .routers
            .iter()
            .copied()
            .filter(|d| d.ifindex == ifindex)
            .collect();
        nd.routers.retain(|d| d.ifindex != ifindex);
        nd.prefixes.retain(|p| p.ifindex != ifindex);
        if nd.defifindex == ifindex {
            nd.defifindex = 0;
        }
        r
    };
    for d in routers {
        if d.installed {
            delreq(n, st, &d);
        }
    }
    defrouter_select(n, st);
}

/// Whether `nd6_timer` has anything to age: a router or prefix with a
/// lifetime, an address with one, or a tentative address not under DAD.
pub(crate) fn needs_timer(n: &Net, nd: &nd6::Nd6) -> bool {
    if !nd.routers.is_empty() || nd.prefixes.iter().any(|p| p.vltime != INFINITE) {
        return true;
    }
    n.ifaces.iter().any(|i| {
        i.addrs.iter().any(|a| {
            a.family == Family::Inet6
                && (a.vltime != INFINITE
                    || a.pltime != INFINITE
                    || (a.flags6 & in6_iff::TENTATIVE != 0
                        && !nd
                            .dad
                            .iter()
                            .any(|d| d.ifindex == i.index && d.addr == a.addr)))
        })
    })
}

/// `nd6_timer`: routers past their lifetime go; addresses past their
/// valid lifetime go and past their preferred one are deprecated; a
/// tentative address gets DAD; addresses on an interface that is down
/// become tentative; prefixes past their lifetime go.
pub(crate) fn nd6_timer(n: &mut Net, st: &State, now_ns: u64) {
    let _ = now_ns;
    let now = net::uptime();
    // `nd6_defrouter_timer`.
    let expired: Vec<DefRouter> = {
        let mut nd = st.nd6.lock();
        let e = nd
            .routers
            .iter()
            .copied()
            .filter(|d| d.expire != 0 && d.expire < now)
            .collect();
        nd.routers.retain(|d| !(d.expire != 0 && d.expire < now));
        e
    };
    for d in expired {
        defrouter_del(n, st, &d);
    }
    // The addresses.
    let mut purge_addrs = Vec::new();
    let mut start_dad = Vec::new();
    let dad: Vec<(u16, In6)> = st
        .nd6
        .lock()
        .dad
        .iter()
        .map(|d| (d.ifindex, d.addr))
        .collect();
    for i in &mut n.ifaces {
        let down = i.flags & (abi::net::iff::UP | abi::net::iff::RUNNING)
            != abi::net::iff::UP | abi::net::iff::RUNNING
            || i.nd6_flags & nd6_iff::IFDISABLED != 0;
        let no_dad = i.nd6_flags & nd6_iff::NO_DAD != 0;
        for a in i.addrs.iter_mut().filter(|a| a.family == Family::Inet6) {
            if a.is_invalid(now) {
                purge_addrs.push((i.name.clone(), a.addr));
            } else if a.is_deprecated(now) {
                a.flags6 |= in6_iff::DEPRECATED;
            } else if a.flags6 & in6_iff::TENTATIVE != 0 {
                if !dad.contains(&(i.index, a.addr)) {
                    start_dad.push((i.index, a.addr));
                }
            } else {
                if !no_dad && down && i.flags & abi::net::iff::LOOPBACK == 0 {
                    a.flags6 &= !in6_iff::DUPLICATED;
                    a.flags6 |= in6_iff::TENTATIVE;
                }
                a.flags6 &= !in6_iff::DEPRECATED;
            }
        }
    }
    for (name, a) in purge_addrs {
        let _ = n.delete_addr(name.as_bytes(), Family::Inet6, Some(&a));
    }
    for (ifindex, a) in start_dad {
        nd6::dad_start(n, st, ifindex, &a, nd6::rtr_delay_ms());
    }
    // The prefixes.
    let (gone, offlink): (Vec<Prefix>, Vec<Prefix>) = {
        let mut nd = st.nd6.lock();
        let mut gone = Vec::new();
        let mut off = Vec::new();
        nd.prefixes.retain(|p| {
            if p.vltime == INFINITE || now.saturating_sub(p.lastupdate) <= u64::from(p.vltime) {
                return true;
            }
            if p.addrcnt == 0 {
                gone.push(p.clone());
                return false;
            }
            if p.stateflags & NDPRF_ONLINK != 0 {
                off.push(p.clone());
            }
            true
        });
        (gone, off)
    };
    for p in offlink {
        prefix_offlink(n, st, &p);
    }
    for p in gone {
        if p.stateflags & NDPRF_ONLINK != 0 {
            prefix_offlink(n, st, &p);
        }
    }
    // `nd6_slowtimo` is folded in: each interface's reachable time is
    // drawn again every two hours.
    for i in &mut n.ifaces {
        if i.nd.initialized {
            i.nd.recalctm -= 1;
            if i.nd.recalctm <= 0 {
                i.nd.recalctm = 7200;
                i.nd.reachable = nd6::compute_rtime(i.nd.basereachable);
            }
        }
    }
    let _ = val;
}

/// `nd6_sysctl_drlist` (`net.inet6.icmp6.nd6_drlist`): a `struct
/// in6_defrouter` for each router; `realtime` and `uptime` are the
/// clocks' seconds now.
#[must_use]
pub fn drlist(vnet: &Vnet, realtime: i64, uptime: u64) -> Vec<u8> {
    let st = crate::state::of(vnet);
    let routers = st.nd6.lock().routers.clone();
    let mut out = Vec::new();
    for d in routers {
        let mut b = [0u8; 48];
        b[..28].copy_from_slice(&net::addr::sockaddr_in6(&d.addr, 0, u32::from(d.ifindex)));
        b[28] = d.raflags;
        b[30..32].copy_from_slice(&d.rtlifetime.to_ne_bytes());
        let expire =
            i64::try_from(d.expire).unwrap_or(0) + realtime - i64::try_from(uptime).unwrap_or(0);
        b[32..40].copy_from_slice(&expire.to_ne_bytes());
        b[40..42].copy_from_slice(&d.ifindex.to_ne_bytes());
        out.extend_from_slice(&b);
    }
    out
}

/// `nd6_sysctl_prlist` (`net.inet6.icmp6.nd6_prlist`): a `struct
/// in6_prefix` for each prefix, then its advertising routers'
/// `sockaddr_in6`s.
#[must_use]
pub fn prlist(vnet: &Vnet, realtime: i64, uptime: u64) -> Vec<u8> {
    let st = crate::state::of(vnet);
    let prefixes = st.nd6.lock().prefixes.clone();
    let mut out = Vec::new();
    for p in prefixes {
        let mut b = [0u8; 64];
        let zone = if is_linklocal(&p.prefix) {
            u32::from(p.ifindex)
        } else {
            0
        };
        b[..28].copy_from_slice(&net::addr::sockaddr_in6(&p.prefix, 0, zone));
        b[28] = u8::from(p.raf_onlink) | (u8::from(p.raf_auto) << 1);
        b[29] = p.plen;
        b[32..36].copy_from_slice(&p.vltime.to_ne_bytes());
        b[36..40].copy_from_slice(&p.pltime.to_ne_bytes());
        let expire = if p.vltime == INFINITE {
            0i64
        } else {
            i64::try_from(p.lastupdate + u64::from(p.vltime)).unwrap_or(i64::MAX) + realtime
                - i64::try_from(uptime).unwrap_or(0)
        };
        b[40..48].copy_from_slice(&expire.to_ne_bytes());
        b[48..52].copy_from_slice(&p.stateflags.to_ne_bytes());
        b[52..56].copy_from_slice(&i32::try_from(p.addrcnt).unwrap_or(0).to_ne_bytes());
        b[56..58].copy_from_slice(&p.ifindex.to_ne_bytes());
        b[58..60].copy_from_slice(&u16::try_from(p.advrtrs.len()).unwrap_or(0).to_ne_bytes());
        out.extend_from_slice(&b);
        for r in &p.advrtrs {
            out.extend_from_slice(&net::addr::sockaddr_in6(r, 0, u32::from(p.ifindex)));
        }
    }
    out
}

/// `SIOCSRTRFLUSH_IN6` (`ndp -R`): every default router goes.
pub fn flush_routers(vnet: &Vnet) {
    let st = crate::state::of(vnet);
    let mut n = vnet.net.lock();
    let routers: Vec<DefRouter> = core::mem::take(&mut st.nd6.lock().routers);
    for d in routers {
        defrouter_del(&mut n, st, &d);
    }
}

/// `SIOCSNDFLUSH_IN6` (`defrouter_reset` and `defrouter_select_fib`):
/// the routers' default routes go and the choice is made again.
pub fn reset_routers(vnet: &Vnet) {
    let st = crate::state::of(vnet);
    let mut n = vnet.net.lock();
    let routers: Vec<DefRouter> = st.nd6.lock().routers.clone();
    for d in routers.iter().filter(|d| d.installed) {
        delreq(&mut n, st, d);
    }
    defrouter_select(&mut n, st);
}

/// `SIOCSPFXFLUSH_IN6` (`ndp -P`): every prefix made by advertisements
/// goes, with the addresses made from it.
pub fn flush_prefixes(vnet: &Vnet) {
    let st = crate::state::of(vnet);
    let mut n = vnet.net.lock();
    let gone: Vec<Prefix> = {
        let mut nd = st.nd6.lock();
        let mut g = Vec::new();
        nd.prefixes.retain(|p| {
            if is_linklocal(&p.prefix) {
                true
            } else {
                g.push(p.clone());
                false
            }
        });
        g
    };
    for p in gone {
        let addrs: Vec<(alloc::string::String, In6)> = n
            .by_index(p.ifindex)
            .map(|i| {
                i.addrs
                    .iter()
                    .filter(|a| {
                        a.family == Family::Inet6
                            && a.flags6 & in6_iff::AUTOCONF != 0
                            && prefix_match(Family::Inet6, &a.addr, &p.prefix, p.plen)
                    })
                    .map(|a| (i.name.clone(), a.addr))
                    .collect()
            })
            .unwrap_or_default();
        for (name, a) in addrs {
            let _ = n.delete_addr(name.as_bytes(), Family::Inet6, Some(&a));
        }
        if p.stateflags & NDPRF_ONLINK != 0 {
            prefix_offlink(&mut n, st, &p);
        }
    }
}

/// The default interface (`V_nd6_defifindex`), 0 for none.
#[must_use]
pub fn defifindex(vnet: &Vnet) -> u16 {
    crate::state::of(vnet).nd6.lock().defifindex
}

/// Sets the default interface (`nd6_setdefaultiface`).
pub fn set_defifindex(vnet: &Vnet, ifindex: u16) {
    crate::state::of(vnet).nd6.lock().defifindex = ifindex;
}
