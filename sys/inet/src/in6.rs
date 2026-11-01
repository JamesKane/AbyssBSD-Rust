// SPDX-License-Identifier: BSD-2-Clause
//! IPv6 addresses as the protocols see them (docs/NETWORKING.md §8):
//! scopes and their zones, the multicast groups an address joins, the
//! address selection policy table, and source address selection
//! (RFC 6724's rules as FreeBSD applies them).
//!
//! A link-local address means something only with its zone, the
//! interface it is on; FreeBSD carries the zone inside the address
//! (KAME's embedded scope), this crate beside it, as an interface index.
//!
//! Behavior reference: sys/netinet6/scope6.c (`in6_addrscope`,
//! `in6_setscope`), sys/netinet6/in6_src.c (`in6_selectsrc`,
//! `in6_selectif`, `in6_selecthlim`, `lookup_addrsel_policy`),
//! sys/netinet6/in6.c (`in6_solicited_node_maddr`, `in6_matchlen`),
//! sys/netinet6/in6_ifattach.c (`in6_nigroup`).

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::net::{In6Addrpolicy, iff, nd6_iff, rtf};
use net::Family;
use net::addr::{In6, LOOPBACK6, is_multicast6, is_unspecified};
use net::ifnet::{Ifnet, Net, Route, in6_iff, route_zone};
use sync::SpinLock;

use crate::state::{State, ip6s, on};

/// `in6addr_linklocal_allnodes`, `in6addr_linklocal_allrouters`,
/// `in6addr_linklocal_allv2routers`, `in6addr_nodelocal_allnodes`.
pub const ALLNODES: In6 = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
pub const ALLROUTERS: In6 = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
pub const ALLV2ROUTERS: In6 = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x16];
pub const NODELOCAL_ALLNODES: In6 = [0xff, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

/// `IPV6_ADDR_SCOPE_*`.
pub mod scope {
    pub const INTFACELOCAL: u8 = 0x01;
    pub const LINKLOCAL: u8 = 0x02;
    pub const SITELOCAL: u8 = 0x05;
    pub const GLOBAL: u8 = 0x0e;
}

/// `IN6_IS_ADDR_LINKLOCAL`: `fe80::/10` unicast.
#[must_use]
pub const fn is_linklocal(a: &In6) -> bool {
    a[0] == 0xfe && a[1] & 0xc0 == 0x80
}

/// `IN6_IS_ADDR_LOOPBACK`.
#[must_use]
pub fn is_loopback(a: &In6) -> bool {
    *a == LOOPBACK6
}

/// `IN6_IS_ADDR_MC_*` scope of a multicast address (`IPV6_ADDR_MC_SCOPE`).
#[must_use]
pub const fn mc_scope(a: &In6) -> u8 {
    a[1] & 0x0f
}

/// `in6_addrscope`.
#[must_use]
pub fn addrscope(a: &In6) -> u8 {
    if is_multicast6(a) {
        // Scope F is treated as global.
        return if mc_scope(a) == 0x0f {
            scope::GLOBAL
        } else {
            mc_scope(a)
        };
    }
    if is_linklocal(a) || is_loopback(a) {
        return scope::LINKLOCAL;
    }
    if a[0] == 0xfe && a[1] & 0xc0 == 0xc0 {
        return scope::SITELOCAL;
    }
    scope::GLOBAL
}

/// `IN6_IS_SCOPE_LINKLOCAL` or interface-local: whether `a` has a zone
/// (in6_setscope embeds one), the interface it is on.
#[must_use]
pub fn is_zoned(a: &In6) -> bool {
    !is_loopback(a) && matches!(addrscope(a), scope::INTFACELOCAL | scope::LINKLOCAL)
}

/// `in6_setscope`: `a`'s zone on interface `ifp`, 0 for none; `None` for
/// the loopback address on another interface than a loopback one.
#[must_use]
pub fn zone_on(a: &In6, ifp: &Ifnet) -> Option<u16> {
    if is_loopback(a) {
        return (ifp.flags & iff::LOOPBACK != 0).then_some(0);
    }
    Some(if is_zoned(a) { ifp.index } else { 0 })
}

/// `in6_solicited_node_maddr`: `ff02::1:ffXX:XXXX` for `a`.
#[must_use]
pub fn solicited_node(a: &In6) -> In6 {
    let mut m = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0, 0, 0];
    m[13..].copy_from_slice(&a[13..]);
    m
}

/// `in6_nigroup`: the node information group for host name `name`
/// (`ff02::2:ffXX:XXXX`, from the MD5 hash of its first label, folded to
/// lower case); none for an empty name or a label too long.
#[must_use]
pub fn nigroup(name: &str) -> Option<In6> {
    let label = name.as_bytes().split(|&b| b == b'.').next()?;
    if label.is_empty() || label.len() > 63 {
        return None;
    }
    let mut buf = Vec::with_capacity(label.len() + 1);
    #[allow(clippy::cast_possible_truncation, reason = "at most 63")]
    buf.push(label.len() as u8);
    buf.extend(label.iter().map(u8::to_ascii_lowercase));
    let d = crate::md5::digest(&buf);
    let mut m = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0xff, 0, 0, 0];
    m[13..].copy_from_slice(&d[..3]);
    Some(m)
}

/// `in6_matchlen`: the leading bits `a` and `b` share.
#[must_use]
pub fn matchlen(a: &In6, b: &In6) -> u8 {
    let mut n = 0u8;
    for (x, y) in a.iter().zip(b) {
        let d = x ^ y;
        if d == 0 {
            n += 8;
        } else {
            #[allow(clippy::cast_possible_truncation, reason = "under 8")]
            {
                n += d.leading_zeros() as u8;
            }
            break;
        }
    }
    n
}

/// `ADDR_LABEL_NOTAPP`: no policy applies.
pub const LABEL_NOTAPP: i32 = -1;

/// The address selection policy table (`addrsel_policytab`), which
/// `ip6addrctl(8)` installs.
static POLICY: SpinLock<Vec<In6Addrpolicy>> = SpinLock::new(Vec::new());

/// The prefix length of mask `m`, or `None` if it is not a prefix.
fn mask_plen(m: &In6) -> Option<u8> {
    let ones: u32 = m.iter().map(|b| b.count_ones()).sum();
    let plen = u8::try_from(ones).ok()?;
    (net::ifnet::masked(Family::Inet6, &[0xff; 16], plen) == *m).then_some(plen)
}

/// `add_addrsel_policyent` (`SIOCAADDRCTL_POLICY`).
///
/// # Errors
///
/// `EINVAL` for `ADDR_LABEL_NOTAPP` or a mask that is not a prefix,
/// `EEXIST` for one already there.
pub fn policy_add(mut p: In6Addrpolicy) -> Result<(), Errno> {
    if p.label == LABEL_NOTAPP || mask_plen(&p.addrmask.sin6_addr).is_none() {
        return Err(Errno::EINVAL);
    }
    let mut t = POLICY.lock();
    if t.iter().any(|e| same_policy(e, &p)) {
        return Err(Errno::EEXIST);
    }
    p.r#use = 0;
    t.push(p);
    Ok(())
}

/// `delete_addrsel_policyent` (`SIOCDADDRCTL_POLICY`).
///
/// # Errors
///
/// `EINVAL` for a mask that is not a prefix, `ESRCH` for none.
pub fn policy_delete(p: &In6Addrpolicy) -> Result<(), Errno> {
    if mask_plen(&p.addrmask.sin6_addr).is_none() {
        return Err(Errno::EINVAL);
    }
    let mut t = POLICY.lock();
    let i = t
        .iter()
        .position(|e| same_policy(e, p))
        .ok_or(Errno::ESRCH)?;
    t.remove(i);
    Ok(())
}

fn same_policy(a: &In6Addrpolicy, b: &In6Addrpolicy) -> bool {
    a.addr.sin6_addr == b.addr.sin6_addr && a.addrmask.sin6_addr == b.addrmask.sin6_addr
}

/// The table, as `net.inet6.ip6.addrctlpolicy` reports it.
#[must_use]
pub fn policies() -> Vec<In6Addrpolicy> {
    POLICY.lock().clone()
}

/// `lookup_addrsel_policy`: the label of the most specific policy whose
/// prefix holds `a` (its use counted), `ADDR_LABEL_NOTAPP` for none.
fn policy_label(a: &In6) -> i32 {
    let mut t = POLICY.lock();
    let mut best: Option<(usize, u8)> = None;
    for (i, p) in t.iter().enumerate() {
        let m = &p.addrmask.sin6_addr;
        let held = a
            .iter()
            .zip(m)
            .zip(&p.addr.sin6_addr)
            .all(|((x, m), y)| x & m == y & m);
        let plen = mask_plen(m).unwrap_or(0);
        if held && best.is_none_or(|(_, l)| plen > l) {
            best = Some((i, plen));
        }
    }
    match best {
        Some((i, _)) => {
            t[i].r#use += 1;
            t[i].label
        }
        None => LABEL_NOTAPP,
    }
}

/// What a socket and its packet options say about the source to choose.
#[derive(Clone, Copy, Debug, Default)]
pub struct SrcReq {
    /// The destination and its zone (an interface index, for a link-local
    /// one).
    pub dst: In6,
    pub zone: u16,
    /// `IPV6_PKTINFO`'s address and interface.
    pub pktinfo: Option<(In6, u16)>,
    /// The socket's bound address, unspecified for none.
    pub laddr: In6,
    /// `IPV6_MULTICAST_IF`'s interface, 0 for none.
    pub mif: u16,
    /// The interface the caller already has (`*ifpp`), 0 for none.
    pub oifp: u16,
    pub fib: u16,
    /// `INP_BINDANY`.
    pub bindany: bool,
    /// `IPV6_PREFER_TEMPADDR`: -1 for the system's choice.
    pub prefer_tempaddr: i32,
}

/// `in6_selectif` (by way of `selectroute` with routes optional): the
/// interface a packet for `r.dst` leaves by, if one is known.
///
/// # Errors
///
/// `EHOSTUNREACH` with no route (a reject route to a host too),
/// `ENETUNREACH` for a reject or blackhole route to a network.
pub(crate) fn selectif(n: &Net, st: &State, r: &SrcReq) -> Result<Option<u16>, Errno> {
    if let Some((_, ifindex)) = r.pktinfo
        && ifindex != 0
        && n.by_index(ifindex).is_some()
    {
        return Ok(Some(ifindex));
    }
    if is_multicast6(&r.dst) && r.mif != 0 {
        return Ok(Some(r.mif));
    }
    if is_multicast6(&r.dst)
        && matches!(mc_scope(&r.dst), scope::INTFACELOCAL | scope::LINKLOCAL)
        && r.zone != 0
    {
        return n
            .by_index(r.zone)
            .map(|i| Some(i.index))
            .ok_or(Errno::EHOSTUNREACH);
    }
    let Some(route) = n.route6(r.fib, &r.dst, r.zone) else {
        st.ip6stat.inc(ip6s::NOROUTE);
        // A caller with an interface may go on without a route.
        if r.oifp != 0 && r.fib == 0 {
            return Ok(Some(r.oifp));
        }
        return Err(Errno::EHOSTUNREACH);
    };
    if route.flags & (rtf::REJECT | rtf::BLACKHOLE) != 0 {
        return Err(if route.flags & rtf::HOST != 0 {
            Errno::EHOSTUNREACH
        } else {
            Errno::ENETUNREACH
        });
    }
    Ok(Some(route_zone(route)))
}

/// `nh_aifp` of route `r`: the interface whose address or link it is.
#[must_use]
pub fn route_aifp(r: &Route) -> u16 {
    route_zone(r)
}

/// `in6_selectsrc`: the source for a packet as `r` describes it, and the
/// interface it leaves by (0 if not decided).
///
/// # Errors
///
/// `EADDRNOTAVAIL` for an `IPV6_PKTINFO` address not this host's (or not
/// ready), or with no address to choose; [`selectif`]'s errors.
pub(crate) fn selectsrc(n: &Net, st: &State, r: &SrcReq) -> Result<(In6, u16), Errno> {
    let dst = r.dst;
    if let Some((a, _)) = r.pktinfo
        && !is_unspecified(&a)
    {
        let ifp = selectif(n, st, r)?.unwrap_or(0);
        if !r.bindany {
            let ok = n.ifaces.iter().any(|i| {
                i.addrs.iter().any(|x| {
                    x.family == Family::Inet6
                        && x.addr == a
                        && x.flags6 & (in6_iff::ANYCAST | in6_iff::NOTREADY) == 0
                        && (!is_zoned(&a) || ifp == 0 || i.index == ifp)
                })
            });
            if !ok {
                return Err(Errno::EADDRNOTAVAIL);
            }
        }
        return Ok((a, ifp));
    }
    if !is_unspecified(&r.laddr) {
        return Ok((r.laddr, 0));
    }
    let ifp_index = selectif(n, st, r)?.unwrap_or(r.oifp);
    let Some(ifp) = n.by_index(ifp_index) else {
        st.ip6stat.inc(ip6s::SOURCES_NONE);
        return Err(Errno::EADDRNOTAVAIL);
    };
    let Some(odstzone) = zone_on(&dst, ifp) else {
        return Err(Errno::EINVAL);
    };
    let prefer_temp = if r.prefer_tempaddr < 0 {
        on(&st.conf6.prefer_tempaddr)
    } else {
        r.prefer_tempaddr > 0
    };
    let use_deprecated = on(&st.conf6.use_deprecated);
    let now = net::uptime();
    let dst_scope = addrscope(&dst);
    let mut dst_label: Option<i32> = None;
    // The best so far: its interface, address, and what the rules
    // compared (scope, label, match length).
    struct Best<'a> {
        ifp: &'a Ifnet,
        a: &'a net::ifnet::Ifaddr,
        scope: u8,
        label: i32,
        matchlen: u8,
    }
    /// What a rule says of a candidate against the best so far.
    enum Verdict {
        Next,
        Replace(usize),
        Take(usize),
    }
    let mut best: Option<Best<'_>> = None;
    let mut rule = 0usize;
    'cand: for i1 in &n.ifaces {
        for a in i1.addrs.iter().filter(|a| a.family == Family::Inet6) {
            if zone_on(&dst, i1) != Some(odstzone) {
                continue;
            }
            let src = a.addr;
            match (zone_on(&src, ifp), zone_on(&src, i1)) {
                (Some(x), Some(y)) if x == y => {}
                _ => continue,
            }
            if a.flags6 & (in6_iff::NOTREADY | in6_iff::ANYCAST | in6_iff::DETACHED) != 0 {
                continue;
            }
            let deprecated = a.flags6 & in6_iff::DEPRECATED != 0 || a.is_deprecated(now);
            if !use_deprecated && deprecated {
                continue;
            }
            let new_scope = addrscope(&src);
            let verdict = 'v: {
                // Rule 1: the destination itself.
                if src == dst {
                    break 'v Verdict::Take(1);
                }
                let Some(b) = &best else {
                    break 'v Verdict::Replace(0);
                };
                // Rule 2: the appropriate scope.
                if b.scope < new_scope {
                    break 'v if b.scope < dst_scope {
                        Verdict::Replace(2)
                    } else {
                        Verdict::Next
                    };
                } else if new_scope < b.scope {
                    break 'v if new_scope < dst_scope {
                        Verdict::Next
                    } else {
                        Verdict::Replace(2)
                    };
                }
                // Rule 3: avoid deprecated addresses.
                let b_dep = b.a.flags6 & in6_iff::DEPRECATED != 0 || b.a.is_deprecated(now);
                if !b_dep && deprecated {
                    break 'v Verdict::Next;
                }
                if b_dep && !deprecated {
                    break 'v Verdict::Replace(3);
                }
                // Rule 5: the outgoing interface's.
                if ifp.nd6_flags & nd6_iff::NO_PREFER_IFACE == 0 {
                    if b.ifp.index == ifp.index && i1.index != ifp.index {
                        break 'v Verdict::Next;
                    }
                    if b.ifp.index != ifp.index && i1.index == ifp.index {
                        break 'v Verdict::Replace(5);
                    }
                }
                // Rule 6: a matching label.
                let dl = *dst_label.get_or_insert_with(|| policy_label(&dst));
                if dl != LABEL_NOTAPP {
                    let nl = policy_label(&src);
                    if dl == b.label && dl != nl {
                        break 'v Verdict::Next;
                    }
                    if dl != b.label && dl == nl {
                        break 'v Verdict::Replace(6);
                    }
                }
                // Rule 7: temporary addresses, as preferred.
                let b_tmp = b.a.flags6 & in6_iff::TEMPORARY != 0;
                let n_tmp = a.flags6 & in6_iff::TEMPORARY != 0;
                if !b_tmp && n_tmp {
                    break 'v if prefer_temp {
                        Verdict::Replace(7)
                    } else {
                        Verdict::Next
                    };
                }
                if b_tmp && !n_tmp {
                    break 'v if prefer_temp {
                        Verdict::Next
                    } else {
                        Verdict::Replace(7)
                    };
                }
                // Rule 8: an interface that is up.
                let b_up = b.ifp.flags & iff::UP != 0;
                let n_up = i1.flags & iff::UP != 0;
                if b_up && !n_up {
                    break 'v Verdict::Next;
                }
                if !b_up && n_up {
                    break 'v Verdict::Replace(8);
                }
                // Rule 10: `IN6_IFF_PREFER_SOURCE`.
                let b_pref = b.a.flags6 & in6_iff::PREFER_SOURCE != 0;
                let n_pref = a.flags6 & in6_iff::PREFER_SOURCE != 0;
                if !b_pref && n_pref {
                    break 'v Verdict::Replace(10);
                }
                if b_pref && !n_pref {
                    break 'v Verdict::Next;
                }
                // Rule 14: the longest match.
                if b.matchlen < matchlen(&src, &dst) {
                    break 'v Verdict::Replace(14);
                }
                Verdict::Next
            };
            match verdict {
                Verdict::Next => {}
                Verdict::Replace(r) | Verdict::Take(r) => {
                    rule = r;
                    best = Some(Best {
                        ifp: i1,
                        a,
                        scope: new_scope,
                        label: policy_label(&src),
                        matchlen: matchlen(&src, &dst),
                    });
                    if matches!(verdict, Verdict::Take(_)) {
                        break 'cand;
                    }
                }
            }
        }
    }
    let Some(b) = best else {
        st.ip6stat.inc(ip6s::SOURCES_NONE);
        return Err(Errno::EADDRNOTAVAIL);
    };
    st.ip6stat.inc(ip6s::SOURCES_RULE + rule);
    let sc = usize::from(b.scope & 0x0f);
    if b.ifp.index == ifp.index {
        st.ip6stat.inc(ip6s::SOURCES_SAMEIF + sc);
    } else {
        st.ip6stat.inc(ip6s::SOURCES_OTHERIF + sc);
    }
    if dst_scope == b.scope {
        st.ip6stat.inc(ip6s::SOURCES_SAMESCOPE + sc);
    } else {
        st.ip6stat.inc(ip6s::SOURCES_OTHERSCOPE + sc);
    }
    if b.a.flags6 & in6_iff::DEPRECATED != 0 || b.a.is_deprecated(now) {
        st.ip6stat.inc(ip6s::SOURCES_DEPRECATED + sc);
    }
    Ok((b.a.addr, ifp.index))
}

/// `in6_selecthlim`: the socket's hop limit if set, else the interface's
/// `CurHopLimit`, else the route's interface's, else the default.
#[must_use]
pub(crate) fn selecthlim(n: &Net, hops: i16, ifp: u16, faddr: Option<(&In6, u16)>) -> u8 {
    if hops >= 0 {
        return u8::try_from(hops).unwrap_or(64);
    }
    if let Some(i) = n.by_index(ifp) {
        return i.nd.chlim;
    }
    if let Some((a, zone)) = faddr
        && !is_unspecified(a)
        && let Some(r) = n.route6(0, a, zone)
        && let Some(i) = n.by_index(r.ifindex)
    {
        return i.nd.chlim;
    }
    crate::udp::hops(-1)
}

/// `in6ifa_ifpforlinklocal`: interface `ifp`'s first link-local address
/// without any of `ignore`'s flags.
#[must_use]
pub fn linklocal_of(ifp: &Ifnet, ignore: i32) -> Option<In6> {
    ifp.addrs
        .iter()
        .find(|a| a.family == Family::Inet6 && is_linklocal(&a.addr) && a.flags6 & ignore == 0)
        .map(|a| a.addr)
}

/// `in6ifa_ifpwithaddr`: whether `a` is one of interface `ifp`'s IPv6
/// addresses, with its flags.
#[must_use]
pub fn ifa_flags(ifp: &Ifnet, a: &In6) -> Option<i32> {
    ifp.addrs
        .iter()
        .find(|x| x.family == Family::Inet6 && x.addr == *a)
        .map(|x| x.flags6)
}

/// `in6ifa_ifwithaddr` with a zone: the interface that has address `a`
/// (for a zoned one, only interface `zone`), and the address's flags.
#[must_use]
pub fn ifa_with(n: &Net, a: &In6, zone: u16) -> Option<(u16, i32)> {
    n.ifaces.iter().find_map(|i| {
        if is_zoned(a) && zone != 0 && i.index != zone {
            return None;
        }
        ifa_flags(i, a).map(|f| (i.index, f))
    })
}

/// `in6_localip`: whether `a` is an address of this host.
#[must_use]
pub fn is_local(n: &Net, a: &In6) -> bool {
    n.ifaces.iter().any(|i| ifa_flags(i, a).is_some())
}

/// Whether address `a` with zone `zone` names something on interface
/// `ifp` (for scope checks): unzoned ones are anywhere.
#[must_use]
pub fn zone_matches(a: &In6, zone: u16, ifp: u16) -> bool {
    !is_zoned(a) || zone == ifp
}
