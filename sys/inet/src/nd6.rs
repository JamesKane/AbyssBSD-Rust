// SPDX-License-Identifier: BSD-2-Clause
//! Neighbor discovery (docs/NETWORKING.md §7, §8): the IPv6 entries of the
//! link-layer table with neighbor unreachability detection's states and
//! timers, neighbor solicitations and advertisements in and out, address
//! resolution with a queue of packets held for it, duplicate address
//! detection, redirects, and the path MTUs ICMPv6 learns. Router
//! advertisements and the prefix and router lists are `nd6_rtr`'s.
//!
//! What runs from the interface layer (resolution, an address or
//! interface coming and going) runs under the interfaces' lock; this
//! crate's ND state is taken after it, never before.
//!
//! Behavior reference: sys/netinet6/nd6.c (`nd6_resolve`,
//! `nd6_resolve_slow`, `nd6_llinfo_setstate`, `nd6_llinfo_timer`,
//! `nd6_is_stale`, `nd6_cache_lladdr`, `nd6_is_router`,
//! `nd6_is_new_addr_neighbor`, `nd6_options`, `nd6_timer`, `nd6_free`),
//! sys/netinet6/nd6_nbr.c (`nd6_ns_input`, `nd6_ns_output_fib`,
//! `nd6_na_input`, `nd6_na_output_fib`, `nd6_dad_*`), sys/netinet6/icmp6.c
//! (`icmp6_redirect_input`, `icmp6_redirect_output`), sys/netinet6/in6.c
//! (`in6_broadcast_ifa`, `in6_update_ifa_join_mc`, `in6_purgeaddr`,
//! `in6_if_up`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::net::{IFT_ETHER, iff, nd6_iff, rtf};
use mbuf::Packet;
use net::Family;
use net::Vnet;
use net::addr::{In6, is_multicast6, is_unspecified};
use net::arp::Resolved;
use net::ifnet::{Gateway, Net, Route, icmp6_ifs, ifa6_update, in6_iff};
use net::lltable::{Lle, lle, state as lls};

use crate::icmp6::{self, ty};
use crate::in6::{self, is_linklocal, is_zoned};
use crate::ip6::{self, HDR_LEN, Ip6, Mopts6, Opts6, out, proto};
use crate::state::{self, State, icp6s, on, val};

/// ND's options (`ND_OPT_*`).
pub mod opt {
    pub const SOURCE_LINKADDR: u8 = 1;
    pub const TARGET_LINKADDR: u8 = 2;
    pub const PREFIX_INFORMATION: u8 = 3;
    pub const REDIRECTED_HEADER: u8 = 4;
    pub const MTU: u8 = 5;
    pub const NONCE: u8 = 14;
    pub const RDNSS: u8 = 25;
    pub const DNSSL: u8 = 31;
}

/// Neighbor advertisement flags (`ND_NA_FLAG_*`), in the message's first
/// byte of flags.
pub mod na {
    pub const ROUTER: u8 = 0x80;
    pub const SOLICITED: u8 = 0x40;
    pub const OVERRIDE: u8 = 0x20;
}

/// `ND_OPT_NONCE_LEN`.
pub const NONCE_LEN: usize = 6;
/// `MAX_RTR_SOLICITATION_DELAY`, in milliseconds.
pub const MAX_RTR_SOLICITATION_DELAY_MS: u64 = 1000;

const MS: u64 = 1_000_000;
/// `RTF_DYNAMIC`: a route a redirect made.
const RTF_DYNAMIC: i32 = 0x10;
const SEC: u64 = 1_000_000_000;

/// A duplicate address detection under way (`struct dadq`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Dad {
    pub(crate) ifindex: u16,
    pub(crate) addr: In6,
    /// Solicitations to send (`dad_count`), and what came and went:
    /// solicitations in, advertisements in, solicitations out, tries,
    /// and our own looped back.
    count: u32,
    ns_icount: u32,
    na_icount: u32,
    ns_ocount: u32,
    ns_tcount: u32,
    ns_lcount: u32,
    loopbackprobe: u32,
    nonce: [u8; NONCE_LEN],
    /// When its timer fires, nanoseconds.
    at: u64,
}

/// A path MTU learned from packet too big (the host cache's `rmx_mtu`).
#[derive(Clone, Copy, Debug)]
struct Pmtu {
    dst: In6,
    mtu: u32,
    /// When it is forgotten, nanoseconds.
    expire: u64,
}

/// A VNET's ND and MLD state.
#[derive(Debug, Default)]
pub struct Nd6 {
    pub(crate) dad: Vec<Dad>,
    pub(crate) mld: crate::mld6::Mld,
    pub(crate) prefixes: Vec<crate::nd6_rtr::Prefix>,
    pub(crate) routers: Vec<crate::nd6_rtr::DefRouter>,
    pmtu: Vec<Pmtu>,
    /// `V_nd6_defifindex`.
    pub defifindex: u16,
    /// When `nd6_timer` runs next, and the half-second `frag6` tick and
    /// MLD's fast tick.
    pub(crate) slow_at: u64,
    pub(crate) frag_at: u64,
}

impl Nd6 {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dad: Vec::new(),
            mld: crate::mld6::Mld::new(),
            prefixes: Vec::new(),
            routers: Vec::new(),
            pmtu: Vec::new(),
            defifindex: 0,
            slow_at: 0,
            frag_at: 0,
        }
    }
}

/// The host cache's lifetime for a path MTU (`tcp_hostcache.expire`).
const PMTU_EXPIRE: u64 = 3600 * SEC;

/// The path MTU learned toward `dst`, 0 for none.
pub(crate) fn pmtu_get(st: &State, dst: &In6) -> u32 {
    let now = crate::now();
    st.nd6
        .lock()
        .pmtu
        .iter()
        .find(|p| p.dst == *dst && p.expire > now)
        .map_or(0, |p| p.mtu)
}

/// Remembers path MTU `mtu` toward `dst` (`tcp_hc_updatemtu`).
pub(crate) fn pmtu_set(st: &State, dst: &In6, mtu: u32) {
    let expire = crate::now() + PMTU_EXPIRE;
    let mut nd = st.nd6.lock();
    nd.pmtu.retain(|p| p.expire > expire - PMTU_EXPIRE);
    if let Some(p) = nd.pmtu.iter_mut().find(|p| p.dst == *dst) {
        p.mtu = mtu;
        p.expire = expire;
    } else {
        nd.pmtu.push(Pmtu {
            dst: *dst,
            mtu,
            expire,
        });
    }
}

// ---------------------------------------------------------------------
// Timers.

/// The earliest deadline of any VNET's IPv6 timers, nanoseconds;
/// `u64::MAX` for none.
static NEXT: AtomicU64 = AtomicU64::new(u64::MAX);

/// Asks for the timers to run by `at` (nanoseconds), waking the timer
/// thread if that is sooner than it planned.
pub(crate) fn arm(at: u64) {
    // AcqRel: the thread reads the deadline after the kick.
    let prev = NEXT.fetch_min(at, Ordering::AcqRel);
    if at < prev {
        crate::timer_kick();
    }
}

/// When the IPv6 timers next need to run (nanoseconds of the protocols'
/// clock), `u64::MAX` for never.
#[must_use]
pub fn next_deadline() -> u64 {
    // Acquire: pairs with `arm`.
    NEXT.load(Ordering::Acquire)
}

/// Runs every VNET's IPv6 timers that are due at `now` (neighbor
/// entries, DAD, MLD, `nd6_timer`, reassembly), and returns when they
/// next need to run.
pub fn run_timers(now: u64) -> u64 {
    // Release: the thread's next wait sees the deadline computed here.
    NEXT.store(u64::MAX, Ordering::Release);
    let mut next = u64::MAX;
    for v in net::Vnet::all() {
        if v.inet.get().is_none() {
            continue;
        }
        next = next.min(run_vnet(&v, now));
    }
    // AcqRel: a timer armed meanwhile stays the earlier.
    NEXT.fetch_min(next, Ordering::AcqRel);
    next_deadline()
}

/// One VNET's timers.
fn run_vnet(vnet: &Arc<Vnet>, now: u64) -> u64 {
    let st = state::of(vnet);
    let mut errors: Vec<(Packet, u16)> = Vec::new();
    let mut next = u64::MAX;
    {
        let mut n = vnet.net.lock();
        llinfo_timers(&mut n, st, now, &mut errors);
        dad_timers(&mut n, st, now);
        crate::mld6::fasttimo(&mut n, st, now);
        let due = {
            let nd = st.nd6.lock();
            nd.slow_at != 0 && nd.slow_at <= now
        };
        if due {
            crate::nd6_rtr::nd6_timer(&mut n, st, now);
        }
        next = next.min(deadlines(&n, st, now));
    }
    // `frag6_slowtimo`, every half second while fragments wait.
    let frag_due = {
        let mut nd = st.nd6.lock();
        if st.frag6.lock().is_empty() {
            nd.frag_at = 0;
            false
        } else if nd.frag_at == 0 {
            nd.frag_at = now + crate::frag6::TICK_MS * MS;
            false
        } else if nd.frag_at <= now {
            nd.frag_at = now + crate::frag6::TICK_MS * MS;
            true
        } else {
            false
        }
    };
    if frag_due {
        crate::frag6::tick(vnet, st);
    }
    {
        let nd = st.nd6.lock();
        if nd.frag_at != 0 {
            next = next.min(nd.frag_at);
        }
    }
    // `nd6_llinfo_timer`'s unreachable errors, sent once the locks are
    // let go.
    for (m, ifindex) in errors {
        let mut m = m;
        m.hdr.rcvif = ifindex;
        icmp6::error(vnet, m, ty::DST_UNREACH, icmp6::code::DST_UNREACH_ADDR, 0);
    }
    next
}

/// The next deadline among a VNET's timers.
fn deadlines(n: &Net, st: &State, now: u64) -> u64 {
    let mut next = u64::MAX;
    for e in n
        .lle
        .iter()
        .filter(|e| e.family == Family::Inet6 && e.ntimer != 0)
    {
        next = next.min(e.ntimer);
    }
    let mut nd = st.nd6.lock();
    for d in &nd.dad {
        next = next.min(d.at);
    }
    next = next.min(nd.mld.next_tick());
    // `nd6_timer` runs each second while something ages.
    if crate::nd6_rtr::needs_timer(n, &nd) {
        if nd.slow_at == 0 || nd.slow_at <= now {
            nd.slow_at = now + SEC;
        }
        next = next.min(nd.slow_at);
    } else {
        nd.slow_at = 0;
    }
    next
}

// ---------------------------------------------------------------------
// The neighbor cache.

/// `ND6_LLINFO_PERMANENT`: no expiry, and past incomplete.
const fn permanent(e: &Lle) -> bool {
    e.expire == 0 && e.state > lls::ND6_INCOMPLETE
}

/// `ND_COMPUTE_RTIME`: a random reachable time (seconds) around
/// `base` milliseconds.
pub(crate) fn compute_rtime(base: u32) -> u32 {
    let unit = base >> 10;
    (512 * unit + (crate::random() & (1024 * unit))) / 1000
}

/// [`compute_rtime`], for `SIOCSIFINFO_IN6`.
#[must_use]
pub fn rtime(base: u32) -> u32 {
    compute_rtime(base)
}

/// Draws an interface's reachable time the first time IPv6 sees it
/// (`nd6_ifattach`).
pub(crate) fn nd_init(n: &mut Net, ifindex: u16) {
    if let Some(i) = n.ifaces.iter_mut().find(|i| i.index == ifindex)
        && !i.nd.initialized
    {
        i.nd.reachable = compute_rtime(i.nd.basereachable);
        i.nd.initialized = true;
    }
}

/// `nd6_llinfo_setstate`: entry `e` (of interface `ifp`'s) moves to
/// `new`, its timer set for the state.
fn setstate(e: &mut Lle, new: i16, nd: &net::ifnet::NdInfo, st: &State, now: u64) {
    let up = net::uptime();
    let mut delay = 0u64;
    let mut remtime = 0u64;
    match new {
        lls::ND6_INCOMPLETE => delay = u64::from(nd.retrans) * MS,
        lls::ND6_REACHABLE => {
            if !permanent(e) {
                delay = u64::from(nd.reachable) * SEC;
            }
        }
        lls::ND6_STALE => {
            // `llentry_request_feedback`.
            e.hit = 0;
            let d = u64::try_from(val(&st.conf6.nd6_delay)).unwrap_or(5);
            let gc = u64::try_from(val(&st.conf6.nd6_gctimer)).unwrap_or(86_400);
            delay = d.min(gc) * SEC;
            remtime = gc * SEC - delay;
        }
        lls::ND6_DELAY => {
            e.asked = 0;
            delay = u64::try_from(val(&st.conf6.nd6_delay)).unwrap_or(5) * SEC;
        }
        _ => {}
    }
    if delay > 0 {
        settimer(e, delay, now, up);
    }
    e.remtime = remtime;
    e.state = new;
}

/// `nd6_llinfo_settimer_locked`.
fn settimer(e: &mut Lle, delay: u64, now: u64, up: u64) {
    e.expire = up + delay / SEC;
    e.ntimer = now + delay;
    arm(e.ntimer);
}

/// `nd6_llinfo_get_holdsrc`: the source of the first packet held.
fn holdsrc(e: &Lle) -> Option<In6> {
    let m = e.hold.first()?;
    let mut s = [0u8; 16];
    (m.data.copy_out(8, &mut s) == 16).then_some(s)
}

/// `nd6_is_new_addr_neighbor`: whether `a` is on interface `ifindex`'s
/// link: a link-local address of its zone, a destination a route puts
/// there directly, an on-link prefix of it, or the default interface's
/// when no router is known.
fn is_new_neighbor(n: &Net, nd: &Nd6, a: &In6, zone: u16, ifindex: u16) -> bool {
    if is_linklocal(a) {
        return zone == ifindex;
    }
    let Some(ifp) = n.by_index(ifindex) else {
        return false;
    };
    if let Some(r) = n.route6(ifp.fib, a, 0)
        && in6::route_aifp(r) == ifindex
        && r.flags & rtf::GATEWAY == 0
    {
        return true;
    }
    if nd.prefixes.iter().any(|p| {
        p.ifindex == ifindex
            && p.onlink
            && net::addr::prefix_match(Family::Inet6, &p.prefix, a, p.plen)
    }) {
        return true;
    }
    if ifp.flags & iff::POINTOPOINT != 0
        && ifp
            .addrs
            .iter()
            .any(|x| x.family == Family::Inet6 && x.dst == Some(*a))
    {
        return true;
    }
    ifp.nd6_flags & nd6_iff::ACCEPT_RTADV != 0 && nd.routers.is_empty() && nd.defifindex == ifindex
}

/// `nd6_is_addr_neighbor`: the same, or a neighbor entry exists.
pub(crate) fn is_neighbor(n: &Net, nd: &Nd6, a: &In6, zone: u16, ifindex: u16) -> bool {
    is_new_neighbor(n, nd, a, zone, ifindex) || n.lle(ifindex, Family::Inet6, a).is_some()
}

/// The [`net::In6Hooks`] resolution (`nd6_resolve`): neighbor `dst`'s
/// link-layer address on interface `ifindex`, the packet in `m` held
/// (and a solicitation sent) while it is not known.
pub(crate) fn resolve(
    n: &mut Net,
    ifindex: u16,
    dst: &In6,
    m: &mut Option<Packet>,
) -> Result<Resolved, Errno> {
    let Some(vnet) = n.owner.upgrade() else {
        return Err(Errno::ENETDOWN);
    };
    let st = state::of(&vnet);
    // The fast path: any entry with an address, its use noted.
    if let Some(e) = n.lle_mut(ifindex, Family::Inet6, dst)
        && e.flags & lle::VALID != 0
    {
        if e.hit == 0 {
            e.hit = net::uptime().max(1);
        }
        return Ok(Resolved::Addr(e.lladdr));
    }
    let now = crate::now();
    let zone = if is_zoned(dst) { ifindex } else { 0 };
    if n.lle(ifindex, Family::Inet6, dst).is_none() {
        let known = {
            let nd = st.nd6.lock();
            is_neighbor(n, &nd, dst, zone, ifindex)
        };
        if !known {
            *m = None;
            return Err(Errno::ENOBUFS);
        }
        n.lle_link(Lle::new(Family::Inet6, *dst, ifindex, 0));
    }
    let ndi = n.by_index(ifindex).map(|i| i.nd).unwrap_or_default();
    let maxq = usize::try_from(val(&st.conf6.nd6_maxqueuelen))
        .unwrap_or(16)
        .max(1);
    let Some(e) = n.lle_mut(ifindex, Family::Inet6, dst) else {
        return Err(Errno::ENOBUFS);
    };
    if e.state == lls::ND6_STALE {
        setstate(e, lls::ND6_DELAY, &ndi, st, now);
    }
    if e.state > lls::ND6_INCOMPLETE {
        return Ok(Resolved::Addr(e.lladdr));
    }
    if let Some(m) = m.take() {
        // `lltable_append_entry_queue`: the oldest go past the limit.
        e.hold.push(m);
        let mut dropped = 0;
        while e.hold.len() > maxq {
            e.hold.remove(0);
            dropped += 1;
        }
        st.icmp6stat.add(icp6s::DROPPED, dropped);
    }
    let mut send = None;
    if e.asked == 0 {
        e.asked += 1;
        send = Some(holdsrc(e));
        setstate(e, lls::ND6_INCOMPLETE, &ndi, st, now);
    }
    if let Some(src) = send {
        ns_output(n, st, ifindex, src.as_ref(), None, dst, None);
    }
    Ok(Resolved::Held)
}

/// `nd6_llinfo_timer` for each entry whose timer is due.
fn llinfo_timers(n: &mut Net, st: &State, now: u64, errors: &mut Vec<(Packet, u16)>) {
    let due: Vec<(u16, In6)> = n
        .lle
        .iter()
        .filter(|e| e.family == Family::Inet6 && e.ntimer != 0 && e.ntimer <= now)
        .map(|e| (e.ifindex, e.addr))
        .collect();
    let up = net::uptime();
    for (ifindex, addr) in due {
        let Some(ndi) = n.by_index(ifindex).map(|i| (i.nd, i.nd6_flags)) else {
            continue;
        };
        let (ndi, ndflags) = ndi;
        let mut send: Option<(Option<In6>, Option<In6>)> = None;
        let mut free = false;
        let mmax = u16::try_from(val(&st.conf6.nd6_mmaxtries)).unwrap_or(3);
        let umax = u16::try_from(val(&st.conf6.nd6_umaxtries)).unwrap_or(3);
        let gc_s = u64::try_from(val(&st.conf6.nd6_gctimer)).unwrap_or(86_400);
        let delay_s = u64::try_from(val(&st.conf6.nd6_delay)).unwrap_or(5);
        {
            let Some(e) = n.lle_mut(ifindex, Family::Inet6, &addr) else {
                continue;
            };
            e.ntimer = 0;
            if e.flags & lle::STATIC != 0 {
                continue;
            }
            match e.state {
                lls::ND6_INCOMPLETE => {
                    if e.asked < mmax {
                        e.asked += 1;
                        send = Some((holdsrc(e), None));
                    } else {
                        st.icmp6stat.add(icp6s::DROPPED, e.hold.len() as u64);
                        // The first held packet gets an unreachable.
                        if !e.hold.is_empty() {
                            errors.push((e.hold.remove(0), ifindex));
                        }
                        free = true;
                    }
                }
                lls::ND6_REACHABLE => {
                    if !permanent(e) {
                        setstate(e, lls::ND6_STALE, &ndi, st, now);
                    }
                }
                lls::ND6_STALE | lls::ND6_DELAY => {
                    let mut probe = e.state == lls::ND6_DELAY;
                    if e.state == lls::ND6_STALE {
                        // `nd6_is_stale`.
                        if e.hit == 0 {
                            let d = delay_s.min(gc_s) * SEC;
                            let d = if e.remtime > d {
                                e.remtime -= d;
                                d
                            } else {
                                let r = e.remtime;
                                e.remtime = 0;
                                r
                            };
                            if d == 0 {
                                if !permanent(e) {
                                    free = true;
                                }
                            } else {
                                settimer(e, d, now, up);
                            }
                        } else {
                            let since = up.saturating_sub(e.hit);
                            if since < delay_s {
                                settimer(e, (delay_s - since) * SEC, now, up);
                            } else {
                                probe = true;
                            }
                        }
                    }
                    if probe {
                        if ndflags & nd6_iff::PERFORMNUD != 0 {
                            e.asked = 1;
                            setstate(e, lls::ND6_PROBE, &ndi, st, now);
                            send = Some((holdsrc(e), Some(addr)));
                        } else {
                            setstate(e, lls::ND6_STALE, &ndi, st, now);
                        }
                    }
                }
                lls::ND6_PROBE => {
                    if e.asked < umax {
                        e.asked += 1;
                        send = Some((holdsrc(e), Some(addr)));
                    } else {
                        free = true;
                    }
                }
                _ => {}
            }
            if send.is_some() {
                settimer(e, u64::from(ndi.retrans) * MS, now, up);
            }
        }
        if free {
            nd6_free(n, st, ifindex, &addr);
        }
        if let Some((src, dst)) = send {
            ns_output(n, st, ifindex, src.as_ref(), dst.as_ref(), &addr, None);
        }
    }
}

/// `nd6_free`: the entry goes; a router's going makes the default router
/// chosen again, and redirects through it go.
pub(crate) fn nd6_free(n: &mut Net, st: &State, ifindex: u16, addr: &In6) {
    let Ok(e) = n.lle_delete(ifindex, Family::Inet6, addr, false) else {
        return;
    };
    // `nd6_free_redirect`: dynamic routes through it.
    let dynamic: Vec<Route> = n
        .routes
        .iter()
        .filter(|r| {
            r.family == Family::Inet6
                && r.flags & RTF_DYNAMIC != 0
                && r.gateway == Gateway::Addr(*addr)
                && r.ifindex == ifindex
        })
        .cloned()
        .collect();
    for r in dynamic {
        let _ = n.delete_exact(&r);
    }
    if e.router {
        crate::nd6_rtr::defrouter_select(n, st);
    }
}

/// `nd6_is_router`: whether the neighbor is a router after a message of
/// type `t` (code `c`), given whether the entry is new and whether it
/// had and got a link-layer address.
const fn is_router(
    t: u8,
    c: u8,
    is_new: bool,
    old_addr: bool,
    new_addr: bool,
    router: bool,
) -> bool {
    match t {
        ty::NEIGHBOR_SOLICIT => router && !is_new,
        ty::REDIRECT => {
            if c == REDIRECT_ROUTER {
                true
            } else {
                router && !is_new
            }
        }
        ty::ROUTER_SOLICIT => false,
        ty::ROUTER_ADVERT => {
            if (!is_new && (old_addr || new_addr)) || (is_new && new_addr) {
                true
            } else {
                router
            }
        }
        _ => router,
    }
}

/// `ND_REDIRECT_ONLINK`, `ND_REDIRECT_ROUTER`.
const REDIRECT_ONLINK: u8 = 0;
const REDIRECT_ROUTER: u8 = 1;

/// `nd6_cache_lladdr`: what a message of type `t` (code `c`) from `from`
/// says of it: its link-layer address (stale when new or changed), and
/// whether it is a router.
pub(crate) fn cache_lladdr(
    n: &mut Net,
    st: &State,
    ifindex: u16,
    from: &In6,
    ll: Option<[u8; 6]>,
    t: u8,
    c: u8,
) {
    if is_unspecified(from) {
        return;
    }
    let now = crate::now();
    let ndi = n.by_index(ifindex).map(|i| i.nd).unwrap_or_default();
    let mut is_new = false;
    if n.lle(ifindex, Family::Inet6, from).is_none() {
        let mut e = Lle::new(Family::Inet6, *from, ifindex, 0);
        if let Some(l) = ll {
            e.set_lladdr(l);
            setstate(&mut e, lls::ND6_STALE, &ndi, st, now);
        }
        n.lle_link(e);
        is_new = true;
    }
    let mut flush = Vec::new();
    let router = {
        let Some(e) = n.lle_mut(ifindex, Family::Inet6, from) else {
            return;
        };
        if e.flags & lle::STATIC != 0 {
            return;
        }
        let olladdr = e.flags & lle::VALID != 0;
        let llchange = match ll {
            Some(l) => !olladdr || e.lladdr != l,
            None => false,
        };
        let mut do_update = false;
        if !is_new && llchange {
            do_update = true;
            if let Some(l) = ll {
                e.set_lladdr(l);
            }
            setstate(e, lls::ND6_STALE, &ndi, st, now);
            flush = core::mem::take(&mut e.hold);
        }
        e.router = is_router(t, c, is_new, olladdr, ll.is_some(), e.router);
        if t == ty::REDIRECT && c != REDIRECT_ROUTER {
            e.flags |= lle::REDIRECT;
        }
        e.router && (do_update || is_new)
    };
    if !flush.is_empty()
        && let Some(l) = n.lle(ifindex, Family::Inet6, from).map(|e| e.lladdr)
    {
        flush_hold(n, ifindex, l, flush);
    }
    let accept = n
        .by_index(ifindex)
        .is_some_and(|i| i.nd6_flags & nd6_iff::ACCEPT_RTADV != 0);
    if router && accept {
        crate::nd6_rtr::defrouter_select(n, st);
    }
}

/// `nd6_flush_holdchain`: the held packets go to `dst` now.
fn flush_hold(n: &mut Net, ifindex: u16, dst: [u8; 6], held: Vec<Packet>) {
    let Some(src) = n.by_index(ifindex).and_then(|i| i.lladdr) else {
        return;
    };
    for mut m in held {
        let h = net::ether::Header {
            dst,
            src,
            ty: net::ether::ty::IPV6,
        };
        if net::ether::encap(&mut m, &h).is_ok() {
            n.txq.push((ifindex, None, m));
        }
    }
}

// ---------------------------------------------------------------------
// Options.

/// A message's ND options (`union nd_opts`): offsets into the options'
/// bytes of each kind found (the first of each, and every prefix).
#[derive(Debug, Default)]
pub(crate) struct NdOpts {
    pub src_ll: Option<(usize, usize)>,
    pub tgt_ll: Option<(usize, usize)>,
    pub mtu: Option<usize>,
    pub rh: Option<(usize, usize)>,
    pub nonce: Option<(usize, usize)>,
    pub pi: Vec<(usize, usize)>,
}

/// `nd6_options`: the options in `b`; `Err` for one of length 0 or
/// running past the end (`icp6s_nd_badopt`).
pub(crate) fn options(st: &State, b: &[u8]) -> Result<NdOpts, ()> {
    let mut o = NdOpts::default();
    let max = usize::try_from(val(&st.conf6.nd6_maxndopt)).unwrap_or(10);
    let mut at = 0usize;
    let mut i = 0usize;
    while at < b.len() {
        if at + 1 >= b.len() {
            st.icmp6stat.inc(icp6s::ND_BADOPT);
            return Err(());
        }
        let olen = usize::from(b[at + 1]) << 3;
        if olen == 0 || at + olen > b.len() {
            st.icmp6stat.inc(icp6s::ND_BADOPT);
            return Err(());
        }
        let ent = Some((at, olen));
        match b[at] {
            opt::SOURCE_LINKADDR => o.src_ll = o.src_ll.or(ent),
            opt::TARGET_LINKADDR => o.tgt_ll = o.tgt_ll.or(ent),
            opt::MTU => o.mtu = o.mtu.or(Some(at)),
            opt::REDIRECTED_HEADER => o.rh = o.rh.or(ent),
            opt::NONCE => o.nonce = o.nonce.or(ent),
            opt::PREFIX_INFORMATION => o.pi.push((at, olen)),
            _ => {}
        }
        at += olen;
        i += 1;
        if i > max {
            st.icmp6stat.inc(icp6s::ND_TOOMANYOPT);
            break;
        }
    }
    Ok(o)
}

/// A link-layer address option's address, if it is one an Ethernet
/// interface's would be (8 bytes): `Ok(None)` for none, `Err` for the
/// wrong length.
pub(crate) fn lladdr_opt(b: &[u8], o: Option<(usize, usize)>) -> Result<Option<[u8; 6]>, ()> {
    let Some((at, len)) = o else {
        return Ok(None);
    };
    if len != 8 {
        return Err(());
    }
    Ok(b[at + 2..at + 8].try_into().ok())
}

/// The ICMPv6 message at `off` of `m` (`len` bytes).
pub(crate) fn message(m: &Packet, off: usize, len: usize) -> Vec<u8> {
    let mut b = alloc::vec![0u8; len];
    m.data.copy_out(off, &mut b);
    b
}

/// The `N` bytes of `b` at `at`.
pub(crate) fn bytes_at<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    b.get(at..at + N)
        .and_then(|x| x.try_into().ok())
        .unwrap_or([0; N])
}

// ---------------------------------------------------------------------
// Solicitations and advertisements.

/// A packet from `src` to `dst` of hop limit 255 carrying ICMPv6 message
/// `body`, its checksum computed.
pub(crate) fn icmp6_packet(src: &In6, dst: &In6, hlim: u8, body: &[u8]) -> Option<Packet> {
    let mut m = Packet::for_output(HDR_LEN + body.len()).ok()?;
    m.data.extend_from_slice(body).ok()?;
    let h = Ip6::new(
        proto::ICMPV6,
        *src,
        *dst,
        u16::try_from(body.len()).ok()?,
        hlim,
        0,
    );
    m.prepend(HDR_LEN).ok()?.copy_from_slice(&h.bytes());
    let c = ip6::cksum6(&m, proto::ICMPV6, HDR_LEN, body.len());
    m.data.copy_in(HDR_LEN + 2, &c.to_be_bytes()).ok()?;
    Some(m)
}

/// The options `ip6_output` gets for ND's multicasts out of `ifindex`
/// (hop limit 255, no loopback).
fn nd_opts(ifindex: u16, dst: &In6) -> Opts6 {
    Opts6 {
        mcast: is_multicast6(dst).then_some(Mopts6 {
            ifindex,
            hlim: 255,
            loop_: false,
        }),
        zone: if is_zoned(dst) { ifindex } else { 0 },
        minmtu: ip6::minmtu::MCASTONLY,
        ..Opts6::default()
    }
}

/// `in6_selectsrc_nbr`: the source for an ND message to `dst` out of
/// `ifindex`.
fn selectsrc_nbr(n: &Net, st: &State, ifindex: u16, dst: &In6) -> Option<In6> {
    let req = in6::SrcReq {
        dst: *dst,
        zone: if is_zoned(dst) { ifindex } else { 0 },
        mif: if is_multicast6(dst) { ifindex } else { 0 },
        oifp: ifindex,
        prefer_tempaddr: 0,
        ..in6::SrcReq::default()
    };
    in6::selectsrc(n, st, &req).ok().map(|(a, _)| a)
}

/// `nd6_ns_output_fib`: a neighbor solicitation for `taddr` out of
/// `ifindex`, to `daddr` or the target's solicited-node group, from
/// `saddr` if it is the interface's (else as source selection says);
/// DAD's (with a nonce) from `::` without a link-layer address.
pub(crate) fn ns_output(
    n: &mut Net,
    st: &State,
    ifindex: u16,
    saddr: Option<&In6>,
    daddr: Option<&In6>,
    taddr: &In6,
    nonce: Option<[u8; NONCE_LEN]>,
) {
    if is_multicast6(taddr) {
        return;
    }
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    let mac = ifp.lladdr;
    let dst = daddr.copied().unwrap_or_else(|| in6::solicited_node(taddr));
    let src = if nonce.is_none() {
        match saddr.filter(|s| in6::ifa_flags(ifp, s).is_some()) {
            Some(s) => *s,
            None => match selectsrc_nbr(n, st, ifindex, &dst) {
                Some(s) => s,
                None => return,
            },
        }
    } else {
        [0; 16]
    };
    let mut body = alloc::vec![0u8; 24];
    body[0] = ty::NEIGHBOR_SOLICIT;
    body[8..24].copy_from_slice(taddr);
    if nonce.is_none()
        && let Some(mac) = mac
    {
        body.extend_from_slice(&[opt::SOURCE_LINKADDR, 1]);
        body.extend_from_slice(&mac);
    }
    if let Some(nc) = nonce
        && on(&st.conf6.dad_enhanced)
    {
        body.extend_from_slice(&[opt::NONCE, 1]);
        body.extend_from_slice(&nc);
    }
    let Some(m) = icmp6_packet(&src, &dst, 255, &body) else {
        return;
    };
    let mut o = nd_opts(ifindex, &dst);
    if nonce.is_some() {
        o.flags |= out::UNSPECSRC;
    }
    o.src_zone = if is_zoned(&src) { ifindex } else { 0 };
    let _ = ip6::output_in(n, st, m, &o);
    if let Some(i) = n.by_index(ifindex) {
        i.stats.icmp6_inc(icmp6_ifs::OUT_MSG);
        i.stats.icmp6_inc(icmp6_ifs::OUT_NEIGHBORSOLICIT);
    }
    ip6::count_out(st, ty::NEIGHBOR_SOLICIT);
}

/// `nd6_na_output_fib`: a neighbor advertisement for `taddr` out of
/// `ifindex` to `daddr` (all nodes for `::`), with `flags`; with
/// `tlladdr` the target's link-layer address goes too (`proxy`'s, or the
/// interface's), else override is cleared.
#[allow(clippy::too_many_arguments, reason = "nd6_na_output_fib's")]
pub(crate) fn na_output(
    n: &mut Net,
    st: &State,
    ifindex: u16,
    daddr: &In6,
    taddr: &In6,
    mut flags: u8,
    tlladdr: bool,
    proxy: Option<[u8; 6]>,
) {
    let mut dst = *daddr;
    if is_unspecified(&dst) {
        dst = in6::ALLNODES;
        flags &= !na::SOLICITED;
    }
    let Some(src) = selectsrc_nbr(n, st, ifindex, &dst) else {
        return;
    };
    let mac = proxy.or_else(|| n.by_index(ifindex).and_then(|i| i.lladdr));
    let mut body = alloc::vec![0u8; 24];
    body[0] = ty::NEIGHBOR_ADVERT;
    body[8..24].copy_from_slice(taddr);
    match mac {
        Some(mac) if tlladdr => {
            body.extend_from_slice(&[opt::TARGET_LINKADDR, 1]);
            body.extend_from_slice(&mac);
        }
        _ => flags &= !na::OVERRIDE,
    }
    body[4] = flags;
    let Some(m) = icmp6_packet(&src, &dst, 255, &body) else {
        return;
    };
    let mut o = nd_opts(ifindex, &dst);
    o.src_zone = if is_zoned(&src) { ifindex } else { 0 };
    let _ = ip6::output_in(n, st, m, &o);
    if let Some(i) = n.by_index(ifindex) {
        i.stats.icmp6_inc(icmp6_ifs::OUT_MSG);
        i.stats.icmp6_inc(icmp6_ifs::OUT_NEIGHBORADVERT);
    }
    ip6::count_out(st, ty::NEIGHBOR_ADVERT);
}

/// `nd6_ns_input`: a neighbor solicitation, answered for an address of
/// the interface's (or one it proxies); during DAD it counts against
/// the address.
pub(crate) fn ns_input(vnet: &Arc<Vnet>, st: &State, m: &Packet, off: usize, len: usize) {
    if m.hdr.flags & mbuf::flags::FRAGMENTED != 0 {
        return;
    }
    let mut m = m.copy().ok();
    let Some(ip6) = m.as_mut().and_then(|m| m.header::<Ip6>(0).ok()) else {
        return;
    };
    let Some(m) = m else {
        return;
    };
    let bad = || st.icmp6stat.inc(icp6s::BADNS);
    if ip6.hlim != 255 {
        st.icmp6stat.inc(icp6s::INVLHLIM);
        bad();
        return;
    }
    let b = message(&m, off, len);
    let taddr: In6 = bytes_at(&b, 8);
    let (saddr, daddr) = (ip6.src, ip6.dst);
    let ifindex = m.hdr.rcvif;
    let mut n = vnet.net.lock();
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    let mut rflag = if on(&st.conf6.forwarding) {
        na::ROUTER
    } else {
        0
    };
    if ifp.nd6_flags & nd6_iff::ACCEPT_RTADV != 0 && on(&st.conf6.norbit_raif) {
        rflag = 0;
    }
    if is_unspecified(&saddr) {
        // DAD's: only to the solicited-node group.
        let sn = daddr[0] == 0xff
            && daddr[1] == 0x02
            && daddr[4..8] == [0; 4]
            && daddr[8..12] == [0, 0, 0, 1]
            && daddr[12] == 0xff;
        if !sn {
            bad();
            return;
        }
    } else if !on(&st.conf6.nd6_onlink_ns_rfc4861) {
        let zone = if is_zoned(&saddr) { ifindex } else { 0 };
        let ok = {
            let nd = st.nd6.lock();
            is_neighbor(&n, &nd, &saddr, zone, ifindex)
        };
        if !ok {
            bad();
            return;
        }
    }
    if is_multicast6(&taddr) {
        bad();
        return;
    }
    let Ok(o) = options(st, &b[24..]) else {
        return;
    };
    let opts = &b[24..];
    let Ok(lladdr) = lladdr_opt(opts, o.src_ll) else {
        bad();
        return;
    };
    let lladdr_present = o.src_ll.is_some();
    if is_unspecified(&saddr) && lladdr_present {
        bad();
        return;
    }
    let tlladdr = is_multicast6(&daddr);
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    let mut proxy = None;
    let mut ifa = in6::ifa_flags(ifp, &taddr).map(|f| (taddr, f));
    if ifa.is_none() {
        // `nd6_proxy_fill_sdl`: a published entry for the target.
        if let Some(e) = n
            .lle(ifindex, Family::Inet6, &taddr)
            .filter(|e| e.flags & (lle::PUB | lle::VALID) == lle::PUB | lle::VALID)
        {
            proxy = Some(e.lladdr);
            ifa = ifp
                .addrs
                .iter()
                .find(|a| {
                    a.family == Family::Inet6
                        && is_linklocal(&a.addr)
                        && a.flags6 & (in6_iff::NOTREADY | in6_iff::ANYCAST) == 0
                })
                .map(|a| (a.addr, a.flags6));
        }
    }
    let Some((myaddr, f)) = ifa else {
        return;
    };
    let anycast = f & in6_iff::ANYCAST != 0;
    let tentative = f & in6_iff::TENTATIVE != 0;
    if f & in6_iff::DUPLICATED != 0 {
        return;
    }
    if lladdr_present && lladdr.is_none() {
        bad();
        return;
    }
    if myaddr == saddr {
        return;
    }
    if tentative {
        if is_unspecified(&saddr) {
            let nonce = o
                .nonce
                .filter(|&(_, l)| l == 8)
                .map(|(at, _)| bytes_at::<NONCE_LEN>(opts, at + 2));
            dad_ns_input(st, ifindex, &taddr, nonce);
        }
        return;
    }
    if !anycast && proxy.is_none() && tlladdr {
        rflag |= na::OVERRIDE;
    }
    if !is_unspecified(&saddr) {
        cache_lladdr(&mut n, st, ifindex, &saddr, lladdr, ty::NEIGHBOR_SOLICIT, 0);
        rflag |= na::SOLICITED;
    }
    na_output(&mut n, st, ifindex, &saddr, &taddr, rflag, tlladdr, proxy);
}

/// `nd6_na_input`: a neighbor advertisement, completing or updating the
/// target's entry (RFC 4861 7.2.5); during DAD for one of ours it is a
/// duplicate.
pub(crate) fn na_input(vnet: &Arc<Vnet>, st: &State, m: &Packet, off: usize, len: usize) {
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
    let bad = || st.icmp6stat.inc(icp6s::BADNA);
    if ip6.hlim != 255 {
        st.icmp6stat.inc(icp6s::INVLHLIM);
        bad();
        return;
    }
    let b = message(m, off, len);
    let flags = b[4];
    let is_router = flags & na::ROUTER != 0;
    let is_solicited = flags & na::SOLICITED != 0;
    let is_override = flags & na::OVERRIDE != 0;
    let taddr: In6 = bytes_at(&b, 8);
    if is_multicast6(&taddr) {
        bad();
        return;
    }
    if is_multicast6(&ip6.dst) && is_solicited {
        bad();
        return;
    }
    let Ok(o) = options(st, &b[24..]) else {
        return;
    };
    let opts = &b[24..];
    let ifindex = m.hdr.rcvif;
    let mut n = vnet.net.lock();
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    if let Some(f) = in6::ifa_flags(ifp, &taddr) {
        if f & in6_iff::TENTATIVE != 0 {
            dad_na_input(st, ifindex, &taddr);
        } else {
            crate::log(&alloc::format!(
                "nd6_na_input: duplicate IP6 address {}\n",
                fmt6(&taddr)
            ));
        }
        return;
    }
    let Ok(lladdr) = lladdr_opt(opts, o.tgt_ll) else {
        bad();
        return;
    };
    let now = crate::now();
    let ndi = ifp.nd;
    let accept = ifp.nd6_flags & nd6_iff::ACCEPT_RTADV != 0;
    let mut chain = Vec::new();
    let mut router_gone = false;
    let mut checklink = false;
    {
        let Some(e) = n.lle_mut(ifindex, Family::Inet6, &taddr) else {
            return;
        };
        if e.flags & lle::STATIC != 0 {
            return;
        }
        if e.state == lls::ND6_INCOMPLETE {
            let Some(l) = lladdr else {
                return;
            };
            e.set_lladdr(l);
            setstate(
                e,
                if is_solicited {
                    lls::ND6_REACHABLE
                } else {
                    lls::ND6_STALE
                },
                &ndi,
                st,
                now,
            );
            e.router = is_router;
            checklink = is_router;
        } else {
            let llchange = match lladdr {
                Some(l) => e.flags & lle::VALID == 0 || e.lladdr != l,
                None => false,
            };
            if !is_override && lladdr.is_some() && llchange {
                // (1): only reachable becomes stale.
                if e.state == lls::ND6_REACHABLE {
                    setstate(e, lls::ND6_STALE, &ndi, st, now);
                }
                return;
            }
            // (2a), (2b), (2c).
            if let Some(l) = lladdr {
                e.set_lladdr(l);
            }
            if is_solicited {
                setstate(e, lls::ND6_REACHABLE, &ndi, st, now);
            } else if lladdr.is_some() && llchange {
                setstate(e, lls::ND6_STALE, &ndi, st, now);
            }
            if e.router && !is_router {
                router_gone = true;
            }
            e.router = is_router;
        }
        e.asked = 0;
        if e.flags & lle::VALID != 0 {
            chain = core::mem::take(&mut e.hold);
        }
    }
    if !chain.is_empty()
        && let Some(l) = n.lle(ifindex, Family::Inet6, &taddr).map(|e| e.lladdr)
    {
        flush_hold(&mut n, ifindex, l, chain);
    }
    if router_gone {
        // It is no longer a router: off the default router list, and
        // its routes with it.
        let removed = crate::nd6_rtr::defrouter_remove(&mut n, st, &ip6.src, ifindex);
        if !removed && accept {
            crate::nd6_rtr::rt6_flush(&mut n, &ip6.src, ifindex);
        }
    }
    if checklink {
        crate::nd6_rtr::pfxlist_onlink_check(&mut n, st);
    }
}

// ---------------------------------------------------------------------
// Duplicate address detection.

/// `nd6_dad_start`: DAD for `addr` on `ifindex` in `delay_ms`; none for
/// an anycast address, with `dad_count` 0, or `ND6_IFF_NO_DAD` (the
/// address is ready at once), and none yet while the interface is down.
pub(crate) fn dad_start(n: &mut Net, st: &State, ifindex: u16, addr: &In6, delay_ms: u64) {
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    let Some(f) = in6::ifa_flags(ifp, addr) else {
        return;
    };
    if f & in6_iff::TENTATIVE == 0 {
        return;
    }
    if f & in6_iff::ANYCAST != 0
        || val(&st.conf6.dad_count) == 0
        || ifp.nd6_flags & nd6_iff::NO_DAD != 0
    {
        set_flags6(n, ifindex, addr, 0, in6_iff::TENTATIVE);
        return;
    }
    if ifp.flags & (iff::UP | iff::RUNNING) != iff::UP | iff::RUNNING
        || ifp.nd6_flags & nd6_iff::IFDISABLED != 0
    {
        return;
    }
    let mut nd = st.nd6.lock();
    if nd
        .dad
        .iter()
        .any(|d| d.ifindex == ifindex && d.addr == *addr)
    {
        return;
    }
    let at = crate::now() + delay_ms * MS;
    nd.dad.push(Dad {
        ifindex,
        addr: *addr,
        count: u32::try_from(val(&st.conf6.dad_count)).unwrap_or(1),
        ns_icount: 0,
        na_icount: 0,
        ns_ocount: 0,
        ns_tcount: 0,
        ns_lcount: 0,
        loopbackprobe: 0,
        nonce: [0; NONCE_LEN],
        at,
    });
    drop(nd);
    arm(at);
}

/// `nd6_dad_stop`.
pub(crate) fn dad_stop(st: &State, ifindex: u16, addr: &In6) {
    st.nd6
        .lock()
        .dad
        .retain(|d| !(d.ifindex == ifindex && d.addr == *addr));
}

/// Sets and clears an IPv6 address's flags.
pub(crate) fn set_flags6(n: &mut Net, ifindex: u16, addr: &In6, set: i32, clear: i32) {
    if let Some(i) = n.ifaces.iter_mut().find(|i| i.index == ifindex)
        && let Some(a) = i
            .addrs
            .iter_mut()
            .find(|a| a.family == Family::Inet6 && a.addr == *addr)
    {
        a.flags6 = (a.flags6 & !clear) | set;
    }
}

/// `nd6_dad_timer` for each DAD that is due.
fn dad_timers(n: &mut Net, st: &State, now: u64) {
    let due: Vec<Dad> = st
        .nd6
        .lock()
        .dad
        .iter()
        .copied()
        .filter(|d| d.at <= now)
        .collect();
    for d in due {
        dad_timer(n, st, d, now);
    }
}

/// `nd6_dad_timer`: the next solicitation, or the verdict once they have
/// all gone.
fn dad_timer(n: &mut Net, st: &State, mut d: Dad, now: u64) {
    let finish = |st: &State, d: &Dad| dad_stop(st, d.ifindex, &d.addr);
    let Some(ifp) = n.by_index(d.ifindex) else {
        finish(st, &d);
        return;
    };
    let name = ifp.name.clone();
    if ifp.nd6_flags & nd6_iff::IFDISABLED != 0 {
        crate::log(&alloc::format!(
            "nd6_dad_timer: cancel DAD on {name} because of ND6_IFF_IFDISABLED.\n"
        ));
        finish(st, &d);
        return;
    }
    let Some(f) = in6::ifa_flags(ifp, &d.addr) else {
        finish(st, &d);
        return;
    };
    if f & in6_iff::DUPLICATED != 0 || f & in6_iff::TENTATIVE == 0 {
        finish(st, &d);
        return;
    }
    let running = ifp.flags & (iff::UP | iff::RUNNING) == iff::UP | iff::RUNNING;
    let retrans = u64::from(ifp.nd.retrans) * MS;
    let maxtry = u32::try_from(val(&st.conf6.dad_maxtry)).unwrap_or(15);
    if d.ns_tcount > maxtry && !running {
        finish(st, &d);
        return;
    }
    if d.ns_ocount < d.count {
        d.at = now + retrans;
        dad_ns_output(n, st, &mut d);
        update(st, &d);
        arm(d.at);
        return;
    }
    if d.ns_icount > 0 || d.na_icount > 0 {
        dad_duplicated(n, &d);
        finish(st, &d);
    } else if on(&st.conf6.dad_enhanced) && d.ns_lcount > 0 && d.ns_lcount > d.loopbackprobe {
        crate::log(&alloc::format!(
            "{name}: a looped back NS message is detected during DAD for {}.  Another DAD probes are being sent.\n",
            fmt6(&d.addr)
        ));
        d.loopbackprobe = d.ns_lcount;
        d.count = d.ns_ocount + u32::try_from(val(&st.conf6.nd6_mmaxtries)).unwrap_or(3) - 1;
        d.at = now + retrans;
        dad_ns_output(n, st, &mut d);
        update(st, &d);
        arm(d.at);
    } else {
        set_flags6(n, d.ifindex, &d.addr, 0, in6_iff::TENTATIVE);
        if d.ns_lcount > 0 {
            crate::log(&alloc::format!(
                "{name}: DAD completed while a looped back NS message is detected during DAD for {}.\n",
                fmt6(&d.addr)
            ));
        }
        finish(st, &d);
    }
}

/// Puts back a DAD's changed state.
fn update(st: &State, d: &Dad) {
    if let Some(x) = st
        .nd6
        .lock()
        .dad
        .iter_mut()
        .find(|x| x.ifindex == d.ifindex && x.addr == d.addr)
    {
        *x = *d;
    }
}

/// `nd6_dad_ns_output`.
fn dad_ns_output(n: &mut Net, st: &State, d: &mut Dad) {
    d.ns_tcount += 1;
    let Some(ifp) = n.by_index(d.ifindex) else {
        return;
    };
    if ifp.flags & (iff::UP | iff::RUNNING) != iff::UP | iff::RUNNING {
        return;
    }
    d.ns_ocount += 1;
    if on(&st.conf6.dad_enhanced) {
        let a = crate::random().to_ne_bytes();
        let b = crate::random().to_ne_bytes();
        d.nonce[..4].copy_from_slice(&a);
        d.nonce[4..].copy_from_slice(&b[..2]);
    }
    let nonce = d.nonce;
    ns_output(n, st, d.ifindex, None, None, &d.addr, Some(nonce));
}

/// `nd6_dad_ns_input`: a DAD solicitation for a tentative address; our
/// own looped back (its nonce ours) counts apart.
fn dad_ns_input(st: &State, ifindex: u16, addr: &In6, nonce: Option<[u8; NONCE_LEN]>) {
    let nonce = if on(&st.conf6.dad_enhanced) {
        nonce
    } else {
        None
    };
    let mut nd = st.nd6.lock();
    if let Some(d) = nd
        .dad
        .iter_mut()
        .find(|d| d.ifindex == ifindex && d.addr == *addr)
    {
        if nonce.is_some_and(|nc| nc == d.nonce) {
            d.ns_lcount += 1;
        } else {
            d.ns_icount += 1;
        }
    }
}

/// `nd6_dad_na_input`.
fn dad_na_input(st: &State, ifindex: u16, addr: &In6) {
    if let Some(d) = st
        .nd6
        .lock()
        .dad
        .iter_mut()
        .find(|d| d.ifindex == ifindex && d.addr == *addr)
    {
        d.na_icount += 1;
    }
}

/// `nd6_dad_duplicated`: the address is a duplicate; a link-local one
/// made from the hardware address disables IPv6 on the interface.
fn dad_duplicated(n: &mut Net, d: &Dad) {
    let Some(ifp) = n.by_index(d.ifindex) else {
        return;
    };
    let name = ifp.name.clone();
    let hw = ifp.lladdr.and_then(|m| net::ifnet::eui64_linklocal(&m));
    let ether = ifp.ty == IFT_ETHER;
    crate::log(&alloc::format!(
        "{name}: DAD detected duplicate IPv6 address {}: NS in/out/loopback={}/{}/{}, NA in={}\n",
        fmt6(&d.addr),
        d.ns_icount,
        d.ns_ocount,
        d.ns_lcount,
        d.na_icount
    ));
    set_flags6(
        n,
        d.ifindex,
        &d.addr,
        in6_iff::DUPLICATED,
        in6_iff::TENTATIVE,
    );
    crate::log(&alloc::format!(
        "{name}: DAD complete for {} - duplicate found\n",
        fmt6(&d.addr)
    ));
    crate::log(&alloc::format!("{name}: manual intervention required\n"));
    if is_linklocal(&d.addr) && ether && hw.is_some_and(|h| h[8..] == d.addr[8..]) {
        if let Some(i) = n.ifaces.iter_mut().find(|i| i.index == d.ifindex) {
            i.nd6_flags |= nd6_iff::IFDISABLED;
        }
        crate::log(&alloc::format!(
            "{name}: possible hardware address duplication detected, disable IPv6\n"
        ));
    }
}

/// `inet_ntop` of an IPv6 address, for messages.
pub(crate) fn fmt6(a: &In6) -> alloc::string::String {
    use core::fmt::Write;
    let w: Vec<u16> = a
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes(*c))
        .collect();
    // The longest run of two or more zero words goes.
    let (mut best, mut best_len) = (usize::MAX, 0usize);
    let mut i = 0;
    while i < 8 {
        if w[i] == 0 {
            let s = i;
            while i < 8 && w[i] == 0 {
                i += 1;
            }
            if i - s > best_len && i - s >= 2 {
                best = s;
                best_len = i - s;
            }
        } else {
            i += 1;
        }
    }
    let mut out = alloc::string::String::new();
    let mut i = 0;
    while i < 8 {
        if i == best {
            out.push_str("::");
            i += best_len;
            continue;
        }
        if !out.is_empty() && !out.ends_with(':') {
            out.push(':');
        }
        let _ = write!(out, "{:x}", w[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------
// What the interface layer sets off (`net::In6Hooks`).

/// The VNET's protocol state, from under the interfaces' lock.
fn st_of(n: &Net) -> Option<Arc<State>> {
    n.owner.upgrade().map(|v| state::of_arc(&v))
}

/// A random delay up to `MAX_RTR_SOLICITATION_DELAY` (`arc4random() %
/// hz` with `hz` 1000), milliseconds.
pub(crate) fn rtr_delay_ms() -> u64 {
    u64::from(crate::random()) % MAX_RTR_SOLICITATION_DELAY_MS
}

/// `in6_broadcast_ifa`: a new address joins its groups
/// (`in6_update_ifa_join_mc`) and starts DAD if tentative; an
/// interface's own link-local address puts its prefix on the list.
pub(crate) fn ifa_added(n: &mut Net, ifindex: u16, addr: &In6, new: bool, update: u32) {
    let Some(st) = st_of(n) else {
        return;
    };
    nd_init(n, ifindex);
    if !new {
        if update & ifa6_update::AIFADDR != 0 {
            crate::nd6_rtr::aifaddr_prefix(n, &st, ifindex, addr, false);
        }
        return;
    }
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    let multicast = ifp.flags & iff::MULTICAST != 0;
    let daddelay = update & ifa6_update::DADDELAY != 0;
    if multicast {
        // The solicited-node group (its report delayed as the address's
        // DAD may be: `arc4random() % hz`, counted in MLD's ticks),
        // all-nodes, the node information group, and interface-local
        // all-nodes.
        let sol_delay = if daddelay {
            u32::try_from(rtr_delay_ms()).unwrap_or(0)
        } else {
            0
        };
        let mut groups = alloc::vec![(in6::solicited_node(addr), sol_delay), (in6::ALLNODES, 0)];
        if let Some(g) = in6::nigroup(&crate::hostname()) {
            let d = if daddelay {
                u32::try_from(rtr_delay_ms()).unwrap_or(0)
            } else {
                0
            };
            groups.push((g, d));
        }
        groups.push((in6::NODELOCAL_ALLNODES, 0));
        for (g, delay) in groups {
            crate::mld6::join(n, &st, ifindex, &g, delay);
            crate::mld6::add_membership(&st, ifindex, addr, &g);
        }
    }
    if update & ifa6_update::LINKLOCAL != 0 {
        crate::nd6_rtr::linklocal_prefix(n, &st, ifindex, addr);
    }
    if update & ifa6_update::AIFADDR != 0 {
        crate::nd6_rtr::aifaddr_prefix(n, &st, ifindex, addr, true);
    }
    let tentative = n
        .by_index(ifindex)
        .and_then(|i| in6::ifa_flags(i, addr))
        .is_some_and(|f| f & in6_iff::TENTATIVE != 0);
    if tentative {
        let delay = if daddelay { rtr_delay_ms() } else { 0 };
        dad_start(n, &st, ifindex, addr, delay);
    }
}

/// `in6_purgeaddr`: an address's DAD stops, it leaves its groups, and
/// its prefix loses it.
pub(crate) fn ifa_removed(n: &mut Net, ifindex: u16, a: &net::ifnet::Ifaddr) {
    let Some(st) = st_of(n) else {
        return;
    };
    dad_stop(&st, ifindex, &a.addr);
    for g in crate::mld6::take_memberships(&st, ifindex, &a.addr) {
        crate::mld6::leave(n, &st, ifindex, &g);
    }
    crate::nd6_rtr::ifa_gone(n, &st, ifindex, a);
}

/// `in6_if_up`: DAD again for each tentative address, after a random
/// delay.
pub(crate) fn if_up(n: &mut Net, ifindex: u16) {
    let Some(st) = st_of(n) else {
        return;
    };
    nd_init(n, ifindex);
    let tentative: Vec<In6> = n
        .by_index(ifindex)
        .map(|i| {
            i.addrs
                .iter()
                .filter(|a| a.family == Family::Inet6 && a.flags6 & in6_iff::TENTATIVE != 0)
                .map(|a| a.addr)
                .collect()
        })
        .unwrap_or_default();
    for a in tentative {
        dad_start(n, &st, ifindex, &a, rtr_delay_ms());
    }
}

/// `in6_ifdetach` with `nd6_purge`: an interface's DAD, groups, routers,
/// and prefixes go as it leaves.
pub(crate) fn if_detach(n: &mut Net, ifindex: u16) {
    let Some(st) = st_of(n) else {
        return;
    };
    st.nd6.lock().dad.retain(|d| d.ifindex != ifindex);
    crate::mld6::purge(n, &st, ifindex);
    crate::nd6_rtr::purge(n, &st, ifindex);
}

/// `nd6_lle_event`: the interface joins (or leaves) the solicited-node
/// group of an address it answers for as a proxy.
pub(crate) fn proxy(n: &mut Net, ifindex: u16, addr: &In6, join: bool) {
    let Some(st) = st_of(n) else {
        return;
    };
    let g = in6::solicited_node(addr);
    if join {
        crate::mld6::join(n, &st, ifindex, &g, 0);
    } else {
        crate::mld6::leave(n, &st, ifindex, &g);
    }
}

// ---------------------------------------------------------------------
// Redirects.

/// `icmp6_redirect_input`: a host follows a redirect from its first-hop
/// router (RFC 4861 8.1) with a host route through the target, or to the
/// destination on the link.
pub(crate) fn redirect_input(vnet: &Arc<Vnet>, st: &State, m: &Packet, off: usize, len: usize) {
    if on(&st.conf6.forwarding) || !on(&st.conf6.rediraccept) {
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
    let bad = || st.icmp6stat.inc(icp6s::BADREDIRECT);
    let b = message(m, off, len);
    let redtgt: In6 = bytes_at(&b, 8);
    let reddst: In6 = bytes_at(&b, 24);
    let src = ip6.src;
    let ifindex = m.hdr.rcvif;
    if !is_linklocal(&src) {
        bad();
        return;
    }
    if ip6.hlim != 255 {
        st.icmp6stat.inc(icp6s::INVLHLIM);
        bad();
        return;
    }
    let mut n = vnet.net.lock();
    let fib = n.by_index(ifindex).map_or(0, |i| i.fib);
    let dzone = if is_zoned(&reddst) { ifindex } else { 0 };
    match n.route6(fib, &reddst, dzone) {
        Some(r) if r.flags & rtf::GATEWAY != 0 && r.gateway == Gateway::Addr(src) => {}
        _ => {
            bad();
            return;
        }
    }
    if is_multicast6(&reddst) {
        bad();
        return;
    }
    let is_router = is_linklocal(&redtgt);
    let is_onlink = redtgt == reddst;
    if !is_router && !is_onlink {
        bad();
        return;
    }
    let Ok(o) = options(st, &b[40..]) else {
        return;
    };
    let Ok(lladdr) = lladdr_opt(&b[40..], o.tgt_ll) else {
        bad();
        return;
    };
    let code = if is_onlink {
        REDIRECT_ONLINK
    } else {
        REDIRECT_ROUTER
    };
    cache_lladdr(&mut n, st, ifindex, &redtgt, lladdr, ty::REDIRECT, code);
    // `rib_add_redirect` in every table.
    let src_ifa = n
        .by_index(ifindex)
        .and_then(|i| in6::linklocal_of(i, 0))
        .unwrap_or(src);
    let secs = u64::try_from(val(&st.conf6.redirtimeout)).unwrap_or(600);
    let expire = net::uptime() + secs;
    for fib in 0..u16::try_from(net::numfibs()).unwrap_or(1) {
        let r = Route {
            family: Family::Inet6,
            dst: reddst,
            plen: 128,
            gateway: if is_router {
                Gateway::Addr(redtgt)
            } else {
                Gateway::Link(ifindex)
            },
            flags: rtf::UP | rtf::HOST | RTF_DYNAMIC | if is_router { rtf::GATEWAY } else { 0 },
            ifindex,
            ifa: src_ifa,
            mtu: 0,
            weight: 1,
            expire,
            nhidx: 0,
            fib,
        };
        let _ = n.delete_route_fib(Family::Inet6, &reddst, 128, fib);
        let _ = n.add_route(r);
    }
}

/// `icmp6_redirect_output`: a router tells the sender of `m0` (which it
/// forwarded back out of interface `ifindex`) of a better first hop,
/// `gw`, or that the destination is on the link.
pub(crate) fn redirect_output(
    vnet: &Arc<Vnet>,
    st: &State,
    m0: Packet,
    ifindex: u16,
    gw: Option<In6>,
) {
    let _ = st;
    let st = state::of(vnet);
    st.icmp6stat.inc(icp6s::OREDIRECT);
    if !on(&st.conf6.forwarding) {
        return;
    }
    let mut m0 = m0;
    let Ok(sip6) = m0.header::<Ip6>(0) else {
        return;
    };
    let mut n = vnet.net.lock();
    let zone = if is_zoned(&sip6.src) { ifindex } else { 0 };
    let ok = {
        let nd = st.nd6.lock();
        is_neighbor(&n, &nd, &sip6.src, zone, ifindex)
    };
    if !ok || is_multicast6(&sip6.dst) {
        return;
    }
    if icmp6::ratelimit(st, ty::REDIRECT) {
        return;
    }
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    let Some(ifp_ll) = in6::linklocal_of(ifp, in6_iff::NOTREADY | in6_iff::ANYCAST) else {
        return;
    };
    let router_ll = gw.filter(is_linklocal);
    if gw.is_some() && router_ll.is_none() {
        return;
    }
    let mut body = alloc::vec![0u8; 40];
    body[0] = ty::REDIRECT;
    match router_ll {
        Some(r) => body[8..24].copy_from_slice(&r),
        None => body[8..24].copy_from_slice(&sip6.dst),
    }
    body[24..40].copy_from_slice(&sip6.dst);
    let maxlen = ip6::MMTU as usize;
    if let Some(r) = router_ll
        && let Some(e) = n
            .lle(ifindex, Family::Inet6, &r)
            .filter(|e| e.flags & lle::VALID != 0)
    {
        body.extend_from_slice(&[opt::TARGET_LINKADDR, 1]);
        body.extend_from_slice(&e.lladdr);
    }
    // The redirected header: as much of the packet as fits.
    let p = HDR_LEN + body.len();
    if p <= maxlen {
        let len = (maxlen - p) & !7;
        let mut q = alloc::vec![0u8; m0.len()];
        m0.data.copy_out(0, &mut q);
        if len.saturating_sub(8) < q.len() {
            q.truncate(len.saturating_sub(8));
        } else {
            q.truncate(q.len() - q.len() % 8);
        }
        let olen = q.len() + 8;
        #[allow(clippy::cast_possible_truncation, reason = "under 1280 / 8")]
        body.extend_from_slice(&[opt::REDIRECTED_HEADER, (olen >> 3) as u8, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(&q);
    }
    let Some(m) = icmp6_packet(&ifp_ll, &sip6.src, 255, &body) else {
        return;
    };
    let o = Opts6 {
        zone,
        src_zone: ifindex,
        minmtu: ip6::minmtu::MCASTONLY,
        ..Opts6::default()
    };
    if let Ok(outif) = ip6::output_if(&mut n, st, m, &o)
        && let Some(i) = n.by_index(outif)
    {
        i.stats.icmp6_inc(icmp6_ifs::OUT_MSG);
        i.stats.icmp6_inc(icmp6_ifs::OUT_REDIRECT);
    }
    ip6::count_out(st, ty::REDIRECT);
}
