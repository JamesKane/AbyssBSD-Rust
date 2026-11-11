// SPDX-License-Identifier: BSD-2-Clause
//! `bridge`: a learning Ethernet bridge between member interfaces
//! (docs/NETWORKING.md §5.5). Each frame a member receives teaches the
//! bridge where its source is; it goes on out of the member its
//! destination was learned on, or out of every other member when that is
//! not known or it is a broadcast. Frames for the bridge's own address
//! (or a member's) are this host's, and arrive on the bridge interface,
//! which carries the addresses.
//!
//! As FreeBSD 15.1's: members of the bridge's VNET of Ethernet type, put
//! in promiscuous mode, their offloads cut to what all share; the
//! forwarding table with its size and timeout, static and sticky
//! entries, per-member address limits, private and span ports, and VLAN
//! filtering (untagged and tagged VLANs per member, Q-in-Q). The spanning
//! tree's parameters are kept and reported as `bridgestp` reports them
//! with no port taking part; ports do not take part (`stp` is refused),
//! and BPDUs are dropped, as a bridge without STP ports drops them.
//!
//! Behavior reference: sys/net/if_bridge.c, sys/net/bridgestp.c
//! (`bstp_attach`, `bstp_create`, `bstp_calc_path_cost`, `bstp_reinit`,
//! the `bstp_set_*` checks).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::bridge::{IFBF_FLUSHALL, Ifbareq, Ifbreq, Ifbropreq, bstp, ifbaf, ifbif, ifbrf};
use abi::errno::Errno;
use abi::net::{IFT_BRIDGE, IFT_ETHER, IFT_GIF, IFT_L2VLAN, LINK_STATE_DOWN, LINK_STATE_UNKNOWN};
use abi::net::{LINK_STATE_UP, ifcap, iff, nd6_iff};
use mbuf::{NoBufs, Packet, flags};
use sync::PreemptLock;

use crate::bpf::{Tap, Way};
use crate::clone::Ctx;
use crate::ether::{self, Header};
use crate::ifnet::{Ifnet, Net, Stats};
use crate::link::{self, Link, LinkRef};
use crate::view::View;
use crate::{Family, Vnet};

/// `BRIDGE_RTABLE_MAX`, `BRIDGE_RTABLE_TIMEOUT`,
/// `BRIDGE_RTABLE_PRUNE_PERIOD`.
pub const RTABLE_MAX: u32 = 2000;
pub const RTABLE_TIMEOUT: u32 = 20 * 60;
pub const PRUNE_PERIOD: u64 = 5 * 60;
/// `BRIDGE_IFCAPS_MASK`, `BRIDGE_IFCAPS_STRIP`.
const IFCAPS_MASK: u32 =
    ifcap::TOE | ifcap::TSO | ifcap::TXCSUM | ifcap::TXCSUM_IPV6 | ifcap::MEXTPG;
const IFCAPS_STRIP: u32 = ifcap::LRO;
/// `DOT1Q_VID_NULL`, `DOT1Q_VID_MAX`, `DOT1Q_VID_RSVD_IMPL`.
pub const VID_NULL: u16 = 0;
pub const VID_MAX: u16 = 4094;
pub const VID_RSVD_IMPL: u16 = 4095;
/// `bstp_etheraddr`.
const BSTP_ADDR: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x00];

/// The `net.link.bridge` settings, each VNET's own (`VNET_DEFINE`).
#[derive(Debug)]
pub struct Conf {
    pub pfil_onlyip: AtomicI32,
    pub pfil_bridge: AtomicI32,
    pub ipfw_arp: AtomicI32,
    pub pfil_member: AtomicI32,
    pub pfil_local_phys: AtomicI32,
    pub log_stp: AtomicI32,
    pub inherit_mac: AtomicI32,
    pub allow_llz_overlap: AtomicI32,
    pub log_mac_flap: AtomicI32,
    pub member_ifaddrs: AtomicI32,
    pub ipfw: AtomicI32,
}

impl Conf {
    /// FreeBSD 15.1's defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pfil_onlyip: AtomicI32::new(1),
            pfil_bridge: AtomicI32::new(0),
            ipfw_arp: AtomicI32::new(0),
            pfil_member: AtomicI32::new(0),
            pfil_local_phys: AtomicI32::new(0),
            log_stp: AtomicI32::new(0),
            inherit_mac: AtomicI32::new(0),
            allow_llz_overlap: AtomicI32::new(0),
            log_mac_flap: AtomicI32::new(1),
            member_ifaddrs: AtomicI32::new(1),
            ipfw: AtomicI32::new(0),
        }
    }

    fn get(a: &AtomicI32) -> bool {
        // Relaxed: a setting.
        a.load(Ordering::Relaxed) != 0
    }
}

impl Default for Conf {
    fn default() -> Self {
        Self::new()
    }
}

/// `VLANTAGOF`: a frame's VLAN, `VID_NULL` if untagged.
const fn vlan_of(m: &Packet) -> u16 {
    if m.hdr.flags & flags::VLANTAG != 0 {
        m.hdr.vlan_tag & 0xfff
    } else {
        VID_NULL
    }
}

/// A VLAN set (`ifbvlan_set_t`), one bit per VLAN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VlanSet(pub [u64; abi::bridge::BRVLAN_WORDS]);

impl VlanSet {
    const EMPTY: Self = Self([0; abi::bridge::BRVLAN_WORDS]);

    #[must_use]
    pub fn test(&self, v: u16) -> bool {
        let v = usize::from(v);
        self.0.get(v / 64).is_some_and(|w| w & (1 << (v % 64)) != 0)
    }
}

/// A member (`struct bridge_iflist` with its `bstp_port`).
struct Member {
    index: u16,
    /// `bif_flags`.
    flags: u32,
    savedcaps: u32,
    addrmax: u32,
    addrcnt: u32,
    addrexceeded: u32,
    pvid: u16,
    vlans: VlanSet,
    vlanproto: u16,
    /// Its port's priority, path cost, and `BSTP_PORT_*` flags.
    priority: u8,
    path_cost: u32,
    stp_flags: u32,
    /// Its device (`bif_ifp->if_transmit`).
    link: Arc<dyn Link>,
}

/// A forwarding table entry (`struct bridge_rtnode`): where an address
/// was seen, until when, and of what type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RtNode {
    member: u16,
    expire: u64,
    flags: u8,
    /// Its place in the list (`sc_rtlist`, newest first).
    seq: u64,
}

/// The bridge's state.
struct State {
    /// Members, newest first (`CK_LIST_INSERT_HEAD`).
    members: Vec<Member>,
    /// Span ports, newest first.
    spans: Vec<(u16, Arc<dyn Link>)>,
    /// The forwarding table by address and VLAN.
    rt: BTreeMap<([u8; 6], u16), RtNode>,
    seq: u64,
    brtmax: u32,
    brttimeout: u32,
    brtexceeded: u32,
    /// `sc_flags`, `sc_defpvid`.
    flags: u32,
    defpvid: u16,
    /// The member whose address the bridge took (`sc_ifaddr`).
    ifaddr: Option<u16>,
    /// It runs (`IFF_DRV_RUNNING`), and prunes its table next then.
    running: bool,
    next_prune: u64,
    /// The spanning tree's parameters (`struct bstp_state`): times in
    /// seconds; the identifiers are set when it first starts.
    priority: u16,
    htime: u8,
    fdelay: u8,
    maxage: u8,
    holdcount: u8,
    protover: u8,
    reinit: bool,
    last_tc: (i64, i64),
    /// `log_mac_flap`'s rate limit (`ppsratecheck`).
    log_sec: u64,
    log_count: u32,
}

/// A bridge (`struct bridge_softc`).
pub struct Bridge {
    pub unit: i32,
    /// Itself, for the members it gives its `Arc`.
    me: Weak<Bridge>,
    stats: Arc<Stats>,
    tap: Arc<Tap>,
    /// `sc_defaddr`.
    defaddr: [u8; 6],
    st: PreemptLock<State>,
}

impl core::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bridge")
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

/// Every bridge, for the pruning tick.
static ALL: PreemptLock<Vec<Weak<Bridge>>> = PreemptLock::new(Vec::new());

/// A frame to send out of a member, decided under the bridge's lock and
/// sent after it.
struct Out {
    index: u16,
    link: Arc<dyn Link>,
    m: Packet,
}

/// One frame's pass through the bridge: what it decided to send, and
/// what to log, once the lock goes.
struct Pass<'a> {
    view: &'a View,
    now: u64,
    outs: Vec<Out>,
    flap: Option<Flap>,
}

/// A note to log once the lock is let go (`log_mac_flap`).
struct Flap {
    addr: [u8; 6],
    vlan: u16,
    from: u16,
    to: u16,
}

/// What a member's capabilities and flags are, as the view or the
/// interfaces show them.
#[derive(Clone, Copy)]
struct Iface {
    flags: u32,
    capenable: u32,
    lladdr: Option<[u8; 6]>,
}

impl Bridge {
    /// Its interface's VNET and index.
    #[must_use]
    pub fn home(&self) -> (Option<Arc<Vnet>>, u16) {
        self.tap.home()
    }

    /// Itself, shared.
    #[must_use]
    pub fn arc(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }

    /// Whether interface `index` is a member.
    #[must_use]
    pub fn has_member(&self, index: u16) -> bool {
        self.st.lock().members.iter().any(|b| b.index == index)
    }

    // ---- The forwarding table ----

    /// `bridge_rtnode_lookup`: `addr`'s entry on `vlan`, any VLAN's for
    /// `VID_RSVD_IMPL`.
    fn rt_find(st: &State, addr: &[u8; 6], vlan: u16) -> Option<(u16, RtNode)> {
        if vlan == VID_RSVD_IMPL {
            return st
                .rt
                .range((*addr, 0)..=(*addr, u16::MAX))
                .next()
                .map(|((_, v), n)| (*v, *n));
        }
        st.rt.get(&(*addr, vlan)).map(|n| (vlan, *n))
    }

    fn member_mut(st: &mut State, index: u16) -> Option<&mut Member> {
        st.members.iter_mut().find(|m| m.index == index)
    }

    /// `bridge_rtupdate`: `dst` on `vlan` was seen behind member `index`
    /// (or is set there by hand, `set` with `flags`).
    fn rt_update(
        st: &mut State,
        dst: &[u8; 6],
        vlan: u16,
        index: u16,
        set: Option<u8>,
        now: u64,
        flap: &mut Option<Flap>,
    ) -> Result<(), Errno> {
        if ether::is_multicast(dst) || *dst == [0; 6] {
            return Err(Errno::EINVAL);
        }
        let key = (*dst, vlan);
        if !st.rt.contains_key(&key) {
            if st.rt.len() >= st.brtmax as usize {
                st.brtexceeded += 1;
                return Err(Errno::ENOSPC);
            }
            let b = Self::member_mut(st, index).ok_or(Errno::ENOENT)?;
            if b.addrmax != 0 && b.addrcnt >= b.addrmax {
                b.addrexceeded += 1;
                return Err(Errno::ENOSPC);
            }
            b.addrcnt += 1;
            let f = if b.flags & ifbif::STICKY != 0 {
                ifbaf::STICKY
            } else {
                ifbaf::DYNAMIC
            };
            st.seq += 1;
            let seq = st.seq;
            st.rt.insert(
                key,
                RtNode {
                    member: index,
                    expire: 0,
                    flags: f,
                    seq,
                },
            );
        }
        let timeout = u64::from(st.brttimeout);
        let Some(n) = st.rt.get(&key).copied() else {
            return Ok(());
        };
        let mut n2 = n;
        if n.flags & ifbaf::TYPEMASK == ifbaf::DYNAMIC && n.member != index {
            if let Some(o) = Self::member_mut(st, n.member) {
                o.addrcnt = o.addrcnt.saturating_sub(1);
            }
            if let Some(b) = Self::member_mut(st, index) {
                b.addrcnt += 1;
            }
            *flap = Some(Flap {
                addr: *dst,
                vlan,
                from: n.member,
                to: index,
            });
            n2.member = index;
        }
        if set.unwrap_or(ifbaf::DYNAMIC) & ifbaf::TYPEMASK == ifbaf::DYNAMIC {
            n2.expire = now + timeout;
        }
        if let Some(f) = set {
            n2.flags = f;
        }
        st.rt.insert(key, n2);
        Ok(())
    }

    /// `bridge_rtnode_destroy` of every entry `keep` refuses.
    fn rt_retain(st: &mut State, mut keep: impl FnMut(&RtNode) -> bool) {
        let gone: Vec<_> = st
            .rt
            .iter()
            .filter(|(_, n)| !keep(n))
            .map(|(k, n)| (*k, n.member))
            .collect();
        for (k, member) in gone {
            st.rt.remove(&k);
            if let Some(b) = Self::member_mut(st, member) {
                b.addrcnt = b.addrcnt.saturating_sub(1);
            }
        }
    }

    fn dynamic(n: &RtNode) -> bool {
        n.flags & ifbaf::TYPEMASK == ifbaf::DYNAMIC
    }

    /// `bridge_rtage`.
    fn rt_age(st: &mut State, now: u64) {
        Self::rt_retain(st, |n| !Self::dynamic(n) || now < n.expire);
    }

    /// `bridge_rttrim`: down to the table's size, aging first, then
    /// dropping dynamic entries oldest-listed last.
    fn rt_trim(st: &mut State, now: u64) {
        if st.rt.len() <= st.brtmax as usize {
            return;
        }
        Self::rt_age(st, now);
        while st.rt.len() > st.brtmax as usize {
            // `sc_rtlist` is newest first; FreeBSD walks it from the head.
            let Some(k) = st
                .rt
                .iter()
                .filter(|(_, n)| Self::dynamic(n))
                .max_by_key(|(_, n)| n.seq)
                .map(|(k, _)| *k)
            else {
                return;
            };
            if let Some(n) = st.rt.remove(&k)
                && let Some(b) = Self::member_mut(st, n.member)
            {
                b.addrcnt = b.addrcnt.saturating_sub(1);
            }
        }
    }

    /// `bridge_timer`'s pruning, once a prune period has passed since the
    /// bridge started (call every second).
    fn tick(&self, now: u64) {
        let mut st = self.st.lock();
        if st.running && now >= st.next_prune {
            Self::rt_age(&mut st, now);
            st.next_prune = now + PRUNE_PERIOD;
        }
    }

    // ---- The packet paths ----

    /// The flags, capabilities, and address of interface `index`.
    fn iface(view: &View, index: u16) -> Option<Iface> {
        view.by_index(index).map(|i| Iface {
            flags: i.flags,
            capenable: i.capenable,
            lladdr: i.lladdr,
        })
    }

    fn running(view: &View, index: u16) -> bool {
        Self::iface(view, index).is_some_and(|i| i.flags & iff::RUNNING != 0)
    }

    /// `bridge_vfilter_in`: whether member `b` takes frame `m`, which may
    /// gain its untagged VLAN.
    fn vfilter_in(st: &State, b: &Member, m: &mut Packet, ety: u16) -> bool {
        let vlan = vlan_of(m);
        if vlan > VID_MAX {
            return false;
        }
        if st.flags & ifbrf::VLANFILTER == 0 {
            if vlan == VID_NULL && b.pvid != VID_NULL {
                m.hdr.vlan_tag = b.pvid;
                m.hdr.flags |= flags::VLANTAG;
            }
            return true;
        }
        if b.flags & ifbif::QINQ == 0 && (ety == ether::ty::VLAN || ety == ether::ty::QINQ) {
            return false;
        }
        if vlan == VID_NULL {
            if b.pvid == VID_NULL {
                return false;
            }
            m.hdr.vlan_tag = b.pvid;
            m.hdr.flags |= flags::VLANTAG;
            true
        } else {
            b.vlans.test(vlan)
        }
    }

    /// `bridge_vfilter_out`: whether frame `m` may leave by member `b`.
    fn vfilter_out(st: &State, b: &Member, m: &Packet, dst: &[u8; 6]) -> bool {
        if st.flags & ifbrf::VLANFILTER == 0 {
            return true;
        }
        let vlan = vlan_of(m);
        if vlan == VID_NULL && *dst == BSTP_ADDR {
            return true;
        }
        if vlan == VID_NULL {
            return false;
        }
        vlan == b.pvid || b.vlans.test(vlan)
    }

    /// `bridge_enqueue`'s decisions for member `b`: the filter, then the
    /// tag taken off for its untagged VLAN, or put in the frame where the
    /// member cannot or the protocol is not 802.1Q.
    fn prepare(st: &State, b: &Member, view: &View, mut m: Packet, dst: &[u8; 6]) -> Option<Out> {
        if !Self::vfilter_out(st, b, &m, dst) {
            return None;
        }
        if b.pvid != VID_NULL && vlan_of(&m) == b.pvid {
            m.hdr.flags &= !flags::VLANTAG;
            m.hdr.vlan_tag = 0;
        }
        let hw =
            Self::iface(view, b.index).is_some_and(|i| i.capenable & ifcap::VLAN_HWTAGGING != 0);
        if m.hdr.flags & flags::VLANTAG != 0 && (!hw || b.vlanproto != ether::ty::VLAN) {
            let tag = m.hdr.vlan_tag;
            if vlan_encap(&mut m, tag, b.vlanproto).is_err() {
                return None;
            }
            m.hdr.flags &= !flags::VLANTAG;
        }
        Some(Out {
            index: b.index,
            link: Arc::clone(&b.link),
            m,
        })
    }

    /// Sends what was decided, counting it on the bridge.
    fn send(&self, vnet: &Arc<Vnet>, outs: Vec<Out>) {
        for o in outs {
            let len = o.m.len() as u64;
            let mcast = o.m.hdr.flags & flags::MCAST != 0;
            match o.link.transmit(vnet, o.index, None, o.m) {
                Ok(()) => {
                    self.stats.opackets.add(1);
                    self.stats.obytes.add(len);
                    if mcast {
                        self.stats.omcasts.add(1);
                    }
                }
                Err(_) => self.stats.oerrors.add(1),
            }
        }
    }

    /// `bridge_span`: a copy for every running span port.
    fn span(st: &State, view: &View, m: &Packet, outs: &mut Vec<Out>) {
        for (index, link) in &st.spans {
            if !Self::running(view, *index) {
                continue;
            }
            if let Ok(c) = m.copy() {
                outs.push(Out {
                    index: *index,
                    link: Arc::clone(link),
                    m: c,
                });
            }
        }
    }

    /// `bridge_broadcast`: copies out of every member but `src`, as its
    /// flags allow.
    fn broadcast(
        st: &State,
        view: &View,
        src: Option<u16>,
        m: Packet,
        dst: &[u8; 6],
        outs: &mut Vec<Out>,
    ) {
        let sflags = src
            .and_then(|s| st.members.iter().find(|b| b.index == s))
            .map(|b| b.flags);
        let mcast = m.hdr.flags & (flags::BCAST | flags::MCAST) != 0;
        let targets: Vec<&Member> = st
            .members
            .iter()
            .filter(|b| Some(b.index) != src)
            .filter(|b| sflags.is_none_or(|f| f & b.flags & ifbif::PRIVATE == 0))
            .filter(|b| b.flags & ifbif::DISCOVER != 0 || mcast)
            .filter(|b| Self::running(view, b.index))
            .collect();
        let mut m = Some(m);
        let last = targets.len().saturating_sub(1);
        for (k, b) in targets.iter().enumerate() {
            let c = if k == last {
                m.take()
            } else {
                m.as_ref().and_then(|m| m.copy().ok())
            };
            if let Some(c) = c
                && let Some(o) = Self::prepare(st, b, view, c, dst)
            {
                outs.push(o);
            }
        }
    }

    /// `bridge_forward`: frame `m` from member `src`, learned from, out
    /// of where its destination is or everywhere, as `p` collects.
    fn forward(&self, st: &mut State, p: &mut Pass<'_>, src: u16, m: Packet, eh: &Header) {
        let (view, now) = (p.view, p.now);
        let (outs, flap) = (&mut p.outs, &mut p.flap);
        self.stats.ipackets.add(1);
        self.stats.ibytes.add(m.len() as u64);
        let vlan = vlan_of(&m);
        let Some(sb) = st.members.iter().find(|b| b.index == src) else {
            return;
        };
        let (sflags, addrmax) = (sb.flags, sb.addrmax);
        if sflags & ifbif::LEARNING != 0
            && Self::rt_update(st, &eh.src, vlan, src, None, now, flap).is_err()
            && addrmax != 0
        {
            return;
        }
        let multi = m.hdr.flags & (flags::BCAST | flags::MCAST) != 0;
        let dst = if multi {
            // 802.1D §7.12.6's reserved addresses are not forwarded.
            if eh.dst[..5] == BSTP_ADDR[..5] && eh.dst[5] <= 0x0f {
                return;
            }
            self.stats.imcasts.add(1);
            None
        } else {
            let d = Self::rt_find(st, &eh.dst, vlan).map(|(_, n)| n.member);
            if d == Some(src) {
                return;
            }
            d
        };
        if dst.is_some() || !multi {
            self.tap.mtap(&m, Way::In);
        }
        let Some(d) = dst else {
            Self::broadcast(st, view, Some(src), m, &eh.dst, outs);
            return;
        };
        if !Self::running(view, d) {
            return;
        }
        let Some(db) = st.members.iter().find(|b| b.index == d) else {
            return;
        };
        if sflags & db.flags & ifbif::PRIVATE != 0 {
            return;
        }
        if let Some(o) = Self::prepare(st, db, view, m, &eh.dst) {
            outs.push(o);
        }
    }

    /// `bridge_input`: frame `m` (header `eh`) arrived on member
    /// `m.hdr.rcvif`. What this host takes comes back, its receiving
    /// interface the one it is for; the rest is forwarded here.
    pub fn input(
        &self,
        vnet: &Arc<Vnet>,
        view: &View,
        mut m: Packet,
        eh: &Header,
    ) -> Option<Packet> {
        let src = m.hdr.rcvif;
        let (_, bindex) = self.home();
        let bif = Self::iface(view, bindex)?;
        if bif.flags & iff::RUNNING == 0 {
            return Some(m);
        }
        if bif.flags & iff::MONITOR != 0 {
            m.hdr.rcvif = bindex;
            self.tap.mtap(&m, Way::In);
            self.stats.ipackets.add(1);
            self.stats.ibytes.add(m.len() as u64);
            return None;
        }
        let conf = vnet.bridge.member_ifaddrs.load(Ordering::Relaxed) != 0;
        let mut p = Pass {
            view,
            now: crate::uptime(),
            outs: Vec::new(),
            flap: None,
        };
        let ret = self.claim(&mut p, src, bindex, bif.lladdr, conf, m, eh);
        self.send(vnet, p.outs);
        self.log_flap(vnet, p.flap);
        let (m, up) = ret?;
        if let Some(c) = up {
            ether::input(vnet, c);
        }
        Some(m)
    }

    /// [`Bridge::input`]'s decisions under the lock: the frame for this
    /// host (and a copy of a broadcast for the bridge), if any.
    #[allow(clippy::too_many_arguments, reason = "one frame's facts")]
    fn claim(
        &self,
        p: &mut Pass<'_>,
        src: u16,
        bindex: u16,
        blladdr: Option<[u8; 6]>,
        member_ifaddrs: bool,
        mut m: Packet,
        eh: &Header,
    ) -> Option<(Packet, Option<Packet>)> {
        let view = p.view;
        let mut st = self.st.lock();
        let b = st.members.iter().find(|b| b.index == src)?;
        if !Self::vfilter_in(&st, b, &mut m, eh.ty) {
            self.stats.ierrors.add(1);
            return None;
        }
        let vlan = vlan_of(&m);
        Self::span(&st, view, &m, &mut p.outs);
        if m.hdr.flags & (flags::BCAST | flags::MCAST) != 0 {
            // BPDUs go to the spanning tree, which no port takes part in
            // (`bstp_input` of an inactive port).
            if eh.dst == BSTP_ADDR {
                return None;
            }
            if let Ok(c) = m.copy() {
                self.forward(&mut st, p, src, c, eh);
            }
            // A copy arrives on the bridge too, where its addresses can
            // claim it; the original stays the member's.
            let up = m.copy().ok().map(|mut c| {
                c.hdr.rcvif = bindex;
                c
            });
            return Some((m, up));
        }
        // `GRAB_OUR_PACKETS`: for this interface's address (learning
        // where it came from), or from it (a frame of ours come back).
        let now = p.now;
        let flap = &mut p.flap;
        let mut grab = |st: &mut State, iface: u16, ll: Option<[u8; 6]>| {
            let ll = ll?;
            if ll == eh.dst {
                let (lf, max) = st
                    .members
                    .iter()
                    .find(|b| b.index == src)
                    .map_or((0, 0), |b| (b.flags, b.addrmax));
                if lf & ifbif::LEARNING != 0
                    && Self::rt_update(st, &eh.src, vlan, src, None, now, flap).is_err()
                    && max != 0
                {
                    return Some(None);
                }
                return Some(Some(iface));
            }
            if ll == eh.src {
                return Some(None);
            }
            None
        };
        let mut hit = grab(&mut st, bindex, blladdr);
        if hit.is_none() && member_ifaddrs {
            let me = Self::iface(view, src).and_then(|i| i.lladdr);
            hit = grab(&mut st, src, me);
            if hit.is_none() {
                let others: Vec<u16> = st.members.iter().map(|b| b.index).collect();
                for o in others {
                    if view.by_index(o).is_some_and(|i| i.ty == IFT_GIF) {
                        continue;
                    }
                    let ll = Self::iface(view, o).and_then(|i| i.lladdr);
                    hit = grab(&mut st, o, ll);
                    if hit.is_some() {
                        break;
                    }
                }
            }
        }
        match hit {
            Some(None) => None,
            Some(Some(to)) => {
                m.hdr.rcvif = to;
                if to != src {
                    // Passing over or to the bridge, locally.
                    self.tap.mtap(&m, Way::In);
                    self.stats.ipackets.add(1);
                    self.stats.ibytes.add(m.len() as u64);
                    if to != bindex
                        && let Some(t) = view.by_index(to).and_then(|i| i.bpf.as_ref())
                    {
                        t.mtap(&m, Way::In);
                    }
                    // Tagged frames are `vlan(4)`'s, and there is none.
                    if vlan_of(&m) != VID_NULL {
                        return None;
                    }
                }
                Some((m, None))
            }
            None => {
                self.forward(&mut st, p, src, m, eh);
                None
            }
        }
    }

    /// `bridge_output`: a frame this host sends out of member `src`,
    /// bridged: out of where its destination is, or every member.
    pub fn output(&self, vnet: &Arc<Vnet>, src: u16, m: Packet) {
        let section = crate::view::enter();
        let view = vnet.net.view(&section);
        let Ok(eh) = header(&m) else {
            return;
        };
        let (_, bindex) = self.home();
        let mut outs = Vec::new();
        {
            let st = self.st.lock();
            let brun = Self::running(view, bindex);
            let dst = if !brun {
                Some(src)
            } else if ether::is_multicast(&eh.dst) {
                None
            } else {
                Self::rt_find(&st, &eh.dst, vlan_of(&m)).map(|(_, n)| n.member)
            };
            if brun && dst != Some(src) {
                self.tap.mtap(&m, Way::Out);
            }
            match dst {
                None => {
                    Self::span(&st, view, &m, &mut outs);
                    let targets: Vec<&Member> = st
                        .members
                        .iter()
                        .filter(|b| view.by_index(b.index).is_none_or(|i| i.ty != IFT_GIF))
                        .filter(|b| Self::running(view, b.index))
                        .collect();
                    let mut m = Some(m);
                    let last = targets.len().saturating_sub(1);
                    for (k, b) in targets.iter().enumerate() {
                        let c = if k == last {
                            m.take()
                        } else {
                            m.as_ref().and_then(|m| m.copy().ok())
                        };
                        if let Some(c) = c
                            && let Some(o) = Self::prepare(&st, b, view, c, &eh.dst)
                        {
                            outs.push(o);
                        }
                    }
                }
                Some(d) => {
                    Self::span(&st, view, &m, &mut outs);
                    if Self::running(view, d)
                        && let Some(b) = st.members.iter().find(|b| b.index == d)
                        && let Some(o) = Self::prepare(&st, b, view, m, &eh.dst)
                    {
                        outs.push(o);
                    }
                }
            }
        }
        drop(section);
        self.send(vnet, outs);
    }

    /// Logs a MAC address seen moving between members (`log_mac_flap`, at
    /// most five a second).
    fn log_flap(&self, vnet: &Arc<Vnet>, flap: Option<Flap>) {
        let Some(f) = flap else {
            return;
        };
        if !Conf::get(&vnet.bridge.log_mac_flap) {
            return;
        }
        let now = crate::uptime();
        {
            let mut st = self.st.lock();
            if st.log_sec != now {
                st.log_sec = now;
                st.log_count = 0;
            }
            st.log_count += 1;
            if st.log_count > 5 {
                return;
            }
        }
        let n = vnet.net.lock();
        let name = |i: u16| n.by_index(i).map(|i| i.name.clone()).unwrap_or_default();
        let (_, me) = self.home();
        let a = f.addr;
        crate::log(
            5,
            &format!(
                "{}: mac address {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} vlan {} moved from {} to {}\n",
                name(me),
                a[0],
                a[1],
                a[2],
                a[3],
                a[4],
                a[5],
                f.vlan,
                name(f.from),
                name(f.to)
            ),
        );
    }

    // ---- Members ----

    /// `bridge_linkcheck`: up while a member's link is, or with members
    /// none of which reports its link; down otherwise.
    pub(crate) fn linkcheck(&self, n: &mut Net) {
        let (_, me) = self.home();
        let members: Vec<u16> = self.st.lock().members.iter().map(|b| b.index).collect();
        let mut state = LINK_STATE_DOWN;
        let mut hasls = false;
        for i in &members {
            if let Some(ifp) = n.by_index(*i) {
                if ifp.capabilities & ifcap::LINKSTATE != 0 {
                    hasls = true;
                }
                if ifp.link_state == LINK_STATE_UP {
                    state = LINK_STATE_UP;
                    break;
                }
            }
        }
        if !members.is_empty() && !hasls {
            state = LINK_STATE_UP;
        }
        n.link_state_change(me, state);
    }

    /// `bridge_mutecaps`: every member's offloads cut to those all of
    /// them had.
    fn mutecaps(&self, n: &mut Net) {
        let (mask, members) = {
            let st = self.st.lock();
            let mask = st.members.iter().fold(IFCAPS_MASK, |m, b| m & b.savedcaps);
            (mask, st.members.iter().map(|b| b.index).collect::<Vec<_>>())
        };
        for i in members {
            let Some(cur) = n.by_index(i).map(|x| x.capenable) else {
                continue;
            };
            let want = (cur & !IFCAPS_STRIP & !IFCAPS_MASK) | mask;
            set_caps(n, i, want);
        }
    }

    /// The member named `name`'s index.
    fn lookup(&self, n: &Net, name: &[u8]) -> Result<u16, Errno> {
        let i = n.by_name(name).ok_or(Errno::ENOENT)?.index;
        if self.has_member(i) {
            Ok(i)
        } else {
            Err(Errno::ENOENT)
        }
    }

    /// `BRDGADD` (`bridge_ioctl_add`): interface `name` joins.
    ///
    /// # Errors
    ///
    /// `ENOENT` for no such interface, `EBUSY` if it is a span port or
    /// another bridge's, `EEXIST` if it is this one's, `EINVAL` for one
    /// not Ethernet, with addresses where they are not allowed, or whose
    /// MTU cannot be the bridge's.
    pub fn add(self: &Arc<Self>, vnet: &Arc<Vnet>, n: &mut Net, name: &[u8]) -> Result<(), Errno> {
        let (_, me) = self.home();
        let ifs = n.by_name(name).ok_or(Errno::ENOENT)?;
        let (index, ty, mtu, capenable, lladdr) =
            (ifs.index, ifs.ty, ifs.mtu, ifs.capenable, ifs.lladdr);
        let link = ifs.link.0.clone().ok_or(Errno::EINVAL)?;
        if self.st.lock().spans.iter().any(|(i, _)| *i == index) {
            return Err(Errno::EBUSY);
        }
        if let Some(b) = &ifs.bridge {
            return Err(if core::ptr::eq(Arc::as_ptr(b), Arc::as_ptr(self)) {
                Errno::EEXIST
            } else {
                Errno::EBUSY
            });
        }
        if !matches!(ty, IFT_ETHER | IFT_L2VLAN) {
            return Err(Errno::EINVAL);
        }
        // Link-local scope zones must not overlap: with one on the
        // bridge, members lose their IPv6 addresses.
        if !Conf::get(&vnet.bridge.allow_llz_overlap) && has_ll6(n, me) {
            let members: Vec<u16> = self.st.lock().members.iter().map(|b| b.index).collect();
            for i in members.into_iter().chain([index]) {
                if has_ll6(n, i) {
                    in6_ifdetach(n, i);
                    let (bn, mn) = (name_of(n, me), name_of(n, i));
                    crate::log(
                        6,
                        &format!(
                            "{bn}: IPv6 addresses on {mn} have been removed before adding it as a member to prevent IPv6 address scope violation.\n"
                        ),
                    );
                }
            }
        }
        let has_addr = n.by_index(index).is_some_and(|i| !i.addrs.is_empty());
        if has_addr {
            if !Conf::get(&vnet.bridge.member_ifaddrs) {
                return Err(Errno::EINVAL);
            }
            let (bn, mn) = (name_of(n, me), name_of(n, index));
            crate::log(
                6,
                &format!(
                    "{bn}: WARNING: Adding member interface {mn} which has an IP address assigned is deprecated and will be unsupported in a future release.\n"
                ),
            );
        }
        // The first member sets the bridge's MTU; the others take it.
        let first = self.st.lock().members.is_empty();
        if first {
            let bname = name_of(n, me);
            n.set_mtu(bname.as_bytes(), mtu)?;
        } else {
            let bmtu = n.by_index(me).map_or(mtu, |i| i.mtu);
            if bmtu != mtu {
                if !(72..=65535).contains(&bmtu) {
                    return Err(Errno::EINVAL);
                }
                n.set_mtu(name, bmtu)?;
            }
        }
        let (stp_flags, path_cost) = {
            let ifp = n.by_index(index);
            let state = ifp.map_or(LINK_STATE_UNKNOWN, |i| i.link_state);
            let baud = ifp.map_or(0, |i| i.baudrate);
            port_cost(state, baud)
        };
        {
            let mut st = self.st.lock();
            let flags = ifbif::LEARNING
                | ifbif::DISCOVER
                | if st.flags & ifbrf::DEFQINQ != 0 {
                    ifbif::QINQ
                } else {
                    0
                };
            let pvid = st.defpvid;
            st.members.insert(
                0,
                Member {
                    index,
                    flags,
                    savedcaps: capenable,
                    addrmax: 0,
                    addrcnt: 0,
                    addrexceeded: 0,
                    pvid,
                    vlans: VlanSet::EMPTY,
                    vlanproto: ether::ty::VLAN,
                    priority: bstp::DEFAULT_PORT_PRIORITY,
                    path_cost,
                    stp_flags,
                    link,
                },
            );
        }
        // `inherit_mac`: the first member's address, while the bridge has
        // its own still.
        if Conf::get(&vnet.bridge.inherit_mac)
            && first
            && n.by_index(me).and_then(|i| i.lladdr) == Some(self.defaddr)
            && let Some(ll) = lladdr
        {
            n.with_index(me, |i| i.lladdr = Some(ll));
            self.st.lock().ifaddr = Some(index);
        }
        n.with_index(index, |i| i.bridge = Some(Arc::clone(self)));
        self.mutecaps(n);
        self.linkcheck(n);
        if let Err(e) = n.promisc(index, true) {
            self.delete(n, index, false);
            return Err(e);
        }
        Ok(())
    }

    /// `bridge_delete_member`: member `index` leaves; unless it is going
    /// (`gone`), it is no longer promiscuous and gets its offloads back.
    pub(crate) fn delete(&self, n: &mut Net, index: u16, gone: bool) {
        let (_, me) = self.home();
        let Some(b) = ({
            let mut st = self.st.lock();
            let at = st.members.iter().position(|b| b.index == index);
            at.map(|at| st.members.remove(at))
        }) else {
            return;
        };
        n.with_index(index, |i| i.bridge = None);
        let inherit = n
            .owner
            .upgrade()
            .is_some_and(|v| Conf::get(&v.bridge.inherit_mac));
        let from_it = self.st.lock().ifaddr == Some(index);
        if inherit && from_it {
            let next = self.st.lock().members.first().map(|b| b.index);
            let ll = match next {
                Some(f) => n.by_index(f).and_then(|i| i.lladdr),
                None => Some(self.defaddr),
            };
            n.with_index(me, |i| i.lladdr = ll);
            self.st.lock().ifaddr = next;
        }
        self.linkcheck(n);
        self.mutecaps(n);
        {
            let mut st = self.st.lock();
            st.rt.retain(|_, r| r.member != index);
        }
        if !gone {
            let promisc = n
                .by_index(index)
                .is_some_and(|i| i.flags & iff::PROMISC != 0);
            if promisc {
                let _ = n.promisc(index, false);
            }
            set_caps(n, index, b.savedcaps);
        }
    }

    /// Every member and span port leaves (`bridge_clone_destroy`,
    /// `bridge_reassign`).
    pub(crate) fn delete_all(&self, n: &mut Net) {
        let members: Vec<u16> = self.st.lock().members.iter().map(|b| b.index).collect();
        for i in members {
            self.delete(n, i, false);
        }
        let mut st = self.st.lock();
        st.spans.clear();
        st.rt.clear();
        st.running = false;
    }

    /// `bridge_ifdetach`: interface `index` is leaving its VNET; as a
    /// member or span port it leaves the bridge.
    pub(crate) fn departed(&self, n: &mut Net, index: u16) {
        if self.has_member(index) {
            self.delete(n, index, true);
        }
        self.st.lock().spans.retain(|(i, _)| *i != index);
    }

    /// `bridge_ioctl`'s `SIOCSIFMTU`: every member takes the bridge's new
    /// MTU (the caller sets the bridge's).
    ///
    /// # Errors
    ///
    /// `EINVAL` below `IF_MINMTU`.
    pub fn set_mtu(&self, n: &mut Net, mtu: u32) -> Result<(), Errno> {
        if mtu < 72 {
            return Err(Errno::EINVAL);
        }
        let members: Vec<u16> = self.st.lock().members.iter().map(|b| b.index).collect();
        for i in members {
            let name = name_of(n, i);
            n.set_mtu(name.as_bytes(), mtu)?;
        }
        Ok(())
    }

    /// `BRDGDEL`.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member.
    pub fn del(&self, n: &mut Net, name: &[u8]) -> Result<(), Errno> {
        let i = self.lookup(n, name)?;
        self.delete(n, i, false);
        Ok(())
    }

    /// `BRDGADDS`: interface `name` gets a copy of every frame.
    ///
    /// # Errors
    ///
    /// `ENOENT` for no such interface, `EBUSY` if it is one already,
    /// `EEXIST` if it is a member, `EINVAL` for one not Ethernet.
    pub fn add_span(&self, n: &Net, name: &[u8]) -> Result<(), Errno> {
        let ifs = n.by_name(name).ok_or(Errno::ENOENT)?;
        let index = ifs.index;
        if self.st.lock().spans.iter().any(|(i, _)| *i == index) {
            return Err(Errno::EBUSY);
        }
        if ifs.bridge.is_some() {
            return Err(Errno::EEXIST);
        }
        if !matches!(ifs.ty, IFT_ETHER | IFT_L2VLAN) {
            return Err(Errno::EINVAL);
        }
        let link = ifs.link.0.clone().ok_or(Errno::EINVAL)?;
        self.st.lock().spans.insert(0, (index, link));
        Ok(())
    }

    /// `BRDGDELS`.
    ///
    /// # Errors
    ///
    /// `ENOENT` for no such interface or span port.
    pub fn del_span(&self, n: &Net, name: &[u8]) -> Result<(), Errno> {
        let index = n.by_name(name).ok_or(Errno::ENOENT)?.index;
        let mut st = self.st.lock();
        let at = st
            .spans
            .iter()
            .position(|(i, _)| *i == index)
            .ok_or(Errno::ENOENT)?;
        st.spans.remove(at);
        Ok(())
    }

    /// A member's record (`bridge_ioctl_gifflags`).
    fn member_req(b: &Member, name: &str) -> Ifbreq {
        let mut r = Ifbreq::default();
        put_name(&mut r.ifbr_ifsname, name);
        r.ifbr_ifsflags = b.flags;
        if b.stp_flags & bstp::PORT_AUTOEDGE != 0 {
            r.ifbr_ifsflags |= ifbif::BSTP_AUTOEDGE;
        }
        if b.stp_flags & bstp::PORT_AUTOPTP != 0 {
            r.ifbr_ifsflags |= ifbif::BSTP_AUTOPTP;
        }
        if b.stp_flags & bstp::PORT_ADMCOST != 0 {
            r.ifbr_ifsflags |= ifbif::BSTP_ADMCOST;
        }
        r.ifbr_state = bstp::IFSTATE_DISCARDING;
        r.ifbr_priority = b.priority;
        r.ifbr_path_cost = b.path_cost;
        r.ifbr_portno = u8::try_from(b.index & 0xfff).unwrap_or(0);
        r.ifbr_proto = bstp::PROTO_RSTP;
        r.ifbr_role = bstp::ROLE_DISABLED;
        r.ifbr_stpflags = b.stp_flags;
        r.ifbr_addrcnt = b.addrcnt;
        r.ifbr_addrmax = b.addrmax;
        r.ifbr_addrexceeded = b.addrexceeded;
        r.ifbr_pvid = b.pvid;
        r.ifbr_vlanproto = b.vlanproto;
        r
    }

    /// `BRDGGIFFLGS`.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member.
    pub fn member(&self, n: &Net, name: &[u8]) -> Result<Ifbreq, Errno> {
        let i = self.lookup(n, name)?;
        let st = self.st.lock();
        let b = st
            .members
            .iter()
            .find(|b| b.index == i)
            .ok_or(Errno::ENOENT)?;
        Ok(Self::member_req(b, &name_of(n, i)))
    }

    /// `BRDGGIFS`: every member's record, then every span port's.
    #[must_use]
    pub fn members(&self, n: &Net) -> Vec<Ifbreq> {
        let st = self.st.lock();
        let mut out: Vec<Ifbreq> = st
            .members
            .iter()
            .map(|b| Self::member_req(b, &name_of(n, b.index)))
            .collect();
        for (i, _) in &st.spans {
            let mut r = Ifbreq::default();
            put_name(&mut r.ifbr_ifsname, &name_of(n, *i));
            r.ifbr_ifsflags = ifbif::SPAN;
            r.ifbr_portno = u8::try_from(i & 0xfff).unwrap_or(0);
            out.push(r);
        }
        out
    }

    /// `BRDGSIFFLGS`.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member, `EINVAL` to set `SPAN`,
    /// `EOPNOTSUPP` to turn the spanning tree on, which is not here.
    pub fn set_member_flags(&self, n: &Net, name: &[u8], f: u32) -> Result<(), Errno> {
        let i = self.lookup(n, name)?;
        if f & ifbif::SPAN != 0 {
            return Err(Errno::EINVAL);
        }
        if f & ifbif::STP != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let mut st = self.st.lock();
        let b = Self::member_mut(&mut st, i).ok_or(Errno::ENOENT)?;
        // `bstp_set_autoedge` and `bstp_set_autoptp`.
        b.stp_flags &= !(bstp::PORT_AUTOEDGE | bstp::PORT_AUTOPTP);
        if f & ifbif::BSTP_AUTOEDGE != 0 {
            b.stp_flags |= bstp::PORT_AUTOEDGE;
        }
        if f & ifbif::BSTP_AUTOPTP != 0 {
            b.stp_flags |= bstp::PORT_AUTOPTP;
        }
        b.flags = f & ifbif::MASK;
        Ok(())
    }

    /// The member's record, changed by `f` (`BRDGSIFPRIO`, `BRDGSIFCOST`,
    /// `BRDGSIFAMAX`, `BRDGSIFPVID`, `BRDGSIFVLANSET`,
    /// `BRDGSIFVLANPROTO`).
    fn with_member<R>(
        &self,
        n: &Net,
        name: &[u8],
        f: impl FnOnce(&mut Member) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        let i = self.lookup(n, name)?;
        let mut st = self.st.lock();
        f(Self::member_mut(&mut st, i).ok_or(Errno::ENOENT)?)
    }

    /// `BRDGSIFPRIO` (`bstp_set_port_priority`): in steps of 16.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member, `EINVAL` past the limit.
    pub fn set_port_priority(&self, n: &Net, name: &[u8], pri: u8) -> Result<(), Errno> {
        if pri > bstp::MAX_PORT_PRIORITY {
            return Err(Errno::EINVAL);
        }
        self.with_member(n, name, |b| {
            b.priority = pri - pri % 16;
            Ok(())
        })
    }

    /// `BRDGSIFCOST` (`bstp_set_path_cost`): 0 for the computed one.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member, `EINVAL` past the limit.
    pub fn set_path_cost(&self, n: &Net, name: &[u8], cost: u32) -> Result<(), Errno> {
        if cost > bstp::MAX_PATH_COST {
            return Err(Errno::EINVAL);
        }
        let i = self.lookup(n, name)?;
        let (state, baud) = n
            .by_index(i)
            .map_or((LINK_STATE_UNKNOWN, 0), |x| (x.link_state, x.baudrate));
        self.with_member(n, name, |b| {
            if cost == 0 {
                b.stp_flags &= !bstp::PORT_ADMCOST;
                let (f, c) = port_cost(state, baud);
                b.stp_flags |= f & bstp::PORT_PNDCOST;
                b.path_cost = c;
            } else {
                b.path_cost = cost;
                b.stp_flags |= bstp::PORT_ADMCOST;
            }
            Ok(())
        })
    }

    /// `BRDGSIFAMAX`.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member.
    pub fn set_addrmax(&self, n: &Net, name: &[u8], max: u32) -> Result<(), Errno> {
        self.with_member(n, name, |b| {
            b.addrmax = max;
            Ok(())
        })
    }

    /// `BRDGSIFPVID`.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member, `EINVAL` for no such VLAN.
    pub fn set_pvid(&self, n: &Net, name: &[u8], pvid: u16) -> Result<(), Errno> {
        if pvid > VID_MAX {
            return Err(Errno::EINVAL);
        }
        self.with_member(n, name, |b| {
            b.pvid = pvid;
            Ok(())
        })
    }

    /// `BRDGSIFVLANSET`: replaces, adds to, or takes from the member's
    /// tagged VLANs.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member, `EINVAL` for VLAN 0 or 4095 in the
    /// set or an unknown operation.
    pub fn set_vlans(&self, n: &Net, name: &[u8], op: u8, set: &VlanSet) -> Result<(), Errno> {
        if set.test(VID_NULL) || set.test(VID_RSVD_IMPL) {
            return Err(Errno::EINVAL);
        }
        self.with_member(n, name, |b| {
            for (w, s) in b.vlans.0.iter_mut().zip(set.0) {
                match op {
                    abi::bridge::BRDG_VLAN_OP_SET => *w = s,
                    abi::bridge::BRDG_VLAN_OP_ADD => *w |= s,
                    abi::bridge::BRDG_VLAN_OP_DEL => *w &= !s,
                    _ => return Err(Errno::EINVAL),
                }
            }
            Ok(())
        })
    }

    /// `BRDGGIFVLANSET`.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member.
    pub fn vlans(&self, n: &Net, name: &[u8]) -> Result<VlanSet, Errno> {
        self.with_member(n, name, |b| Ok(b.vlans.clone()))
    }

    /// `BRDGSIFVLANPROTO`: 802.1Q or 802.1ad.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member, `EINVAL` for another protocol.
    pub fn set_vlanproto(&self, n: &Net, name: &[u8], proto: u16) -> Result<(), Errno> {
        if proto != ether::ty::VLAN && proto != ether::ty::QINQ {
            return Err(Errno::EINVAL);
        }
        self.with_member(n, name, |b| {
            b.vlanproto = proto;
            Ok(())
        })
    }

    // ---- The table's requests ----

    /// `BRDGRTS`: the table, newest first.
    #[must_use]
    pub fn addresses(&self, n: &Net, now: u64) -> Vec<Ifbareq> {
        let st = self.st.lock();
        let mut all: Vec<_> = st.rt.iter().collect();
        all.sort_by_key(|(_, r)| core::cmp::Reverse(r.seq));
        all.into_iter()
            .map(|((addr, vlan), r)| {
                let mut a = Ifbareq::default();
                put_name(&mut a.ifba_ifsname, &name_of(n, r.member));
                a.ifba_dst = *addr;
                a.ifba_vlan = *vlan;
                a.ifba_expire = if Self::dynamic(r) && now < r.expire {
                    r.expire - now
                } else {
                    0
                };
                a.ifba_flags = r.flags;
                a
            })
            .collect()
    }

    /// `BRDGSADDR`: `addr` on `vlan` behind member `name`, of type
    /// `flags`.
    ///
    /// # Errors
    ///
    /// `ENOENT` if it is not a member, `EINVAL` for a group or zero
    /// address, `ENOSPC` with the table or the member's share full.
    pub fn set_address(
        &self,
        n: &Net,
        name: &[u8],
        addr: &[u8; 6],
        vlan: u16,
        flags: u8,
    ) -> Result<(), Errno> {
        let i = self.lookup(n, name)?;
        let now = crate::uptime();
        let mut flap = None;
        let mut st = self.st.lock();
        Self::rt_update(&mut st, addr, vlan, i, Some(flags), now, &mut flap)
    }

    /// `BRDGDADDR`: `addr`'s entry on `vlan`, every VLAN's for 0.
    ///
    /// # Errors
    ///
    /// `ENOENT` if there is none.
    pub fn delete_address(&self, addr: &[u8; 6], vlan: u16) -> Result<(), Errno> {
        let vlan = if vlan == 0 { VID_RSVD_IMPL } else { vlan };
        let mut st = self.st.lock();
        let mut found = false;
        while let Some((v, _)) = Self::rt_find(&st, addr, vlan) {
            if let Some(r) = st.rt.remove(&(*addr, v))
                && let Some(b) = Self::member_mut(&mut st, r.member)
            {
                b.addrcnt = b.addrcnt.saturating_sub(1);
            }
            found = true;
        }
        if found { Ok(()) } else { Err(Errno::ENOENT) }
    }

    /// `BRDGFLUSH`: the learned entries, or with `IFBF_FLUSHALL` all.
    pub fn flush(&self, how: u32) {
        let mut st = self.st.lock();
        let full = how != 0 && how & IFBF_FLUSHALL != 0;
        Self::rt_retain(&mut st, |r| !(full || Self::dynamic(r)));
    }

    /// `BRDGSCACHE`, `BRDGGCACHE`, `BRDGSTO`, `BRDGGTO`, `BRDGGRTE`.
    pub fn set_cache(&self, max: u32) {
        let mut st = self.st.lock();
        st.brtmax = max;
        Self::rt_trim(&mut st, crate::uptime());
    }

    #[must_use]
    pub fn cache(&self) -> u32 {
        self.st.lock().brtmax
    }

    pub fn set_timeout(&self, t: u32) {
        self.st.lock().brttimeout = t;
    }

    #[must_use]
    pub fn timeout(&self) -> u32 {
        self.st.lock().brttimeout
    }

    #[must_use]
    pub fn exceeded(&self) -> u32 {
        self.st.lock().brtexceeded
    }

    /// `BRDGGFLAGS`, `BRDGSFLAGS`, `BRDGGDEFPVID`, `BRDGSDEFPVID`.
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.st.lock().flags
    }

    pub fn set_flags(&self, f: u32) {
        self.st.lock().flags = f;
    }

    #[must_use]
    pub fn defpvid(&self) -> u16 {
        self.st.lock().defpvid
    }

    /// # Errors
    ///
    /// `EINVAL` for no such VLAN (0 is none).
    pub fn set_defpvid(&self, v: u16) -> Result<(), Errno> {
        if v > VID_MAX {
            return Err(Errno::EINVAL);
        }
        self.st.lock().defpvid = v;
        Ok(())
    }

    // ---- The spanning tree's parameters ----

    /// `BRDGPARAM` (`bridge_ioctl_gbparam`): with no port taking part,
    /// the bridge and root identifiers are its priority alone once it
    /// has started, zero before (`bstp_reinit`).
    #[must_use]
    pub fn params(&self) -> Ifbropreq {
        let st = self.st.lock();
        let id = if st.reinit {
            u64::from(st.priority) << 48
        } else {
            0
        };
        Ifbropreq {
            ifbop_holdcount: st.holdcount,
            ifbop_maxage: st.maxage,
            ifbop_hellotime: st.htime,
            ifbop_fwddelay: st.fdelay,
            ifbop_protocol: st.protover,
            ifbop_priority: st.priority,
            ifbop_root_port: 0,
            ifbop_root_path_cost: 0,
            ifbop_bridgeid: id,
            ifbop_designated_root: id,
            ifbop_designated_bridge: id,
            ifbop_last_tc_time: abi::time::Timeval {
                tv_sec: st.last_tc.0,
                tv_usec: st.last_tc.1,
            },
            ..Ifbropreq::default()
        }
    }

    /// `BRDGGPRI`, `BRDGGHT`, `BRDGGFD`, `BRDGGMA`.
    #[must_use]
    pub fn priority(&self) -> u16 {
        self.st.lock().priority
    }

    #[must_use]
    pub fn hellotime(&self) -> u8 {
        self.st.lock().htime
    }

    #[must_use]
    pub fn fwddelay(&self) -> u8 {
        self.st.lock().fdelay
    }

    #[must_use]
    pub fn maxage(&self) -> u8 {
        self.st.lock().maxage
    }

    /// `BRDGSPRI` (`bstp_set_priority`): in steps of 4096.
    ///
    /// # Errors
    ///
    /// `EINVAL` past the limit.
    pub fn set_priority(&self, pri: u16) -> Result<(), Errno> {
        if pri > bstp::MAX_PRIORITY {
            return Err(Errno::EINVAL);
        }
        let mut st = self.st.lock();
        st.priority = pri - pri % 4096;
        st.reinit = true;
        Ok(())
    }

    /// `BRDGSHT` (`bstp_set_htime`): only in legacy STP mode.
    ///
    /// # Errors
    ///
    /// `EPERM` in RSTP mode, `EINVAL` out of range.
    pub fn set_hellotime(&self, t: u8) -> Result<(), Errno> {
        let mut st = self.st.lock();
        if st.protover != bstp::PROTO_STP {
            return Err(Errno::EPERM);
        }
        if !(bstp::MIN_HELLO_TIME..=bstp::MAX_HELLO_TIME).contains(&t) {
            return Err(Errno::EINVAL);
        }
        st.htime = t;
        st.reinit = true;
        Ok(())
    }

    /// `BRDGSFD` (`bstp_set_fdelay`).
    ///
    /// # Errors
    ///
    /// `EINVAL` out of range.
    pub fn set_fwddelay(&self, t: u8) -> Result<(), Errno> {
        if !(bstp::MIN_FORWARD_DELAY..=bstp::MAX_FORWARD_DELAY).contains(&t) {
            return Err(Errno::EINVAL);
        }
        let mut st = self.st.lock();
        st.fdelay = t;
        st.reinit = true;
        Ok(())
    }

    /// `BRDGSMA` (`bstp_set_maxage`).
    ///
    /// # Errors
    ///
    /// `EINVAL` out of range.
    pub fn set_maxage(&self, t: u8) -> Result<(), Errno> {
        if !(bstp::MIN_MAX_AGE..=bstp::MAX_MAX_AGE).contains(&t) {
            return Err(Errno::EINVAL);
        }
        let mut st = self.st.lock();
        st.maxage = t;
        st.reinit = true;
        Ok(())
    }

    /// `BRDGSTXHC` (`bstp_set_holdcount`).
    ///
    /// # Errors
    ///
    /// `EINVAL` out of range.
    pub fn set_holdcount(&self, c: u8) -> Result<(), Errno> {
        if !(bstp::MIN_HOLD_COUNT..=bstp::MAX_HOLD_COUNT).contains(&c) {
            return Err(Errno::EINVAL);
        }
        self.st.lock().holdcount = c;
        Ok(())
    }

    /// `BRDGSPROTO` (`bstp_set_protocol`): STP or RSTP, the hello time
    /// back to its default.
    ///
    /// # Errors
    ///
    /// `EINVAL` for another.
    pub fn set_protocol(&self, p: u8) -> Result<(), Errno> {
        if p != bstp::PROTO_STP && p != bstp::PROTO_RSTP {
            return Err(Errno::EINVAL);
        }
        let mut st = self.st.lock();
        st.protover = p;
        st.htime = bstp::DEFAULT_HELLO_TIME;
        st.reinit = true;
        Ok(())
    }
}

impl Link for Bridge {
    /// `bridge_transmit`: a frame from the bridge interface, out of the
    /// member its destination is behind, or every member.
    fn transmit(
        &self,
        vnet: &Arc<Vnet>,
        index: u16,
        _family: Option<Family>,
        m: Packet,
    ) -> Result<(), NoBufs> {
        self.tap.mtap(&m, Way::Out);
        let Ok(eh) = header(&m) else {
            return Ok(());
        };
        let section = crate::view::enter();
        let view = vnet.net.view(&section);
        let mut outs = Vec::new();
        {
            let st = self.st.lock();
            let d = if m.hdr.flags & (flags::BCAST | flags::MCAST) == 0 {
                Self::rt_find(&st, &eh.dst, vlan_of(&m)).map(|(_, n)| n.member)
            } else {
                None
            };
            match d.and_then(|d| st.members.iter().find(|b| b.index == d)) {
                Some(b) => {
                    if let Some(o) = Self::prepare(&st, b, view, m, &eh.dst) {
                        outs.push(o);
                    }
                }
                None => Self::broadcast(&st, view, Some(index), m, &eh.dst, &mut outs),
            }
        }
        drop(section);
        self.send(vnet, outs);
        Ok(())
    }

    /// `bridge_init` and `bridge_stop`: it runs while up, its table
    /// pruned; stopping forgets what it learned.
    fn runs_while_up(&self) -> bool {
        true
    }

    fn flags_changed(&self, f: u32) {
        let mut st = self.st.lock();
        let run = f & iff::RUNNING != 0;
        if run && !st.running {
            st.running = true;
            st.next_prune = crate::uptime() + PRUNE_PERIOD;
            // `bstp_init`.
            st.reinit = true;
        } else if !run && st.running {
            st.running = false;
            Self::rt_retain(&mut st, |r| !Self::dynamic(r));
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A frame's Ethernet header.
fn header(m: &Packet) -> Result<Header, NoBufs> {
    let mut b = [0u8; ether::HDR_LEN];
    if m.data.copy_out(0, &mut b) != b.len() {
        return Err(NoBufs);
    }
    mbuf::Header::parse(&b).ok_or(NoBufs)
}

/// `ether_vlanencap_proto`: an 802.1Q (or 802.1ad) header with `tag`
/// after the addresses.
fn vlan_encap(m: &mut Packet, tag: u16, proto: u16) -> Result<(), NoBufs> {
    let h = header(m)?;
    m.data.trim_front(ether::HDR_LEN);
    let b = m.prepend(ether::HDR_LEN + ether::VLAN_ENCAP_LEN)?;
    b[..12].copy_from_slice(&h.bytes()[..12]);
    b[12..14].copy_from_slice(&proto.to_be_bytes());
    b[14..16].copy_from_slice(&tag.to_be_bytes());
    b[16..18].copy_from_slice(&h.ty.to_be_bytes());
    Ok(())
}

/// `bstp_create`'s flags and `bstp_calc_path_cost`: a port whose link is
/// down waits for it (`BSTP_PORT_PNDCOST`); one with no speed costs the
/// default; else 20 Gb/s over its speed.
fn port_cost(link_state: u8, baudrate: u64) -> (u32, u32) {
    let auto = bstp::PORT_AUTOEDGE | bstp::PORT_AUTOPTP;
    if link_state == LINK_STATE_DOWN {
        return (auto | bstp::PORT_PNDCOST, bstp::DEFAULT_PATH_COST);
    }
    if baudrate < 1000 {
        return (auto, bstp::DEFAULT_PATH_COST);
    }
    let c = 20_000_000_000u64 / (baudrate / 1000);
    (
        auto,
        u32::try_from(c.min(u64::from(bstp::MAX_PATH_COST))).unwrap_or(bstp::MAX_PATH_COST),
    )
}

/// `bridge_set_ifcap`: interface `index`'s enabled offloads become
/// `want`, as far as it has them.
fn set_caps(n: &mut Net, index: u16, want: u32) {
    n.with_index(index, |i| {
        if i.capenable != want {
            i.capenable = want & i.capabilities;
        }
    });
}

fn name_of(n: &Net, index: u16) -> String {
    n.by_index(index)
        .map(|i| i.name.clone())
        .unwrap_or_default()
}

fn put_name(dst: &mut [u8; abi::net::IFNAMSIZ], name: &str) {
    let b = name.as_bytes();
    let k = b.len().min(abi::net::IFNAMSIZ - 1);
    dst[..k].copy_from_slice(&b[..k]);
}

/// `in6ifa_llaonifp`: whether interface `index` has a link-local IPv6
/// address.
fn has_ll6(n: &Net, index: u16) -> bool {
    n.by_index(index).is_some_and(|i| {
        i.addrs
            .iter()
            .any(|a| a.family == Family::Inet6 && crate::addr::is_linklocal6(&a.addr))
    })
}

/// `in6_ifdetach`'s addresses: every IPv6 address of interface `index`
/// goes.
fn in6_ifdetach(n: &mut Net, index: u16) {
    let name = name_of(n, index);
    let six: Vec<_> = n.by_index(index).map_or(Vec::new(), |i| {
        i.addrs
            .iter()
            .filter(|a| a.family == Family::Inet6)
            .map(|a| a.addr)
            .collect()
    });
    for a in six {
        let _ = n.delete_addr(name.as_bytes(), Family::Inet6, Some(&a));
    }
}

/// `in_aifaddr_ioctl` and `in6_addifaddr` on interface `i` of `n`: a
/// bridge's member may have addresses, with a warning, only while
/// `net.link.bridge.member_ifaddrs` allows.
///
/// # Errors
///
/// `EINVAL` when it does not.
pub fn member_addr_check(n: &Net, i: &Ifnet) -> Result<(), Errno> {
    if i.bridge.is_none() || i.ty == IFT_GIF {
        return Ok(());
    }
    if n.owner
        .upgrade()
        .is_some_and(|v| !Conf::get(&v.bridge.member_ifaddrs))
    {
        return Err(Errno::EINVAL);
    }
    crate::log(
        6,
        &format!(
            "{}: WARNING: Assigning an IP address to an interface which is also a bridge member is deprecated and will be unsupported in a future release.\n",
            i.name
        ),
    );
    Ok(())
}

/// `bridge_clone_create`: bridge `unit` in `vnet`, down, with an address
/// of its own; returns its name.
pub(crate) fn create(vnet: &Arc<Vnet>, unit: i32, ctx: &Ctx) -> String {
    let name = format!("bridge{unit}");
    let defaddr = link::gen_addr(&ctx.hostuuid, &name, &ctx.jail).unwrap_or_else(|| {
        let r = &ctx.random;
        link::random_addr([r[0], r[1], r[2], r[3], r[4], r[5]])
    });
    let tap = Tap::ether();
    let b = Arc::new_cyclic(|me| Bridge {
        unit,
        me: me.clone(),
        stats: Arc::new(Stats::default()),
        tap: Arc::clone(&tap),
        defaddr,
        st: PreemptLock::new(State {
            members: Vec::new(),
            spans: Vec::new(),
            rt: BTreeMap::new(),
            seq: 0,
            brtmax: RTABLE_MAX,
            brttimeout: RTABLE_TIMEOUT,
            brtexceeded: 0,
            flags: 0,
            defpvid: 0,
            ifaddr: None,
            running: false,
            next_prune: 0,
            priority: bstp::DEFAULT_BRIDGE_PRIORITY,
            htime: bstp::DEFAULT_HELLO_TIME,
            fdelay: bstp::DEFAULT_FORWARD_DELAY,
            maxage: bstp::DEFAULT_MAX_AGE,
            holdcount: bstp::DEFAULT_HOLD_COUNT,
            protover: bstp::PROTO_RSTP,
            reinit: false,
            last_tc: (ctx.now, 0),
            log_sec: 0,
            log_count: 0,
        }),
    });
    {
        let mut all = ALL.lock();
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&b));
    }
    let mut n = vnet.net.lock();
    let index = n.next_index();
    let i = Ifnet {
        index,
        name: name.clone(),
        dname: "bridge",
        dunit: unit,
        home: vnet.id,
        fib: 0,
        flags: iff::BROADCAST | iff::SIMPLEX | iff::MULTICAST,
        mtu: crate::clone::ETHERMTU,
        metric: 0,
        ty: IFT_BRIDGE,
        link_state: LINK_STATE_UNKNOWN,
        capabilities: ifcap::VLAN_HWTAGGING,
        capenable: ifcap::VLAN_HWTAGGING,
        // `nd6_ifattach`: no automatic link-local address on a bridge.
        nd6_flags: nd6_iff::PERFORMNUD | nd6_iff::IFDISABLED,
        nd: crate::ifnet::NdInfo::DEFAULT,
        description: None,
        groups: alloc::vec![String::from("bridge")],
        addrs: Vec::new(),
        epoch: ctx.now,
        lastchange: (ctx.now, 0),
        stats: Arc::clone(&b.stats),
        lladdr: Some(defaddr),
        baudrate: 0,
        link: LinkRef(Some(Arc::clone(&b) as Arc<dyn Link>)),
        bpf: Some(tap),
        pcount: 0,
        maddrs: Vec::new(),
        pcp: crate::ifnet::PCP_NONE,
        bridge: None,
    };
    n.attach(i);
    name
}

/// The bridge interface `i` is, if it is one.
#[must_use]
pub fn of(i: &Ifnet) -> Option<&Bridge> {
    i.link
        .0
        .as_ref()
        .and_then(|l| l.as_any().downcast_ref::<Bridge>())
}

/// Prunes every bridge's table as its period comes (`bridge_timer`):
/// call once a second with the uptime.
pub fn tick(now: u64) {
    let all: Vec<Arc<Bridge>> = ALL.lock().iter().filter_map(Weak::upgrade).collect();
    for b in all {
        b.tick(now);
    }
}
