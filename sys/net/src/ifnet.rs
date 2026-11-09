// SPDX-License-Identifier: BSD-2-Clause
//! Interfaces, their addresses, and the routing table: the loopback
//! interface `lo0` (FreeBSD's `if_loop`), the IPv4 and IPv6 addresses it
//! carries, and the routes those addresses imply plus the ones `route(8)`
//! adds, looked up by longest prefix.
//!
//! Each VNET has its own (`Vnet::net`). There is one table (FIB 0); no
//! packet leaves the machine yet, so a route either reaches a local
//! address or rejects.
//!
//! Behavior reference: sys/net/if_loop.c, sys/net/if.c (`ifioctl`),
//! sys/netinet/in.c (`in_aifaddr_ioctl`, `in_addprefix`),
//! sys/netinet6/in6.c (`in6_update_ifa`), sys/netinet6/in6_ifattach.c
//! (`in6_ifattach_loopback`, `in6_ifattach_linklocal`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::net::{IFT_LOOP, LINK_STATE_UP, ifcap, iff, nd6_iff, rtf};

use crate::Family;
use crate::addr::{In6, LOOPBACK6, from_v4, is_linklocal6, is_loopback4, prefix_match, v4_of};
use crate::announce::Event;

/// `LOMTU`.
pub const LOMTU: u32 = 16384;
/// `ND6_INFINITE_LIFETIME`.
pub const INFINITE: u32 = u32::MAX;

/// An interface's counters (`if_counter`), counted as packets pass.
#[derive(Debug)]
pub struct Stats {
    pub ipackets: sync::PerCpuU64,
    pub opackets: sync::PerCpuU64,
    pub ibytes: sync::PerCpuU64,
    pub obytes: sync::PerCpuU64,
    pub imcasts: sync::PerCpuU64,
    pub omcasts: sync::PerCpuU64,
    pub iqdrops: sync::PerCpuU64,
    pub noproto: sync::PerCpuU64,
    pub oerrors: sync::PerCpuU64,
    pub ierrors: sync::PerCpuU64,
    /// IPv6's (`struct in6_ifstat`, [`in6_ifs`]) and ICMPv6's
    /// (`struct icmp6_ifstat`, [`icmp6_ifs`]) for it.
    pub in6: [AtomicU64; in6_ifs::N],
    pub icmp6: [AtomicU64; icmp6_ifs::N],
}

/// `struct in6_ifstat`'s fields.
pub mod in6_ifs {
    pub const IN_RECEIVE: usize = 0;
    pub const IN_HDRERR: usize = 1;
    pub const IN_TOOBIG: usize = 2;
    pub const IN_NOROUTE: usize = 3;
    pub const IN_ADDRERR: usize = 4;
    pub const IN_PROTOUNKNOWN: usize = 5;
    pub const IN_TRUNCATED: usize = 6;
    pub const IN_DISCARD: usize = 7;
    pub const IN_DELIVER: usize = 8;
    pub const OUT_FORWARD: usize = 9;
    pub const OUT_REQUEST: usize = 10;
    pub const OUT_DISCARD: usize = 11;
    pub const OUT_FRAGOK: usize = 12;
    pub const OUT_FRAGFAIL: usize = 13;
    pub const OUT_FRAGCREAT: usize = 14;
    pub const REASS_REQD: usize = 15;
    pub const REASS_OK: usize = 16;
    pub const REASS_FAIL: usize = 17;
    pub const IN_MCAST: usize = 18;
    pub const OUT_MCAST: usize = 19;
    pub const N: usize = 20;
}

/// `struct icmp6_ifstat`'s fields.
pub mod icmp6_ifs {
    pub const IN_MSG: usize = 0;
    pub const IN_ERROR: usize = 1;
    pub const IN_DSTUNREACH: usize = 2;
    pub const IN_ADMINPROHIB: usize = 3;
    pub const IN_TIMEEXCEED: usize = 4;
    pub const IN_PARAMPROB: usize = 5;
    pub const IN_PKTTOOBIG: usize = 6;
    pub const IN_ECHO: usize = 7;
    pub const IN_ECHOREPLY: usize = 8;
    pub const IN_ROUTERSOLICIT: usize = 9;
    pub const IN_ROUTERADVERT: usize = 10;
    pub const IN_NEIGHBORSOLICIT: usize = 11;
    pub const IN_NEIGHBORADVERT: usize = 12;
    pub const IN_REDIRECT: usize = 13;
    pub const IN_MLDQUERY: usize = 14;
    pub const IN_MLDREPORT: usize = 15;
    pub const IN_MLDDONE: usize = 16;
    pub const OUT_MSG: usize = 17;
    pub const OUT_ERROR: usize = 18;
    pub const OUT_DSTUNREACH: usize = 19;
    pub const OUT_ADMINPROHIB: usize = 20;
    pub const OUT_TIMEEXCEED: usize = 21;
    pub const OUT_PARAMPROB: usize = 22;
    pub const OUT_PKTTOOBIG: usize = 23;
    pub const OUT_ECHO: usize = 24;
    pub const OUT_ECHOREPLY: usize = 25;
    pub const OUT_ROUTERSOLICIT: usize = 26;
    pub const OUT_ROUTERADVERT: usize = 27;
    pub const OUT_NEIGHBORSOLICIT: usize = 28;
    pub const OUT_NEIGHBORADVERT: usize = 29;
    pub const OUT_REDIRECT: usize = 30;
    pub const OUT_MLDQUERY: usize = 31;
    pub const OUT_MLDREPORT: usize = 32;
    pub const OUT_MLDDONE: usize = 33;
    pub const N: usize = 34;
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            ipackets: sync::PerCpuU64::new(),
            opackets: sync::PerCpuU64::new(),
            ibytes: sync::PerCpuU64::new(),
            obytes: sync::PerCpuU64::new(),
            imcasts: sync::PerCpuU64::new(),
            omcasts: sync::PerCpuU64::new(),
            iqdrops: sync::PerCpuU64::new(),
            noproto: sync::PerCpuU64::new(),
            oerrors: sync::PerCpuU64::new(),
            ierrors: sync::PerCpuU64::new(),
            in6: [const { AtomicU64::new(0) }; in6_ifs::N],
            icmp6: [const { AtomicU64::new(0) }; icmp6_ifs::N],
        }
    }
}

impl Stats {
    /// Counts one of IPv6's (`in6_ifstat_inc`).
    pub fn in6_inc(&self, i: usize) {
        if let Some(c) = self.in6.get(i) {
            // Relaxed: a counter.
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counts one of ICMPv6's (`icmp6_ifstat_inc`).
    pub fn icmp6_inc(&self, i: usize) {
        if let Some(c) = self.icmp6.get(i) {
            // Relaxed: a counter.
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// IPv6's counters as `SIOCGIFSTAT_IN6` returns them.
    #[must_use]
    pub fn in6_bytes(&self) -> Vec<u8> {
        // Relaxed: a snapshot of counters.
        self.in6
            .iter()
            .flat_map(|c| c.load(Ordering::Relaxed).to_ne_bytes())
            .collect()
    }

    /// ICMPv6's, as `SIOCGIFSTAT_ICMP6` returns them.
    #[must_use]
    pub fn icmp6_bytes(&self) -> Vec<u8> {
        // Relaxed: a snapshot of counters.
        self.icmp6
            .iter()
            .flat_map(|c| c.load(Ordering::Relaxed).to_ne_bytes())
            .collect()
    }

    /// One packet of `bytes` looped back: out and in again
    /// (`if_simloop`).
    pub fn looped(&self, bytes: usize) {
        let b = bytes as u64;
        // Relaxed: counters only.
        self.opackets.add(1);
        self.ipackets.add(1);
        self.obytes.add(b);
        self.ibytes.add(b);
    }
}

/// An interface address (`struct ifaddr` with its `in_ifaddr` or
/// `in6_ifaddr`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ifaddr {
    pub family: Family,
    /// The address; IPv4 in the last four bytes.
    pub addr: In6,
    pub plen: u8,
    /// The destination of a point-to-point address (loopback's own).
    pub dst: Option<In6>,
    pub broadcast: Option<In6>,
    /// `IN6_IFF_*`.
    pub flags6: i32,
    /// Valid and preferred lifetimes, seconds.
    pub vltime: u32,
    pub pltime: u32,
    /// The CARP virtual host (`ifa_vhid`).
    pub vhid: u32,
    /// When it was made and when its lifetimes were last set, seconds of
    /// uptime (`ia6_createtime`, `ia6_updatetime`); set by
    /// [`Net::add_addr`].
    pub created: u64,
    pub updated: u64,
}

impl Ifaddr {
    /// `IFA6_IS_DEPRECATED`: its preferred lifetime has run out at `now`.
    #[must_use]
    pub const fn is_deprecated(&self, now: u64) -> bool {
        self.pltime != INFINITE && now.saturating_sub(self.updated) >= self.pltime as u64
    }

    /// `IFA6_IS_INVALID`: its valid lifetime has run out at `now`.
    #[must_use]
    pub const fn is_invalid(&self, now: u64) -> bool {
        self.vltime != INFINITE && now.saturating_sub(self.updated) >= self.vltime as u64
    }
}

/// An interface (`struct ifnet`).
#[derive(Clone, Debug)]
pub struct Ifnet {
    pub index: u16,
    pub name: String,
    /// Its driver's name and unit (`if_dname`, `if_dunit`).
    pub dname: &'static str,
    pub dunit: i32,
    /// The VNET it was made in (`if_home_vnet`), its id.
    pub home: u32,
    /// The routing table its addresses' routes go to (`if_fib`; with
    /// `net.add_addr_allfibs` 0, only that one).
    pub fib: u16,
    /// `if_flags` (with `IFF_DRV_RUNNING`).
    pub flags: u32,
    pub mtu: u32,
    pub metric: u32,
    pub ty: u8,
    pub link_state: u8,
    pub capabilities: u32,
    pub capenable: u32,
    /// `ND6_IFF_*` (`nd_ifinfo.flags`).
    pub nd6_flags: u32,
    /// The rest of its `nd_ifinfo`.
    pub nd: NdInfo,
    pub description: Option<Vec<u8>>,
    pub groups: Vec<String>,
    pub addrs: Vec<Ifaddr>,
    /// `if_epoch`, `if_lastchange` (seconds since the epoch, and µs).
    pub epoch: i64,
    pub lastchange: (i64, i64),
    /// Its counters, shared with the code that counts.
    pub stats: Arc<Stats>,
    /// Its link-layer address, for Ethernet (`if_addr`).
    pub lladdr: Option<[u8; 6]>,
    pub baudrate: u64,
    /// Its device.
    pub link: crate::link::LinkRef,
    /// Its `bpf` tap (`if_bpf`).
    pub bpf: Option<Arc<crate::bpf::Tap>>,
    /// Promiscuous mode's takers (`if_pcount`).
    pub pcount: u32,
    /// Its link-layer multicast memberships (`if_multiaddrs`), each with
    /// its references.
    pub maddrs: Vec<([u8; 6], u32)>,
    /// The 802.1p priority its frames are tagged with (`if_pcp`),
    /// [`PCP_NONE`] for none.
    pub pcp: u8,
}

/// `IFNET_PCP_NONE`.
pub const PCP_NONE: u8 = 0xff;

/// An interface's neighbor discovery parameters (`struct nd_ifinfo`,
/// less its flags), as router advertisements may set them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NdInfo {
    /// The link MTU a router advertised, 0 for the interface's.
    pub linkmtu: u32,
    /// `BaseReachableTime` (milliseconds), the `ReachableTime` drawn
    /// from it (seconds), and `RetransTimer` (milliseconds).
    pub basereachable: u32,
    pub reachable: u32,
    pub retrans: u32,
    /// The hop limit for this host's packets (`CurHopLimit`).
    pub chlim: u8,
    /// Seconds until `ReachableTime` is drawn again (`recalctm`).
    pub recalctm: i32,
    /// Whether IPv6 has drawn its reachable time yet.
    pub initialized: bool,
}

impl NdInfo {
    /// `nd6_ifattach`'s: `REACHABLE_TIME`, `RETRANS_TIMER`,
    /// `IPV6_DEFHLIM`; IPv6 draws the reachable time when it first sees
    /// the interface.
    pub const DEFAULT: Self = Self {
        linkmtu: 0,
        basereachable: 30_000,
        reachable: 30,
        retrans: 1000,
        chlim: 64,
        recalctm: 7200,
        initialized: false,
    };
}

impl Default for NdInfo {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Ifnet {
    /// `IN6_LINKMTU`: the advertised link MTU if it is below the
    /// interface's, else the interface's.
    #[must_use]
    pub const fn linkmtu6(&self) -> u32 {
        if self.nd.linkmtu != 0 && self.nd.linkmtu < self.mtu {
            self.nd.linkmtu
        } else {
            self.mtu
        }
    }
}

/// Where a route goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gateway {
    /// Out of an interface (`link#N`).
    Link(u16),
    /// Through an address.
    Addr(In6),
}

/// A route (`struct rtentry` with its nexthop).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub family: Family,
    pub dst: In6,
    pub plen: u8,
    pub gateway: Gateway,
    /// `RTF_*`.
    pub flags: i32,
    pub ifindex: u16,
    /// The address packets on it come from (`nh_ifa`).
    pub ifa: In6,
    pub mtu: u32,
    pub weight: u32,
    /// When it expires (`rt_expire`), seconds of the realtime clock; 0
    /// for never.
    pub expire: u64,
    /// Its next hop's index (`nh_idx`), given when it is added; each
    /// path has its own (FreeBSD shares one between identical paths).
    pub nhidx: u32,
    /// The routing table it is in.
    pub fib: u16,
}

/// The interfaces and routes.
#[derive(Debug)]
pub struct Net {
    pub ifaces: Vec<Ifnet>,
    pub routes: Vec<Route>,
    /// Changes not yet announced ([`crate::announce`]).
    pub outbox: Vec<crate::announce::Event>,
    /// The last next-hop index given.
    pub last_nh: u32,
    /// Bumped on any change (netlink dumps, `getifaddrs` callers).
    pub generation: u64,
    /// The VNET these are in, for interfaces' taps.
    pub owner: alloc::sync::Weak<crate::Vnet>,
    /// It is a VNET's, so its interface indexes are the system's.
    pub(crate) in_vnet: bool,
    /// The link-layer table's entries ([`crate::lltable`]).
    pub lle: Vec<crate::lltable::Lle>,
    /// Frames to send once the lock is let go: the interface, the family
    /// of a packet for a loopback one, and the frame (with its header on
    /// an Ethernet one).
    pub txq: Vec<(u16, Option<Family>, mbuf::Packet)>,
    /// Packets for this host's own input, queued to `netisr` once the
    /// lock is let go (looped broadcasts and multicasts).
    pub isrq: Vec<(crate::netisr::Proto, mbuf::Packet)>,
    /// ARP's settings and counts.
    pub arp: crate::arp::Conf,
    pub arpstat: crate::arp::Stat,
    /// Gratuitous ARPs to repeat.
    pub garp: Vec<crate::arp::Garp>,
    /// `net.route.hash_outbound`: flows of this host's packets hashed
    /// over multipath routes, on since the first one.
    pub hash_outbound: bool,
}

/// [`Net::route_in`] in `routes`.
#[must_use]
pub fn route_in<'a>(routes: &'a [Route], fib: u16, family: Family, dst: &In6) -> Option<&'a Route> {
    routes
        .iter()
        .filter(|r| r.family == family && r.fib == fib && prefix_match(family, &r.dst, dst, r.plen))
        // The longest prefix; of a multipath group's paths, the first.
        .min_by_key(|r| (core::cmp::Reverse(r.plen), r.nhidx))
}

/// Why a route lookup failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoRoute {
    /// No route at all (`ENETUNREACH`).
    Unreachable,
    /// A reject route (`EHOSTUNREACH`).
    Rejected,
}

impl From<NoRoute> for Errno {
    fn from(n: NoRoute) -> Self {
        match n {
            NoRoute::Unreachable => Errno::ENETUNREACH,
            NoRoute::Rejected => Errno::EHOSTUNREACH,
        }
    }
}

fn prefix_bytes(family: Family) -> u8 {
    match family {
        Family::Inet => 32,
        Family::Inet6 => 128,
    }
}

/// The 16-byte form of `a` masked to `plen` bits of `family`.
#[must_use]
pub fn masked(family: Family, a: &In6, plen: u8) -> In6 {
    let (off, bits) = match family {
        Family::Inet => (12usize, plen.min(32)),
        Family::Inet6 => (0usize, plen.min(128)),
    };
    let mut out = [0u8; 16];
    for i in 0..usize::from(bits.div_ceil(8)) {
        let keep = (usize::from(bits) - i * 8).min(8);
        let mask = if keep == 8 {
            0xff
        } else {
            0xffu8 << (8 - keep)
        };
        out[off + i] = a[off + i] & mask;
    }
    out
}

impl Default for Net {
    fn default() -> Self {
        Self::new()
    }
}

/// The interface indexes in use (`ifindex_table`).
static INDEXES: sync::SpinLock<alloc::collections::BTreeSet<u16>> =
    sync::SpinLock::new(alloc::collections::BTreeSet::new());

impl Net {
    /// No interfaces or routes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ifaces: Vec::new(),
            routes: Vec::new(),
            owner: alloc::sync::Weak::new(),
            in_vnet: false,
            lle: Vec::new(),
            txq: Vec::new(),
            isrq: Vec::new(),
            arp: crate::arp::Conf::DEFAULT,
            arpstat: crate::arp::Stat::ZERO,
            garp: Vec::new(),
            hash_outbound: false,
            outbox: Vec::new(),
            last_nh: 0,
            generation: 0,
        }
    }

    /// The lowest index no interface has (`ifindex_alloc`), taken: an
    /// index is the system's, not a VNET's, and stays with its interface
    /// when it moves. A [`Net`] outside any VNET counts its own.
    pub fn next_index(&self) -> u16 {
        if !self.in_vnet {
            return (1..=u16::MAX)
                .find(|i| self.by_index(*i).is_none())
                .unwrap_or(u16::MAX);
        }
        let mut used = INDEXES.lock();
        let i = (1..=u16::MAX)
            .find(|i| !used.contains(i))
            .unwrap_or(u16::MAX);
        used.insert(i);
        i
    }

    /// Lets interface index `i` be taken again (`ifindex_free`).
    pub fn free_index(i: u16) {
        INDEXES.lock().remove(&i);
    }

    /// Adds an interface (`if_attach`).
    pub fn attach(&mut self, i: Ifnet) {
        if let Some(t) = &i.bpf {
            t.set_home(self.owner.clone(), i.index);
        }
        self.outbox.push(Event::Announce {
            ifp: i.clone(),
            arrival: true,
        });
        self.ifaces.push(i);
        self.generation += 1;
    }

    /// Removes interface `index` with its addresses and the routes
    /// through it (`if_detach`), returning it.
    pub fn detach(&mut self, index: u16) -> Option<Ifnet> {
        let at = self.ifaces.iter().position(|i| i.index == index)?;
        // `if_detach`: IPv6's state for it goes (`in6_ifdetach`), then its
        // addresses and routes, announced.
        if let Some(h) = crate::in6_hooks() {
            (h.if_detach)(self, index);
        }
        let at = self
            .ifaces
            .iter()
            .position(|i| i.index == index)
            .unwrap_or(at);
        let addrs: Vec<Ifaddr> = self.ifaces[at].addrs.clone();
        for a in addrs {
            let _ = self.delete_addr_at(index, a.family, &a.addr);
        }
        let routes: Vec<Route> = self
            .routes
            .iter()
            .filter(|r| r.ifindex == index)
            .cloned()
            .collect();
        for r in routes {
            let _ = self.delete_exact(&r);
        }
        let at = self.ifaces.iter().position(|i| i.index == index)?;
        let i = self.ifaces.remove(at);
        self.lle_purge(index);
        self.outbox.push(Event::Announce {
            ifp: i.clone(),
            arrival: false,
        });
        self.generation += 1;
        Some(i)
    }

    /// The interface with `index`.
    #[must_use]
    pub fn by_index(&self, index: u16) -> Option<&Ifnet> {
        self.ifaces.iter().find(|i| i.index == index)
    }

    /// The interface named `name`.
    #[must_use]
    pub fn by_name(&self, name: &[u8]) -> Option<&Ifnet> {
        self.ifaces.iter().find(|i| i.name.as_bytes() == name)
    }

    fn by_name_mut(&mut self, name: &[u8]) -> Option<&mut Ifnet> {
        self.ifaces.iter_mut().find(|i| i.name.as_bytes() == name)
    }

    /// Whether `a` (of `family`, IPv6 with its `zone`) is an address of
    /// an interface that is up (`ifa_ifwithaddr`).
    #[must_use]
    pub fn is_local(&self, family: Family, a: &In6, zone: u32) -> bool {
        self.ifaces.iter().any(|i| {
            i.addrs.iter().any(|x| {
                x.family == family
                    && same_addr(family, &x.addr, a)
                    && (family == Family::Inet
                        || !is_linklocal6(a)
                        || zone == 0
                        || zone == u32::from(i.index))
            })
        })
    }

    /// The interface address `a` is, if it is one (with its interface).
    #[must_use]
    pub fn ifaddr(&self, family: Family, a: &In6) -> Option<(&Ifnet, &Ifaddr)> {
        self.ifaces.iter().find_map(|i| {
            i.addrs
                .iter()
                .find(|x| x.family == family && same_addr(family, &x.addr, a))
                .map(|x| (i, x))
        })
    }

    /// The route to `dst` (longest prefix first).
    ///
    /// # Errors
    ///
    /// [`NoRoute`] when nothing matches, or a reject route does.
    pub fn lookup(&self, family: Family, dst: &In6) -> Result<&Route, NoRoute> {
        let r = self.route_in(0, family, dst).ok_or(NoRoute::Unreachable)?;
        if r.flags & (rtf::REJECT | rtf::BLACKHOLE) != 0 {
            return Err(NoRoute::Rejected);
        }
        if self
            .by_index(r.ifindex)
            .is_none_or(|i| i.flags & iff::UP == 0)
        {
            return Err(NoRoute::Unreachable);
        }
        Ok(r)
    }

    /// The route to `dst` in table `fib`, longest prefix first, whatever
    /// its flags (`fib4_lookup`, `fib6_lookup`).
    #[must_use]
    pub fn route_in(&self, fib: u16, family: Family, dst: &In6) -> Option<&Route> {
        route_in(&self.routes, fib, family, dst)
    }

    /// `fib6_lookup`: the IPv6 route to `dst` in table `fib`, longest
    /// prefix first; a link-local destination is looked up in `zone`'s
    /// (an interface index), where a route to a link-local prefix belongs
    /// to the interface whose it is.
    #[must_use]
    pub fn route6(&self, fib: u16, dst: &In6, zone: u16) -> Option<&Route> {
        let scoped_dst = is_linklocal6(dst);
        self.routes
            .iter()
            .filter(|r| {
                r.family == Family::Inet6
                    && r.fib == fib
                    && prefix_match(Family::Inet6, &r.dst, dst, r.plen)
                    && (!route_scoped(r) || (scoped_dst && route_zone(r) == zone))
            })
            .min_by_key(|r| (core::cmp::Reverse(r.plen), r.nhidx))
    }

    /// Source address selection for a packet to `dst` (`in_pcbladdr`,
    /// `in6_selectsrc`): the destination itself if it is local, else
    /// the route's interface address of the family; with the zone of a
    /// link-local one. The destination must be routable.
    ///
    /// # Errors
    ///
    /// `ENETUNREACH` or `EHOSTUNREACH` without a route;
    /// `EADDRNOTAVAIL` without an address to send from.
    pub fn source(&self, family: Family, dst: &In6, zone: u32) -> Result<(In6, u32), Errno> {
        if family == Family::Inet6 && is_linklocal6(dst) {
            // A link-local destination needs its zone (the route
            // `fe80::/10 ... -reject` refuses it otherwise).
            let Some(i) = self.ifaces.iter().find(|i| u32::from(i.index) == zone) else {
                return Err(Errno::EHOSTUNREACH);
            };
            if self.is_local(family, dst, zone) {
                return Ok((*dst, zone));
            }
            return i
                .addrs
                .iter()
                .find(|x| x.family == family && is_linklocal6(&x.addr))
                .map(|x| (x.addr, zone))
                .ok_or(Errno::EADDRNOTAVAIL);
        }
        let r = self.lookup(family, dst)?;
        if self.is_local(family, dst, zone) {
            return Ok((*dst, 0));
        }
        let ifp = self.by_index(r.ifindex).ok_or(Errno::ENETUNREACH)?;
        ifp.addrs
            .iter()
            .find(|x| x.family == family && (family == Family::Inet || !is_linklocal6(&x.addr)))
            .map(|x| (x.addr, 0))
            .ok_or(Errno::EADDRNOTAVAIL)
    }

    pub(crate) fn bump(&mut self) {
        self.generation += 1;
    }

    /// Deletes exactly route `r` (one path of a multipath group).
    ///
    /// # Errors
    ///
    /// `ESRCH` if it is not there.
    pub fn delete_exact(&mut self, r: &Route) -> Result<Route, Errno> {
        let i = self
            .routes
            .iter()
            .position(|x| x == r)
            .ok_or(Errno::ESRCH)?;
        self.bump();
        let r = self.routes.remove(i);
        self.outbox.push(Event::Route {
            ty: abi::net::rtm::DELETE,
            route: r.clone(),
            ifp: self.by_index(r.ifindex).cloned(),
            rtsock: false,
        });
        Ok(r)
    }

    /// Deletes the routes whose time has come (`expire_callout`),
    /// announced.
    pub fn expire(&mut self, now: u64) {
        let gone: Vec<Route> = self
            .routes
            .iter()
            .filter(|r| r.expire != 0 && r.expire <= now)
            .cloned()
            .collect();
        for r in gone {
            let _ = self.delete_exact(&r);
        }
    }

    /// Adds a route.
    ///
    /// # Errors
    ///
    /// `EEXIST` if one for the same prefix exists; `ESRCH` if the
    /// interface does not.
    pub fn add_route(&mut self, r: Route) -> Result<(), Errno> {
        if self.by_index(r.ifindex).is_none() {
            return Err(Errno::ESRCH);
        }
        let dst = masked(r.family, &r.dst, r.plen);
        // `ROUTE_MPATH`: another path to a prefix is a new member of its
        // group; the same path again is `EEXIST`.
        if self.routes.iter().any(|x| {
            x.family == r.family
                && x.fib == r.fib
                && x.plen == r.plen
                && x.dst == dst
                && x.gateway == r.gateway
                && x.ifindex == r.ifindex
        }) {
            return Err(Errno::EEXIST);
        }
        // The first multipath group turns on hashing of this host's own
        // packets (`nhgrp_ctl_alloc_default`'s `V_fib_hash_outbound`).
        if self.routes.iter().any(|x| {
            x.family == r.family
                && x.fib == r.fib
                && x.plen == r.plen
                && x.dst == dst
                && (!route_scoped(&r) || route_zone(x) == route_zone(&r))
        }) {
            self.hash_outbound = true;
        }
        self.last_nh += 1;
        let r = Route {
            dst,
            nhidx: self.last_nh,
            ..r
        };
        self.outbox.push(Event::Route {
            ty: abi::net::rtm::ADD,
            route: r.clone(),
            ifp: self.by_index(r.ifindex).cloned(),
            rtsock: false,
        });
        self.routes.push(r);
        self.bump();
        Ok(())
    }

    /// Deletes the route for `dst/plen`.
    ///
    /// # Errors
    ///
    /// `ESRCH` if there is none.
    pub fn delete_route(&mut self, family: Family, dst: &In6, plen: u8) -> Result<Route, Errno> {
        let dst = masked(family, dst, plen);
        let i = self
            .routes
            .iter()
            .position(|x| x.family == family && x.plen == plen && x.dst == dst)
            .ok_or(Errno::ESRCH)?;
        self.bump();
        let r = self.routes.remove(i);
        self.outbox.push(Event::Route {
            ty: abi::net::rtm::DELETE,
            route: r.clone(),
            ifp: self.by_index(r.ifindex).cloned(),
            rtsock: false,
        });
        Ok(r)
    }

    /// [`Net::delete_route`] in table `fib` only.
    ///
    /// # Errors
    ///
    /// `ESRCH` if there is none.
    pub fn delete_route_fib(
        &mut self,
        family: Family,
        dst: &In6,
        plen: u8,
        fib: u16,
    ) -> Result<Route, Errno> {
        let dst = masked(family, dst, plen);
        let r = self
            .routes
            .iter()
            .find(|x| x.family == family && x.plen == plen && x.dst == dst && x.fib == fib)
            .cloned()
            .ok_or(Errno::ESRCH)?;
        self.delete_exact(&r)
    }

    /// The routes an address implies on interface `index`
    /// (`in_addprefix`, `nd6_prefix_onlink`, `ifa_maintain_loopback_route`):
    /// its prefix route through the interface, and a host route to the
    /// address itself through `lo0` with the interface as gateway. On a
    /// loopback interface an IPv4 address gets only its host route
    /// (`rtinitflags`).
    fn implied(&self, index: u16, a: &Ifaddr) -> Vec<Route> {
        let Some(ifp) = self.by_index(index) else {
            return Vec::new();
        };
        let loopback = ifp.flags & iff::LOOPBACK != 0;
        let lo = self
            .ifaces
            .iter()
            .find(|i| i.flags & iff::LOOPBACK != 0 && i.dunit == 0)
            .map_or((index, LOMTU), |l| (l.index, l.mtu));
        let mut out = Vec::new();
        let route = |dst: In6, plen: u8, flags: i32, (via, mtu): (u16, u32)| Route {
            family: a.family,
            dst,
            plen,
            gateway: Gateway::Link(index),
            flags,
            ifindex: via,
            ifa: a.addr,
            mtu,
            weight: 1,
            expire: 0,
            nhidx: 0,
            fib: ifp.fib,
        };
        let here = (index, ifp.mtu);
        let local = rtf::UP | rtf::HOST | rtf::STATIC | rtf::PINNED;
        match a.family {
            Family::Inet if loopback => {
                out.push(route(
                    a.dst.unwrap_or(a.addr),
                    32,
                    rtf::UP | rtf::HOST | rtf::PINNED,
                    here,
                ));
            }
            Family::Inet => {
                if a.plen < 32 {
                    out.push(route(a.addr, a.plen, rtf::UP | rtf::PINNED, here));
                }
                out.push(route(a.addr, 32, local, lo));
            }
            // `in6_update_ifa` announces the address's host route before
            // its prefix's.
            Family::Inet6 => {
                out.push(route(a.addr, 128, local, if loopback { here } else { lo }));
                if a.plen < 128 {
                    out.push(route(a.addr, a.plen, rtf::UP | rtf::PINNED, here));
                }
            }
        }
        out
    }

    /// Adds (or changes) an address on interface `name`
    /// (`SIOCAIFADDR`, `SIOCAIFADDR_IN6`, `RTM_NEWADDR`), with the
    /// routes it implies. An address already there is replaced.
    ///
    /// # Errors
    ///
    /// `ENXIO` for no such interface, `EINVAL` for a bad prefix length.
    pub fn add_addr(&mut self, name: &[u8], a: Ifaddr) -> Result<(), Errno> {
        self.add_addr_flags(name, a, 0)
    }

    /// [`Net::add_addr`] with `in6_update_ifa`'s flags ([`ifa6_update`]).
    ///
    /// # Errors
    ///
    /// As [`Net::add_addr`].
    pub fn add_addr_flags(&mut self, name: &[u8], mut a: Ifaddr, update: u32) -> Result<(), Errno> {
        if a.plen > prefix_bytes(a.family) {
            return Err(Errno::EINVAL);
        }
        let now = crate::uptime();
        let ifp = self.by_name_mut(name).ok_or(Errno::ENXIO)?;
        let index = ifp.index;
        let old = ifp
            .addrs
            .iter()
            .position(|x| x.family == a.family && same_addr(a.family, &x.addr, &a.addr));
        // `in6_validate_ifra`: an existing address keeps its prefix length.
        if a.family == Family::Inet6 && old.is_some_and(|i| ifp.addrs[i].plen != a.plen) {
            return Err(Errno::EINVAL);
        }
        a.updated = now;
        a.created = old.map_or(now, |i| ifp.addrs[i].created);
        // `in_aifaddr_ioctl`: with none given, the subnet's.
        if a.family == Family::Inet && ifp.flags & iff::BROADCAST != 0 && a.broadcast.is_none() {
            let v = u32::from_be_bytes(crate::addr::v4_of(&a.addr));
            let mask = u32::MAX.checked_shl(32 - u32::from(a.plen)).unwrap_or(0);
            let b = if a.plen == 31 { u32::MAX } else { v | !mask };
            a.broadcast = Some(crate::addr::from_v4(b.to_be_bytes()));
        }
        if a.family == Family::Inet6 {
            // `in6_update_ifa_internal`: a new address, or any on a
            // disabled interface, is tentative until DAD passes.
            a.flags6 &= !in6_iff::DUPLICATED;
            if in6if_do_dad(ifp) && (old.is_none() || ifp.nd6_flags & nd6_iff::IFDISABLED != 0) {
                a.flags6 |= in6_iff::TENTATIVE;
            }
            if a.flags6 & in6_iff::DEPRECATED != 0 {
                a.pltime = 0;
            }
        }
        // An address already there is changed in place.
        if let Some(i) = old {
            let gone = core::mem::replace(&mut ifp.addrs[i], a.clone());
            self.drop_implied(index, &gone);
        } else {
            ifp.addrs.push(a.clone());
        }
        let ifp = self.by_name_mut(name).ok_or(Errno::ENXIO)?;
        // An address brings the interface up (`in_ifinit` and
        // `in6_update_ifa` call `if_up`), which attaches IPv6.
        let was = ifp.flags & iff::UP != 0;
        ifp.flags |= iff::UP | iff::RUNNING;
        let snap = ifp.clone();
        if !was {
            self.outbox.push(Event::Info(snap.clone()));
        }
        self.outbox.push(Event::Addr {
            add: true,
            ifp: snap,
            addr: a.clone(),
        });
        for r in self.implied(index, &a) {
            // `in_addprefix`: a prefix another address already routes
            // keeps its route.
            if r.flags & rtf::HOST == 0 && self.prefix_route(&r).is_some() {
                continue;
            }
            let _ = self.add_route(r);
        }
        if !was {
            self.in6_if_up(index);
        }
        // `arp_ifinit` and `nd6_add_ifa_lle`: an Ethernet interface's
        // address is announced and gets a permanent entry.
        match a.family {
            Family::Inet => crate::arp::ifinit(self, index, crate::addr::v4_of(&a.addr)),
            Family::Inet6 => {
                let ll = self
                    .by_index(index)
                    .filter(|i| i.ty == abi::net::IFT_ETHER)
                    .and_then(|i| i.lladdr);
                if let Some(ll) = ll {
                    let f = crate::lltable::lle::IFADDR | crate::lltable::lle::STATIC;
                    let mut e = crate::lltable::Lle::new(Family::Inet6, a.addr, index, f);
                    e.set_lladdr(ll);
                    self.lle_link(e);
                }
                // `in6_broadcast_ifa`: its groups, and DAD.
                if let Some(h) = crate::in6_hooks() {
                    (h.ifa_added)(self, index, &a.addr, old.is_none(), update);
                }
                // `in6_aifaddr`: an address set by hand enables IPv6 on the
                // interface, unless a link-local address is a duplicate.
                if update & ifa6_update::AIFADDR != 0 {
                    self.in6_enable(index);
                }
            }
        }
        self.bump();
        Ok(())
    }

    fn drop_implied(&mut self, index: u16, a: &Ifaddr) {
        // `in_scrubprefixlle` and `nd6_rem_ifa_lle`: the address's own
        // entry goes, and with the prefix route every entry inside it.
        let mut freed = false;
        for r in self.implied(index, a) {
            if r.flags & rtf::HOST != 0 {
                // Its own, in its FIB: every loopback has `fe80::1`.
                if let Some(mine) = self
                    .routes
                    .iter()
                    .find(|x| {
                        x.family == r.family
                            && x.fib == r.fib
                            && x.plen == r.plen
                            && x.dst == r.dst
                            && (!route_scoped(&r) || route_zone(x) == route_zone(&r))
                    })
                    .cloned()
                {
                    let _ = self.delete_exact(&mine);
                }
                continue;
            }
            // Only the prefix route this address made goes; another
            // address with the prefix takes it over (`in_scrubprefix`).
            let Some(mine) = self
                .prefix_route(&r)
                .filter(|x| x.ifindex == index && same_addr(a.family, &x.ifa, &a.addr))
            else {
                continue;
            };
            let _ = self.delete_exact(&mine);
            let scoped = route_scoped(&r);
            let heir = self
                .ifaces
                .iter()
                .filter(|i| !scoped || i.index == index)
                .find_map(|i| {
                    i.addrs
                        .iter()
                        .find(|x| {
                            x.family == a.family
                                && x.plen == a.plen
                                && !same_addr(a.family, &x.addr, &a.addr)
                                && masked(a.family, &x.addr, x.plen)
                                    == masked(a.family, &a.addr, a.plen)
                        })
                        .map(|x| (i.index, x.clone()))
                });
            if let Some((hi, ha)) = heir
                && let Some(nr) = self
                    .implied(hi, &ha)
                    .into_iter()
                    .find(|x| x.flags & rtf::HOST == 0)
            {
                let _ = self.add_route(nr);
            } else {
                self.lle_prefix_free(a.family, &a.addr, a.plen, &a.addr, true);
                freed = true;
            }
        }
        if !freed {
            let _ = self.lle_delete(index, a.family, &a.addr, true);
        }
    }

    /// The interface route for `r`'s prefix, if there is one.
    fn prefix_route(&self, r: &Route) -> Option<Route> {
        let dst = masked(r.family, &r.dst, r.plen);
        self.routes
            .iter()
            .find(|x| {
                x.family == r.family
                    && x.fib == r.fib
                    && x.plen == r.plen
                    && x.dst == dst
                    && x.flags & rtf::PINNED != 0
                    // A link-local prefix is each interface's own.
                    && (!route_scoped(r) || route_zone(x) == route_zone(r))
            })
            .cloned()
    }

    /// Deletes an address (`SIOCDIFADDR`, `RTM_DELADDR`); `None` deletes
    /// the interface's first of the family.
    ///
    /// # Errors
    ///
    /// `ENXIO` for no such interface, `EADDRNOTAVAIL` for no such
    /// address.
    pub fn delete_addr(
        &mut self,
        name: &[u8],
        family: Family,
        addr: Option<&In6>,
    ) -> Result<Ifaddr, Errno> {
        let index = self.by_name(name).ok_or(Errno::ENXIO)?.index;
        let a = self
            .by_index(index)
            .and_then(|i| {
                i.addrs.iter().find(|x| {
                    x.family == family && addr.is_none_or(|a| same_addr(family, &x.addr, a))
                })
            })
            .map(|x| x.addr)
            .ok_or(Errno::EADDRNOTAVAIL)?;
        self.delete_addr_at(index, family, &a)
    }

    /// Deletes address `a` of interface `index`, its routes first.
    fn delete_addr_at(&mut self, index: u16, family: Family, a: &In6) -> Result<Ifaddr, Errno> {
        let ifp = self
            .ifaces
            .iter_mut()
            .find(|i| i.index == index)
            .ok_or(Errno::ENXIO)?;
        let i = ifp
            .addrs
            .iter()
            .position(|x| x.family == family && same_addr(family, &x.addr, a))
            .ok_or(Errno::EADDRNOTAVAIL)?;
        let gone = ifp.addrs.remove(i);
        let snap = ifp.clone();
        if family == Family::Inet6
            && let Some(h) = crate::in6_hooks()
        {
            (h.ifa_removed)(self, index, &gone);
        }
        self.drop_implied(index, &gone);
        self.outbox.push(Event::Addr {
            add: false,
            ifp: snap,
            addr: gone.clone(),
        });
        self.bump();
        Ok(gone)
    }

    /// Sets an interface's flags (`SIOCSIFFLAGS`): what may change, and
    /// up or down. Taking it down keeps its addresses; routes through a
    /// down interface do not match.
    ///
    /// # Errors
    ///
    /// `ENXIO` for no such interface.
    pub fn set_flags(&mut self, name: &[u8], flags: u32) -> Result<(), Errno> {
        let ifp = self.by_name_mut(name).ok_or(Errno::ENXIO)?;
        let up = flags & iff::UP != 0;
        let was = ifp.flags & iff::UP != 0;
        let before = ifp.flags;
        ifp.flags = (ifp.flags & iff::CANTCHANGE) | (flags & !iff::CANTCHANGE);
        // `ifhwioctl`: permanent promiscuous mode is promiscuous; without
        // it, promiscuous only while someone takes it.
        if flags & iff::PPROMISC != 0 {
            ifp.flags |= iff::PROMISC;
        } else if ifp.pcount == 0 {
            ifp.flags &= !iff::PROMISC;
        }
        // A loopback interface runs while up (`lo_ioctl`'s `SIOCSIFFLAGS`),
        // as does a driver's whose `if_init` starts it; another keeps its
        // own `IFF_DRV_RUNNING`.
        if ifp.flags & iff::LOOPBACK != 0 || ifp.link.0.as_ref().is_some_and(|l| l.runs_while_up())
        {
            if up {
                ifp.flags |= iff::RUNNING;
            } else {
                ifp.flags &= !iff::RUNNING;
            }
        } else {
            ifp.flags = (ifp.flags & !iff::RUNNING) | (before & iff::RUNNING);
        }
        if ifp.flags != before
            && let Some(l) = &ifp.link.0
        {
            l.flags_changed(ifp.flags);
        }
        let index = ifp.index;
        // A driver started by coming up learns its link
        // (`vtnet_init_locked`'s `vtnet_update_link_status`).
        let mut link_event = None;
        if up
            && !was
            && let Some(link) = ifp.link.0.as_ref().and_then(|l| l.link_status())
        {
            let state = if link {
                abi::net::LINK_STATE_UP
            } else {
                abi::net::LINK_STATE_DOWN
            };
            if ifp.link_state != state {
                ifp.link_state = state;
                link_event = Some(link);
            }
        }
        let snap = ifp.clone();
        // `if_up` and `if_down` announce (`rt_ifmsg`).
        if up != was {
            self.outbox.push(Event::Info(snap.clone()));
        }
        if let Some(link) = link_event {
            self.outbox.push(Event::Link {
                ifp: snap,
                up: link,
            });
        }
        if up && !was {
            self.in6_if_up(index);
        }
        self.bump();
        Ok(())
    }

    /// `nd6_ioctl`'s `SIOCSIFINFO_FLAGS` clearing `ND6_IFF_IFDISABLED`:
    /// refused while a link-local address is a duplicate; else IPv6 is on,
    /// and an interface that is up gets `in6_if_up`. Whether it is on.
    pub fn in6_enable(&mut self, index: u16) -> bool {
        let Some(i) = self.ifaces.iter().find(|i| i.index == index) else {
            return false;
        };
        if i.nd6_flags & nd6_iff::IFDISABLED == 0 {
            return true;
        }
        let dup = i.addrs.iter().any(|a| {
            a.family == Family::Inet6
                && a.flags6 & in6_iff::DUPLICATED != 0
                && a.addr[0] == 0xfe
                && a.addr[1] & 0xc0 == 0x80
        });
        if dup {
            crate::log(
                3,
                "Cannot enable an interface with a link-local address marked duplicate.\n",
            );
            return false;
        }
        let up = i.flags & iff::UP != 0;
        if let Some(i) = self.ifaces.iter_mut().find(|i| i.index == index) {
            i.nd6_flags &= !nd6_iff::IFDISABLED;
        }
        if up {
            self.in6_if_up(index);
        }
        true
    }

    /// `in6_if_up`: DAD again for its tentative addresses, then
    /// `in6_ifattach`.
    pub fn in6_if_up(&mut self, index: u16) {
        if let Some(h) = crate::in6_hooks() {
            (h.if_up)(self, index);
        }
        self.in6_ifattach(index);
    }

    /// Takes or lets go of promiscuous mode on interface `index`
    /// (`ifpromisc`): the first taker turns it on, the last off, unless
    /// it is permanent (`IFF_PPROMISC`).
    ///
    /// # Errors
    ///
    /// `ENXIO` for no such interface.
    pub fn promisc(&mut self, index: u16, on: bool) -> Result<(), Errno> {
        let ifp = self
            .ifaces
            .iter_mut()
            .find(|i| i.index == index)
            .ok_or(Errno::ENXIO)?;
        if on {
            ifp.pcount += 1;
            if ifp.pcount > 1 || ifp.flags & iff::PPROMISC != 0 {
                return Ok(());
            }
            ifp.flags |= iff::PROMISC;
        } else {
            if ifp.pcount == 0 {
                return Ok(());
            }
            ifp.pcount -= 1;
            if ifp.pcount > 0 || ifp.flags & iff::PPROMISC != 0 {
                return Ok(());
            }
            ifp.flags &= !iff::PROMISC;
        }
        if let Some(l) = &ifp.link.0 {
            l.flags_changed(ifp.flags);
        }
        // `if_setflag`'s `rt_ifmsg`.
        let snap = ifp.clone();
        self.outbox.push(Event::Info(snap));
        self.bump();
        Ok(())
    }

    /// Joins link-layer group `ll` on interface `index` (`if_addmulti`):
    /// a new membership goes to the driver's receive filter. A request
    /// from userland may join a group once (`SIOCADDMULTI`).
    ///
    /// # Errors
    ///
    /// `ENXIO` for no such interface, `EOPNOTSUPP` for one without
    /// multicast, `EADDRINUSE` for a group userland already joined.
    pub fn add_multi(&mut self, index: u16, ll: [u8; 6], user: bool) -> Result<(), Errno> {
        let ifp = self
            .ifaces
            .iter_mut()
            .find(|i| i.index == index)
            .ok_or(Errno::ENXIO)?;
        if ifp.flags & iff::MULTICAST == 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        if let Some(m) = ifp.maddrs.iter_mut().find(|m| m.0 == ll) {
            if user {
                return Err(Errno::EADDRINUSE);
            }
            m.1 += 1;
            return Ok(());
        }
        ifp.maddrs.push((ll, 1));
        if let Some(l) = &ifp.link.0 {
            let list: Vec<[u8; 6]> = ifp.maddrs.iter().map(|m| m.0).collect();
            l.multicast_changed(&list);
        }
        self.bump();
        Ok(())
    }

    /// Leaves group `ll` on interface `index` (`if_delmulti`): the last
    /// reference takes it out of the driver's filter.
    ///
    /// # Errors
    ///
    /// `ENXIO` for no such interface, `EADDRNOTAVAIL` for a group it is
    /// not in.
    pub fn del_multi(&mut self, index: u16, ll: [u8; 6]) -> Result<(), Errno> {
        let ifp = self
            .ifaces
            .iter_mut()
            .find(|i| i.index == index)
            .ok_or(Errno::ENXIO)?;
        let at = ifp
            .maddrs
            .iter()
            .position(|m| m.0 == ll)
            .ok_or(Errno::EADDRNOTAVAIL)?;
        ifp.maddrs[at].1 -= 1;
        if ifp.maddrs[at].1 == 0 {
            ifp.maddrs.remove(at);
            if let Some(l) = &ifp.link.0 {
                let list: Vec<[u8; 6]> = ifp.maddrs.iter().map(|m| m.0).collect();
                l.multicast_changed(&list);
            }
        }
        self.bump();
        Ok(())
    }

    /// Mutable access to the interface `name`.
    pub fn with_iface<R>(&mut self, name: &[u8], f: impl FnOnce(&mut Ifnet) -> R) -> Option<R> {
        let r = self.by_name_mut(name).map(f);
        self.bump();
        r
    }

    /// What an interface gets when it comes up (`in6_ifattach`): a
    /// loopback interface `::1/128` (`in6_ifattach_loopback`) when no
    /// interface of the VNET has it yet, and with
    /// IPv6 enabled and `ND6_IFF_AUTO_LINKLOCAL` its link-local address
    /// (`in6_ifattach_linklocal`): `fe80::1/64` on loopback, else one
    /// from its EUI-64 (`in6_get_hw_ifid`), its DAD delayed.
    pub fn in6_ifattach(&mut self, index: u16) {
        let Some(ifp) = self.ifaces.iter().find(|i| i.index == index) else {
            return;
        };
        if ifp.flags & iff::MULTICAST == 0 {
            return;
        }
        let loopback = ifp.flags & iff::LOOPBACK != 0;
        let name = ifp.name.clone();
        let has = |ifp: &Ifnet, f: &dyn Fn(&In6) -> bool| {
            ifp.addrs
                .iter()
                .any(|a| a.family == Family::Inet6 && f(&a.addr))
        };
        let want_lo = loopback && !self.ifaces.iter().any(|i| has(i, &|a| *a == LOOPBACK6));
        let want_ll = ifp.nd6_flags & nd6_iff::IFDISABLED == 0
            && ifp.nd6_flags & nd6_iff::AUTO_LINKLOCAL != 0
            && !has(ifp, &|a| is_linklocal6(a) && a[0] == 0xfe);
        let ll = if loopback {
            let mut ll = [0u8; 16];
            ll[0] = 0xfe;
            ll[1] = 0x80;
            ll[15] = 1;
            Some(ll)
        } else {
            ifp.lladdr.and_then(|m| eui64_linklocal(&m))
        };
        let six = |addr: In6, plen: u8| Ifaddr {
            family: Family::Inet6,
            addr,
            plen,
            dst: None,
            broadcast: None,
            flags6: 0,
            vltime: INFINITE,
            pltime: INFINITE,
            vhid: 0,
            created: 0,
            updated: 0,
        };
        if want_lo {
            let _ = self.add_addr(name.as_bytes(), six(LOOPBACK6, 128));
        }
        if want_ll && let Some(ll) = ll {
            let _ = self.add_addr_flags(
                name.as_bytes(),
                six(ll, 64),
                ifa6_update::DADDELAY | ifa6_update::LINKLOCAL,
            );
        }
    }
}

/// `in6_get_hw_ifid` with `fe80::/64`: the link-local address made from
/// MAC `m` (its EUI-64 with the universal bit flipped), none for a group
/// address or one that is all zeros.
#[must_use]
pub fn eui64_linklocal(m: &[u8; 6]) -> Option<In6> {
    let mut a = [0u8; 16];
    a[0] = 0xfe;
    a[1] = 0x80;
    a[8..11].copy_from_slice(&m[..3]);
    a[11] = 0xff;
    a[12] = 0xfe;
    a[13..].copy_from_slice(&m[3..]);
    // `EUI64_GROUP`.
    if a[8] & 0x01 != 0 {
        return None;
    }
    // `EUI64_TO_IFID`.
    a[8] ^= 0x02;
    if a[8] & !0x03 == 0 && a[9..].iter().all(|&b| b == 0) {
        return None;
    }
    Some(a)
}

/// Whether IPv6 route `r` is to a link-local prefix of one interface
/// (`fe80::%if/64`), rather than one unscoped (`fe80::/10`, `ff02::/16`).
#[must_use]
pub fn route_scoped(r: &Route) -> bool {
    r.family == Family::Inet6 && is_linklocal6(&r.dst) && r.plen > 16
}

/// A route's place in a radix walk: its destination, a scoped one with the
/// zone in its second word, as FreeBSD's key has it.
#[must_use]
pub fn route_key(r: &Route) -> In6 {
    let mut k = r.dst;
    if route_scoped(r) {
        k[2..4].copy_from_slice(&route_zone(r).to_be_bytes());
    }
    k
}

/// The interface a scoped route's prefix belongs to: an address's routes
/// name it as their gateway, others by where they go.
#[must_use]
pub const fn route_zone(r: &Route) -> u16 {
    match r.gateway {
        Gateway::Link(i) => i,
        _ => r.ifindex,
    }
}

/// `in6if_do_dad`: whether interface `i`'s addresses get duplicate
/// address detection.
#[must_use]
pub const fn in6if_do_dad(i: &Ifnet) -> bool {
    i.flags & iff::LOOPBACK == 0
        && i.flags & iff::MULTICAST != 0
        && i.nd6_flags & (nd6_iff::IFDISABLED | nd6_iff::NO_DAD) == 0
}

/// `IN6_IFF_*`: an IPv6 address's flags.
pub mod in6_iff {
    pub const ANYCAST: i32 = 0x01;
    pub const TENTATIVE: i32 = 0x02;
    pub const DUPLICATED: i32 = 0x04;
    pub const DETACHED: i32 = 0x08;
    pub const DEPRECATED: i32 = 0x10;
    pub const NODAD: i32 = 0x20;
    pub const AUTOCONF: i32 = 0x40;
    pub const TEMPORARY: i32 = 0x80;
    pub const PREFER_SOURCE: i32 = 0x100;
    /// `IN6_IFF_NOTREADY`.
    pub const NOTREADY: i32 = TENTATIVE | DUPLICATED;
}

/// `in6_update_ifa`'s flags.
pub mod ifa6_update {
    /// `IN6_IFAUPDATE_DADDELAY`: DAD and the solicited-node report wait a
    /// random part of a second.
    pub const DADDELAY: u32 = 0x1;
    /// The interface's own link-local address (`in6_ifattach_linklocal`),
    /// whose `fe80::/64` goes on the prefix list.
    pub const LINKLOCAL: u32 = 0x100;
    /// Set by hand (`in6_aifaddr`): its prefix goes on the prefix list.
    pub const AIFADDR: u32 = 0x200;
}

/// Whether two addresses of `family` are the same (IPv4: their last four
/// bytes).
#[must_use]
pub fn same_addr(family: Family, a: &In6, b: &In6) -> bool {
    match family {
        Family::Inet => v4_of(a) == v4_of(b),
        Family::Inet6 => a == b,
    }
}

/// `lo0` as `if_loop` attaches it (`lo_clone_create`): down, with no
/// addresses, `IFF_LOOPBACK | IFF_MULTICAST`, MTU 16384, checksum
/// offload and link state capabilities, IPv6 with
/// `ND6_IFF_PERFORMNUD | ND6_IFF_AUTO_LINKLOCAL` (`nd6_ifattach`).
#[must_use]
pub fn loopback(index: u16, now: i64, stats: Arc<Stats>) -> Ifnet {
    let caps =
        ifcap::RXCSUM | ifcap::TXCSUM | ifcap::RXCSUM_IPV6 | ifcap::TXCSUM_IPV6 | ifcap::LINKSTATE;
    Ifnet {
        index,
        name: String::from("lo0"),
        dname: "lo",
        dunit: 0,
        home: 0,
        fib: 0,
        flags: iff::LOOPBACK | iff::MULTICAST,
        mtu: LOMTU,
        metric: 0,
        ty: IFT_LOOP,
        link_state: LINK_STATE_UP,
        capabilities: caps,
        capenable: caps,
        nd6_flags: nd6_iff::PERFORMNUD | nd6_iff::AUTO_LINKLOCAL,
        nd: NdInfo::DEFAULT,
        description: None,
        groups: alloc::vec![String::from("lo")],
        addrs: Vec::new(),
        epoch: now,
        lastchange: (now, 0),
        stats,
        lladdr: None,
        baudrate: 0,
        link: crate::link::LinkRef(None),
        bpf: Some(crate::bpf::Tap::null()),
        pcount: 0,
        maddrs: Vec::new(),
        pcp: PCP_NONE,
    }
}

/// Gives `lo0` what FreeBSD's `rc.d/netif` gives it with the stock
/// `rc.conf` (`ifconfig lo0 inet 127.0.0.1/8`, which brings it up):
/// `127.0.0.1/8`, and with that `::1/128` and `fe80::1%lo0/64`; its
/// attach time becomes `now`.
pub fn configure_loopback(n: &mut Net, now: i64) {
    crate::vnet::ensure_lo0(n, now);
    if let Some(lo) = n.by_name_mut(b"lo0") {
        lo.epoch = now;
        lo.lastchange = (now, 0);
    }
    let v4 = Ifaddr {
        family: Family::Inet,
        addr: from_v4([127, 0, 0, 1]),
        plen: 8,
        dst: Some(from_v4([127, 0, 0, 1])),
        broadcast: None,
        flags6: 0,
        vltime: INFINITE,
        pltime: INFINITE,
        vhid: 0,
        created: 0,
        updated: 0,
    };
    let _ = n.add_addr(b"lo0", v4);
}

/// Whether `a` is the IPv4 loopback network (`127/8`).
#[must_use]
pub fn is_loopback_net(a: &In6) -> bool {
    is_loopback4(&v4_of(a))
}
