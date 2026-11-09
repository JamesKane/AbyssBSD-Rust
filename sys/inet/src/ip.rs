// SPDX-License-Identifier: BSD-2-Clause
//! IPv4 (docs/NETWORKING.md §8): input with its checks, the decision
//! whether a packet is this host's, reassembly, delivery to the
//! protocol; output with the route, the source address, broadcast and
//! multicast rules, the checksums the interface does not take,
//! fragmentation; and forwarding when `net.inet.ip.forwarding` is on.
//!
//! A packet's IP header stays on it up to the protocol, as FreeBSD's
//! does since 10.0, its lengths and offsets in network order.
//!
//! Behavior reference: sys/netinet/ip_input.c (`ip_input`,
//! `ip_forward`), sys/netinet/ip_output.c (`ip_output`, `ip_fragment`,
//! `ip_mloopback`, `in_delayed_cksum`), sys/netinet/ip_id.c
//! (`ip_fillid`), sys/netinet/in.c (`in_broadcast`,
//! `in_ifaddr_broadcast`, `in_canforward`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::net::{IFT_LOOP, ifcap, iff, rtf};
use mbuf::{MHLEN, Packet, csum, flags};
use net::addr::{from_v4, is_loopback4, is_multicast4, prefix_match, v4_of};
use net::ifnet::{Gateway, Ifaddr, Ifnet, Net};
use net::{Family, Vnet};

use crate::cksum;
use crate::icmp;
use crate::state::{self, State, ips, on};

/// `IPVERSION`, `sizeof(struct ip)`, `IP_MAXPACKET`.
pub const VERSION: u8 = 4;
pub const HDR_LEN: usize = 20;
pub const MAXPACKET: usize = 65535;
/// `ip_off`'s flags and offset mask.
pub const RF: u16 = 0x8000;
pub const DF: u16 = 0x4000;
pub const MF: u16 = 0x2000;
pub const OFFMASK: u16 = 0x1fff;
/// `IP_DEFAULT_MULTICAST_TTL`.
pub const DEFAULT_MULTICAST_TTL: u8 = 1;
/// `RTF_DYNAMIC`, `RTF_BROADCAST`.
const RTF_DYNAMIC: i32 = 0x10;
const RTF_BROADCAST: i32 = 0x40_0000;

/// The protocols IPv4 carries here (`IPPROTO_*`).
pub mod proto {
    pub const ICMP: u8 = 1;
    pub const IGMP: u8 = 2;
    pub const TCP: u8 = 6;
    pub const UDP: u8 = 17;
}

/// An IPv4 header (`struct ip`), its fields in host order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ip {
    pub v: u8,
    /// The header's length in bytes (`ip_hl << 2`).
    pub hl: u8,
    pub tos: u8,
    pub len: u16,
    pub id: u16,
    pub off: u16,
    pub ttl: u8,
    pub p: u8,
    pub sum: u16,
    pub src: [u8; 4],
    pub dst: [u8; 4],
}

impl mbuf::Header for Ip {
    const LEN: usize = HDR_LEN;

    fn parse(b: &[u8]) -> Option<Self> {
        let w = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        Some(Self {
            v: b[0] >> 4,
            hl: (b[0] & 0xf) << 2,
            tos: b[1],
            len: w(2),
            id: w(4),
            off: w(6),
            ttl: b[8],
            p: b[9],
            sum: w(10),
            src: b[12..16].try_into().ok()?,
            dst: b[16..20].try_into().ok()?,
        })
    }
}

impl Ip {
    /// A header for a datagram of `len` bytes (header included) of
    /// protocol `p`, version and length set as `ip_output` sets them.
    #[must_use]
    pub const fn new(p: u8, src: [u8; 4], dst: [u8; 4], len: u16, ttl: u8, tos: u8) -> Self {
        Self {
            v: VERSION,
            hl: 20,
            tos,
            len,
            id: 0,
            off: 0,
            ttl,
            p,
            sum: 0,
            src,
            dst,
        }
    }

    /// Its first 20 bytes, the checksum as it is.
    #[must_use]
    pub fn bytes(&self) -> [u8; HDR_LEN] {
        let mut b = [0u8; HDR_LEN];
        b[0] = (self.v << 4) | (self.hl >> 2);
        b[1] = self.tos;
        b[2..4].copy_from_slice(&self.len.to_be_bytes());
        b[4..6].copy_from_slice(&self.id.to_be_bytes());
        b[6..8].copy_from_slice(&self.off.to_be_bytes());
        b[8] = self.ttl;
        b[9] = self.p;
        b[10..12].copy_from_slice(&self.sum.to_be_bytes());
        b[12..16].copy_from_slice(&self.src);
        b[16..20].copy_from_slice(&self.dst);
        b
    }

    /// Whether it is one fragment of a datagram.
    #[must_use]
    pub const fn is_fragment(&self) -> bool {
        self.off & (MF | OFFMASK) != 0
    }
}

/// `IN_LINKLOCAL`, `IN_ZERONET`, `IN_EXPERIMENTAL`.
const fn is_linklocal4(a: &[u8; 4]) -> bool {
    a[0] == 169 && a[1] == 254
}

const fn is_zeronet(a: &[u8; 4]) -> bool {
    a[0] == 0
}

const fn is_experimental(a: &[u8; 4]) -> bool {
    a[0] >= 240
}

/// `in_broadcast`: all ones or all zeros.
const fn is_any_broadcast(a: &[u8; 4]) -> bool {
    u32::from_be_bytes(*a) == u32::MAX || u32::from_be_bytes(*a) == 0
}

/// `in_ifaddr_broadcast`: `a` is the broadcast address of interface
/// address `ia` (not of a /32).
fn is_ifa_broadcast(a: &[u8; 4], ia: &Ifaddr) -> bool {
    ia.plen < 32 && ia.broadcast.is_some_and(|b| v4_of(&b) == *a)
}

/// `in_canforward`.
const fn can_forward(a: &[u8; 4]) -> bool {
    !(is_multicast4(a) || is_experimental(a) || is_zeronet(a) || is_loopback4(a))
}

/// The IPv4 addresses of interface `i`.
fn addrs4(i: &Ifnet) -> impl Iterator<Item = &Ifaddr> {
    i.addrs.iter().filter(|a| a.family == Family::Inet)
}

/// Whether `a` is an IPv4 address of some interface (`in_localip`).
pub(crate) fn is_local4(n: &Net, a: &[u8; 4]) -> bool {
    n.ifaces
        .iter()
        .any(|i| addrs4(i).any(|x| v4_of(&x.addr) == *a))
}

/// What the device takes of the checksum work (`if_hwassist`): a loopback
/// interface with transmit checksums enabled does it all, never on the
/// wire; the others leave it here.
fn hwassist(i: &Ifnet) -> u32 {
    if i.ty == IFT_LOOP && i.capenable & ifcap::TXCSUM != 0 {
        csum::IP | csum::IP_UDP | csum::IP_TCP
    } else {
        0
    }
}

/// Fills in the identification (`ip_fillid`): none on an atomic datagram
/// (RFC 6864), a random one with `net.inet.ip.random_id`, else the
/// counter's next.
pub(crate) fn fill_id(st: &State, ip: &mut Ip) {
    if on(&st.conf.rfc6864) && ip.off & DF != 0 {
        ip.id = 0;
    } else if on(&st.conf.random_id) {
        #[allow(clippy::cast_possible_truncation, reason = "16 random bits")]
        {
            ip.id = (crate::random() >> 8) as u16;
        }
    } else {
        // Relaxed: a counter.
        let n = st.id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        #[allow(clippy::cast_possible_truncation, reason = "its low 16 bits")]
        {
            ip.id = n as u16;
        }
    }
}

/// Where a received packet goes.
enum Dest {
    Ours,
    Forward,
    /// Dropped, counted against the statistic.
    Drop(usize),
}

/// `ip_input`'s address checks: whether the packet is for this host
/// (one of its addresses, a broadcast address of the receiving
/// interface, a multicast group, the limited broadcast), to be
/// forwarded, or dropped.
fn classify(n: &Net, st: &State, ip: &Ip, rcvif: u16, ifflags: u32, mflags: u32) -> Dest {
    let any4 = n.ifaces.iter().any(|i| addrs4(i).next().is_some());
    if !any4 && mflags & (flags::MCAST | flags::BCAST) == 0 {
        return Dest::Ours;
    }
    let forwarding = on(&st.conf.forwarding);
    let loopback = ifflags & iff::LOOPBACK != 0;
    let strong_es = on(&st.conf.strong_es) && !forwarding && !loopback;
    for i in &n.ifaces {
        if addrs4(i).any(|a| v4_of(&a.addr) == ip.dst) {
            if strong_es && i.index != rcvif {
                return Dest::Drop(ips::BADADDR);
            }
            // `net.inet.ip.source_address_validation`: a packet from the
            // wire pretending to come from here.
            if on(&st.conf.sav) && !loopback && is_local4(n, &ip.src) {
                return Dest::Drop(ips::BADADDR);
            }
            return Dest::Ours;
        }
    }
    if ifflags & iff::BROADCAST != 0
        && let Some(i) = n.by_index(rcvif)
        && addrs4(i).any(|a| a.broadcast.is_some_and(|b| v4_of(&b) == ip.dst))
    {
        return Dest::Ours;
    }
    // Multicast: the protocols filter by membership.
    if is_multicast4(&ip.dst) || is_any_broadcast(&ip.dst) {
        return Dest::Ours;
    }
    // RFC 3927 2.7: link-local addresses are not forwarded.
    if is_linklocal4(&ip.dst) || is_linklocal4(&ip.src) {
        return Dest::Drop(ips::CANTFORWARD);
    }
    if forwarding {
        Dest::Forward
    } else {
        Dest::Drop(ips::CANTFORWARD)
    }
}

/// Whether the options in `opts` (the header past its first 20 bytes)
/// have RFC 791's structure.
fn options_ok(opts: &[u8]) -> bool {
    let mut at = 0;
    while at < opts.len() {
        match opts[at] {
            0 => break,
            1 => at += 1,
            _ => {
                let Some(&len) = opts.get(at + 1) else {
                    return false;
                };
                let len = usize::from(len);
                if len < 2 || at + len > opts.len() {
                    return false;
                }
                at += len;
            }
        }
    }
    true
}

/// A received IPv4 packet (`ip_input`), by way of `netisr`'s `NETISR_IP`:
/// its interface is `m.hdr.rcvif`.
pub fn input(vnet: &Arc<Vnet>, mut m: Packet) {
    let st = state::of(vnet);
    st.ipstat.inc(ips::TOTAL);
    if m.len() < HDR_LEN {
        st.ipstat.inc(ips::TOOSHORT);
        return;
    }
    let Ok(ip) = m.header::<Ip>(0) else {
        st.ipstat.inc(ips::TOOSMALL);
        return;
    };
    if ip.v != VERSION {
        st.ipstat.inc(ips::BADVERS);
        return;
    }
    let hlen = usize::from(ip.hl);
    if hlen < HDR_LEN || hlen > m.len() {
        st.ipstat.inc(ips::BADHLEN);
        return;
    }
    let rcvif = m.hdr.rcvif;
    let ifflags = vnet.net.lock().by_index(rcvif).map_or(0, |i| i.flags);
    // RFC 1122: the loopback network never on the wire.
    if (is_loopback4(&ip.dst) || is_loopback4(&ip.src)) && ifflags & iff::LOOPBACK == 0 {
        st.ipstat.inc(ips::BADADDR);
        return;
    }
    let bad = if m.hdr.csum_flags & csum::IP_CHECKED != 0 {
        m.hdr.csum_flags & csum::IP_VALID == 0
    } else if m.hdr.csum_flags & csum::IP != 0 {
        // From this host, its checksum left to a device it never met.
        false
    } else {
        cksum::of_chain(&m.data, 0, hlen) != 0
    };
    if bad {
        st.ipstat.inc(ips::BADSUM);
        return;
    }
    let len = usize::from(ip.len);
    if len < hlen {
        st.ipstat.inc(ips::BADLEN);
        return;
    }
    if m.len() < len {
        st.ipstat.inc(ips::TOOSHORT);
        return;
    }
    if m.len() > len && m.data.split_off(len).is_err() {
        return;
    }
    if ip.dst == [0; 4] {
        st.ipstat.inc(ips::BADADDR);
        return;
    }
    if hlen > HDR_LEN {
        let mut opts = [0u8; 40];
        let opts = &mut opts[..hlen - HDR_LEN];
        m.data.copy_out(HDR_LEN, opts);
        if !options_ok(opts) {
            st.ipstat.inc(ips::BADOPTIONS);
            icmp::error(vnet, m, icmp::ty::PARAMPROB, 20, [0; 4], 0);
            return;
        }
    }
    let dest = classify(&vnet.net.lock(), st, &ip, rcvif, ifflags, m.hdr.flags);
    match dest {
        Dest::Drop(stat) => {
            st.ipstat.inc(stat);
        }
        Dest::Forward => forward(vnet, st, m, ip),
        Dest::Ours => {
            let (m, ip) = if ip.is_fragment() {
                match crate::reass::reass(st, m, &ip) {
                    Some(m) => {
                        let mut m = m;
                        let Ok(ip) = m.header::<Ip>(0) else {
                            return;
                        };
                        (m, ip)
                    }
                    None => return,
                }
            } else {
                (m, ip)
            };
            st.ipstat.inc(ips::DELIVERED);
            deliver(vnet, m, &ip);
        }
    }
}

/// To the protocol (`ip_protox`).
fn deliver(vnet: &Arc<Vnet>, m: Packet, ip: &Ip) {
    match ip.p {
        proto::ICMP => icmp::input(vnet, m, ip),
        proto::UDP => crate::udp::input4(vnet, m, ip),
        proto::IGMP => crate::igmp::input(vnet, m, ip),
        proto::TCP => crate::tcp::input4(vnet, m, ip),
        _ => crate::raw::input4(vnet, m, ip, false),
    }
}

/// `ip_output`'s flags.
pub mod out {
    /// A packet being forwarded: its header is kept.
    pub const FORWARDING: u32 = 0x1;
    /// A raw socket's header: kept.
    pub const RAWOUTPUT: u32 = 0x2;
    /// `IP_SENDONES`: to the all-ones broadcast of the interface whose
    /// broadcast address the destination is.
    pub const SENDONES: u32 = 0x4;
    /// `SO_DONTROUTE`: out of the interface on the destination's network.
    pub const ROUTETOIF: u32 = 0x10;
    /// `SO_BROADCAST`.
    pub const ALLOWBROADCAST: u32 = 0x20;
}

/// A socket's multicast choices (`struct ip_moptions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mopts {
    /// `IP_MULTICAST_IF`'s interface, 0 for the route's.
    pub ifindex: u16,
    pub ttl: u8,
    pub loop_: bool,
}

/// What `ip_output` is told besides the packet.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    /// [`out`] bits.
    pub flags: u32,
    pub mcast: Option<Mopts>,
    pub fib: u16,
    /// `IP_VLAN_PCP`'s priority for the frame (`EVL_APPLY_PRI`).
    pub pcp: Option<u8>,
    /// The Router Alert option goes in the header (IGMP's `m_raopt`).
    pub router_alert: bool,
    /// The packet's flow (`m_pkthdr.flowid`), which picks among a
    /// multipath route's next hops.
    pub flowid: u32,
}

/// The Router Alert option (RFC 2113), as IGMP sends it.
const RA_OPTION: [u8; 4] = [0x94, 0x04, 0, 0];

/// The interface and next hop a packet takes.
struct Path {
    ifindex: u16,
    mtu: u32,
    gw: Option<[u8; 4]>,
    src: [u8; 4],
    bcast: bool,
}

/// `ip_output`'s choice of interface for `dst`.
fn path(n: &Net, st: &State, ip: &mut Ip, o: &Opts) -> Result<Option<Path>, Errno> {
    let dst = ip.dst;
    let find = |pred: &dyn Fn(&Ifaddr) -> bool| {
        n.ifaces
            .iter()
            .find_map(|i| addrs4(i).find(|a| pred(a)).map(|a| (i, a)))
    };
    if o.flags & out::SENDONES != 0 {
        let Some((i, a)) = find(&|a| {
            a.broadcast.is_some_and(|b| v4_of(&b) == dst) || a.dst.is_some_and(|d| v4_of(&d) == dst)
        }) else {
            st.ipstat.inc(ips::NOROUTE);
            return Err(Errno::ENETUNREACH);
        };
        ip.dst = [0xff; 4];
        ip.ttl = 1;
        return Ok(Some(Path {
            ifindex: i.index,
            mtu: i.mtu,
            gw: None,
            src: v4_of(&a.addr),
            bcast: true,
        }));
    }
    if o.flags & out::ROUTETOIF != 0 {
        let Some((i, a)) = find(&|a| a.dst.is_some_and(|d| v4_of(&d) == dst))
            .or_else(|| find(&|a| prefix_match(Family::Inet, &a.addr, &from_v4(dst), a.plen)))
        else {
            st.ipstat.inc(ips::NOROUTE);
            return Err(Errno::ENETUNREACH);
        };
        ip.ttl = 1;
        let bcast =
            i.flags & iff::BROADCAST != 0 && (is_any_broadcast(&dst) || is_ifa_broadcast(&dst, a));
        return Ok(Some(Path {
            ifindex: i.index,
            mtu: i.mtu,
            gw: None,
            src: v4_of(&a.addr),
            bcast,
        }));
    }
    if is_multicast4(&dst)
        && let Some(mo) = o.mcast
        && mo.ifindex != 0
    {
        let i = n.by_index(mo.ifindex).ok_or(Errno::ENETUNREACH)?;
        return Ok(Some(Path {
            ifindex: i.index,
            mtu: i.mtu,
            gw: None,
            src: addrs4(i).next().map_or([0; 4], |a| v4_of(&a.addr)),
            bcast: false,
        }));
    }
    let Some(r) = n.route_in(o.fib, Family::Inet, &from_v4(dst)) else {
        st.ipstat.inc(ips::NOROUTE);
        return Err(Errno::EHOSTUNREACH);
    };
    let r = mpath_select(n, r, o.flowid);
    // `looutput` on a reject or blackhole next hop.
    if r.flags & rtf::BLACKHOLE != 0 {
        return Ok(None);
    }
    if r.flags & rtf::REJECT != 0 {
        return Err(Errno::EHOSTUNREACH);
    }
    let i = n.by_index(r.ifindex).ok_or(Errno::EHOSTUNREACH)?;
    let gw = match r.gateway {
        Gateway::Addr(a) if r.flags & rtf::GATEWAY != 0 => Some(v4_of(&a)),
        _ => None,
    };
    let src = v4_of(&r.ifa);
    let ia = addrs4(i).find(|a| v4_of(&a.addr) == src);
    let target = gw.unwrap_or(dst);
    let bcast = (r.flags & (rtf::HOST | RTF_BROADCAST) == rtf::HOST | RTF_BROADCAST)
        || (i.flags & iff::BROADCAST != 0
            && (is_any_broadcast(&dst) || ia.is_some_and(|a| is_ifa_broadcast(&target, a))));
    Ok(Some(Path {
        ifindex: i.index,
        mtu: if r.mtu == 0 { i.mtu } else { r.mtu.min(i.mtu) },
        gw,
        src,
        bcast,
    }))
}

/// `mpath_entropy_key`.
pub(crate) const MPATH_KEY: [u8; 40] = [
    0x6d, 0x5a, 0x56, 0xda, 0x25, 0x5b, 0x0e, 0xc2, 0x41, 0x67, 0x25, 0x3d, 0x43, 0xa3, 0x8f, 0xb0,
    0xd0, 0xca, 0x2b, 0xcb, 0xae, 0x7b, 0x30, 0xb4, 0x77, 0xcb, 0x2d, 0xa3, 0x80, 0x30, 0xf2, 0x0c,
    0x6a, 0x42, 0xb7, 0x3b, 0xbe, 0xac, 0x01, 0xfa,
];

/// `toeplitz_hash` of `data` with `key`.
pub(crate) fn toeplitz(key: &[u8; 40], data: &[u8]) -> u32 {
    let mut hash = 0u32;
    let mut v = u32::from_be_bytes([key[0], key[1], key[2], key[3]]);
    for (i, d) in data.iter().enumerate() {
        for b in 0..8 {
            if d & (1 << (7 - b)) != 0 {
                hash ^= v;
            }
            v <<= 1;
            if key.get(i + 4).is_some_and(|k| k & (1 << (7 - b)) != 0) {
                v |= 1;
            }
        }
    }
    hash
}

/// `fib4_calc_software_hash`: a flow for a packet of this host's, from
/// its addresses, ports (in network order, as `struct _hash_5tuple_ipv4`
/// holds them), and protocol.
#[must_use]
pub fn flow_hash(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, proto: u8) -> u32 {
    let mut d = [0u8; 16];
    d[..4].copy_from_slice(&src);
    d[4..8].copy_from_slice(&dst);
    d[8..10].copy_from_slice(&sport.to_be_bytes());
    d[10..12].copy_from_slice(&dport.to_be_bytes());
    d[12] = proto;
    toeplitz(&MPATH_KEY, &d)
}

/// Whether this host's packets get hashed flows (`V_fib_hash_outbound`).
pub(crate) fn hash_outbound(vnet: &Vnet) -> bool {
    vnet.net.lock().hash_outbound
}

/// `RIB_MAX_MPATH_WIDTH`.
const MAX_MPATH_WIDTH: u32 = 64;

/// `nhop_select`: of the paths of `r`'s prefix (a multipath group, by
/// next-hop index), the one flow `flowid` takes, each path given slots in
/// proportion to its weight (`calc_min_mpath_slots`).
pub(crate) fn mpath_select<'a>(
    n: &'a Net,
    r: &'a net::ifnet::Route,
    flowid: u32,
) -> &'a net::ifnet::Route {
    let same = |x: &&net::ifnet::Route| {
        x.family == r.family
            && x.fib == r.fib
            && x.plen == r.plen
            && x.dst == r.dst
            // Each interface's link-local prefix is its own.
            && (!net::ifnet::route_scoped(r) || net::ifnet::route_zone(x) == net::ifnet::route_zone(r))
    };
    // A route of one path, as most are: nothing to choose, nothing to
    // allocate.
    if n.routes.iter().filter(same).nth(1).is_none() {
        return r;
    }
    let mut group: Vec<&net::ifnet::Route> = n.routes.iter().filter(same).collect();
    group.sort_by_key(|x| x.nhidx);
    let gcd = |mut a: u32, mut b: u32| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    let unit = group.iter().fold(0, |g, x| gcd(g, x.weight.max(1)));
    let mut slots: Vec<&net::ifnet::Route> = Vec::new();
    for x in &group {
        for _ in 0..x.weight.max(1) / unit.max(1) {
            slots.push(x);
        }
    }
    if slots.len() > MAX_MPATH_WIDTH as usize {
        // Too fine to slot exactly: each path once.
        slots = group;
    }
    slots[flowid as usize % slots.len()]
}

/// `in_delayed_cksum`: the transport checksum the device would have
/// finished, done here; the field holds the pseudo-header's sum.
fn delayed_cksum(m: &mut Packet, hlen: usize) {
    let off = hlen + m.hdr.csum_data as usize;
    let mut c = cksum::of_chain(&m.data, hlen, m.len().saturating_sub(hlen));
    if c == 0 && m.hdr.csum_flags & csum::IP_UDP != 0 {
        c = 0xffff;
    }
    let _ = m.data.copy_in(off, &c.to_be_bytes());
    m.hdr.csum_flags &= !(csum::IP_UDP | csum::IP_TCP);
}

/// The header bytes `ip` (with `opts` after its first 20), its checksum
/// computed unless the device does it.
fn header_bytes(ip: &Ip, opts: &[u8], sum: bool) -> Vec<u8> {
    let mut ip = *ip;
    ip.sum = 0;
    let mut b = Vec::with_capacity(usize::from(ip.hl));
    b.extend_from_slice(&ip.bytes());
    b.extend_from_slice(opts);
    if sum {
        let c = cksum::of(&b);
        b[10..12].copy_from_slice(&c.to_be_bytes());
    }
    b
}

/// Sends IPv4 packet `m`, its header first (`ip_output`): the
/// destination, protocol, length, TTL, TOS, and offset set, the source
/// set or 0 for the route's. Unless forwarding or a raw socket's header,
/// the version, length, and identification are filled in.
///
/// # Errors
///
/// `EHOSTUNREACH` or `ENETUNREACH` without a route; `EACCES` to a
/// broadcast address without `SO_BROADCAST`; `EADDRNOTAVAIL` for a
/// broadcast on an interface without one, or the loopback network off
/// `lo0`; `EMSGSIZE` for a broadcast or `IP_DF` packet over the MTU;
/// `ENETDOWN`, `ENOBUFS`, and ARP's errors from the interface.
pub fn output(vnet: &Arc<Vnet>, mut m: Packet, o: &Opts) -> Result<(), Errno> {
    let st = state::of(vnet);
    let mut ip = m.header::<Ip>(0).map_err(|_| Errno::EINVAL)?;
    if o.flags & (out::FORWARDING | out::RAWOUTPUT) == 0 {
        ip.v = VERSION;
        ip.hl = 20;
        // `ip_insertoptions`.
        if o.router_alert {
            m.data.trim_front(HDR_LEN);
            m.prepend(HDR_LEN + RA_OPTION.len())
                .map_err(|_| Errno::ENOBUFS)?[HDR_LEN..]
                .copy_from_slice(&RA_OPTION);
            ip.hl = 24;
            ip.len += 4;
        }
        fill_id(st, &mut ip);
    }
    let hlen = usize::from(ip.hl);
    if hlen < HDR_LEN || hlen > m.len() {
        return Err(Errno::EINVAL);
    }
    let mut opts = [0u8; 40];
    let opts = &mut opts[..hlen - HDR_LEN];
    m.data.copy_out(HDR_LEN, opts);
    if o.flags & out::FORWARDING == 0 {
        st.ipstat.inc(ips::LOCALOUT);
    }
    let now = net::uptime();
    let mut n = vnet.net.lock();
    let Some(p) = path(&n, st, &mut ip, o)? else {
        return Ok(());
    };
    let i = n.by_index(p.ifindex).ok_or(Errno::EHOSTUNREACH)?;
    let (ifflags, assist) = (i.flags, hwassist(i));
    let mtu = p.mtu as usize;
    if is_multicast4(&ip.dst) {
        m.hdr.flags |= flags::MCAST;
        ip.ttl = o.mcast.map_or(DEFAULT_MULTICAST_TTL, |mo| mo.ttl);
        if ifflags & iff::MULTICAST == 0 {
            st.ipstat.inc(ips::NOROUTE);
            return Err(Errno::ENETUNREACH);
        }
        if ip.src == [0; 4] {
            ip.src = p.src;
        }
        // `ip_mloopback`: a copy goes up here, as received on the
        // interface; input drops it unless some socket joined the group.
        if o.mcast.is_none_or(|mo| mo.loop_) {
            let mut c = m.copy().map_err(|_| Errno::ENOBUFS)?;
            if c.hdr.csum_flags & (csum::IP_UDP | csum::IP_TCP) != 0 {
                delayed_cksum(&mut c, hlen);
            }
            c.data
                .copy_in(0, &header_bytes(&ip, opts, true))
                .map_err(|_| Errno::ENOBUFS)?;
            c.hdr.csum_flags = csum::IP_CHECKED | csum::IP_VALID;
            c.hdr.rcvif = p.ifindex;
            n.isrq.push((net::netisr::Proto::Ip, c));
        }
        // Not to the wire with TTL 0, nor out of a loopback interface:
        // the copy above was it.
        if ip.ttl == 0 || ifflags & iff::LOOPBACK != 0 {
            return Ok(());
        }
    } else {
        if ip.src == [0; 4] {
            ip.src = p.src;
        }
        if p.bcast {
            if ifflags & iff::BROADCAST == 0 {
                return Err(Errno::EADDRNOTAVAIL);
            }
            if o.flags & out::ALLOWBROADCAST == 0 {
                return Err(Errno::EACCES);
            }
            if usize::from(ip.len) > mtu {
                return Err(Errno::EMSGSIZE);
            }
            m.hdr.flags |= flags::BCAST;
        } else {
            m.hdr.flags &= !flags::BCAST;
        }
    }
    if (is_loopback4(&ip.dst) || is_loopback4(&ip.src)) && ifflags & iff::LOOPBACK == 0 {
        st.ipstat.inc(ips::BADADDR);
        return Err(Errno::EADDRNOTAVAIL);
    }
    if let Some(pcp) = o.pcp {
        m.hdr.vlan_tag = u16::from(pcp & 7) << 13;
        m.hdr.flags |= flags::VLANTAG;
    }
    m.hdr.csum_flags |= csum::IP;
    if m.hdr.csum_flags & (csum::IP_UDP | csum::IP_TCP) & !assist != 0 {
        delayed_cksum(&mut m, hlen);
    }
    let nexthop = from_v4(p.gw.unwrap_or(ip.dst));
    let is_gw = p.gw.is_some();
    if usize::from(ip.len) <= mtu {
        let sum = assist & csum::IP == 0;
        if sum {
            m.hdr.csum_flags &= !csum::IP;
        }
        m.data
            .copy_in(0, &header_bytes(&ip, opts, sum))
            .map_err(|_| Errno::ENOBUFS)?;
        return net::output::output(&mut n, p.ifindex, Family::Inet, &nexthop, is_gw, m, now);
    }
    if ip.off & DF != 0 {
        st.ipstat.inc(ips::CANTFRAG);
        return Err(Errno::EMSGSIZE);
    }
    let frags = fragment(st, m, &ip, opts, mtu, assist)?;
    let mut r = Ok(());
    for f in frags {
        if r.is_ok() {
            r = net::output::output(&mut n, p.ifindex, Family::Inet, &nexthop, is_gw, f, now);
        }
    }
    if r.is_ok() {
        st.ipstat.inc(ips::FRAGMENTED);
    }
    r
}

/// `ip_optcopy`: the options a fragment after the first carries (those
/// with the copied bit, and no-ops), padded to a word.
fn optcopy(opts: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < opts.len() {
        let o = opts[at];
        if o == 0 {
            break;
        }
        if o == 1 {
            out.push(1);
            at += 1;
            continue;
        }
        let len = usize::from(*opts.get(at + 1).unwrap_or(&0)).max(2);
        let end = (at + len).min(opts.len());
        if o & 0x80 != 0 {
            out.extend_from_slice(&opts[at..end]);
        }
        at = end;
    }
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

/// `ip_fragment`: `m` (header `ip`, options `opts`) cut into fragments
/// that fit `mtu`, the first first. Data is shared, not copied.
fn fragment(
    st: &State,
    mut m: Packet,
    ip: &Ip,
    opts: &[u8],
    mtu: usize,
    assist: u32,
) -> Result<Vec<Packet>, Errno> {
    let hlen = usize::from(ip.hl);
    let mut len = mtu.saturating_sub(hlen) & !7;
    if ip.off & DF != 0 || len < 8 {
        st.ipstat.inc(ips::CANTFRAG);
        return Err(Errno::EMSGSIZE);
    }
    // The device does not checksum fragments.
    if m.hdr.csum_flags & (csum::IP_UDP | csum::IP_TCP) != 0 {
        delayed_cksum(&mut m, hlen);
    }
    let ip_len = usize::from(ip.len);
    // Large fragments carry whole pages.
    let page = 4096;
    let mut off;
    if len > page {
        off = mtu.min(m.len());
        let mut newlen;
        if off < hlen {
            newlen = len;
            off = hlen + len;
        } else {
            off = ((off - hlen) & !7) + hlen;
            newlen = mtu & !(page - 1);
            if newlen + HDR_LEN > mtu {
                newlen = len;
                off = hlen + len;
            }
        }
        len = newlen;
    } else {
        off = hlen + len;
    }
    let firstlen = off - hlen;
    let later = optcopy(opts);
    let mhlen = HDR_LEN + later.len();
    let sum = assist & csum::IP == 0;
    let mut out = Vec::new();
    let mut nfrags = 1u64;
    while off < ip_len {
        let flen = len.min(ip_len - off);
        let mut h = *ip;
        h.hl = u8::try_from(mhlen).unwrap_or(20);
        #[allow(clippy::cast_possible_truncation, reason = "under 64 KiB / 8")]
        {
            h.off = (((off - hlen) >> 3) as u16) + ip.off;
        }
        if off + flen < ip_len {
            h.off |= MF;
        }
        h.len = u16::try_from(flen + mhlen).unwrap_or(u16::MAX);
        let data = m.data.copy(off, flen).map_err(|_| {
            st.ipstat.inc(ips::ODROPPED);
            Errno::ENOBUFS
        })?;
        let mut f = Packet { hdr: m.hdr, data };
        if sum {
            f.hdr.csum_flags &= !csum::IP;
        }
        f.prepend(mhlen)
            .map_err(|_| Errno::ENOBUFS)?
            .copy_from_slice(&header_bytes(&h, &later, sum));
        out.push(f);
        off += len;
        nfrags += 1;
    }
    st.ipstat.add(ips::OFRAGMENTS, nfrags);
    let _ = m
        .data
        .split_off(hlen + firstlen)
        .map_err(|_| Errno::ENOBUFS)?;
    let mut h = *ip;
    h.len = u16::try_from(hlen + firstlen).unwrap_or(u16::MAX);
    h.off = ip.off | MF;
    if sum {
        m.hdr.csum_flags &= !csum::IP;
    }
    m.data
        .copy_in(0, &header_bytes(&h, opts, sum))
        .map_err(|_| Errno::ENOBUFS)?;
    out.insert(0, m);
    Ok(out)
}

/// `ip_forward`: a packet for another host, with its TTL taken down one;
/// ICMP time exceeded at the end of its life, unreachable when it cannot
/// go on, and a redirect when it leaves by the interface it came in on
/// for a gateway on its sender's network.
fn forward(vnet: &Arc<Vnet>, st: &State, m: Packet, mut ip: Ip) {
    if m.hdr.flags & (flags::BCAST | flags::MCAST) != 0 || !can_forward(&ip.dst) {
        st.ipstat.inc(ips::CANTFORWARD);
        return;
    }
    if ip.ttl <= 1 {
        icmp::error(
            vnet,
            m,
            icmp::ty::TIMXCEED,
            icmp::code::TIMXCEED_INTRANS,
            [0; 4],
            0,
        );
        return;
    }
    // The start of the packet, for an error about it.
    let quote = m
        .data
        .copy(0, m.len().min(MHLEN as usize))
        .ok()
        .map(|data| Packet { hdr: m.hdr, data });
    let rcvif = m.hdr.rcvif;
    // A redirect if it goes back out where it came in, to a host on its
    // sender's network.
    // The route first (`ip_findroute`): none is unreachable; a blackhole
    // drops it, a reject answers it, neither forwarding it.
    let route = vnet
        .net
        .lock()
        .route_in(m.hdr.fib, Family::Inet, &from_v4(ip.dst))
        .map(|r| r.flags);
    match route {
        None => {
            st.ipstat.inc(ips::NOROUTE);
            st.ipstat.inc(ips::CANTFORWARD);
            icmp::error(
                vnet,
                m,
                icmp::ty::UNREACH,
                icmp::code::UNREACH_HOST,
                [0; 4],
                0,
            );
            return;
        }
        Some(f) if f & rtf::BLACKHOLE != 0 => {
            st.ipstat.inc(ips::CANTFORWARD);
            return;
        }
        Some(f) if f & rtf::REJECT != 0 => {
            st.ipstat.inc(ips::CANTFORWARD);
            icmp::error(
                vnet,
                m,
                icmp::ty::UNREACH,
                icmp::code::UNREACH_HOST,
                [0; 4],
                0,
            );
            return;
        }
        Some(_) => {}
    }
    let (redirect, out_mtu) = {
        let n = vnet.net.lock();
        match n.route_in(m.hdr.fib, Family::Inet, &from_v4(ip.dst)) {
            Some(r) if r.flags & (rtf::REJECT | rtf::BLACKHOLE) == 0 => {
                let ifmtu = n.by_index(r.ifindex).map_or(0, |i| i.mtu);
                let mut redirect = None;
                if on(&st.conf.redirect) && r.ifindex == rcvif {
                    let ia = n
                        .by_index(r.ifindex)
                        .and_then(|i| addrs4(i).find(|a| a.addr == r.ifa).cloned());
                    if let Some(ia) = ia
                        && prefix_match(Family::Inet, &ia.addr, &from_v4(ip.src), ia.plen)
                    {
                        redirect = match r.gateway {
                            Gateway::Addr(g) if r.flags & rtf::GATEWAY != 0 => Some(v4_of(&g)),
                            _ if r.flags & rtf::GATEWAY == 0 => Some(ip.dst),
                            _ => None,
                        };
                    }
                }
                (redirect, ifmtu)
            }
            _ => (None, 0),
        }
    };
    ip.ttl -= 1;
    let mut m = m;
    let hlen = usize::from(ip.hl);
    let mut opts = [0u8; 40];
    let opts = &mut opts[..hlen.saturating_sub(HDR_LEN)];
    m.data.copy_out(HDR_LEN, opts);
    if m.data.copy_in(0, &header_bytes(&ip, opts, true)).is_err() {
        return;
    }
    m.hdr.flags &= !(flags::BCAST | flags::MCAST);
    let o = Opts {
        flags: out::FORWARDING,
        mcast: None,
        fib: m.hdr.fib,
        pcp: None,
        router_alert: false,
        flowid: m.hdr.flowid,
    };
    let r = output(vnet, m, &o);
    match r {
        Ok(()) => {
            st.ipstat.inc(ips::FORWARD);
            // `ip_tryforward` takes what has no options and needs no
            // redirect, and counts it fast-forwarded too.
            if hlen == HDR_LEN && redirect.is_none() {
                st.ipstat.inc(ips::FASTFORWARD);
            }
            if let Some(gw) = redirect {
                st.ipstat.inc(ips::REDIRECTSENT);
                if let Some(q) = quote {
                    icmp::error(
                        vnet,
                        q,
                        icmp::ty::REDIRECT,
                        icmp::code::REDIRECT_HOST,
                        gw,
                        0,
                    );
                }
            }
            return;
        }
        Err(_) => st.ipstat.inc(ips::CANTFORWARD),
    }
    let Some(q) = quote else {
        return;
    };
    let (code, mtu) = match r {
        Err(Errno::EMSGSIZE) => {
            st.ipstat.inc(ips::CANTFRAG);
            (icmp::code::UNREACH_NEEDFRAG, out_mtu)
        }
        Err(Errno::ENOBUFS | Errno::EACCES) => return,
        _ => (icmp::code::UNREACH_HOST, 0),
    };
    icmp::error(vnet, q, icmp::ty::UNREACH, code, [0; 4], mtu);
}

/// A redirect learned from ICMP (`rib_add_redirect`): a host route to
/// `dst` through `gw`, expiring after `secs`.
pub(crate) fn add_redirect(
    n: &mut Net,
    dst: [u8; 4],
    gw: [u8; 4],
    ifindex: u16,
    src: [u8; 4],
    secs: u64,
    now: u64,
) {
    let r = net::ifnet::Route {
        family: Family::Inet,
        dst: from_v4(dst),
        plen: 32,
        gateway: Gateway::Addr(from_v4(gw)),
        flags: rtf::UP | rtf::GATEWAY | rtf::HOST | RTF_DYNAMIC,
        ifindex,
        ifa: from_v4(src),
        mtu: 0,
        weight: 1,
        expire: now + secs,
        nhidx: 0,
        fib: 0,
    };
    let _ = n.delete_route(Family::Inet, &from_v4(dst), 32);
    let _ = n.add_route(r);
}
