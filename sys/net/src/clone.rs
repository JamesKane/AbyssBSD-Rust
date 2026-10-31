// SPDX-License-Identifier: BSD-2-Clause
//! Interface cloners (`ifconfig <name> create`, `destroy`, `-C`): `lo`,
//! further loopback interfaces, and `epair`, pairs of Ethernet interfaces
//! whose frames come out of each other, either end of which can be moved
//! into a VNET jail.
//!
//! Behavior reference: sys/net/if_clone.c (`ifc_name2unit`,
//! `ifc_alloc_unit`, `if_clone_create`, `if_clone_destroy`), sys/net/if_loop.c
//! (`lo_clone_create`, `if_simloop`), sys/net/if_epair.c.

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::net::{IFT_ETHER, LINK_STATE_UP, ifcap, iff, nd6_iff};
use mbuf::{NoBufs, Packet};
use sync::SpinLock;

use crate::ifnet::{self, Ifnet, Stats};
use crate::link::{self, Link, LinkRef, Media, ifm};
use crate::netisr::Proto;
use crate::{Family, Vnet};

/// `ETHERMTU`.
pub const ETHERMTU: u32 = 1500;
/// `IF_MAXUNIT`.
const MAXUNIT: i32 = 0x7fff;

/// The cloners, as `SIOCIFGCLONERS` lists them (most recently attached
/// first: `epair` is a module loaded after `lo`).
pub const CLONERS: [&str; 2] = ["epair", "lo"];

/// What a create needs from the creating thread: the time, and for
/// `epair`'s addresses the host UUID and jail name, or random bytes.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub now: i64,
    pub hostuuid: String,
    pub jail: String,
    pub random: [u8; 12],
}

/// `ifc_name2unit`: the unit at the end of `name` (`None` for none).
///
/// # Errors
///
/// `EINVAL` for a leading zero, a non-digit after the first digit, or
/// overflow.
pub fn name2unit(name: &[u8]) -> Result<Option<i32>, Errno> {
    let Some(at) = name.iter().position(u8::is_ascii_digit) else {
        return Ok(None);
    };
    let digits = &name[at..];
    if digits[0] == b'0' && digits.len() > 1 {
        return Err(Errno::EINVAL);
    }
    let mut unit: i32 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return Err(Errno::EINVAL);
        }
        unit = unit
            .checked_mul(10)
            .and_then(|u| u.checked_add(i32::from(c - b'0')))
            .ok_or(Errno::EINVAL)?;
    }
    Ok(Some(unit))
}

/// Whether `name` is `prefix` with nothing or digits after it
/// (`ifc_simple_match`, and `epair_clone_match`).
fn matches(prefix: &str, name: &[u8]) -> bool {
    name.strip_prefix(prefix.as_bytes())
        .is_some_and(|rest| rest.iter().all(u8::is_ascii_digit))
}

/// A loopback interface's device: output comes back in through `netisr`
/// (`if_simloop`).
struct Loop {
    stats: Arc<Stats>,
}

impl Link for Loop {
    fn transmit(
        &self,
        vnet: &Arc<Vnet>,
        index: u16,
        family: Option<Family>,
        mut m: Packet,
    ) -> Result<(), NoBufs> {
        let len = m.len();
        self.stats.looped(len);
        m.hdr.rcvif = index;
        let proto = match family {
            Some(Family::Inet6) => Proto::Ipv6,
            _ => Proto::Ip,
        };
        vnet.netisr.queue(proto, m).map_err(|_| {
            self.stats.iqdrops.fetch_add(1, Ordering::Relaxed);
            NoBufs
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// One end of an `epair`.
pub struct Epair {
    stats: Arc<Stats>,
    /// Where this end is: its VNET and index there.
    home: SpinLock<(Weak<Vnet>, u16)>,
    /// The VNET it was made in, whose cloner holds its unit.
    origin: Weak<Vnet>,
    unit: i32,
    peer: SpinLock<Weak<Epair>>,
}

impl core::fmt::Debug for Epair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Epair")
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

impl Epair {
    /// Where this end is now.
    #[must_use]
    pub fn home(&self) -> (Option<Arc<Vnet>>, u16) {
        let h = self.home.lock();
        (h.0.upgrade(), h.1)
    }

    /// Records a move to `vnet` as interface `index`.
    pub fn moved(&self, vnet: &Arc<Vnet>, index: u16) {
        *self.home.lock() = (Arc::downgrade(vnet), index);
    }

    fn peer(&self) -> Option<Arc<Self>> {
        self.peer.lock().upgrade()
    }
}

/// Whether interface `index` of `vnet` is up and running.
fn running(vnet: &Vnet, index: u16) -> bool {
    vnet.net
        .lock()
        .by_index(index)
        .is_some_and(|i| i.flags & (iff::UP | iff::RUNNING) == iff::UP | iff::RUNNING)
}

impl Link for Epair {
    fn transmit(
        &self,
        vnet: &Arc<Vnet>,
        index: u16,
        _family: Option<Family>,
        mut m: Packet,
    ) -> Result<(), NoBufs> {
        let peer = self.peer();
        let there = peer.as_ref().map(|p| p.home());
        let ok = running(vnet, index) && matches!(&there, Some((Some(v), i)) if running(v, *i));
        let (Some(peer), Some((Some(pv), pi)), true) = (peer, there, ok) else {
            // `epair_transmit` with either end down: dropped.
            self.stats.oerrors.fetch_add(1, Ordering::Relaxed);
            return Err(NoBufs);
        };
        let len = m.len() as u64;
        self.stats.opackets.fetch_add(1, Ordering::Relaxed);
        self.stats.obytes.fetch_add(len, Ordering::Relaxed);
        peer.stats.ipackets.fetch_add(1, Ordering::Relaxed);
        peer.stats.ibytes.fetch_add(len, Ordering::Relaxed);
        m.hdr.rcvif = pi;
        pv.netisr.queue(Proto::Ether, m).map_err(|_| {
            peer.stats.iqdrops.fetch_add(1, Ordering::Relaxed);
            NoBufs
        })
    }

    fn media(&self) -> Option<Media> {
        Some(Media {
            current: ifm::ETHER | ifm::T10G,
            active: ifm::ETHER | ifm::T10G | ifm::FDX,
            status: ifm::AVALID | ifm::ACTIVE,
            supported: ifm::ETHER | ifm::T10G,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `epair_setup_ifp` and `ether_ifattach`: an `epair` end, running with
/// its link up.
fn epair_ifnet(
    name: String,
    unit: i32,
    index: u16,
    now: i64,
    lladdr: [u8; 6],
    link: Arc<Epair>,
) -> Ifnet {
    let caps = ifcap::VLAN_MTU
        | ifcap::VLAN_HWTAGGING
        | ifcap::TXCSUM
        | ifcap::RXCSUM
        | ifcap::TXCSUM_IPV6
        | ifcap::RXCSUM_IPV6;
    Ifnet {
        index,
        name,
        dname: "epair",
        dunit: unit,
        home: 0,
        fib: 0,
        flags: iff::BROADCAST | iff::SIMPLEX | iff::MULTICAST | iff::RUNNING,
        mtu: ETHERMTU,
        metric: 0,
        ty: IFT_ETHER,
        link_state: LINK_STATE_UP,
        capabilities: caps,
        capenable: caps,
        // `nd6_ifattach`: IPv6 off until enabled (`ifdisabled`).
        nd6_flags: nd6_iff::PERFORMNUD | nd6_iff::IFDISABLED | nd6_iff::AUTO_LINKLOCAL,
        description: None,
        groups: alloc::vec![String::from("epair")],
        addrs: Vec::new(),
        epoch: now,
        lastchange: (now, 0),
        stats: Arc::clone(&link.stats),
        lladdr: Some(lladdr),
        baudrate: 10_000_000_000,
        link: LinkRef(Some(link)),
    }
}

/// A loopback interface with `stats` (`lo_clone_create`).
#[must_use]
pub fn lo_ifnet(unit: i32, index: u16, now: i64, stats: Arc<Stats>) -> Ifnet {
    let mut i = ifnet::loopback(index, now, Arc::clone(&stats));
    i.name = format!("lo{unit}");
    i.dunit = unit;
    i.link = LinkRef(Some(Arc::new(Loop { stats })));
    i
}

impl Vnet {
    /// Takes `unit` of cloner `dname`, or the lowest free one
    /// (`ifc_alloc_unit`); an interface already named with it is
    /// `EEXIST` too.
    fn alloc_unit(
        &self,
        dname: &'static str,
        unit: Option<i32>,
        taken: &dyn Fn(i32) -> bool,
    ) -> Result<i32, Errno> {
        let mut units = self.units.lock();
        let free = |u: i32| !units.contains(&(dname, u)) && !taken(u);
        let u = match unit {
            Some(u) if u > MAXUNIT => return Err(Errno::ENOSPC),
            Some(u) if !free(u) => return Err(Errno::EEXIST),
            Some(u) => u,
            None => (0..=MAXUNIT).find(|u| free(*u)).ok_or(Errno::ENOSPC)?,
        };
        units.push((dname, u));
        Ok(u)
    }

    fn free_unit(&self, dname: &'static str, unit: i32) {
        self.units.lock().retain(|x| *x != (dname, unit));
    }
}

/// `if_clone_create`: makes the interface `name` asks for in `vnet` and
/// returns its name (`epair` makes two, and names the first).
///
/// # Errors
///
/// `EEXIST` if it exists, `EINVAL` for no cloner of that name or a bad
/// unit, `ENOSPC` for no unit left.
pub fn create(vnet: &Arc<Vnet>, name: &[u8], ctx: &Ctx) -> Result<String, Errno> {
    if vnet.net.lock().by_name(name).is_some() {
        return Err(Errno::EEXIST);
    }
    let unit = name2unit(name)?;
    if matches("epair", name) {
        return create_epair(vnet, unit, ctx);
    }
    if matches("lo", name) {
        let named = |u: i32| {
            vnet.net
                .lock()
                .by_name(format!("lo{u}").as_bytes())
                .is_some()
        };
        let u = vnet.alloc_unit("lo", unit, &named)?;
        let mut n = vnet.net.lock();
        let mut i = lo_ifnet(u, n.next_index(), ctx.now, Arc::new(Stats::default()));
        i.home = vnet.id;
        let out = i.name.clone();
        n.attach(i);
        return Ok(out);
    }
    Err(Errno::EINVAL)
}

fn create_epair(vnet: &Arc<Vnet>, unit: Option<i32>, ctx: &Ctx) -> Result<String, Errno> {
    let named = |u: i32| {
        let n = vnet.net.lock();
        n.by_name(format!("epair{u}a").as_bytes()).is_some()
            || n.by_name(format!("epair{u}b").as_bytes()).is_some()
    };
    let u = vnet.alloc_unit("epair", unit, &named)?;
    let (na, nb) = (format!("epair{u}a"), format!("epair{u}b"));
    let addr = |name: &str, r: &[u8]| {
        link::gen_addr(&ctx.hostuuid, name, &ctx.jail)
            .unwrap_or_else(|| link::random_addr([r[0], r[1], r[2], r[3], r[4], r[5]]))
    };
    let (la, lb) = (addr(&na, &ctx.random[..6]), addr(&nb, &ctx.random[6..]));
    let end = || {
        Arc::new(Epair {
            stats: Arc::new(Stats::default()),
            home: SpinLock::new((Arc::downgrade(vnet), 0)),
            origin: Arc::downgrade(vnet),
            unit: u,
            peer: SpinLock::new(Weak::new()),
        })
    };
    let (a, b) = (end(), end());
    *a.peer.lock() = Arc::downgrade(&b);
    *b.peer.lock() = Arc::downgrade(&a);
    let mut n = vnet.net.lock();
    let ia = n.next_index();
    a.moved(vnet, ia);
    let mut ea = epair_ifnet(na.clone(), u, ia, ctx.now, la, Arc::clone(&a));
    ea.home = vnet.id;
    n.attach(ea);
    let ib = n.next_index();
    b.moved(vnet, ib);
    let mut eb = epair_ifnet(nb, u, ib, ctx.now, lb, b);
    eb.home = vnet.id;
    n.attach(eb);
    Ok(na)
}

/// `if_clone_destroy`: destroys the cloned interface `name` of `vnet`
/// (both ends of an `epair`, wherever the other is). Returns the
/// interfaces removed, with their VNETs, for the announcements.
///
/// # Errors
///
/// `ENXIO` if there is no such interface, `EINVAL` if no cloner made it
/// or it is `lo0`.
pub fn destroy(vnet: &Arc<Vnet>, name: &[u8]) -> Result<Vec<(Arc<Vnet>, Ifnet)>, Errno> {
    let (dname, dunit, link, index) = {
        let n = vnet.net.lock();
        let i = n.by_name(name).ok_or(Errno::ENXIO)?;
        (i.dname, i.dunit, i.link.clone(), i.index)
    };
    if !CLONERS.contains(&dname) || dname == "lo" && dunit == 0 {
        return Err(Errno::EINVAL);
    }
    let mut gone = Vec::new();
    if let Some(e) = link
        .0
        .as_ref()
        .and_then(|l| l.as_any().downcast_ref::<Epair>())
    {
        if let Some(p) = e.peer()
            && let (Some(pv), pi) = p.home()
            && let Some(i) = pv.net.lock().detach(pi)
        {
            gone.push((Arc::clone(&pv), i));
        }
        if let Some(origin) = e.origin.upgrade() {
            origin.free_unit("epair", e.unit);
        }
    } else {
        vnet.free_unit(dname, dunit);
    }
    if let Some(i) = vnet.net.lock().detach(index) {
        gone.insert(0, (Arc::clone(vnet), i));
    }
    Ok(gone)
}

/// Sends `m` out of interface `index` of `vnet`, an IPv4 or IPv6 packet
/// for a loopback interface (`if_output`).
///
/// # Errors
///
/// `ENETDOWN` if it is down, `ENXIO` if it is gone, `ENOBUFS` if its
/// device cannot take it.
pub fn output(vnet: &Arc<Vnet>, index: u16, family: Family, m: Packet) -> Result<(), Errno> {
    let link = {
        let n = vnet.net.lock();
        let i = n.by_index(index).ok_or(Errno::ENXIO)?;
        if i.flags & iff::UP == 0 {
            return Err(Errno::ENETDOWN);
        }
        i.link.0.clone().ok_or(Errno::ENXIO)?
    };
    link.transmit(vnet, index, Some(family), m)
        .map_err(Errno::from)
}

/// `if_vmove`: moves interface `name` of `from` into `to`, down and
/// without its addresses and routes, under the lowest index free there.
///
/// # Errors
///
/// `ENXIO` if there is no such interface, `EEXIST` if `to` has one of
/// that name.
pub fn vmove(from: &Arc<Vnet>, name: &[u8], to: &Arc<Vnet>) -> Result<(), Errno> {
    let index = from.net.lock().by_name(name).ok_or(Errno::ENXIO)?.index;
    if to.net.lock().by_name(name).is_some() {
        return Err(Errno::EEXIST);
    }
    let mut i = from.net.lock().detach(index).ok_or(Errno::ENXIO)?;
    i.flags &= !iff::UP;
    i.addrs.clear();
    let mut n = to.net.lock();
    i.index = n.next_index();
    if let Some(e) = i
        .link
        .0
        .as_ref()
        .and_then(|l| l.as_any().downcast_ref::<Epair>())
    {
        e.moved(to, i.index);
    }
    n.attach(i);
    Ok(())
}

/// `vnet_destroy`'s interface teardown: interfaces from other VNETs go
/// home (`if_vnet_shutdown`); the ones made here are destroyed, an
/// `epair`'s other end with it wherever that is.
pub fn shutdown(vnet: &Arc<Vnet>) {
    let all: Vec<(Vec<u8>, u32)> = vnet
        .net
        .lock()
        .ifaces
        .iter()
        .map(|i| (i.name.as_bytes().to_vec(), i.home))
        .collect();
    for (name, home) in all {
        if home != vnet.id
            && let Some(h) = Vnet::all().into_iter().find(|v| v.id == home)
            && vmove(vnet, &name, &h).is_ok()
        {
            continue;
        }
        if destroy(vnet, &name).is_err() {
            // `lo0`: goes with the VNET.
            let index = vnet.net.lock().by_name(&name).map(|i| i.index);
            if let Some(index) = index {
                vnet.net.lock().detach(index);
            }
        }
    }
}
