// SPDX-License-Identifier: BSD-2-Clause
//! Netlink's `NETLINK_ROUTE` family over the interfaces and routes
//! ([`crate::ifnet`]): what `ifconfig(8)`, `route(8)`, and `netstat -r` of
//! FreeBSD 15 send and read. A request is processed when it is written,
//! its replies (and acknowledgement) queued for reading as FreeBSD's
//! `nl_process_nbuf` does.
//!
//! Handled: `RTM_GETLINK` (one interface or a dump), `RTM_GETADDR`,
//! `RTM_NEWADDR`, `RTM_DELADDR`, `RTM_GETROUTE` (a lookup or a dump),
//! `RTM_NEWROUTE`, `RTM_DELROUTE`, and `RTM_GETNEIGH` (loopback has no
//! neighbors). Others are `EOPNOTSUPP`, as `rtnl_handle_message` answers
//! a type it has no handler for. No group notifications are sent.
//!
//! Behavior reference: sys/netlink/netlink_io.c (`nl_receive_message`),
//! sys/netlink/netlink_message_writer.c (`nlmsg_ack`,
//! `_nlmsg_end_dump`), sys/netlink/route/iface.c (`dump_iface`,
//! `dump_iface_addr`, `rtnl_handle_getlink`, `rtnl_handle_getaddr`,
//! `handle_newaddr_inet`, `handle_newaddr_inet6`), sys/netlink/route/rt.c
//! (`dump_px`, `rtnl_handle_newroute`, `rtnl_handle_delroute`,
//! `rtnl_handle_getroute`).

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::inet::{AF_INET, AF_INET6, AF_UNSPEC};
use abi::net::{iff, rtf};

use crate::Family;
use crate::addr::{In6, from_v4, is_linklocal6, v4_of};
use crate::announce::NetLock;
use crate::ifnet::{Gateway, INFINITE, Ifaddr, LOMTU, Net, Route};

/// Netlink protocols (`NETLINK_*`) and the socket option level.
pub const NETLINK_ROUTE: i32 = 0;
pub const NETLINK_GENERIC: i32 = 16;
pub const SOL_NETLINK: i32 = 270;
/// Socket options.
pub mod opt {
    pub const ADD_MEMBERSHIP: i32 = 1;
    pub const DROP_MEMBERSHIP: i32 = 2;
    pub const LIST_MEMBERSHIPS: i32 = 9;
    pub const CAP_ACK: i32 = 10;
    pub const EXT_ACK: i32 = 11;
    pub const GET_STRICT_CHK: i32 = 12;
    pub const MSG_INFO: i32 = 257;
}

/// Header flags (`NLM_F_*`).
pub mod nlm {
    pub const REQUEST: u16 = 0x01;
    pub const MULTI: u16 = 0x02;
    pub const ACK: u16 = 0x04;
    pub const ROOT: u16 = 0x100;
    pub const MATCH: u16 = 0x200;
    pub const DUMP: u16 = ROOT | MATCH;
    pub const REPLACE: u16 = 0x100;
    pub const EXCL: u16 = 0x200;
    pub const CREATE: u16 = 0x400;
    pub const APPEND: u16 = 0x800;
    pub const CAPPED: u16 = 0x100;
    pub const ACK_TLVS: u16 = 0x200;
}

/// Message types.
pub mod ty {
    pub const NOOP: u16 = 1;
    pub const ERROR: u16 = 2;
    pub const DONE: u16 = 3;
    pub const MIN_TYPE: u16 = 0x10;
    pub const NEWLINK: u16 = 16;
    pub const DELLINK: u16 = 17;
    pub const GETLINK: u16 = 18;
    pub const NEWADDR: u16 = 20;
    pub const DELADDR: u16 = 21;
    pub const GETADDR: u16 = 22;
    pub const NEWROUTE: u16 = 24;
    pub const DELROUTE: u16 = 25;
    pub const GETROUTE: u16 = 26;
    pub const NEWNEIGH: u16 = 28;
    pub const DELNEIGH: u16 = 29;
    pub const GETNEIGH: u16 = 30;
}

/// Neighbor attributes, flags, and states (`netlink/route/neigh.h`).
mod nda {
    pub(super) const DST: u16 = 1;
    pub(super) const LLADDR: u16 = 2;
    pub(super) const CACHEINFO: u16 = 3;
    pub(super) const PROBES: u16 = 4;
    pub(super) const IFINDEX: u16 = 8;
    pub(super) const FLAGS_EXT: u16 = 15;
    pub(super) const FREEBSD: u16 = 18;
    pub(super) const F_NEXT_STATE_TS: u16 = 1;
    pub(super) const NTF_SELF: u8 = 0x02;
    pub(super) const NTF_PROXY: u8 = 0x08;
    pub(super) const NTF_STICKY: u8 = 0x40;
    pub(super) const NUD_INCOMPLETE: u16 = 0x01;
    pub(super) const NUD_REACHABLE: u16 = 0x02;
    pub(super) const NUD_STALE: u16 = 0x04;
    pub(super) const NUD_DELAY: u16 = 0x08;
    pub(super) const NUD_PROBE: u16 = 0x10;
}

/// Attributes, by their `abi` names.
mod ifla {
    pub(super) use abi::netlink::{
        IF_OPER_DOWN as OPER_DOWN, IF_OPER_UP as OPER_UP, IFLA_ADDRESS as ADDRESS,
        IFLA_BROADCAST as BROADCAST, IFLA_CARRIER as CARRIER, IFLA_FREEBSD as FREEBSD,
        IFLA_IFALIAS as IFALIAS, IFLA_IFNAME as IFNAME, IFLA_MTU as MTU,
        IFLA_OPERSTATE as OPERSTATE, IFLA_PROMISCUITY as PROMISCUITY, IFLA_STATS64 as STATS64,
        IFLAF_CAPS as CAPS, IFLAF_ORIG_HWADDR as ORIG_HWADDR, NLA_BITSET_MASK as BITSET_MASK,
        NLA_BITSET_SIZE as BITSET_SIZE, NLA_BITSET_VALUE as BITSET_VALUE,
    };
}
mod ifa {
    pub(super) use abi::netlink::{
        IFA_ADDRESS as ADDRESS, IFA_BROADCAST as BROADCAST, IFA_CACHEINFO as CACHEINFO,
        IFA_FLAGS as FLAGS, IFA_FREEBSD as FREEBSD, IFA_LABEL as LABEL, IFA_LOCAL as LOCAL,
        IFAF_FLAGS as F_FLAGS, IFAF_VHID as F_VHID,
    };
}
mod rta {
    pub(super) use abi::netlink::{
        NL_RTA_DST as DST, NL_RTA_GATEWAY as GATEWAY, NL_RTA_KNH_ID as KNH_ID,
        NL_RTA_METRICS as METRICS, NL_RTA_MULTIPATH as MULTIPATH, NL_RTA_OIF as OIF,
        NL_RTA_RTFLAGS as RTFLAGS, NL_RTA_TABLE as TABLE, NL_RTA_WEIGHT as WEIGHT,
        NL_RTAX_MTU as AX_MTU,
    };
}
mod rtn {
    pub(super) use abi::netlink::{
        RT_SCOPE_HOST as SCOPE_HOST, RT_SCOPE_LINK as SCOPE_LINK,
        RT_SCOPE_UNIVERSE as SCOPE_UNIVERSE, RTN_BLACKHOLE as BLACKHOLE, RTN_PROHIBIT as PROHIBIT,
        RTN_UNICAST as UNICAST, RTN_UNREACHABLE as UNREACHABLE, RTPROT_KERNEL as PROT_KERNEL,
        RTPROT_STATIC as PROT_STATIC,
    };
}
/// `NLA_F_NESTED`, `NLA_F_NET_BYTEORDER`.
const NLA_TYPE_MASK: u16 = !((1 << 15) | (1 << 14));

/// `NLMSG_ALIGN`.
const fn align(n: usize) -> usize {
    (n + 3) & !3
}

/// A message being built (`nl_writer`): a header, a fixed part, and
/// attributes.
struct Msg {
    buf: Vec<u8>,
}

impl Msg {
    fn new(ty: u16, flags: u16, seq: u32, pid: u32) -> Self {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&ty.to_ne_bytes());
        buf.extend_from_slice(&flags.to_ne_bytes());
        buf.extend_from_slice(&seq.to_ne_bytes());
        buf.extend_from_slice(&pid.to_ne_bytes());
        Self { buf }
    }

    fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
        self.buf.resize(align(self.buf.len()), 0);
    }

    fn attr(&mut self, ty: u16, data: &[u8]) {
        let len = u16::try_from(4 + data.len()).unwrap_or(u16::MAX);
        self.buf.extend_from_slice(&len.to_ne_bytes());
        self.buf.extend_from_slice(&ty.to_ne_bytes());
        self.raw(data);
    }

    fn u32(&mut self, ty: u16, v: u32) {
        self.attr(ty, &v.to_ne_bytes());
    }

    fn u8(&mut self, ty: u16, v: u8) {
        self.attr(ty, &[v]);
    }

    fn string(&mut self, ty: u16, s: &[u8]) {
        let mut v = s.to_vec();
        v.push(0);
        self.attr(ty, &v);
    }

    fn nest(&mut self, ty: u16) -> usize {
        let at = self.buf.len();
        self.attr(ty, &[]);
        at
    }

    fn end_nest(&mut self, at: usize) {
        let len = u16::try_from(self.buf.len() - at).unwrap_or(u16::MAX);
        self.buf[at..at + 2].copy_from_slice(&len.to_ne_bytes());
    }

    fn finish(mut self) -> Vec<u8> {
        let len = u32::try_from(self.buf.len()).unwrap_or(u32::MAX);
        self.buf[0..4].copy_from_slice(&len.to_ne_bytes());
        self.buf
    }
}

/// A request's header.
#[derive(Clone, Copy, Debug)]
struct Hdr {
    len: usize,
    ty: u16,
    flags: u16,
    seq: u32,
}

fn rd16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .and_then(|x| x.try_into().ok())
        .map_or(0, u16::from_ne_bytes)
}

fn rd32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .and_then(|x| x.try_into().ok())
        .map_or(0, u32::from_ne_bytes)
}

/// The attributes after a message's fixed part: (type, data).
fn attrs(b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= b.len() {
        let len = usize::from(rd16(b, at));
        let t = rd16(b, at + 2) & NLA_TYPE_MASK;
        if len < 4 || at + len > b.len() {
            break;
        }
        out.push((t, &b[at + 4..at + len]));
        at += align(len);
    }
    out
}

fn find<'a>(a: &[(u16, &'a [u8])], t: u16) -> Option<&'a [u8]> {
    a.iter().find(|(x, _)| *x == t).map(|(_, d)| *d)
}

/// A socket's settings that shape replies, and what its creator may
/// change (`nlp_has_priv`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctx {
    /// The socket's port id (`nl_port`), stamped on its messages.
    pub port: u32,
    /// `NETLINK_EXT_ACK`, `NETLINK_CAP_ACK`.
    pub ext_ack: bool,
    pub cap_ack: bool,
    /// `PRIV_NET_ADDIFADDR`, `PRIV_NET_DELIFADDR`, `PRIV_NET_ROUTE`.
    pub may_addaddr: bool,
    pub may_deladdr: bool,
    pub may_route: bool,
    /// The socket's routing table (`nlp_get_fib`), for requests that name
    /// none.
    pub fib: u16,
}

/// What a request's handler leaves: an error, and its message for the
/// extended acknowledgement.
#[derive(Debug, Default)]
struct Outcome {
    error: Option<Errno>,
    msg: Option<&'static [u8]>,
    /// A dump ended with `NLMSG_DONE`, which stands for the ack.
    done: bool,
}

/// Processes one written buffer of requests (`nl_process_nbuf`): every
/// reply, in order, in one buffer.
#[must_use]
pub fn handle(ctx: &Ctx, net: &NetLock, packet: &[u8], now_s: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    while packet.len() - at >= 16 {
        let h = Hdr {
            len: rd32(packet, at) as usize,
            ty: rd16(packet, at + 4),
            flags: rd16(packet, at + 6),
            seq: rd32(packet, at + 8),
        };
        if h.len < 16 || h.len > packet.len() - at {
            break;
        }
        let msg = &packet[at..at + h.len];
        let mut o = Outcome::default();
        if h.flags & nlm::REQUEST != 0 && h.ty >= ty::MIN_TYPE {
            dispatch(ctx, net, &h, msg, &mut out, &mut o, now_s);
        }
        let error = o.error;
        if !o.done && (h.flags & nlm::ACK != 0 || error.is_some()) {
            ack(ctx, &h, msg, error, o.msg, &mut out);
        }
        at += align(h.len);
    }
    out
}

/// `nlmsg_ack`: an `NLMSG_ERROR` with the error (0 for none) and the
/// request's header, or all of it for an error without `NETLINK_CAP_ACK`.
fn ack(
    ctx: &Ctx,
    h: &Hdr,
    msg: &[u8],
    error: Option<Errno>,
    text: Option<&[u8]>,
    out: &mut Vec<u8>,
) {
    let capped = error.is_none() || ctx.cap_ack;
    let mut flags = if capped { nlm::CAPPED } else { 0 };
    if text.is_some() && ctx.ext_ack {
        flags |= nlm::ACK_TLVS;
    }
    let mut m = Msg::new(ty::ERROR, flags, h.seq, ctx.port);
    m.raw(&error.map_or(0, Errno::get).to_ne_bytes());
    let mut req = if capped {
        msg[..16].to_vec()
    } else {
        msg.to_vec()
    };
    // The request as stamped with the sender (`nl_receive_message`).
    req[12..16].copy_from_slice(&ctx.port.to_ne_bytes());
    m.raw(&req);
    if let (Some(t), true) = (text, ctx.ext_ack) {
        m.string(1, t);
    }
    out.extend(m.finish());
}

/// `_nlmsg_end_dump`.
fn done(ctx: &Ctx, h: &Hdr, error: Option<Errno>, out: &mut Vec<u8>, o: &mut Outcome) {
    let mut m = Msg::new(ty::DONE, h.flags | nlm::MULTI, h.seq, ctx.port);
    m.raw(&error.map_or(0, Errno::get).to_ne_bytes());
    out.extend(m.finish());
    o.done = true;
    o.error = error;
}

fn dispatch(
    ctx: &Ctx,
    net: &NetLock,
    h: &Hdr,
    msg: &[u8],
    out: &mut Vec<u8>,
    o: &mut Outcome,
    now_s: u64,
) {
    let body = &msg[16..];
    let r = match h.ty {
        ty::GETLINK => getlink(ctx, net, h, body, out, o),
        ty::GETADDR => getaddr(ctx, net, h, body, out, o),
        ty::NEWADDR if !ctx.may_addaddr => Err(Errno::EPERM),
        ty::DELADDR if !ctx.may_deladdr => Err(Errno::EPERM),
        ty::NEWADDR => newaddr(net, body, o),
        ty::DELADDR => deladdr(net, body, o),
        ty::GETROUTE => getroute(ctx, net, h, body, out, o),
        ty::NEWROUTE | ty::DELROUTE if !ctx.may_route => Err(Errno::EPERM),
        ty::NEWROUTE => newroute(ctx, net, h, body, o),
        ty::DELROUTE => delroute(ctx, net, body, o),
        ty::GETNEIGH => getneigh(ctx, net, h, body, out, o, now_s),
        ty::NEWNEIGH | ty::DELNEIGH if !ctx.may_route => Err(Errno::EPERM),
        ty::NEWNEIGH => newneigh(net, h, body, o, now_s),
        ty::DELNEIGH => delneigh(net, body, o),
        _ => Err(Errno::EOPNOTSUPP),
    };
    if let Err(e) = r {
        o.error = Some(e);
    }
}

/// `dump_iface`.
fn link_msg(ctx: &Ctx, h: &Hdr, n: &Net, i: &crate::ifnet::Ifnet, multi: bool) -> Vec<u8> {
    let flags = if multi { h.flags | nlm::MULTI } else { h.flags };
    let mut m = Msg::new(ty::NEWLINK, flags, h.seq, ctx.port);
    // `get_operstate`: an Ethernet interface's from its media, others'
    // from `IFF_UP`.
    let up = match i.link.0.as_ref().and_then(|l| l.media()) {
        Some(md) if i.ty == abi::net::IFT_ETHER => md.status & crate::link::ifm::ACTIVE != 0,
        _ => i.flags & iff::UP != 0,
    };
    let mut ifi = [0u8; 16];
    ifi[0] = AF_UNSPEC;
    ifi[2..4].copy_from_slice(&u16::from(i.ty).to_ne_bytes());
    ifi[4..8].copy_from_slice(&u32::from(i.index).to_ne_bytes());
    let fl = i.flags | if up { iff::LOWER_UP } else { 0 };
    ifi[8..12].copy_from_slice(&fl.to_ne_bytes());
    m.raw(&ifi);
    m.string(ifla::IFNAME, i.name.as_bytes());
    m.u8(
        ifla::OPERSTATE,
        if up { ifla::OPER_UP } else { ifla::OPER_DOWN },
    );
    m.u8(ifla::CARRIER, u8::from(up));
    if let Some(a) = &i.lladdr {
        m.attr(ifla::ADDRESS, a);
        m.attr(ifla::BROADCAST, &[0xff; 6]);
    }
    m.u32(ifla::MTU, i.mtu);
    if let Some(d) = &i.description {
        m.string(ifla::IFALIAS, d);
    }
    let fb = m.nest(ifla::FREEBSD);
    if let Some(a) = &i.lladdr {
        m.attr(ifla::ORIG_HWADDR, a);
    }
    let caps = m.nest(ifla::CAPS);
    // `IFCAP_B_SIZE` bits, two 32-bit words of each.
    m.u32(ifla::BITSET_SIZE, 64);
    let mut mask = [0u8; 8];
    mask[..4].copy_from_slice(&i.capenable.to_ne_bytes());
    m.attr(ifla::BITSET_MASK, &mask);
    let mut val = [0u8; 8];
    val[..4].copy_from_slice(&i.capabilities.to_ne_bytes());
    m.attr(ifla::BITSET_VALUE, &val);
    m.end_nest(caps);
    m.end_nest(fb);
    // `rtnl_link_stats64`: 24 counters.
    let mut st = [0u64; 24];
    {
        let s = &i.stats;
        st[0] = s.ipackets.load(Ordering::Relaxed);
        st[1] = s.opackets.load(Ordering::Relaxed);
        st[2] = s.ibytes.load(Ordering::Relaxed);
        st[3] = s.obytes.load(Ordering::Relaxed);
        st[6] = s.iqdrops.load(Ordering::Relaxed);
        st[8] = s.imcasts.load(Ordering::Relaxed);
        st[23] = s.noproto.load(Ordering::Relaxed);
    }
    let mut sb = Vec::with_capacity(24 * 8);
    for v in st {
        sb.extend_from_slice(&v.to_ne_bytes());
    }
    m.attr(ifla::STATS64, &sb);
    m.u32(ifla::PROMISCUITY, u32::from(i.flags & iff::PROMISC != 0));
    let _ = (n, ifla::ADDRESS, ifla::ORIG_HWADDR);
    m.finish()
}

/// `rtnl_handle_getlink`.
fn getlink(
    ctx: &Ctx,
    net: &NetLock,
    h: &Hdr,
    body: &[u8],
    out: &mut Vec<u8>,
    o: &mut Outcome,
) -> Result<(), Errno> {
    let index = rd32(body, 4);
    let a = attrs(body.get(16..).unwrap_or(&[]));
    let name = find(&a, ifla::IFNAME).map(|n| n.split(|&b| b == 0).next().unwrap_or(n));
    let n = net.lock();
    if index != 0 || name.is_some() {
        let i = if index != 0 {
            n.ifaces.iter().find(|i| u32::from(i.index) == index)
        } else {
            name.and_then(|nm| n.by_name(nm))
        };
        let i = i.ok_or(Errno::ENODEV)?;
        if name.is_some_and(|nm| nm != i.name.as_bytes()) {
            return Err(Errno::ENODEV);
        }
        out.extend(link_msg(ctx, h, &n, i, false));
        return Ok(());
    }
    for i in &n.ifaces {
        out.extend(link_msg(ctx, h, &n, i, true));
    }
    drop(n);
    done(ctx, h, None, out, o);
    Ok(())
}

/// `ifa_get_scope`.
fn scope(a: &Ifaddr) -> u8 {
    match a.family {
        Family::Inet if v4_of(&a.addr)[0] == 127 => rtn::SCOPE_HOST,
        Family::Inet if v4_of(&a.addr)[..2] == [169, 254] => rtn::SCOPE_LINK,
        Family::Inet6 if a.addr == crate::addr::LOOPBACK6 => rtn::SCOPE_HOST,
        Family::Inet6 if is_linklocal6(&a.addr) => rtn::SCOPE_LINK,
        _ => rtn::SCOPE_UNIVERSE,
    }
}

fn addr_bytes(f: Family, a: &In6) -> &[u8] {
    match f {
        Family::Inet => &a[12..],
        Family::Inet6 => &a[..],
    }
}

/// `dump_iface_addr`.
fn addr_msg(ctx: &Ctx, h: &Hdr, i: &crate::ifnet::Ifnet, a: &Ifaddr, now_s: u64) -> Vec<u8> {
    let mut m = Msg::new(ty::NEWADDR, h.flags | nlm::MULTI, h.seq, ctx.port);
    let mut ifa_hdr = [0u8; 8];
    ifa_hdr[0] = a.family.af();
    ifa_hdr[1] = a.plen;
    ifa_hdr[3] = scope(a);
    ifa_hdr[4..8].copy_from_slice(&u32::from(i.index).to_ne_bytes());
    m.raw(&ifa_hdr);
    let p2p = i.flags & iff::POINTOPOINT != 0;
    match (p2p, a.dst) {
        (true, Some(d)) => {
            m.attr(ifa::ADDRESS, addr_bytes(a.family, &d));
            m.attr(ifa::LOCAL, addr_bytes(a.family, &a.addr));
        }
        _ => {
            m.attr(ifa::ADDRESS, addr_bytes(a.family, &a.addr));
            if a.family == Family::Inet {
                m.attr(ifa::LOCAL, addr_bytes(a.family, &a.addr));
            }
        }
    }
    if i.flags & iff::BROADCAST != 0
        && let Some(b) = a.broadcast
    {
        m.attr(ifa::BROADCAST, addr_bytes(a.family, &b));
    }
    m.string(ifa::LABEL, i.name.as_bytes());
    // `in6_flags_to_nl` (whose permanent and failed-DAD bits never reach
    // the result).
    let mut nlf = 0u32;
    if a.family == Family::Inet6 {
        use crate::ifnet::in6_iff;
        for (f, n) in [
            (in6_iff::TEMPORARY, 0x01),
            (in6_iff::NODAD, 0x02),
            (in6_iff::DEPRECATED, 0x20),
            (in6_iff::TENTATIVE, 0x40),
        ] {
            if a.flags6 & f != 0 {
                nlf |= n;
            }
        }
    }
    m.u32(ifa::FLAGS, nlf);
    if a.family == Family::Inet6 {
        // `export_cache_info6`: lifetimes, and when the address was made
        // and last set, in milliseconds of uptime.
        let _ = now_s;
        let ms = |s: u64| u32::try_from(s.saturating_mul(1000)).unwrap_or(0);
        let mut ci = [0u8; 16];
        ci[0..4].copy_from_slice(&a.pltime.to_ne_bytes());
        ci[4..8].copy_from_slice(&a.vltime.to_ne_bytes());
        ci[8..12].copy_from_slice(&ms(a.created).to_ne_bytes());
        ci[12..16].copy_from_slice(&ms(a.updated).to_ne_bytes());
        m.attr(ifa::CACHEINFO, &ci);
    }
    let fb = m.nest(ifa::FREEBSD);
    if a.vhid != 0 {
        m.u32(ifa::F_VHID, a.vhid);
    }
    if a.family == Family::Inet6 {
        m.u32(ifa::F_FLAGS, a.flags6.cast_unsigned());
    }
    m.end_nest(fb);
    m.finish()
}

/// `rtnl_handle_getaddr`.
fn getaddr(
    ctx: &Ctx,
    net: &NetLock,
    h: &Hdr,
    body: &[u8],
    out: &mut Vec<u8>,
    o: &mut Outcome,
) -> Result<(), Errno> {
    let family = body.first().copied().unwrap_or(0);
    let index = rd32(body, 4);
    let n = net.lock();
    let mut err = None;
    if index != 0 && n.by_index(u16::try_from(index).unwrap_or(0)).is_none() {
        err = Some(Errno::ENOENT);
    } else {
        for i in &n.ifaces {
            if index != 0 && u32::from(i.index) != index {
                continue;
            }
            for a in &i.addrs {
                if family != 0 && family != a.family.af() {
                    continue;
                }
                out.extend(addr_msg(ctx, h, i, a, i.epoch.cast_unsigned()));
            }
        }
    }
    drop(n);
    done(ctx, h, err, out, o);
    Ok(())
}

/// An address attribute of `family`'s length.
fn ip_attr(family: Family, d: Option<&[u8]>) -> Option<In6> {
    let d = d?;
    match family {
        Family::Inet if d.len() == 4 => Some(from_v4([d[0], d[1], d[2], d[3]])),
        Family::Inet6 if d.len() == 16 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(d);
            Some(a)
        }
        _ => None,
    }
}

/// The interface name of index `index`.
fn ifname(n: &Net, index: u32) -> Result<Vec<u8>, Errno> {
    n.ifaces
        .iter()
        .find(|i| u32::from(i.index) == index)
        .map(|i| i.name.as_bytes().to_vec())
        .ok_or(Errno::ENOENT)
}

/// `rtnl_handle_newaddr`: `handle_newaddr_inet` (`SIOCAIFADDR`) and
/// `handle_newaddr_inet6` (`SIOCAIFADDR_IN6`).
fn newaddr(net: &NetLock, body: &[u8], o: &mut Outcome) -> Result<(), Errno> {
    let family = match body.first().copied() {
        Some(AF_INET) => Family::Inet,
        Some(AF_INET6) => Family::Inet6,
        _ => return Err(Errno::EOPNOTSUPP),
    };
    let plen = body.get(1).copied().unwrap_or(0);
    let index = rd32(body, 4);
    let a = attrs(body.get(8..).unwrap_or(&[]));
    let local = ip_attr(family, find(&a, ifa::LOCAL));
    let address = ip_attr(family, find(&a, ifa::ADDRESS));
    let Some(addr) = local.or(address) else {
        o.msg = Some(b"IFA_LOCAL/IFA_ADDRESS is not set");
        return Err(Errno::EINVAL);
    };
    let max = if family == Family::Inet { 32 } else { 128 };
    if plen > max {
        o.msg = Some(b"invalid prefix length");
        return Err(Errno::EINVAL);
    }
    let fb = find(&a, ifa::FREEBSD).map(attrs).unwrap_or_default();
    let flags6 = find(&fb, ifa::F_FLAGS)
        .map_or(0, |d| rd32(d, 0))
        .cast_signed();
    let vhid = find(&fb, ifa::F_VHID).map_or(0, |d| rd32(d, 0));
    let (vltime, pltime) =
        find(&a, ifa::CACHEINFO)
            .filter(|d| d.len() >= 8)
            .map_or((INFINITE, INFINITE), |d| {
                let pl = rd32(d, 0);
                let vl = rd32(d, 4);
                (
                    if vl == 0 { INFINITE } else { vl },
                    if pl == 0 { INFINITE } else { pl },
                )
            });
    let mut n = net.lock();
    let name = ifname(&n, index)?;
    let loopback = n
        .by_name(&name)
        .is_some_and(|i| i.flags & iff::LOOPBACK != 0);
    let dst = match (local, address) {
        (Some(_), Some(d)) if d != addr => Some(d),
        // A loopback IPv4 address is its own destination.
        _ if loopback && family == Family::Inet => Some(addr),
        _ => None,
    };
    let broadcast = ip_attr(family, find(&a, ifa::BROADCAST));
    // `in6_aifaddr` (by way of `in6_control`): its prefix is listed.
    let update = if family == Family::Inet6 {
        crate::ifnet::ifa6_update::AIFADDR
    } else {
        0
    };
    n.add_addr_flags(
        &name,
        Ifaddr {
            family,
            addr,
            plen,
            dst,
            broadcast,
            flags6,
            vltime,
            pltime,
            vhid,
            created: 0,
            updated: 0,
        },
        update,
    )
}

/// `rtnl_handle_deladdr`.
fn deladdr(net: &NetLock, body: &[u8], o: &mut Outcome) -> Result<(), Errno> {
    let family = match body.first().copied() {
        Some(AF_INET) => Family::Inet,
        Some(AF_INET6) => Family::Inet6,
        _ => return Err(Errno::EOPNOTSUPP),
    };
    let index = rd32(body, 4);
    let a = attrs(body.get(8..).unwrap_or(&[]));
    let Some(addr) =
        ip_attr(family, find(&a, ifa::LOCAL)).or(ip_attr(family, find(&a, ifa::ADDRESS)))
    else {
        o.msg = Some(b"IFA_LOCAL is not set");
        return Err(Errno::EINVAL);
    };
    let mut n = net.lock();
    let name = ifname(&n, index)?;
    n.delete_addr(&name, family, Some(&addr)).map(|_| ())
}

/// A neighbor request's fixed part and attributes (`ndmsg_parser`):
/// family, flags, interface, destination, link-layer address.
struct NeighReq<'a> {
    family: Option<Family>,
    flags: u32,
    ifindex: u32,
    dst: Option<In6>,
    lladdr: Option<&'a [u8]>,
    next_ts: u32,
}

fn neigh_req(body: &[u8]) -> NeighReq<'_> {
    let family = Family::from_af(body.first().copied().unwrap_or(0));
    let a = attrs(body.get(12..).unwrap_or(&[]));
    let mut ifindex = rd32(body, 4);
    if let Some(d) = find(&a, nda::IFINDEX) {
        ifindex = rd32(d, 0);
    }
    let mut flags = u32::from(body.get(10).copied().unwrap_or(0));
    if let Some(d) = find(&a, nda::FLAGS_EXT) {
        flags = rd32(d, 0);
    }
    let next_ts = find(&a, nda::FREEBSD)
        .map(attrs)
        .and_then(|n| find(&n, nda::F_NEXT_STATE_TS).map(|d| rd32(d, 0)))
        .unwrap_or(0);
    NeighReq {
        family,
        flags,
        ifindex,
        dst: family.and_then(|f| ip_attr(f, find(&a, nda::DST))),
        lladdr: find(&a, nda::LLADDR),
        next_ts,
    }
}

/// `dump_lle_locked`: one entry as `RTM_NEWNEIGH`.
fn neigh_msg(ctx: &Ctx, h: &Hdr, e: &crate::lltable::Lle, multi: bool, now_s: u64) -> Vec<u8> {
    use crate::lltable::{lle, state};
    let flags = if multi { h.flags | nlm::MULTI } else { h.flags };
    let mut m = Msg::new(ty::NEWNEIGH, flags, h.seq, ctx.port);
    let st = match e.family {
        Family::Inet => {
            let s = if e.flags & (lle::STATIC | lle::IFADDR) != 0 {
                state::REACHABLE
            } else {
                e.state
            };
            match s {
                state::INCOMPLETE => nda::NUD_INCOMPLETE,
                state::REACHABLE => nda::NUD_REACHABLE,
                state::VERIFY => nda::NUD_PROBE,
                _ => 0,
            }
        }
        // `ND6_LLINFO_*`: incomplete 0, reachable 1, stale 2, delay 3,
        // probe 4.
        Family::Inet6 => match e.state {
            0 => nda::NUD_INCOMPLETE,
            1 => nda::NUD_REACHABLE,
            2 => nda::NUD_STALE,
            3 => nda::NUD_DELAY,
            4 => nda::NUD_PROBE,
            _ => 0,
        },
    };
    let mut nf = 0u8;
    if e.flags & lle::IFADDR != 0 {
        nf |= nda::NTF_SELF;
    }
    if e.flags & lle::PUB != 0 {
        nf |= nda::NTF_PROXY;
    }
    if e.flags & lle::STATIC != 0 {
        nf |= nda::NTF_STICKY;
    }
    let mut ndm = [0u8; 12];
    ndm[0] = e.family.af();
    ndm[4..8].copy_from_slice(&u32::from(e.ifindex).to_ne_bytes());
    ndm[8..10].copy_from_slice(&st.to_ne_bytes());
    ndm[10] = nf;
    m.raw(&ndm);
    match e.family {
        Family::Inet => m.attr(nda::DST, &v4_of(&e.addr)),
        Family::Inet6 => m.attr(nda::DST, &e.addr),
    }
    if e.flags & lle::VALID != 0 {
        m.attr(nda::LLADDR, &e.lladdr);
    }
    m.u32(nda::PROBES, u32::from(e.asked));
    // `struct nda_cacheinfo`: confirmed, used, updated, refcnt.
    m.attr(nda::CACHEINFO, &[0u8; 16]);
    let at = m.nest(nda::FREEBSD);
    // `get_lle_next_ts`: when it next changes, on the realtime clock.
    let ts = if e.expire == 0 {
        0
    } else {
        let real = u64::try_from(crate::realtime()).unwrap_or(0);
        u32::try_from((e.expire + real).saturating_sub(crate::uptime())).unwrap_or(u32::MAX)
    };
    m.u32(nda::F_NEXT_STATE_TS, ts);
    m.end_nest(at);
    let _ = now_s;
    m.finish()
}

/// `rtnl_handle_getneigh`: every entry (of a family, an interface), or
/// the one for a destination.
fn getneigh(
    ctx: &Ctx,
    net: &NetLock,
    h: &Hdr,
    body: &[u8],
    out: &mut Vec<u8>,
    o: &mut Outcome,
    now_s: u64,
) -> Result<(), Errno> {
    let r = neigh_req(body);
    let n = net.lock();
    let ifindex = u16::try_from(r.ifindex).unwrap_or(0);
    if r.ifindex != 0 && n.by_index(ifindex).is_none() {
        return Err(Errno::ENOENT);
    }
    if let Some(dst) = r.dst {
        if r.ifindex == 0 {
            o.msg = Some(b"has NDA_DST but no ifindex provided");
            return Err(Errno::EINVAL);
        }
        let family = r.family.ok_or(Errno::EINVAL)?;
        let e = n.lle(ifindex, family, &dst).ok_or(Errno::ESRCH)?;
        out.extend(neigh_msg(ctx, h, e, false, now_s));
        return Ok(());
    }
    // `dump_llts`: interface by interface, IPv4 before IPv6.
    for i in &n.ifaces {
        if r.ifindex != 0 && i.index != ifindex {
            continue;
        }
        for fam in [Family::Inet, Family::Inet6] {
            if r.family.is_some_and(|f| f != fam) {
                continue;
            }
            for e in n
                .lle
                .iter()
                .filter(|e| e.ifindex == i.index && e.family == fam)
            {
                out.extend(neigh_msg(ctx, h, e, true, now_s));
            }
        }
    }
    drop(n);
    done(ctx, h, None, out, o);
    Ok(())
}

/// `rtnl_handle_newneigh`: a static (`NTF_STICKY`) or expiring entry, a
/// published one with `NTF_PROXY`; with `NLM_F_REPLACE` over one that is
/// not an interface's own.
fn newneigh(net: &NetLock, h: &Hdr, body: &[u8], o: &mut Outcome, now_s: u64) -> Result<(), Errno> {
    use crate::lltable::{Lle, lle};
    let r = neigh_req(body);
    let mut n = net.lock();
    let ifindex = u16::try_from(r.ifindex).unwrap_or(0);
    let (Some(ifp), Some(dst), Some(ll)) = (n.by_index(ifindex), r.dst, r.lladdr) else {
        o.msg = Some(if r.ifindex == 0 || n.by_index(ifindex).is_none() {
            b"NDA_IFINDEX / ndm_ifindex not set"
        } else if r.dst.is_none() {
            b"NDA_DST not set"
        } else {
            b"NDA_LLADDR not set"
        });
        return Err(Errno::EINVAL);
    };
    let family = r.family.ok_or(Errno::EINVAL)?;
    if ifp.lladdr.is_none() || ll.len() != 6 {
        o.msg = Some(b"NDA_LLADDR address length is different from expected");
        return Err(Errno::EINVAL);
    }
    let supported = u32::from(nda::NTF_PROXY | nda::NTF_STICKY);
    if r.flags & !supported != 0 {
        o.msg = Some(b"ndm_flags not supported");
        return Err(Errno::EOPNOTSUPP);
    }
    if h.flags & (nlm::CREATE | nlm::REPLACE) == 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let mut flags = 0;
    if r.flags & u32::from(nda::NTF_PROXY) != 0 {
        flags |= lle::PUB;
    }
    if r.flags & u32::from(nda::NTF_STICKY) != 0 {
        flags |= lle::STATIC;
    }
    let mut e = Lle::new(family, dst, ifindex, flags);
    e.set_lladdr(ll.try_into().map_err(|_| Errno::EINVAL)?);
    e.state = crate::lltable::state::REACHABLE;
    if flags & lle::STATIC == 0 {
        // `ndaf_next_ts - time_second + time_uptime`.
        let real = u64::try_from(crate::realtime()).unwrap_or(0);
        e.expire = (u64::from(r.next_ts) + crate::uptime()).saturating_sub(real);
    }
    match n.lle(ifindex, family, &dst) {
        Some(old) if h.flags & nlm::REPLACE == 0 => {
            let _ = old;
            return Err(Errno::EEXIST);
        }
        Some(old) if old.flags & lle::IFADDR != 0 => return Err(Errno::EPERM),
        None if h.flags & nlm::CREATE == 0 => return Err(Errno::ENOENT),
        _ => {}
    }
    let publish = flags & lle::PUB != 0;
    n.lle_link(e);
    // `in_lltable_post_resolved`: a published IPv4 entry is announced.
    if publish && family == Family::Inet {
        let a = v4_of(&dst);
        let ll: [u8; 6] = ll.try_into().map_err(|_| Errno::EINVAL)?;
        let _ = crate::arp::request(&mut n, ifindex, Some(a), a, Some(ll));
    }
    let _ = now_s;
    Ok(())
}

/// `rtnl_handle_delneigh`.
fn delneigh(net: &NetLock, body: &[u8], o: &mut Outcome) -> Result<(), Errno> {
    let r = neigh_req(body);
    let Some(dst) = r.dst else {
        o.msg = Some(b"NDA_DST not set");
        return Err(Errno::EINVAL);
    };
    let mut n = net.lock();
    let ifindex = u16::try_from(r.ifindex).unwrap_or(0);
    if r.ifindex == 0 || n.by_index(ifindex).is_none() {
        o.msg = Some(b"no ifindex provided");
        return Err(Errno::EINVAL);
    }
    let family = r.family.ok_or(Errno::EAFNOSUPPORT)?;
    n.lle_delete(ifindex, family, &dst, false).map(|_| ())
}

/// `dump_px`.
/// `dump_px`: one prefix, with its one path (`dump_rc_nhop`) or its
/// multipath group (`dump_rc_nhg`: an `rtnexthop` for each, its weight
/// in `rtnh_hops`).
fn route_msg(ctx: &Ctx, h: &Hdr, group: &[&Route], multi: bool) -> Vec<u8> {
    let r = group[0];
    let flags = if multi { h.flags | nlm::MULTI } else { h.flags };
    let mut m = Msg::new(ty::NEWROUTE, flags, h.seq, ctx.port);
    let mut rtm = [0u8; 12];
    rtm[0] = r.family.af();
    rtm[1] = r.plen;
    rtm[4] = 0;
    rtm[5] = if r.flags & rtf::STATIC != 0 {
        rtn::PROT_STATIC
    } else {
        rtn::PROT_KERNEL
    };
    rtm[6] = rtn::SCOPE_UNIVERSE;
    rtm[7] = if r.flags & rtf::BLACKHOLE != 0 {
        rtn::BLACKHOLE
    } else if r.flags & rtf::REJECT != 0 {
        rtn::PROHIBIT
    } else {
        rtn::UNICAST
    };
    m.raw(&rtm);
    m.u32(rta::TABLE, u32::from(r.fib));
    m.attr(rta::DST, addr_bytes(r.family, &r.dst));
    if let Gateway::Addr(g) = r.gateway {
        m.attr(rta::GATEWAY, addr_bytes(r.family, &g));
    }
    let flags_of = |r: &Route| {
        let mut fl = r.flags.cast_unsigned();
        if matches!(r.gateway, Gateway::Addr(_)) {
            fl |= rtf::GATEWAY.cast_unsigned();
        }
        fl
    };
    if group.len() > 1 {
        m.u32(rta::KNH_ID, r.nhidx);
        let base = flags_of(r);
        m.u32(rta::RTFLAGS, base);
        let at = m.nest(rta::MULTIPATH);
        for p in group {
            // `struct rtnexthop`: its length (patched), flags, hops, index.
            let nh = m.buf.len();
            m.buf.extend_from_slice(&0u16.to_ne_bytes());
            m.buf.push(0);
            m.buf.push(u8::try_from(p.weight).unwrap_or(u8::MAX));
            m.buf.extend_from_slice(&i32::from(p.ifindex).to_ne_bytes());
            if let Gateway::Addr(g) = p.gateway {
                m.attr(rta::GATEWAY, addr_bytes(p.family, &g));
            }
            if flags_of(p) != base {
                m.u32(rta::RTFLAGS, flags_of(p));
            }
            let len = u16::try_from(m.buf.len() - nh).unwrap_or(u16::MAX);
            m.buf[nh..nh + 2].copy_from_slice(&len.to_ne_bytes());
        }
        m.end_nest(at);
        return m.finish();
    }
    m.u32(rta::KNH_ID, r.nhidx);
    m.u32(rta::RTFLAGS, flags_of(r));
    if r.mtu != LOMTU {
        let at = m.nest(rta::METRICS);
        m.u32(rta::AX_MTU, r.mtu);
        m.end_nest(at);
    }
    m.u32(rta::OIF, u32::from(r.ifindex));
    if r.weight != 1 {
        m.u32(rta::WEIGHT, r.weight);
    }
    m.finish()
}

/// Routes grouped by prefix, in a radix walk's order. A link-local
/// prefix is each interface's own (FreeBSD embeds the zone in it).
fn groups<'a>(routes: impl Iterator<Item = &'a Route>) -> Vec<Vec<&'a Route>> {
    let zone = |r: &Route| {
        if crate::ifnet::route_scoped(r) {
            crate::ifnet::route_zone(r)
        } else {
            0
        }
    };
    let mut all: Vec<&Route> = routes.collect();
    all.sort_by_key(|r| (r.family, r.dst, r.plen, zone(r)));
    let mut out: Vec<Vec<&Route>> = Vec::new();
    for r in all {
        match out.last_mut() {
            Some(g)
                if g[0].family == r.family
                    && g[0].dst == r.dst
                    && g[0].plen == r.plen
                    && zone(g[0]) == zone(r) =>
            {
                g.push(r)
            }
            _ => out.push(alloc::vec![r]),
        }
    }
    out
}

/// `rtnl_handle_getroute`: a dump, or the route a destination takes.
fn getroute(
    ctx: &Ctx,
    net: &NetLock,
    h: &Hdr,
    body: &[u8],
    out: &mut Vec<u8>,
    o: &mut Outcome,
) -> Result<(), Errno> {
    let family = body.first().copied().unwrap_or(0);
    let a = attrs(body.get(12..).unwrap_or(&[]));
    let fib = find(&a, rta::TABLE).map_or(u32::from(ctx.fib), |t| rd32(t, 0));
    if fib >= crate::numfibs() {
        o.msg = Some(b"invalid fib");
        return Err(Errno::EINVAL);
    }
    #[allow(clippy::cast_possible_truncation, reason = "below RT_MAXFIBS")]
    let fib = fib as u16;
    let n = net.lock();
    if h.flags & nlm::DUMP != 0 {
        for g in groups(
            n.routes
                .iter()
                .filter(|r| r.fib == fib && (family == 0 || r.family.af() == family)),
        ) {
            out.extend(route_msg(ctx, h, &g, true));
        }
        drop(n);
        done(ctx, h, None, out, o);
        return Ok(());
    }
    let f = match family {
        AF_INET => Family::Inet,
        AF_INET6 => Family::Inet6,
        _ => return Err(Errno::EAFNOSUPPORT),
    };
    let Some(dst) = ip_attr(f, find(&a, rta::DST)) else {
        o.msg = Some(b"No RTA_DST supplied");
        return Err(Errno::EINVAL);
    };
    // `handle_rtm_getroute`: with `RTM_F_PREFIX` the route to exactly
    // `RTA_DST/rtm_dst_len` (`route get default` asks so), else the
    // longest match.
    let rtm_flags = rd32(body, 8);
    let plen = body.get(1).copied().unwrap_or(0);
    let r = if rtm_flags & RTM_F_PREFIX != 0 {
        let key = crate::ifnet::masked(f, &dst, plen);
        n.routes
            .iter()
            .filter(|r| r.fib == fib && r.family == f && r.plen == plen && r.dst == key)
            .min_by_key(|r| r.nhidx)
    } else {
        n.routes
            .iter()
            .filter(|r| {
                r.fib == fib && r.family == f && crate::addr::prefix_match(f, &r.dst, &dst, r.plen)
            })
            .max_by_key(|r| r.plen)
    }
    .ok_or(Errno::ESRCH)?;
    let group: Vec<&Route> = n
        .routes
        .iter()
        .filter(|x| x.fib == r.fib && x.family == r.family && x.plen == r.plen && x.dst == r.dst)
        .collect();
    let msg = route_msg(ctx, h, &group, false);
    out.extend(msg);
    Ok(())
}

/// `RTM_F_PREFIX`: a lookup of the prefix exactly.
const RTM_F_PREFIX: u32 = 0x800;

/// `rtnl_handle_newroute`: a route to `RTA_DST/rtm_dst_len` through
/// `RTA_GATEWAY` or out of `RTA_OIF`, rejecting or blackholing by
/// `rtm_type`.
fn newroute(ctx: &Ctx, net: &NetLock, h: &Hdr, body: &[u8], o: &mut Outcome) -> Result<(), Errno> {
    let family = match body.first().copied() {
        Some(AF_INET) => Family::Inet,
        Some(AF_INET6) => Family::Inet6,
        _ => return Err(Errno::EAFNOSUPPORT),
    };
    let plen = body.get(1).copied().unwrap_or(0);
    let rtype = body.get(7).copied().unwrap_or(rtn::UNICAST);
    let a = attrs(body.get(12..).unwrap_or(&[]));
    let Some(dst) = ip_attr(family, find(&a, rta::DST)) else {
        o.msg = Some(b"missing RTA_DST");
        return Err(Errno::EINVAL);
    };
    let fib = find(&a, rta::TABLE).map_or(u32::from(ctx.fib), |t| rd32(t, 0));
    if fib >= crate::numfibs() {
        o.msg = Some(b"invalid fib");
        return Err(Errno::EINVAL);
    }
    #[allow(clippy::cast_possible_truncation, reason = "below RT_MAXFIBS")]
    let fib = fib as u16;
    let gw = ip_attr(family, find(&a, rta::GATEWAY));
    let oif = find(&a, rta::OIF).map(|d| rd32(d, 0));
    let mut flags = find(&a, rta::RTFLAGS).map_or(0, |d| rd32(d, 0).cast_signed());
    match rtype {
        rtn::BLACKHOLE => flags |= rtf::BLACKHOLE,
        rtn::PROHIBIT | rtn::UNREACHABLE => flags |= rtf::REJECT,
        _ => {}
    }
    let mtu = find(&a, rta::METRICS)
        .map(attrs)
        .and_then(|m| find(&m, rta::AX_MTU).map(|d| rd32(d, 0)))
        .unwrap_or(LOMTU);
    let mut n = net.lock();
    // The interface: the one named, or the one the gateway is reached
    // through (or is an address of).
    let ifindex = match (oif, gw) {
        (Some(i), _) => u16::try_from(i).map_err(|_| Errno::EINVAL)?,
        (None, Some(g)) => {
            if let Some((i, _)) = n.ifaddr(family, &g) {
                i.index
            } else {
                n.lookup(family, &g)
                    .map_err(|_| Errno::EHOSTUNREACH)?
                    .ifindex
            }
        }
        // `nhop_set_blackhole`: a blackhole or reject route needs none;
        // it goes through `lo0`.
        (None, None) if flags & (rtf::REJECT | rtf::BLACKHOLE) != 0 => n
            .ifaces
            .iter()
            .find(|i| i.flags & iff::LOOPBACK != 0 && i.dunit == 0)
            .map(|i| i.index)
            .ok_or(Errno::EINVAL)?,
        (None, None) => return Err(Errno::EINVAL),
    };
    let ifa = n
        .by_index(ifindex)
        .and_then(|i| i.addrs.iter().find(|x| x.family == family))
        .map_or([0; 16], |x| x.addr);
    let gateway = match gw {
        // A gateway that is the interface's own address is an
        // interface route (`-iface`).
        Some(g)
            if n.ifaddr(family, &g).is_none() || flags & (rtf::REJECT | rtf::BLACKHOLE) != 0 =>
        {
            flags |= rtf::GATEWAY;
            Gateway::Addr(g)
        }
        _ => Gateway::Link(ifindex),
    };
    let max = if family == Family::Inet { 32 } else { 128 };
    if plen > max {
        return Err(Errno::EINVAL);
    }
    if plen == max {
        flags |= rtf::HOST;
    }
    let r = Route {
        family,
        dst,
        plen,
        gateway,
        flags: flags | rtf::UP | rtf::STATIC,
        ifindex,
        ifa,
        mtu,
        weight: 1,
        expire: 0,
        nhidx: 0,
        fib,
    };
    let exists = n.routes.iter().position(|x| {
        x.family == family
            && x.fib == fib
            && x.plen == plen
            && x.dst == crate::ifnet::masked(family, &dst, plen)
    });
    match exists {
        Some(i) if h.flags & nlm::REPLACE != 0 => {
            n.routes[i] = Route {
                dst: crate::ifnet::masked(family, &dst, plen),
                ..r
            };
            Ok(())
        }
        // `rib_add_route` with `RTM_F_APPEND` (`route add`'s default): a
        // new path joins the prefix's multipath group if both can
        // (`nhop_can_multipath`); the same path again is `EEXIST`.
        Some(i)
            if h.flags & nlm::APPEND != 0
                && h.flags & nlm::EXCL == 0
                && can_multipath(&n.routes[i])
                && can_multipath(&r) =>
        {
            n.add_route(r)
        }
        Some(_) => Err(Errno::EEXIST),
        None => n.add_route(r),
    }
}

/// `nhop_can_multipath`: a gateway route that is not a redirect's.
fn can_multipath(r: &Route) -> bool {
    /// `RTF_DYNAMIC`.
    const DYNAMIC: i32 = 0x10;
    r.flags & rtf::GATEWAY != 0 && r.flags & DYNAMIC == 0
}

/// `rtnl_handle_delroute`.
fn delroute(ctx: &Ctx, net: &NetLock, body: &[u8], o: &mut Outcome) -> Result<(), Errno> {
    let family = match body.first().copied() {
        Some(AF_INET) => Family::Inet,
        Some(AF_INET6) => Family::Inet6,
        _ => return Err(Errno::EAFNOSUPPORT),
    };
    let plen = body.get(1).copied().unwrap_or(0);
    let a = attrs(body.get(12..).unwrap_or(&[]));
    let Some(dst) = ip_attr(family, find(&a, rta::DST)) else {
        o.msg = Some(b"RTA_DST is not set");
        return Err(Errno::ESRCH);
    };
    let fib =
        find(&a, rta::TABLE).map_or(ctx.fib, |t| u16::try_from(rd32(t, 0)).unwrap_or(u16::MAX));
    let mut n = net.lock();
    let pinned = n.routes.iter().any(|x| {
        x.family == family
            && x.fib == fib
            && x.plen == plen
            && x.dst == crate::ifnet::masked(family, &dst, plen)
            && x.flags & rtf::PINNED != 0
    });
    let force = find(&a, rta::RTFLAGS).is_some_and(|d| rd32(d, 0).cast_signed() & rtf::PINNED != 0);
    if pinned && !force {
        return Err(Errno::EADDRINUSE);
    }
    let d = crate::ifnet::masked(family, &dst, plen);
    let r = n
        .routes
        .iter()
        .find(|x| x.family == family && x.fib == fib && x.plen == plen && x.dst == d)
        .cloned()
        .ok_or(Errno::ESRCH)?;
    n.delete_exact(&r).map(|_| ())
}

/// Netlink's multicast groups (`RTNLGRP_*`).
pub mod group {
    pub const LINK: u32 = 1;
    pub const IPV4_IFADDR: u32 = 5;
    pub const IPV4_ROUTE: u32 = 7;
    pub const IPV6_IFADDR: u32 = 9;
    pub const IPV6_ROUTE: u32 = 11;
}

/// The notification a change makes for netlink listeners, and its group
/// (`rtnl_handle_ifevent`, `rtnl_handle_ifaddr`, `rtnl_handle_route_event`).
#[must_use]
pub fn notify(e: &crate::announce::Event, now_s: u64) -> Option<(u32, Vec<u8>)> {
    use crate::announce::Event;
    let ctx = Ctx::default();
    let hdr = |t: u16| Hdr {
        len: 0,
        ty: t,
        flags: 0,
        seq: 0,
    };
    // A notification carries no request's flags.
    let fix = |mut m: Vec<u8>, t: u16| {
        m[4..6].copy_from_slice(&t.to_ne_bytes());
        m[6..8].copy_from_slice(&0u16.to_ne_bytes());
        m
    };
    let net = Net::new();
    match e {
        Event::Announce { ifp, arrival } => {
            let t = if *arrival { ty::NEWLINK } else { ty::DELLINK };
            Some((
                group::LINK,
                fix(link_msg(&ctx, &hdr(t), &net, ifp, false), t),
            ))
        }
        Event::Info(ifp) | Event::Link { ifp, .. } => Some((
            group::LINK,
            fix(
                link_msg(&ctx, &hdr(ty::NEWLINK), &net, ifp, false),
                ty::NEWLINK,
            ),
        )),
        Event::Addr { add, ifp, addr } => {
            let t = if *add { ty::NEWADDR } else { ty::DELADDR };
            let g = match addr.family {
                Family::Inet => group::IPV4_IFADDR,
                Family::Inet6 => group::IPV6_IFADDR,
            };
            Some((g, fix(addr_msg(&ctx, &hdr(t), ifp, addr, now_s), t)))
        }
        Event::Route { ty: rt, route, .. } => {
            let t = if *rt == abi::net::rtm::DELETE {
                ty::DELROUTE
            } else {
                ty::NEWROUTE
            };
            let g = match route.family {
                Family::Inet => group::IPV4_ROUTE,
                Family::Inet6 => group::IPV6_ROUTE,
            };
            Some((g, fix(route_msg(&ctx, &hdr(t), &[route], false), t)))
        }
        Event::Reply { .. } => None,
    }
}

/// The generic controller (`nlctrl`): the one family registered, and its
/// notification group. Families of modules AbyssBSD does not have
/// (`carp`, `pfsync`, `wlan`) are absent, as on a kernel without them.
mod genl {
    /// `GENL_ID_CTRL`, `CTRL_CMD_NEWFAMILY`, `CTRL_CMD_GETFAMILY`.
    pub(super) const ID_CTRL: u16 = 16;
    pub(super) const CMD_NEWFAMILY: u8 = 1;
    pub(super) const CMD_GETFAMILY: u8 = 3;
    /// `CTRL_ATTR_*`.
    pub(super) const FAMILY_ID: u16 = 1;
    pub(super) const FAMILY_NAME: u16 = 2;
    pub(super) const VERSION: u16 = 3;
    pub(super) const HDRSIZE: u16 = 4;
    pub(super) const MAXATTR: u16 = 5;
    pub(super) const OPS: u16 = 6;
    pub(super) const MCAST_GROUPS: u16 = 7;
    pub(super) const OP_ID: u16 = 1;
    pub(super) const OP_FLAGS: u16 = 2;
    pub(super) const GRP_NAME: u16 = 1;
    pub(super) const GRP_ID: u16 = 2;
    /// `CTRL_ATTR_MAX`.
    pub(super) const ATTR_MAX: u32 = 10;
    /// `GENL_CMD_CAP_DO | GENL_CMD_CAP_DUMP | GENL_CMD_CAP_HASPOL`.
    pub(super) const GETFAMILY_FLAGS: u32 = 0x2 | 0x4 | 0x8;
    /// `MIN_GROUP_NUM`: the controller's `notify` group.
    pub(super) const NOTIFY_GROUP: u32 = 48;
}

/// `dump_family` of `nlctrl`.
fn family_msg(ctx: &Ctx, h: &Hdr, flags: u16) -> Vec<u8> {
    let mut m = Msg::new(genl::ID_CTRL, flags, h.seq, ctx.port);
    m.raw(&[genl::CMD_NEWFAMILY, 2, 0, 0]);
    m.string(genl::FAMILY_NAME, b"nlctrl");
    m.attr(genl::FAMILY_ID, &genl::ID_CTRL.to_ne_bytes());
    m.u32(genl::VERSION, 2);
    m.u32(genl::HDRSIZE, 0);
    m.u32(genl::MAXATTR, genl::ATTR_MAX);
    let ops = m.nest(genl::OPS);
    let op = m.nest(1);
    m.u32(genl::OP_ID, u32::from(genl::CMD_GETFAMILY));
    m.u32(genl::OP_FLAGS, genl::GETFAMILY_FLAGS);
    m.end_nest(op);
    m.end_nest(ops);
    let groups = m.nest(genl::MCAST_GROUPS);
    let g = m.nest(1);
    m.u32(genl::GRP_ID, genl::NOTIFY_GROUP);
    m.string(genl::GRP_NAME, b"notify");
    m.end_nest(g);
    m.end_nest(groups);
    m.finish()
}

/// `genl_handle_message` for one request.
fn generic_one(
    ctx: &Ctx,
    h: &Hdr,
    msg: &[u8],
    out: &mut Vec<u8>,
    o: &mut Outcome,
) -> Result<(), Errno> {
    if msg.len() < 20 {
        return Err(Errno::EINVAL);
    }
    if h.ty != genl::ID_CTRL || msg[16] != genl::CMD_GETFAMILY {
        return Err(Errno::EOPNOTSUPP);
    }
    let a = attrs(&msg[20..]);
    let id = find(&a, genl::FAMILY_ID)
        .map(|v| rd16(v, 0))
        .filter(|&v| v != 0);
    let name = find(&a, genl::FAMILY_NAME).map(|v| v.split(|&b| b == 0).next().unwrap_or(&[]));
    if id.is_some() || name.is_some() {
        if id.is_some_and(|i| i != genl::ID_CTRL) || name.is_some_and(|n| n != b"nlctrl") {
            return Err(Errno::ENOENT);
        }
        out.extend(family_msg(ctx, h, h.flags));
        return Ok(());
    }
    out.extend(family_msg(ctx, h, h.flags | nlm::MULTI));
    done(ctx, h, None, out, o);
    Ok(())
}

/// Processes one written buffer of `NETLINK_GENERIC` requests.
#[must_use]
pub fn handle_generic(ctx: &Ctx, packet: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    while packet.len() - at >= 16 {
        let h = Hdr {
            len: rd32(packet, at) as usize,
            ty: rd16(packet, at + 4),
            flags: rd16(packet, at + 6),
            seq: rd32(packet, at + 8),
        };
        if h.len < 16 || h.len > packet.len() - at {
            break;
        }
        let msg = &packet[at..at + h.len];
        let mut o = Outcome::default();
        if h.flags & nlm::REQUEST != 0
            && h.ty >= ty::MIN_TYPE
            && let Err(e) = generic_one(ctx, &h, msg, &mut out, &mut o)
        {
            o.error = Some(e);
        }
        let error = o.error;
        if !o.done && (h.flags & nlm::ACK != 0 || error.is_some()) {
            ack(ctx, &h, msg, error, o.msg, &mut out);
        }
        at += align(h.len);
    }
    out
}
