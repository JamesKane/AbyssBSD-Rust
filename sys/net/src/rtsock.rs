// SPDX-License-Identifier: BSD-2-Clause
//! The routing socket (`PF_ROUTE`): requests written to it (`RTM_ADD`,
//! `RTM_DELETE`, `RTM_CHANGE`, `RTM_GET`), each answered with the request
//! completed (`RTF_DONE`, or `rtm_errno`), and the messages that announce
//! changes to every routing socket of the VNET (`RTM_IFANNOUNCE`,
//! `RTM_IFINFO`, `RTM_NEWADDR`, `RTM_DELADDR`, and route changes).
//!
//! Behavior reference: sys/net/rtsock.c (`rts_send`, `route_output`,
//! `fill_addrinfo`, `update_rtm_from_rc`, `rt_ifannouncemsg`, `rt_ifmsg`,
//! `rt_newaddrmsg_fib`, `rt_routemsg`).

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::{AF_INET, AF_INET6, AF_LINK};
use abi::net::{IFNAMSIZ, RTM_VERSION, RtMsghdr, rta, rtf, rtm, rtv};

use crate::Family;
use crate::addr::{In6, from_v4};
use crate::announce::Event;
use crate::ifnet::{Gateway, Ifnet, Net, Route, masked};
use crate::rtmsg::{self, mask_of, max_plen, put, record, sa_of, sa_size};

/// `sizeof(struct rt_msghdr)`.
const HDR: usize = size_of::<RtMsghdr>();
/// `RTAX_MAX`.
const RTAX_MAX: usize = 8;

/// A parsed address.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Sa {
    In(Family, In6),
    /// A `sockaddr_dl`: its index, and its name if it has one.
    Link(u16, Vec<u8>),
    Other,
}

fn parse_sa(b: &[u8]) -> Sa {
    let (Some(&len), Some(&fam)) = (b.first(), b.get(1)) else {
        return Sa::Other;
    };
    let b = &b[..usize::from(len).min(b.len())];
    match fam {
        AF_INET if b.len() >= 8 => Sa::In(Family::Inet, from_v4([b[4], b[5], b[6], b[7]])),
        AF_INET6 if b.len() >= 24 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&b[8..24]);
            Sa::In(Family::Inet6, a)
        }
        AF_LINK if b.len() >= 8 => {
            let index = u16::from_ne_bytes([b[2], b[3]]);
            let nlen = usize::from(b[5]);
            Sa::Link(index, b.get(8..8 + nlen).unwrap_or(&[]).to_vec())
        }
        _ => Sa::Other,
    }
}

/// A netmask's prefix length: its leading ones; a short `sockaddr`
/// (`rtsock_fix_netmask`'s callers send one) has zeros past its end.
fn plen_of(family: Family, b: &[u8]) -> Result<u8, Errno> {
    let (off, bits) = match family {
        Family::Inet => (4usize, 32u8),
        Family::Inet6 => (8usize, 128u8),
    };
    let len = b.first().map_or(0, |l| usize::from(*l)).min(b.len());
    let mut m = [0u8; 16];
    let n = usize::from(bits / 8);
    for (i, x) in m[..n].iter_mut().enumerate() {
        *x = if off + i < len { b[off + i] } else { 0 };
    }
    let mut plen = 0u8;
    let mut seen_zero = false;
    for byte in &m[..n] {
        for k in (0..8).rev() {
            if byte >> k & 1 == 1 {
                if seen_zero {
                    return Err(Errno::EINVAL);
                }
                plen += 1;
            } else {
                seen_zero = true;
            }
        }
    }
    Ok(plen)
}

/// A request's header fields and addresses (`struct rt_addrinfo`).
struct Req<'a> {
    hdr: RtMsghdr,
    sas: [Option<&'a [u8]>; RTAX_MAX],
}

fn parse(msg: &[u8]) -> Result<Req<'_>, Errno> {
    if msg.len() < HDR {
        return Err(Errno::EINVAL);
    }
    let len = usize::from(u16::from_ne_bytes([msg[0], msg[1]]));
    if len < HDR || len > msg.len() {
        return Err(Errno::EINVAL);
    }
    if msg[2] != RTM_VERSION {
        return Err(Errno::EPROTONOSUPPORT);
    }
    // SAFETY: `RtMsghdr` is integers with no padding; `msg` holds at
    // least `HDR` bytes, read unaligned.
    let hdr: RtMsghdr = unsafe { core::ptr::read_unaligned(msg.as_ptr().cast()) };
    let mut sas = [None; RTAX_MAX];
    let mut at = HDR;
    for (i, slot) in sas.iter_mut().enumerate() {
        if hdr.rtm_addrs & (1 << i) == 0 {
            continue;
        }
        let Some(&sl) = msg.get(at) else {
            return Err(Errno::EINVAL);
        };
        let n = sa_size(usize::from(sl));
        let end = (at + usize::from(sl)).min(len);
        if at >= len {
            return Err(Errno::EINVAL);
        }
        *slot = Some(&msg[at..end]);
        at += n;
    }
    Ok(Req { hdr, sas })
}

/// An interface route's gateway: a bare `sockaddr_dl` naming only its
/// index and type (`link#N`).
fn link_gw(index: u16, ty: u8) -> Vec<u8> {
    let mut d = alloc::vec![0u8; 54];
    d[0] = 54;
    d[1] = AF_LINK;
    d[2..4].copy_from_slice(&index.to_ne_bytes());
    d[4] = ty;
    d
}

/// The addresses a reply or announcement of `r` carries.
fn route_addrs(r: &Route, gw_ty: u8, ifp: Option<&Ifnet>) -> [Option<Vec<u8>>; RTAX_MAX] {
    let mut a: [Option<Vec<u8>>; RTAX_MAX] = Default::default();
    a[0] = Some(sa_of(r.family, &r.dst, r.ifindex));
    a[1] = Some(match r.gateway {
        Gateway::Link(i) => link_gw(i, gw_ty),
        Gateway::Addr(g) => sa_of(r.family, &g, r.ifindex),
    });
    if r.plen != max_plen(r.family) {
        a[2] = Some(mask_of(r.family, r.plen));
    }
    if let Some(ifp) = ifp {
        a[4] = Some(rtmsg::link_sockaddr(ifp));
        a[5] = Some(sa_of(r.family, &r.ifa, r.ifindex));
    }
    a
}

/// A route message: `base` with the route's index, flags, metrics, and
/// addresses (`update_rtm_from_rc`).
fn route_msg(r: &Route, gw_ty: u8, ifp: Option<&Ifnet>, mut base: RtMsghdr, ty: u8) -> Vec<u8> {
    base.rtm_index = r.ifindex;
    base.rtm_flags = r.flags
        | rtf::UP
        | if r.plen == max_plen(r.family) {
            rtf::HOST
        } else {
            0
        };
    base.rtm_rmx.rmx_mtu = u64::from(r.mtu);
    base.rtm_rmx.rmx_weight = u64::from(r.weight);
    base.rtm_rmx.rmx_nhidx = u64::from(r.nhidx);
    base.rtm_rmx.rmx_expire = r.expire;
    if r.expire != 0 {
        base.rtm_inits |= rtv::EXPIRE;
    }
    // A deleted route is no longer up.
    if ty == rtm::DELETE {
        base.rtm_flags &= !rtf::UP;
    }
    let mut v = Vec::new();
    put(&mut v, &base);
    let (mut rec, bits) = record(v, ty, &route_addrs(r, gw_ty, ifp));
    rec[core::mem::offset_of!(RtMsghdr, rtm_addrs)..][..4].copy_from_slice(&bits.to_ne_bytes());
    rec
}

/// The route `dst/plen` (exact) or covering `dst` (longest prefix).
fn find(n: &Net, fib: u16, family: Family, dst: &In6, plen: Option<u8>) -> Option<Route> {
    n.routes
        .iter()
        .filter(|r| r.family == family && r.fib == fib)
        .filter(|r| match plen {
            Some(p) => r.plen == p && r.dst == masked(family, dst, p),
            None => crate::addr::prefix_match(family, &r.dst, dst, r.plen),
        })
        .max_by_key(|r| r.plen)
        .cloned()
}

/// Which interface a gateway is reached through (`ifa_ifwithroute`): an
/// interface it names, one of whose prefixes holds it, or one of whose
/// addresses it is.
fn via(n: &Net, family: Family, gw: &Sa) -> Option<(u16, In6, bool)> {
    match gw {
        Sa::Link(index, name) => {
            let ifp = if *index != 0 {
                n.by_index(*index)
            } else {
                n.by_name(name)
            }?;
            let ifa = ifp
                .addrs
                .iter()
                .find(|a| a.family == family)
                .map_or([0; 16], |a| a.addr);
            Some((ifp.index, ifa, false))
        }
        // `ifa_ifwithnet`: the most specific prefix holding it.
        Sa::In(f, g) if *f == family => n
            .ifaces
            .iter()
            .flat_map(|i| i.addrs.iter().map(move |a| (i, a)))
            .filter(|(_, a)| {
                a.family == family
                    && (crate::ifnet::same_addr(family, &a.addr, g)
                        || crate::addr::prefix_match(family, &a.addr, g, a.plen))
            })
            .max_by_key(|(_, a)| a.plen)
            .map(|(i, a)| (i.index, a.addr, true)),
        _ => None,
    }
}

/// What `route_output` keeps of a request's flags.
const KEPT: i32 = rtf::GATEWAY
    | rtf::HOST
    | rtf::REJECT
    | rtf::BLACKHOLE
    | rtf::STATIC
    | rtf::PROTO1
    | rtf::PROTO2
    | rtf::PROTO3
    | rtf::FIXEDMTU;

/// Handles one request (`route_output`) on `n`, as process `pid` sent it:
/// the reply (the request with `RTF_DONE`, or with `rtm_errno` set), and
/// the error the write returns. A change the request makes is announced
/// by the reply itself, not separately.
pub fn request(
    n: &mut Net,
    msg: &[u8],
    pid: i32,
    may_change: bool,
    fib: u16,
) -> (Vec<u8>, Result<(), Errno>) {
    let req = match parse(msg) {
        Ok(r) => r,
        Err(e) => return (Vec::new(), Err(e)),
    };
    let mut base = req.hdr;
    base.rtm_pid = pid;
    let fail = |mut h: RtMsghdr, e: Errno| {
        h.rtm_errno = e.get();
        let mut v = Vec::new();
        put(&mut v, &h);
        // The request's own addresses, back as they came.
        v.truncate(HDR);
        v.extend_from_slice(&msg[HDR..usize::from(h.rtm_msglen).min(msg.len())]);
        (v, Err(e))
    };
    let ty = base.rtm_type;
    if ty != rtm::GET && !may_change {
        return fail(base, Errno::EPERM);
    }
    let dst = match req.sas[0].map(parse_sa) {
        Some(Sa::In(f, a)) => (f, a),
        _ => return fail(base, Errno::EINVAL),
    };
    let (family, dst_addr) = dst;
    let plen = match req.sas[2] {
        Some(m) => match plen_of(family, m) {
            Ok(p) => Some(p),
            Err(e) => return fail(base, e),
        },
        None if ty == rtm::ADD || base.rtm_flags & rtf::HOST != 0 => Some(max_plen(family)),
        None => None,
    };
    let gw = req.sas[1].map(parse_sa);
    let before = n.outbox.len();
    let r = match ty {
        rtm::GET => find(n, fib, family, &dst_addr, plen).ok_or(Errno::ESRCH),
        rtm::ADD => add(
            n,
            fib,
            family,
            &dst_addr,
            plen.unwrap_or(max_plen(family)),
            gw.as_ref(),
            &base,
        ),
        rtm::DELETE => {
            let plen = plen.unwrap_or(max_plen(family));
            // A gateway picks the path of a multipath group.
            match paths(n, fib, family, &dst_addr, plen)
                .into_iter()
                .find(|r| gw.as_ref().is_none_or(|g| gw_matches(r, g)))
            {
                Some(old) => n.delete_exact(&old),
                None => Err(Errno::ESRCH),
            }
        }
        rtm::CHANGE => change(n, fib, family, &dst_addr, plen, gw.as_ref(), &base),
        _ => Err(Errno::EOPNOTSUPP),
    };
    // The reply announces the change to routing sockets; netlink still
    // hears the route events.
    for e in &mut n.outbox[before..] {
        if let Event::Route { rtsock, .. } = e {
            *rtsock = true;
        }
    }
    match r {
        Ok(route) => {
            let mut h = base;
            h.rtm_errno = 0;
            let want_ifp = ty == rtm::GET && req.hdr.rtm_addrs & (rta::IFP | rta::IFA) != 0;
            let gw_ty = match route.gateway {
                Gateway::Link(i) => n.by_index(i).map_or(0, |f| f.ty),
                Gateway::Addr(_) => 0,
            };
            let ifp = want_ifp.then(|| n.by_index(route.ifindex)).flatten();
            let mut out = route_msg(&route, gw_ty, ifp, h, ty);
            let mut flags = route.flags
                | rtf::UP
                | rtf::DONE
                | if route.plen == max_plen(family) {
                    rtf::HOST
                } else {
                    0
                };
            if ty == rtm::DELETE {
                flags &= !rtf::UP;
            }
            let at = core::mem::offset_of!(RtMsghdr, rtm_flags);
            out[at..at + 4].copy_from_slice(&flags.to_ne_bytes());
            (out, Ok(()))
        }
        Err(e) => fail(base, e),
    }
}

fn gw_matches(r: &Route, g: &Sa) -> bool {
    match (g, r.gateway) {
        (Sa::In(_, a), Gateway::Addr(b)) => crate::ifnet::same_addr(r.family, a, &b),
        (Sa::Link(i, _), Gateway::Link(j)) => *i == 0 || *i == j,
        _ => false,
    }
}

fn add(
    n: &mut Net,
    fib: u16,
    family: Family,
    dst: &In6,
    plen: u8,
    gw: Option<&Sa>,
    h: &RtMsghdr,
) -> Result<Route, Errno> {
    let gw = gw.ok_or(Errno::EINVAL)?;
    let (ifindex, ifa, by_addr) = via(n, family, gw).ok_or(Errno::ENETUNREACH)?;
    let gateway = match gw {
        Sa::In(_, g) if by_addr => Gateway::Addr(*g),
        _ => Gateway::Link(ifindex),
    };
    let mut flags = h.rtm_flags & KEPT | rtf::UP;
    if matches!(gateway, Gateway::Link(_)) {
        flags &= !rtf::GATEWAY;
    }
    if plen == max_plen(family) {
        flags |= rtf::HOST;
    }
    let ifmtu = n.by_index(ifindex).map_or(0, |i| i.mtu);
    let mtu = if h.rtm_inits & rtv::MTU != 0 && h.rtm_rmx.rmx_mtu != 0 {
        u32::try_from(h.rtm_rmx.rmx_mtu).unwrap_or(ifmtu).min(ifmtu)
    } else {
        ifmtu
    };
    let r = Route {
        family,
        dst: masked(family, dst, plen),
        plen,
        gateway,
        flags,
        ifindex,
        ifa,
        mtu,
        weight: if h.rtm_inits & rtv::WEIGHT != 0 {
            weight(h.rtm_rmx.rmx_weight)
        } else {
            1
        },
        expire: if h.rtm_inits & rtv::EXPIRE != 0 {
            h.rtm_rmx.rmx_expire
        } else {
            0
        },
        nhidx: 0,
        fib,
    };
    n.add_route(r.clone())?;
    // As stored, with its next hop.
    Ok(n.routes.last().cloned().unwrap_or(r))
}

fn change(
    n: &mut Net,
    fib: u16,
    family: Family,
    dst: &In6,
    plen: Option<u8>,
    gw: Option<&Sa>,
    h: &RtMsghdr,
) -> Result<Route, Errno> {
    let plen = plen.unwrap_or(max_plen(family));
    let all = paths(n, fib, family, dst, plen);
    // In a multipath group the gateway names the path; a single path's
    // gateway may change.
    let old = match (all.len(), gw) {
        (0, _) => return Err(Errno::ESRCH),
        (1, _) | (_, None) => all[0].clone(),
        (_, Some(g)) => all
            .iter()
            .find(|r| gw_matches(r, g))
            .cloned()
            .ok_or(Errno::ESRCH)?,
    };
    let mut new = old.clone();
    if let Some(g) = gw {
        let (ifindex, ifa, by_addr) = via(n, family, g).ok_or(Errno::ENETUNREACH)?;
        new.gateway = match g {
            Sa::In(_, a) if by_addr => Gateway::Addr(*a),
            _ => Gateway::Link(ifindex),
        };
        new.ifindex = ifindex;
        new.ifa = ifa;
    }
    if h.rtm_inits & rtv::MTU != 0 {
        new.mtu = u32::try_from(h.rtm_rmx.rmx_mtu).unwrap_or(new.mtu);
    }
    if h.rtm_inits & rtv::WEIGHT != 0 {
        new.weight = weight(h.rtm_rmx.rmx_weight);
    }
    // The request's flags replace the route's (`rib_change_route` makes
    // the next hop anew from them), the gateway's kind kept.
    new.flags = h.rtm_flags & KEPT & !rtf::GATEWAY
        | rtf::UP
        | match new.gateway {
            Gateway::Addr(_) => rtf::GATEWAY,
            Gateway::Link(_) => 0,
        };
    n.delete_exact(&old)?;
    n.add_route(new.clone())?;
    Ok(n.routes.last().cloned().unwrap_or(new))
}

/// The paths to exactly `dst/plen`.
fn paths(n: &Net, fib: u16, family: Family, dst: &In6, plen: u8) -> Vec<Route> {
    let d = masked(family, dst, plen);
    n.routes
        .iter()
        .filter(|r| r.family == family && r.fib == fib && r.plen == plen && r.dst == d)
        .cloned()
        .collect()
}

/// `RTV_WEIGHT`'s weight: 0 is the default (`RT_DEFAULT_WEIGHT`), and
/// the most is `RT_MAX_WEIGHT`.
fn weight(w: u64) -> u32 {
    match w {
        0 => 1,
        w => u32::try_from(w).unwrap_or(u32::MAX).min(16_777_215),
    }
}

/// The routing message announcing `e`, if routing sockets hear of it.
#[must_use]
pub fn announce(e: &Event) -> Option<(Option<Family>, Vec<u8>)> {
    match e {
        Event::Announce { ifp, arrival } => {
            let (index, name) = (&ifp.index, &ifp.name);
            // `struct if_announcemsghdr`.
            let mut v = alloc::vec![0u8; 24];
            v[0..2].copy_from_slice(&24u16.to_ne_bytes());
            v[2] = RTM_VERSION;
            v[3] = rtm::IFANNOUNCE;
            v[4..6].copy_from_slice(&index.to_ne_bytes());
            let nb = name.as_bytes();
            let k = nb.len().min(IFNAMSIZ - 1);
            v[6..6 + k].copy_from_slice(&nb[..k]);
            let what: u16 = if *arrival { 0 } else { 1 };
            v[22..24].copy_from_slice(&what.to_ne_bytes());
            Some((None, v))
        }
        Event::Info(ifp) => Some((None, rtmsg::ifinfo_record(ifp))),
        Event::Addr { add, ifp, addr } => {
            let ty = if *add { rtm::NEWADDR } else { rtm::DELADDR };
            Some((Some(addr.family), rtmsg::ifa_record_typed(ifp, addr, ty)))
        }
        Event::Reply { family, msg, .. } => Some((*family, msg.clone())),
        Event::Route { rtsock: true, .. } => None,
        Event::Route { ty, route, ifp, .. } => {
            let h = RtMsghdr {
                rtm_version: RTM_VERSION,
                ..RtMsghdr::default()
            };
            let gw_ty = ifp.as_ref().map_or(0, |i| i.ty);
            let mut m = route_msg(route, gw_ty, None, h, *ty);
            let at = core::mem::offset_of!(RtMsghdr, rtm_flags);
            let f = i32::from_ne_bytes(m[at..at + 4].try_into().unwrap_or([0; 4])) | rtf::DONE;
            m[at..at + 4].copy_from_slice(&f.to_ne_bytes());
            Some((Some(route.family), m))
        }
    }
}
