// SPDX-License-Identifier: BSD-2-Clause
//! IPv6 (docs/NETWORKING.md §8): input with its checks, the decision
//! whether a packet is this host's, the extension headers (hop-by-hop
//! and destination options, routing, fragments), delivery to the
//! protocol; output with the route, scope zones, multicast loopback, the
//! checksums the interface does not take, and fragmentation; forwarding
//! when `net.inet6.ip6.forwarding` is on.
//!
//! A packet's IPv6 header stays on it up to the protocol, as FreeBSD's
//! does; an offset says where the protocol's part starts.
//!
//! Behavior reference: sys/netinet6/ip6_input.c (`ip6_input`,
//! `ip6_hopopts_input`, `ip6_process_hopopts`, `ip6_unknown_opt`,
//! `ip6_get_prevhdr`, `ip6_lasthdr`), sys/netinet6/dest6.c,
//! sys/netinet6/route6.c, sys/netinet6/ip6_output.c (`ip6_output`,
//! `ip6_fragment`, `ip6_mloopback`, `in6_delayed_cksum`, `ip6_getpmtu`),
//! sys/netinet6/ip6_forward.c (`ip6_forward`), sys/netinet6/nd6.c
//! (`nd6_output_ifp`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::net::{IFT_LOOP, ifcap, iff, rtf};
use mbuf::{Packet, csum, flags};
use net::addr::{In6, is_multicast6, is_unspecified, is_v4mapped};
use net::ifnet::{Gateway, Net, in6_iff};
use net::{Family, Vnet};

use crate::in6::{self, is_loopback, is_zoned, mc_scope, scope};
use crate::state::{self, State, icp6s, ip6s, on, val};
use crate::{cksum, icmp6};

/// `IPV6_VERSION`, `sizeof(struct ip6_hdr)`, `IPV6_MAXPACKET`,
/// `IPV6_MMTU`, `ICMPV6_PLD_MAXLEN`.
pub const VERSION: u8 = 6;
pub const HDR_LEN: usize = 40;
pub const MAXPACKET: usize = 65535;
pub const MMTU: u32 = 1280;
pub const PLD_MAXLEN: usize = 1232;
/// `IPV6_DEFHLIM`.
pub const DEFHLIM: u8 = 64;
/// `IPV6_FLOWLABEL_MASK`, in host order.
pub const FLOWLABEL_MASK: u32 = 0x000f_ffff;

/// The next headers IPv6 handles here (`IPPROTO_*`).
pub mod proto {
    pub const HOPOPTS: u8 = 0;
    pub const TCP: u8 = 6;
    pub const UDP: u8 = 17;
    pub const ROUTING: u8 = 43;
    pub const FRAGMENT: u8 = 44;
    pub const ESP: u8 = 50;
    pub const AH: u8 = 51;
    pub const ICMPV6: u8 = 58;
    pub const NONE: u8 = 59;
    pub const DSTOPTS: u8 = 60;
}

/// Hop-by-hop and destination option types (`IP6OPT_*`).
pub mod opt {
    pub const PAD1: u8 = 0x00;
    pub const PADN: u8 = 0x01;
    pub const JUMBO: u8 = 0xc2;
    pub const ROUTER_ALERT: u8 = 0x05;
    /// `IP6OPT_RTALERT_LEN`, `IP6OPT_MINLEN`.
    pub const RTALERT_LEN: usize = 4;
    pub const MINLEN: usize = 2;
    /// `IP6OPT_RTALERT_MLD`.
    pub const RTALERT_MLD: u16 = 0;
}

/// An IPv6 header (`struct ip6_hdr`), its fields in host order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ip6 {
    /// Version, traffic class, and flow label (`ip6_flow`).
    pub flow: u32,
    pub plen: u16,
    pub nxt: u8,
    pub hlim: u8,
    pub src: In6,
    pub dst: In6,
}

impl mbuf::Header for Ip6 {
    const LEN: usize = HDR_LEN;

    fn parse(b: &[u8]) -> Option<Self> {
        Some(Self {
            flow: u32::from_be_bytes(b.get(0..4)?.try_into().ok()?),
            plen: u16::from_be_bytes([b[4], b[5]]),
            nxt: b[6],
            hlim: b[7],
            src: b.get(8..24)?.try_into().ok()?,
            dst: b.get(24..40)?.try_into().ok()?,
        })
    }
}

impl Ip6 {
    /// A header of version 6 with traffic class `tclass`.
    #[must_use]
    pub const fn new(nxt: u8, src: In6, dst: In6, plen: u16, hlim: u8, tclass: u8) -> Self {
        Self {
            flow: (6 << 28) | ((tclass as u32) << 20),
            plen,
            nxt,
            hlim,
            src,
            dst,
        }
    }

    #[must_use]
    pub const fn version(&self) -> u8 {
        (self.flow >> 28) as u8
    }

    /// `IPV6_TCLASS`'s traffic class.
    #[must_use]
    pub const fn tclass(&self) -> u8 {
        ((self.flow >> 20) & 0xff) as u8
    }

    #[must_use]
    pub fn bytes(&self) -> [u8; HDR_LEN] {
        let mut b = [0u8; HDR_LEN];
        b[0..4].copy_from_slice(&self.flow.to_be_bytes());
        b[4..6].copy_from_slice(&self.plen.to_be_bytes());
        b[6] = self.nxt;
        b[7] = self.hlim;
        b[8..24].copy_from_slice(&self.src);
        b[24..40].copy_from_slice(&self.dst);
        b
    }
}

/// The IPv6 pseudo-header's sum (`in6_cksum_pseudo`), unfolded.
#[must_use]
pub fn pseudo(src: &In6, dst: &In6, nxt: u8, len: usize) -> u64 {
    cksum::sum(src) + cksum::sum(dst) + len as u64 + u64::from(nxt)
}

/// `in6_cksum`: the checksum of the `len` bytes of transport protocol
/// `nxt` at `off` in `m`, with the pseudo-header of its IPv6 header; 0
/// when they are intact.
#[must_use]
pub fn cksum6(m: &Packet, nxt: u8, off: usize, len: usize) -> u16 {
    let mut h = [0u8; HDR_LEN];
    m.data.copy_out(0, &mut h);
    let src: In6 = h[8..24].try_into().unwrap_or([0; 16]);
    let dst: In6 = h[24..40].try_into().unwrap_or([0; 16]);
    !cksum::fold(pseudo(&src, &dst, nxt, len) + cksum::sum_chain(&m.data, off, len))
}

/// A header's length from its second byte: extension headers count 8
/// bytes past the first 8, AH 4 bytes past the first 8.
fn ext_len(nxt: u8, len_byte: u8) -> usize {
    if nxt == proto::AH {
        (usize::from(len_byte) + 2) << 2
    } else {
        (usize::from(len_byte) + 1) << 3
    }
}

/// `ip6_get_prevhdr`: the offset of the next-header field naming the
/// header at `off`.
#[must_use]
pub fn prevhdr(m: &Packet, off: usize) -> usize {
    if off == HDR_LEN {
        return 6;
    }
    let mut b = [0u8; 2];
    m.data.copy_out(6, &mut b[..1]);
    let mut nxt = b[0];
    let mut len = HDR_LEN;
    let mut prev = 6;
    while len < off {
        if m.data.copy_out(len, &mut b) != 2 {
            break;
        }
        prev = len;
        let l = if nxt == proto::FRAGMENT {
            8
        } else {
            ext_len(nxt, b[1])
        };
        nxt = b[0];
        len += l;
    }
    prev
}

/// `ip6_lasthdr`: past the extension headers from the IPv6 header, the
/// offset of the upper protocol's header and its number; `None` for a
/// malformed chain.
#[must_use]
pub fn lasthdr(m: &Packet) -> Option<(usize, u8)> {
    let mut b = [0u8; 8];
    if m.data.copy_out(0, &mut b[..7]) != 7 {
        return None;
    }
    let mut nxt = b[6];
    let mut off = HDR_LEN;
    loop {
        match nxt {
            proto::HOPOPTS | proto::DSTOPTS | proto::ROUTING | proto::AH => {
                if m.data.copy_out(off, &mut b[..2]) != 2 {
                    return None;
                }
                off += ext_len(nxt, b[1]);
                nxt = b[0];
            }
            proto::FRAGMENT => {
                if m.data.copy_out(off, &mut b) != 8 {
                    return None;
                }
                // Past a later fragment's header nothing is known.
                if u16::from_be_bytes([b[2], b[3]]) & 0xfff8 != 0 {
                    return None;
                }
                nxt = b[0];
                off += 8;
            }
            _ => return Some((off, nxt)),
        }
        if off > m.len() {
            return None;
        }
    }
}

/// What `ip6_unknown_opt` does with an option of unknown type.
enum OptAct {
    /// Skip it.
    Skip,
    /// Drop the packet silently.
    Drop,
    /// Drop it with a parameter problem.
    Icmp,
    /// The same, unless it was multicast.
    IcmpUnlessMcast,
}

/// `ip6_unknown_opt`: an unknown option's type says what to do.
const fn unknown_opt(t: u8) -> OptAct {
    match t & 0xc0 {
        0x00 => OptAct::Skip,
        0x40 => OptAct::Drop,
        0x80 => OptAct::Icmp,
        _ => OptAct::IcmpUnlessMcast,
    }
}

/// Carries out a drop [`unknown_opt`] decided, for the option at `off`.
fn unknown_drop(vnet: &Arc<Vnet>, st: &State, m: Packet, act: &OptAct, off: usize) {
    let send = match act {
        OptAct::Skip | OptAct::Drop => false,
        OptAct::Icmp => true,
        OptAct::IcmpUnlessMcast => {
            let mut d = [0u8; 1];
            m.data.copy_out(24, &mut d);
            m.hdr.flags & (flags::BCAST | flags::MCAST) == 0 && d[0] != 0xff
        }
    };
    if matches!(act, OptAct::Icmp | OptAct::IcmpUnlessMcast) {
        st.ip6stat.inc(ip6s::BADOPTIONS);
    }
    if send {
        icmp6::error(
            vnet,
            m,
            icmp6::ty::PARAM_PROB,
            icmp6::code::PARAMPROB_OPTION,
            off,
        );
    }
}

/// `ip6_hopopts_input` and `ip6_process_hopopts`: the hop-by-hop
/// options at `off`; the packet, the offset past them, the next header,
/// and the Router Alert value if there is one, or `None` if the packet
/// was dropped.
fn hopopts(
    vnet: &Arc<Vnet>,
    st: &State,
    m: Packet,
    off: usize,
) -> Option<(Packet, usize, u8, Option<u16>)> {
    let mut h = [0u8; 2];
    if m.data.copy_out(off, &mut h) != 2 {
        st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
        return None;
    }
    let hbhlen = ext_len(proto::HOPOPTS, h[1]);
    if off + hbhlen > m.len() {
        st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
        return None;
    }
    let mut opts = alloc::vec![0u8; hbhlen - 2];
    m.data.copy_out(off + 2, &mut opts);
    let erroff = HDR_LEN + 2;
    let mut rtalert = None;
    let mut at = 0usize;
    while at < opts.len() {
        let left = opts.len() - at;
        let t = opts[at];
        at += match t {
            opt::PAD1 => 1,
            opt::PADN => {
                if left < opt::MINLEN {
                    st.ip6stat.inc(ip6s::TOOSMALL);
                    return None;
                }
                usize::from(opts[at + 1]) + 2
            }
            opt::ROUTER_ALERT => {
                if left < opt::RTALERT_LEN {
                    st.ip6stat.inc(ip6s::TOOSMALL);
                    return None;
                }
                if usize::from(opts[at + 1]) != opt::RTALERT_LEN - 2 {
                    icmp6::error(
                        vnet,
                        m,
                        icmp6::ty::PARAM_PROB,
                        icmp6::code::PARAMPROB_HEADER,
                        erroff + at + 1,
                    );
                    return None;
                }
                rtalert = Some(u16::from_be_bytes([opts[at + 2], opts[at + 3]]));
                opt::RTALERT_LEN
            }
            // No Jumbo Payload here.
            opt::JUMBO => return None,
            _ => {
                if left < opt::MINLEN {
                    st.ip6stat.inc(ip6s::TOOSMALL);
                    return None;
                }
                let act = unknown_opt(t);
                if !matches!(act, OptAct::Skip) {
                    unknown_drop(vnet, st, m, &act, erroff + at);
                    return None;
                }
                usize::from(opts[at + 1]) + 2
            }
        };
    }
    Some((m, off + hbhlen, h[0], rtalert))
}

/// Where a received packet goes.
enum Dest {
    Ours,
    Forward,
    /// Dropped, counted against the statistic.
    Drop(Option<usize>),
}

/// A received IPv6 packet (`ip6_input`), by way of `netisr`'s
/// `NETISR_IPV6`: its interface is `m.hdr.rcvif`.
pub fn input(vnet: &Arc<Vnet>, mut m: Packet) {
    let st = state::of(vnet);
    // `ip6s_m1`, `ip6s_m2m`, `ip6s_mext*`: how the packet was held.
    if m.data.segments().count() > 1 {
        st.ip6stat.inc(ip6s::MEXT2M);
    } else {
        st.ip6stat.inc(ip6s::MEXT1);
    }
    st.ip6stat.inc(ip6s::TOTAL);
    let Ok(ip6) = m.header::<Ip6>(0) else {
        st.ip6stat.inc(ip6s::TOOSMALL);
        return;
    };
    if ip6.version() != VERSION {
        st.ip6stat.inc(ip6s::BADVERS);
        return;
    }
    st.ip6stat.inc(ip6s::NXTHIST + usize::from(ip6.nxt));
    let rcvif = m.hdr.rcvif;
    let looped = m.hdr.flags & flags::LOOP != 0;
    // Spoofed or corrupt addresses.
    if is_multicast6(&ip6.src)
        || (is_multicast6(&ip6.dst) && mc_scope(&ip6.dst) == scope::INTFACELOCAL && !looped)
        || (is_multicast6(&ip6.dst) && mc_scope(&ip6.dst) == 0)
        || is_v4mapped(&ip6.src)
        || is_v4mapped(&ip6.dst)
        || is_unspecified(&ip6.dst)
    {
        st.ip6stat.inc(ip6s::BADSCOPE);
        return;
    }
    let plen = usize::from(ip6.plen);
    // No jumbograms.
    if plen == 0 {
        st.ip6stat.inc(ip6s::TOOSHORT);
        return;
    }
    let dest = {
        let n = vnet.net.lock();
        let Some(ifp) = n.by_index(rcvif) else {
            return;
        };
        // `in6_setscope`: the loopback address only on a loopback
        // interface.
        if in6::zone_on(&ip6.src, ifp).is_none() || in6::zone_on(&ip6.dst, ifp).is_none() {
            st.ip6stat.inc(ip6s::BADSCOPE);
            return;
        }
        classify(&n, &st, &ip6, rcvif, looped)
    };
    let mut ours = match dest {
        Dest::Ours => true,
        Dest::Forward => false,
        Dest::Drop(stat) => {
            if let Some(s) = stat {
                st.ip6stat.inc(s);
            }
            return;
        }
    };
    let (mut m, off, nxt) = if ip6.nxt == proto::HOPOPTS {
        let Some((m, off, nxt, rtalert)) = hopopts(vnet, &st, m, HDR_LEN) else {
            return;
        };
        let mut m = m;
        if let Some(v) = rtalert {
            // A router hears MLD's own messages.
            if v == opt::RTALERT_MLD && on(&st.conf6.forwarding) {
                ours = true;
            }
            m.hdr.flags |= flags::RTALERT_MLD;
        }
        (m, off, nxt)
    } else {
        (m, HDR_LEN, ip6.nxt)
    };
    if m.len() - HDR_LEN < plen {
        st.ip6stat.inc(ip6s::TOOSHORT);
        return;
    }
    if m.len() > HDR_LEN + plen && m.data.split_off(HDR_LEN + plen).is_err() {
        return;
    }
    if !ours {
        forward(vnet, &st, m, false);
        return;
    }
    st.ip6stat.inc(ip6s::DELIVERED);
    deliver(vnet, &st, m, off, nxt);
}

/// `ip6_input`'s address checks: multicast is this host's (its
/// protocols filter by membership), as is one of its addresses that is
/// ready; anything else is forwarded by a router.
fn classify(n: &Net, st: &State, ip6: &Ip6, rcvif: u16, looped: bool) -> Dest {
    if is_multicast6(&ip6.dst) {
        return Dest::Ours;
    }
    if let Some((_, f)) = in6::ifa_with(n, &ip6.dst, rcvif) {
        if f & in6_iff::NOTREADY != 0 {
            return Dest::Drop(None);
        }
        // `net.inet6.ip6.source_address_validation`.
        if on(&st.conf6.sav) && !looped && in6::is_local(n, &ip6.src) {
            return Dest::Drop(Some(ip6s::BADSCOPE));
        }
        return Dest::Ours;
    }
    if !on(&st.conf6.forwarding) {
        return Dest::Drop(Some(ip6s::CANTFORWARD));
    }
    Dest::Forward
}

/// The protocols for the header at `off` (`ip6_protox`), until one takes
/// the packet.
fn deliver(vnet: &Arc<Vnet>, st: &State, mut m: Packet, mut off: usize, mut nxt: u8) {
    let limit = val(&st.conf6.hdrnestlimit);
    let mut nest = 0;
    loop {
        nest += 1;
        if limit != 0 && nest > limit {
            st.ip6stat.inc(ip6s::TOOMANYHDR);
            return;
        }
        if m.len() < off {
            st.ip6stat.inc(ip6s::TOOSHORT);
            return;
        }
        let next = match nxt {
            proto::DSTOPTS => dest6(vnet, st, m, off),
            proto::ROUTING => route6(vnet, st, m, off),
            proto::FRAGMENT => crate::frag6::input(vnet, st, m, off),
            proto::ICMPV6 => {
                icmp6::input(vnet, m, off);
                return;
            }
            proto::UDP => {
                crate::udp::input6(vnet, m, off);
                return;
            }
            // TCP on the wire is P7.6's; raw sockets still see it.
            proto::TCP => {
                crate::raw6::input(vnet, m, off, nxt, true);
                return;
            }
            _ => {
                crate::raw6::input(vnet, m, off, nxt, false);
                return;
            }
        };
        match next {
            Some((m2, off2, nxt2)) => {
                m = m2;
                off = off2;
                nxt = nxt2;
            }
            None => return,
        }
    }
}

/// `dest6_input`: destination options, each known one skipped and an
/// unknown one handled as its type says.
fn dest6(vnet: &Arc<Vnet>, st: &State, m: Packet, off: usize) -> Option<(Packet, usize, u8)> {
    let mut h = [0u8; 2];
    if m.data.copy_out(off, &mut h) != 2 {
        st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
        return None;
    }
    let len = ext_len(proto::DSTOPTS, h[1]);
    if off + len > m.len() {
        st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
        return None;
    }
    let mut opts = alloc::vec![0u8; len - 2];
    m.data.copy_out(off + 2, &mut opts);
    let mut at = 0usize;
    while at < opts.len() {
        let left = opts.len() - at;
        let t = opts[at];
        if t != opt::PAD1 && (left < opt::MINLEN || usize::from(opts[at + 1]) + 2 > left) {
            st.ip6stat.inc(ip6s::TOOSMALL);
            return None;
        }
        at += match t {
            opt::PAD1 => 1,
            opt::PADN => usize::from(opts[at + 1]) + 2,
            _ => {
                let act = unknown_opt(t);
                if !matches!(act, OptAct::Skip) {
                    unknown_drop(vnet, st, m, &act, off + 2 + at);
                    return None;
                }
                usize::from(opts[at + 1]) + 2
            }
        };
    }
    Some((m, off + len, h[0]))
}

/// `route6_input`: a routing header with segments left is a parameter
/// problem (none are processed here, type 0 being deprecated); with
/// none left it is passed over.
fn route6(vnet: &Arc<Vnet>, st: &State, m: Packet, off: usize) -> Option<(Packet, usize, u8)> {
    let mut h = [0u8; 4];
    if m.data.copy_out(off, &mut h) != 4 {
        st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
        return None;
    }
    if h[3] != 0 {
        st.ip6stat.inc(ip6s::BADOPTIONS);
        icmp6::error(
            vnet,
            m,
            icmp6::ty::PARAM_PROB,
            icmp6::code::PARAMPROB_HEADER,
            off + 2,
        );
        return None;
    }
    Some((m, off + ext_len(proto::ROUTING, h[1]), h[0]))
}

/// `ip6_output`'s flags.
pub mod out {
    /// A packet being forwarded.
    pub const FORWARDING: u32 = 0x02;
    /// `IPV6_UNSPECSRC`: `::` may be its source (DAD, MLD before an
    /// address is ready).
    pub const UNSPECSRC: u32 = 0x01;
    /// `IPV6_MINMTU`.
    pub const MINMTU: u32 = 0x04;
    /// `IP6PO_DONTFRAG`.
    pub const DONTFRAG: u32 = 0x08;
}

/// `IP6PO_MINMTU_*` (`IPV6_USE_MIN_MTU`).
pub mod minmtu {
    pub const MCASTONLY: i8 = -1;
    pub const DISABLE: i8 = 0;
    pub const ALL: i8 = 1;
}

/// A socket's multicast choices (`struct ip6_moptions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mopts6 {
    /// `IPV6_MULTICAST_IF`'s interface, 0 for the route's.
    pub ifindex: u16,
    pub hlim: u8,
    pub loop_: bool,
}

/// What `ip6_output` is told besides the packet: the socket's and the
/// packet's options (`struct ip6_pktopts`, `struct ip6_moptions`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts6 {
    /// [`out`] bits.
    pub flags: u32,
    pub mcast: Option<Mopts6>,
    pub fib: u16,
    pub pcp: Option<u8>,
    /// The packet's flow (`m_pkthdr.flowid`).
    pub flowid: u32,
    /// `IPV6_HOPLIMIT` or the socket's sticky one (`ip6po_hlim`).
    pub hlim: Option<u8>,
    /// `IPV6_TCLASS` (`ip6po_tclass`).
    pub tclass: Option<u8>,
    /// `IPV6_PKTINFO`'s interface (`ipi6_ifindex`), 0 for none.
    pub ifindex: u16,
    /// `IPV6_NEXTHOP`.
    pub nexthop: Option<In6>,
    /// The destination's and source's zones (interface indexes) when
    /// they are link-local.
    pub zone: u16,
    pub src_zone: u16,
    /// MLD's Router Alert hop-by-hop option goes on (`mld_po`).
    pub router_alert: bool,
    /// `IPV6_USE_MIN_MTU` ([`minmtu`]).
    pub minmtu: i8,
}

/// MLD's hop-by-hop header: a PadN of 0 and Router Alert for MLD
/// (`mld_raopt`); its first byte is the next header.
const MLD_HBH: [u8; 8] = [0, 0, opt::PADN, 0, opt::ROUTER_ALERT, 2, 0, 0];

/// What the interface does of the checksum work (`if_hwassist`): a
/// loopback interface with IPv6 transmit checksums does all of it.
fn hwassist6(i: &net::ifnet::Ifnet) -> u32 {
    if i.ty == IFT_LOOP && i.capenable & ifcap::TXCSUM_IPV6 != 0 {
        csum::IP6_UDP | csum::IP6_TCP
    } else {
        0
    }
}

/// `in6_delayed_cksum`: the transport checksum the device would have
/// finished, for the protocol at `off`; the field holds the
/// pseudo-header's sum.
pub(crate) fn delayed_cksum(m: &mut Packet, off: usize) {
    let mut c = !cksum::fold(cksum::sum_chain(&m.data, off, m.len().saturating_sub(off)));
    if c == 0 && m.hdr.csum_flags & csum::IP6_UDP != 0 {
        c = 0xffff;
    }
    let at = off + m.hdr.csum_data as usize;
    let _ = m.data.copy_in(at, &c.to_be_bytes());
    m.hdr.csum_flags &= !(csum::IP6_UDP | csum::IP6_TCP);
}

/// Sends IPv6 packet `m`, its header first (`ip6_output`): the source,
/// destination, next header, hop limit, and traffic class set; the
/// payload length is filled in.
///
/// # Errors
///
/// `EOPNOTSUPP` for an unspecified source without `IPV6_UNSPECSRC` or a
/// multicast one; `EHOSTUNREACH` without a route or across zones;
/// `ENETUNREACH` for multicast out of an interface without it;
/// `EMSGSIZE` over the MTU when it may not be fragmented; `ENETDOWN`,
/// `ENOBUFS`, and neighbor discovery's errors from the interface.
pub fn output(vnet: &Arc<Vnet>, m: Packet, o: &Opts6) -> Result<(), Errno> {
    let st = state::of(vnet);
    let mut n = vnet.net.lock();
    output_in(&mut n, &st, m, o)
}

/// [`output`] under the interfaces' lock (neighbor discovery and MLD
/// send this way).
pub(crate) fn output_in(n: &mut Net, st: &State, m: Packet, o: &Opts6) -> Result<(), Errno> {
    output_if(n, st, m, o).map(|_| ())
}

/// [`output_in`], telling which interface the packet left by (`*ifpp`),
/// 0 for none.
pub(crate) fn output_if(n: &mut Net, st: &State, mut m: Packet, o: &Opts6) -> Result<u16, Errno> {
    let mut ip6 = m.header::<Ip6>(0).map_err(|_| Errno::EINVAL)?;
    if is_unspecified(&ip6.src) && o.flags & out::UNSPECSRC == 0 {
        st.ip6stat.inc(ip6s::BADSCOPE);
        return Err(Errno::EOPNOTSUPP);
    }
    if is_multicast6(&ip6.src) {
        st.ip6stat.inc(ip6s::BADSCOPE);
        return Err(Errno::EOPNOTSUPP);
    }
    // Extension headers the options add (only MLD's hop-by-hop here).
    let mut optlen = 0usize;
    if o.router_alert && o.flags & out::FORWARDING == 0 {
        let mut hbh = MLD_HBH;
        hbh[0] = ip6.nxt;
        m.data.trim_front(HDR_LEN);
        m.prepend(HDR_LEN + hbh.len()).map_err(|_| Errno::ENOBUFS)?[HDR_LEN..]
            .copy_from_slice(&hbh);
        ip6.nxt = proto::HOPOPTS;
        optlen = hbh.len();
    }
    let plen = m.len() - HDR_LEN;
    ip6.plen = u16::try_from(plen).map_err(|_| Errno::EMSGSIZE)?;
    if o.flags & out::FORWARDING == 0 {
        st.ip6stat.inc(ip6s::LOCALOUT);
    }
    if let Some(tc) = o.tclass {
        // Only the parts the header has not set.
        let mut mask = 0u32;
        if ip6.tclass() & 0xfc == 0 {
            mask |= 0xfc;
        }
        if ip6.tclass() & 0x03 == 0 {
            mask |= 0x03;
        }
        ip6.flow |= (u32::from(tc) & mask) << 20;
    }
    if let Some(h) = o.hlim {
        ip6.hlim = h;
    } else if is_multicast6(&ip6.dst) {
        ip6.hlim = o.mcast.map_or_else(|| default_mcast_hlim(st), |mo| mo.hlim);
    }
    // The interface, the one whose address or link it is, and the next
    // hop.
    let Some((ifindex, origif, nexthop, rmtu)) = select_route(n, st, &ip6, o)? else {
        return Ok(0);
    };
    let Some(ifp) = n.by_index(ifindex) else {
        return Err(Errno::EHOSTUNREACH);
    };
    let (ifflags, assist, linkmtu) = (ifp.flags, hwassist6(ifp), ifp.linkmtu6());
    let origifp_loop = n
        .by_index(origif)
        .is_some_and(|i| i.flags & iff::LOOPBACK != 0);
    // Scope zones: the source and destination must be in the zones of
    // the interface the packet is for, unless it loops back.
    let src_zone = if is_zoned(&ip6.src) {
        if o.src_zone != 0 {
            o.src_zone
        } else {
            in6::ifa_with(n, &ip6.src, origif).map_or(0, |(i, _)| i)
        }
    } else {
        0
    };
    let dst_zone = if is_zoned(&ip6.dst) { o.zone } else { 0 };
    let src_ok = !is_zoned(&ip6.src) || src_zone == origif;
    let dst_ok = !is_zoned(&ip6.dst) || dst_zone == origif;
    let loop_ok = |n: &Net| {
        origifp_loop
            && dst_zone != 0
            && (src_zone == 0 || src_zone == dst_zone)
            && n.by_index(dst_zone).is_some()
    };
    if !(src_ok && dst_ok) && !loop_ok(n) {
        st.ip6stat.inc(ip6s::BADSCOPE);
        return Err(Errno::EHOSTUNREACH);
    }
    if is_multicast6(&ip6.dst) {
        m.hdr.flags = (m.hdr.flags & !flags::BCAST) | flags::MCAST;
        if ifflags & iff::MULTICAST == 0 {
            st.ip6stat.inc(ip6s::NOROUTE);
            return Err(Errno::ENETUNREACH);
        }
        // `ip6_mloopback`: a copy goes up as received on the interface;
        // the protocols drop it unless some socket joined the group.
        if o.mcast.is_none_or(|mo| mo.loop_) {
            let mut c = m.copy().map_err(|_| Errno::ENOBUFS)?;
            c.data
                .copy_in(0, &ip6.bytes())
                .map_err(|_| Errno::ENOBUFS)?;
            if c.hdr.csum_flags & (csum::IP6_UDP | csum::IP6_TCP) != 0 {
                c.hdr.csum_flags |= csum::DATA_VALID | csum::PSEUDO_HDR;
                c.hdr.csum_data = 0xffff;
            }
            c.hdr.rcvif = ifindex;
            c.hdr.flags |= flags::LOOP;
            n.isrq.push((net::netisr::Proto::Ipv6, c));
        }
        if ip6.hlim == 0
            || ifflags & iff::LOOPBACK != 0
            || mc_scope(&ip6.dst) == scope::INTFACELOCAL
        {
            return Ok(ifindex);
        }
    } else {
        m.hdr.flags &= !(flags::BCAST | flags::MCAST);
    }
    // `ip6_getpmtu`: the route's MTU (or the host cache's, lower), else
    // the link's.
    let mut mtu = if rmtu != 0 {
        let cached = crate::nd6::pmtu_get(st, &ip6.dst);
        if cached != 0 { cached.min(rmtu) } else { rmtu }
    } else {
        linkmtu
    };
    if mtu > MMTU
        && (o.flags & out::MINMTU != 0
            || o.minmtu == minmtu::ALL
            || (is_multicast6(&ip6.dst) && o.minmtu != minmtu::DISABLE))
    {
        mtu = MMTU;
    }
    if let Some(pcp) = o.pcp {
        m.hdr.vlan_tag = u16::from(pcp & 7) << 13;
        m.hdr.flags |= flags::VLANTAG;
    }
    let toff = HDR_LEN + optlen;
    if m.hdr.csum_flags & (csum::IP6_UDP | csum::IP6_TCP) & !assist != 0 {
        delayed_cksum(&mut m, toff);
    }
    m.data
        .copy_in(0, &ip6.bytes())
        .map_err(|_| Errno::ENOBUFS)?;
    let tlen = m.len();
    let dontfrag = o.flags & out::DONTFRAG != 0;
    if dontfrag && tlen > linkmtu as usize {
        return Err(Errno::EMSGSIZE);
    }
    // `nd6_output_ifp`: a loopback interface carries it for the
    // interface whose address it is.
    let link_for = if ifflags & iff::LOOPBACK != 0 {
        origif
    } else {
        ifindex
    };
    if dontfrag || tlen <= mtu as usize {
        return send(n, ifindex, link_for, &nexthop, m).map(|()| ifindex);
    }
    if mtu < MMTU {
        return Err(Errno::EMSGSIZE);
    }
    let mtu = (mtu as usize).min(MAXPACKET);
    let len = (mtu - toff - 8) & !7;
    if len < 8 {
        return Err(Errno::EMSGSIZE);
    }
    if m.hdr.csum_flags & (csum::IP6_UDP | csum::IP6_TCP) != 0 {
        delayed_cksum(&mut m, toff);
    }
    let id = crate::random();
    let frags = fragment(st, m, toff, len, id)?;
    let mut r = Ok(());
    for f in frags {
        if r.is_ok() {
            r = send(n, ifindex, link_for, &nexthop, f);
        }
    }
    if r.is_ok() {
        st.ip6stat.inc(ip6s::FRAGMENTED);
    }
    r.map(|()| ifindex)
}

/// `V_ip6_defmcasthlim`.
fn default_mcast_hlim(st: &State) -> u8 {
    u8::try_from(val(&st.conf6.defmcasthlim)).unwrap_or(1)
}

/// `ip6_output_send` and `nd6_output_ifp`: to the interface's output,
/// a loopback interface's packet received as on `link_for`.
fn send(
    n: &mut Net,
    ifindex: u16,
    link_for: u16,
    nexthop: &In6,
    mut m: Packet,
) -> Result<(), Errno> {
    if n.by_index(ifindex).is_some_and(|i| i.ty == IFT_LOOP) {
        m.hdr.rcvif = link_for;
    }
    let is_gw = *nexthop != {
        let mut d = [0u8; 16];
        m.data.copy_out(24, &mut d);
        d
    };
    net::output::output(n, ifindex, Family::Inet6, nexthop, is_gw, m, net::uptime())
}

/// `in6_selectroute` for `ip6_output`: the interface, the interface whose
/// address or link it is (`nh_aifp`), the next hop, and the route's MTU
/// (0 for none); `None` for a blackhole.
type Path6 = Option<(u16, u16, In6, u32)>;

fn select_route(n: &Net, st: &State, ip6: &Ip6, o: &Opts6) -> Result<Path6, Errno> {
    let dst = ip6.dst;
    let mc = is_multicast6(&dst);
    // `IPV6_PKTINFO`'s interface: multicast needs no route; unicast must
    // be routed out of it.
    if o.ifindex != 0 && mc {
        if n.by_index(o.ifindex).is_none() {
            st.ip6stat.inc(ip6s::NOROUTE);
            return Err(Errno::EHOSTUNREACH);
        }
        return Ok(Some((o.ifindex, o.ifindex, dst, 0)));
    }
    if mc
        && let Some(mo) = o.mcast
        && mo.ifindex != 0
    {
        return Ok(Some((mo.ifindex, mo.ifindex, dst, 0)));
    }
    if mc && matches!(mc_scope(&dst), scope::INTFACELOCAL | scope::LINKLOCAL) && o.zone != 0 {
        if n.by_index(o.zone).is_none() {
            return Err(Errno::EHOSTUNREACH);
        }
        return Ok(Some((o.zone, o.zone, dst, 0)));
    }
    // `IPV6_NEXTHOP`: an on-link next hop.
    let target = o.nexthop.unwrap_or(dst);
    let Some(r) = n.route6(o.fib, &target, o.zone) else {
        st.ip6stat.inc(ip6s::NOROUTE);
        return Err(Errno::EHOSTUNREACH);
    };
    let r = crate::ip::mpath_select(n, r, o.flowid);
    if o.nexthop.is_some() && r.flags & rtf::GATEWAY != 0 {
        st.ip6stat.inc(ip6s::NOROUTE);
        return Err(Errno::EHOSTUNREACH);
    }
    let aifp = in6::route_aifp(r);
    if o.ifindex != 0 && aifp != o.ifindex {
        st.ip6stat.inc(ip6s::NOROUTE);
        return Err(Errno::EHOSTUNREACH);
    }
    // `looutput` on a blackhole or reject next hop.
    if r.flags & rtf::BLACKHOLE != 0 {
        return Ok(None);
    }
    if r.flags & rtf::REJECT != 0 {
        return Err(Errno::EHOSTUNREACH);
    }
    let nexthop = match (o.nexthop, r.gateway) {
        (Some(nh), _) => nh,
        (None, Gateway::Addr(g)) if r.flags & rtf::GATEWAY != 0 => g,
        _ => dst,
    };
    let ifmtu = n.by_index(r.ifindex).map_or(0, |i| i.mtu);
    let mtu = if r.mtu == 0 { ifmtu } else { r.mtu.min(ifmtu) };
    Ok(Some((r.ifindex, aifp, nexthop, mtu)))
}

/// `ip6_fragment` with `ip6_insertfraghdr`: `m` (its unfragmentable part
/// `hlen` bytes) cut into fragments of `len` bytes, each behind a copy of
/// that part and a fragment header with identification `id`. Data is
/// shared, not copied.
fn fragment(st: &State, m: Packet, hlen: usize, len: usize, id: u32) -> Result<Vec<Packet>, Errno> {
    let tlen = m.len();
    let mut unfrag = alloc::vec![0u8; hlen];
    m.data.copy_out(0, &mut unfrag);
    // The last unfragmentable header's next header becomes the fragment
    // header's; it names a fragment header now.
    let nxt_at = if hlen > HDR_LEN { HDR_LEN } else { 6 };
    let nextproto = unfrag[nxt_at];
    unfrag[nxt_at] = proto::FRAGMENT;
    let mut out = Vec::new();
    let mut off = hlen;
    while off < tlen {
        let flen = len.min(tlen - off);
        let more = off + len < tlen;
        let data = m.data.copy(off, flen).map_err(|_| {
            st.ip6stat.inc(ip6s::ODROPPED);
            Errno::ENOBUFS
        })?;
        let mut f = Packet { hdr: m.hdr, data };
        let mut fh = [0u8; 8];
        fh[0] = nextproto;
        #[allow(clippy::cast_possible_truncation, reason = "under 64 KiB")]
        let offlg = ((off - hlen) & !7) as u16 | u16::from(more);
        fh[2..4].copy_from_slice(&offlg.to_be_bytes());
        fh[4..8].copy_from_slice(&id.to_be_bytes());
        let b = f.prepend(hlen + 8).map_err(|_| {
            st.ip6stat.inc(ip6s::ODROPPED);
            Errno::ENOBUFS
        })?;
        b[..hlen].copy_from_slice(&unfrag);
        b[hlen..].copy_from_slice(&fh);
        #[allow(clippy::cast_possible_truncation, reason = "under 64 KiB")]
        let plen = (flen + hlen + 8 - HDR_LEN) as u16;
        b[4..6].copy_from_slice(&plen.to_be_bytes());
        out.push(f);
        st.ip6stat.inc(ip6s::OFRAGMENTS);
        off += len;
    }
    Ok(out)
}

/// `ip6_forward`: a packet for another host, its hop limit taken down
/// one; ICMPv6 time exceeded at the end of its life, unreachable when it
/// cannot go on, packet too big over the next link's MTU, and a redirect
/// when it leaves by the interface it came in on. `srcrt`: it had a
/// source route (never here).
pub(crate) fn forward(vnet: &Arc<Vnet>, st: &State, mut m: Packet, srcrt: bool) {
    let Ok(mut ip6) = m.header::<Ip6>(0) else {
        return;
    };
    if m.hdr.flags & (flags::BCAST | flags::MCAST) != 0
        || is_multicast6(&ip6.dst)
        || is_unspecified(&ip6.src)
        || is_unspecified(&ip6.dst)
    {
        st.ip6stat.inc(ip6s::CANTFORWARD);
        return;
    }
    if ip6.hlim <= 1 {
        icmp6::error(
            vnet,
            m,
            icmp6::ty::TIME_EXCEEDED,
            icmp6::code::TIME_EXCEED_TRANSIT,
            0,
        );
        return;
    }
    let mcopy = m
        .data
        .copy(0, m.len().min(PLD_MAXLEN))
        .ok()
        .map(|data| Packet { hdr: m.hdr, data });
    ip6.hlim -= 1;
    let rcvif = m.hdr.rcvif;
    let dst_zone = if is_zoned(&ip6.dst) { rcvif } else { 0 };
    let decided = {
        let n = vnet.net.lock();
        match n.route6(m.hdr.fib, &ip6.dst, dst_zone) {
            None => Err(None),
            Some(r) => {
                let r = crate::ip::mpath_select(&n, r, m.hdr.flowid);
                if r.flags & rtf::REJECT != 0 {
                    Err(Some(icmp6::code::DST_UNREACH_REJECT))
                } else if r.flags & rtf::BLACKHOLE != 0 {
                    Err(Some(0xff))
                } else {
                    let outif = r.ifindex;
                    let gw = match r.gateway {
                        Gateway::Addr(g) if r.flags & rtf::GATEWAY != 0 => Some(g),
                        _ => None,
                    };
                    let redirect = on(&st.conf6.redirect) && outif == rcvif && !srcrt;
                    let out_loop = n
                        .by_index(outif)
                        .is_some_and(|i| i.flags & iff::LOOPBACK != 0);
                    let linkmtu = n.by_index(outif).map_or(0, |i| i.linkmtu6());
                    let assist = n.by_index(outif).map_or(0, hwassist6);
                    Ok((outif, gw, redirect, out_loop, linkmtu, assist))
                }
            }
        }
    };
    let (outif, gw, redirect, out_loop, linkmtu, assist) = match decided {
        Err(None) => {
            st.ip6stat.inc(ip6s::NOROUTE);
            if let Some(c) = mcopy {
                icmp6::error(
                    vnet,
                    c,
                    icmp6::ty::DST_UNREACH,
                    icmp6::code::DST_UNREACH_NOROUTE,
                    0,
                );
            }
            return;
        }
        Err(Some(code)) => {
            st.ip6stat.inc(ip6s::CANTFORWARD);
            if code != 0xff
                && let Some(c) = mcopy
            {
                icmp6::error(vnet, c, icmp6::ty::DST_UNREACH, code, 0);
            }
            return;
        }
        Ok(d) => d,
    };
    // A zoned source or destination may not leave its zone.
    if (is_zoned(&ip6.src) || is_zoned(&ip6.dst)) && outif != rcvif && !out_loop {
        st.ip6stat.inc(ip6s::CANTFORWARD);
        st.ip6stat.inc(ip6s::BADSCOPE);
        if is_zoned(&ip6.src)
            && let Some(c) = mcopy
        {
            icmp6::error(
                vnet,
                c,
                icmp6::ty::DST_UNREACH,
                icmp6::code::DST_UNREACH_BEYONDSCOPE,
                0,
            );
        }
        return;
    }
    let origif = if out_loop { rcvif } else { outif };
    if m.data.copy_in(0, &ip6.bytes()).is_err() {
        return;
    }
    if m.len() > linkmtu as usize {
        if let Some(c) = mcopy {
            icmp6::error(vnet, c, icmp6::ty::PACKET_TOO_BIG, 0, linkmtu as usize);
        }
        return;
    }
    if m.hdr.csum_flags & (csum::IP6_UDP | csum::IP6_TCP) & !assist != 0
        && let Some((off, _)) = lasthdr(&m)
    {
        delayed_cksum(&mut m, off);
    }
    let nexthop = gw.unwrap_or(ip6.dst);
    m.hdr.flags &= !(flags::BCAST | flags::MCAST);
    let r = {
        let mut n = vnet.net.lock();
        send(&mut n, outif, origif, &nexthop, m)
    };
    match r {
        Ok(()) => {
            st.ip6stat.inc(ip6s::FORWARD);
            if redirect {
                st.ip6stat.inc(ip6s::REDIRECTSENT);
                if let Some(c) = mcopy {
                    crate::nd6::redirect_output(vnet, st, c, outif, gw);
                }
            }
        }
        Err(e) => {
            st.ip6stat.inc(ip6s::CANTFORWARD);
            if !matches!(e, Errno::EMSGSIZE | Errno::ENOBUFS)
                && let Some(c) = mcopy
            {
                icmp6::error(
                    vnet,
                    c,
                    icmp6::ty::DST_UNREACH,
                    icmp6::code::DST_UNREACH_ADDR,
                    0,
                );
            }
        }
    }
}

/// Counts an ICMPv6 message sent (`icp6s_outhist`).
pub(crate) fn count_out(st: &State, ty: u8) {
    st.icmp6stat.inc(icp6s::OUTHIST + usize::from(ty));
}

/// Whether `a` is the loopback address (re-exported for the protocols).
#[must_use]
pub fn loopback(a: &In6) -> bool {
    is_loopback(a)
}
