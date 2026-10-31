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
#[derive(Debug, Default)]
pub struct Stats {
    pub ipackets: AtomicU64,
    pub opackets: AtomicU64,
    pub ibytes: AtomicU64,
    pub obytes: AtomicU64,
    pub imcasts: AtomicU64,
    pub omcasts: AtomicU64,
    pub iqdrops: AtomicU64,
    pub noproto: AtomicU64,
    pub oerrors: AtomicU64,
}

impl Stats {
    /// One packet of `bytes` looped back: out and in again
    /// (`if_simloop`).
    pub fn looped(&self, bytes: usize) {
        let b = bytes as u64;
        // Relaxed: counters only.
        self.opackets.fetch_add(1, Ordering::Relaxed);
        self.ipackets.fetch_add(1, Ordering::Relaxed);
        self.obytes.fetch_add(b, Ordering::Relaxed);
        self.ibytes.fetch_add(b, Ordering::Relaxed);
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

impl Net {
    /// No interfaces or routes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ifaces: Vec::new(),
            routes: Vec::new(),
            outbox: Vec::new(),
            last_nh: 0,
            generation: 0,
        }
    }

    /// The lowest index no interface has (`ifindex_alloc`).
    #[must_use]
    pub fn next_index(&self) -> u16 {
        (1..=u16::MAX)
            .find(|i| self.by_index(*i).is_none())
            .unwrap_or(u16::MAX)
    }

    /// Adds an interface (`if_attach`).
    pub fn attach(&mut self, i: Ifnet) {
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
        // `if_detach`: its addresses and routes go first, announced.
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
            let _ = self.delete_route(r.family, &r.dst, r.plen);
        }
        let at = self.ifaces.iter().position(|i| i.index == index)?;
        let i = self.ifaces.remove(at);
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
        let r = self
            .routes
            .iter()
            .filter(|r| {
                r.family == family && r.fib == 0 && prefix_match(family, &r.dst, dst, r.plen)
            })
            .max_by_key(|r| r.plen)
            .ok_or(NoRoute::Unreachable)?;
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

    fn bump(&mut self) {
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
            let _ = self.delete_route(r.family, &r.dst, r.plen);
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
        if a.plen > prefix_bytes(a.family) {
            return Err(Errno::EINVAL);
        }
        let ifp = self.by_name_mut(name).ok_or(Errno::ENXIO)?;
        let index = ifp.index;
        let old = ifp
            .addrs
            .iter()
            .position(|x| x.family == a.family && same_addr(a.family, &x.addr, &a.addr));
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
            self.in6_ifattach(index);
        }
        self.bump();
        Ok(())
    }

    fn drop_implied(&mut self, index: u16, a: &Ifaddr) {
        for r in self.implied(index, a) {
            if r.flags & rtf::HOST != 0 {
                let _ = self.delete_route(r.family, &r.dst, r.plen);
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
            let heir = self.ifaces.iter().find_map(|i| {
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
            }
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
        // A loopback interface runs while up (`lo_ioctl`'s `SIOCSIFFLAGS`);
        // a device's driver keeps its own `IFF_DRV_RUNNING`.
        if ifp.flags & iff::LOOPBACK != 0 {
            if up {
                ifp.flags |= iff::RUNNING;
            } else {
                ifp.flags &= !iff::RUNNING;
            }
        } else {
            ifp.flags = (ifp.flags & !iff::RUNNING) | (before & iff::RUNNING);
        }
        let index = ifp.index;
        // `if_up` and `if_down` announce (`rt_ifmsg`).
        if up != was {
            let snap = ifp.clone();
            self.outbox.push(Event::Info(snap));
        }
        if up && !was {
            self.in6_ifattach(index);
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

    /// What an interface gets when it comes up with IPv6 enabled
    /// (`in6_ifattach`): a loopback interface `::1/128`
    /// (`in6_ifattach_loopback`), and with `ND6_IFF_AUTO_LINKLOCAL` its
    /// link-local address, `fe80::1/64` on loopback
    /// (`in6_ifattach_linklocal`).
    pub fn in6_ifattach(&mut self, index: u16) {
        let Some(ifp) = self.ifaces.iter().find(|i| i.index == index) else {
            return;
        };
        if ifp.nd6_flags & nd6_iff::IFDISABLED != 0 || ifp.flags & iff::LOOPBACK == 0 {
            return;
        }
        let name = ifp.name.clone();
        let has = |ifp: &Ifnet, f: &dyn Fn(&In6) -> bool| {
            ifp.addrs
                .iter()
                .any(|a| a.family == Family::Inet6 && f(&a.addr))
        };
        let want_lo = !has(ifp, &|a| *a == LOOPBACK6);
        let want_ll =
            ifp.nd6_flags & nd6_iff::AUTO_LINKLOCAL != 0 && !has(ifp, &|a| is_linklocal6(a));
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
        };
        if want_lo {
            let _ = self.add_addr(name.as_bytes(), six(LOOPBACK6, 128));
        }
        if want_ll {
            let mut ll = [0u8; 16];
            ll[0] = 0xfe;
            ll[1] = 0x80;
            ll[15] = 1;
            let _ = self.add_addr(name.as_bytes(), six(ll, 64));
        }
    }
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
        description: None,
        groups: alloc::vec![String::from("lo")],
        addrs: Vec::new(),
        epoch: now,
        lastchange: (now, 0),
        stats,
        lladdr: None,
        baudrate: 0,
        link: crate::link::LinkRef(None),
    }
}

/// Gives `lo0` what FreeBSD's `rc.d/netif` gives it with the stock
/// `rc.conf` (`ifconfig lo0 inet 127.0.0.1/8`, which brings it up):
/// `127.0.0.1/8`, and with that `::1/128` and `fe80::1%lo0/64`; its
/// attach time becomes `now`.
pub fn configure_loopback(n: &mut Net, now: i64) {
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
    };
    let _ = n.add_addr(b"lo0", v4);
}

/// Whether `a` is the IPv4 loopback network (`127/8`).
#[must_use]
pub fn is_loopback_net(a: &In6) -> bool {
    is_loopback4(&v4_of(a))
}
