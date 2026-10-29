// SPDX-License-Identifier: BSD-2-Clause
//! The routing messages `net.routetable` returns (`sysctl_rtsock`): the
//! interface list (`NET_RT_IFLIST`, `NET_RT_IFLISTL`, which
//! `getifaddrs(3)` and `netstat -i` read) and the routing table
//! (`NET_RT_DUMP`), each record a header and its addresses, every address
//! padded to a `long` (`SA_SIZE`).
//!
//! Behavior reference: sys/net/rtsock.c (`rtsock_msg_buffer`,
//! `sysctl_iflist`, `sysctl_iflist_ifml`, `sysctl_iflist_ifaml`,
//! `sysctl_dumpnhop`), sys/net/if.c (`if_data_copy`, `if_attach`'s link
//! address).

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::inet::{AF_INET, AF_INET6, AF_LINK};
use abi::net::{IfData, RTM_VERSION, iff, net_rt, rta, rtf, rtm};

use crate::Family;
use crate::addr::{In6, is_linklocal6, sockaddr_in, sockaddr_in6, v4_of};
use crate::ifnet::{Gateway, Ifaddr, Ifnet, LO_STATS, Net, masked};

/// `SA_SIZE`: an address's room, rounded up to a `long`.
fn sa_size(len: usize) -> usize {
    if len == 0 { 8 } else { 1 + ((len - 1) | 7) }
}

/// The link address of an interface (`if_attach`): a `sockaddr_dl`
/// with its name, at least `sizeof(struct sockaddr_dl)`.
#[must_use]
pub fn link_sockaddr(ifp: &Ifnet) -> Vec<u8> {
    let name = ifp.name.as_bytes();
    let len = (8 + name.len()).max(54);
    let mut d = alloc::vec![0u8; len];
    d[0] = u8::try_from(len).unwrap_or(u8::MAX);
    d[1] = AF_LINK;
    d[2..4].copy_from_slice(&ifp.index.to_ne_bytes());
    d[4] = ifp.ty;
    d[5] = u8::try_from(name.len()).unwrap_or(0);
    d[8..8 + name.len()].copy_from_slice(name);
    d
}

/// An address's `sockaddr` (an IPv6 link-local one with its zone).
fn sa_of(family: Family, a: &In6, zone: u16) -> Vec<u8> {
    match family {
        Family::Inet => sockaddr_in(v4_of(a), 0),
        Family::Inet6 => sockaddr_in6(a, 0, if is_linklocal6(a) { u32::from(zone) } else { 0 }),
    }
}

/// The netmask of `plen` bits as a `sockaddr` the size of its address
/// (`rtsock_fix_netmask`).
fn mask_of(family: Family, plen: u8) -> Vec<u8> {
    let ones = [0xffu8; 16];
    let m = masked(family, &ones, plen);
    match family {
        Family::Inet => sockaddr_in(v4_of(&m), 0),
        Family::Inet6 => sockaddr_in6(&m, 0, 0),
    }
}

/// `if_data_copy`.
#[must_use]
pub fn if_data(ifp: &Ifnet) -> IfData {
    let s = &LO_STATS;
    let loopback = ifp.flags & iff::LOOPBACK != 0;
    let c = |a: &core::sync::atomic::AtomicU64| {
        if loopback {
            a.load(Ordering::Relaxed)
        } else {
            0
        }
    };
    IfData {
        ifi_type: ifp.ty,
        ifi_physical: 0,
        ifi_addrlen: 0,
        ifi_hdrlen: 0,
        ifi_link_state: ifp.link_state,
        ifi_vhid: 0,
        ifi_datalen: u16::try_from(size_of::<IfData>()).unwrap_or(0),
        ifi_mtu: ifp.mtu,
        ifi_metric: ifp.metric,
        ifi_baudrate: 0,
        ifi_ipackets: c(&s.ipackets),
        ifi_ierrors: 0,
        ifi_opackets: c(&s.opackets),
        ifi_oerrors: 0,
        ifi_collisions: 0,
        ifi_ibytes: c(&s.ibytes),
        ifi_obytes: c(&s.obytes),
        ifi_imcasts: c(&s.imcasts),
        ifi_omcasts: c(&s.omcasts),
        ifi_iqdrops: c(&s.iqdrops),
        ifi_oqdrops: 0,
        ifi_noproto: c(&s.noproto),
        // `LO_CSUM_FEATURES | LO_CSUM_FEATURES6`.
        ifi_hwassist: 0x0000_0e1e,
        ifi_epoch: ifp.epoch,
        ifi_lastchange: [ifp.lastchange.0, ifp.lastchange.1],
    }
}

fn put<T: Copy>(out: &mut Vec<u8>, v: &T) {
    // SAFETY: `T` is an `abi` record of integers with explicit padding
    // fields (no implicit padding), so all its bytes are initialized.
    let b =
        unsafe { core::slice::from_raw_parts(core::ptr::from_ref(v).cast::<u8>(), size_of::<T>()) };
    out.extend_from_slice(b);
}

/// A record: `hdr` (with its length, version, and type set), the
/// addresses in `RTAX_*` order, padded to a `long` (`rtsock_msg_buffer`).
fn record(mut hdr: Vec<u8>, ty: u8, addrs: &[Option<Vec<u8>>]) -> (Vec<u8>, i32) {
    let mut bits = 0;
    for (i, a) in addrs.iter().enumerate() {
        if let Some(a) = a {
            bits |= 1 << i;
            let n = sa_size(a.len());
            let at = hdr.len();
            hdr.extend_from_slice(a);
            hdr.resize(at + n, 0);
        }
    }
    let n = hdr.len().div_ceil(8) * 8;
    hdr.resize(n, 0);
    let len = u16::try_from(hdr.len()).unwrap_or(u16::MAX);
    hdr[0..2].copy_from_slice(&len.to_ne_bytes());
    hdr[2] = RTM_VERSION;
    hdr[3] = ty;
    (hdr, bits)
}

/// `rti_addrs` goes at this offset of each header kind.
const ADDRS_AT: usize = 4;

fn set_addrs(rec: &mut [u8], bits: i32) {
    rec[ADDRS_AT..ADDRS_AT + 4].copy_from_slice(&bits.to_ne_bytes());
}

/// `sysctl_iflist`: each interface (`ifindex` 0 for all) and its
/// addresses of family `af` (0 for all), long (`NET_RT_IFLISTL`) or short.
#[must_use]
pub fn iflist(n: &Net, af: u8, ifindex: i32, long: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for ifp in &n.ifaces {
        if ifindex != 0 && i32::from(ifp.index) != ifindex {
            continue;
        }
        let d = if_data(ifp);
        let flags = ifp.flags.cast_signed();
        let hdr = if long {
            let h = abi::net::IfMsghdrl {
                ifm_flags: flags,
                ifm_index: ifp.index,
                ifm_len: u16::try_from(size_of::<abi::net::IfMsghdrl>()).unwrap_or(0),
                ifm_data_off: u16::try_from(core::mem::offset_of!(abi::net::IfMsghdrl, ifm_data))
                    .unwrap_or(0),
                ifm_data: d,
                ..abi::net::IfMsghdrl::default()
            };
            let mut v = Vec::new();
            put(&mut v, &h);
            v
        } else {
            let h = abi::net::IfMsghdr {
                ifm_flags: flags,
                ifm_index: ifp.index,
                ifm_data: d,
                ..abi::net::IfMsghdr::default()
            };
            let mut v = Vec::new();
            put(&mut v, &h);
            v
        };
        let mut addrs: [Option<Vec<u8>>; 8] = Default::default();
        addrs[4] = Some(link_sockaddr(ifp));
        let (mut r, bits) = record(hdr, rtm::IFINFO, &addrs);
        set_addrs(&mut r, bits);
        out.extend(r);
        for a in &ifp.addrs {
            let fam = match a.family {
                Family::Inet => AF_INET,
                Family::Inet6 => AF_INET6,
            };
            if af != 0 && af != fam {
                continue;
            }
            out.extend(ifa_record(ifp, a, long));
        }
    }
    out
}

/// One address's record (`RTM_NEWADDR`).
fn ifa_record(ifp: &Ifnet, a: &Ifaddr, long: bool) -> Vec<u8> {
    let metric = ifp.metric.cast_signed();
    let hdr = if long {
        let h = abi::net::IfaMsghdrl {
            ifam_index: ifp.index,
            ifam_len: u16::try_from(size_of::<abi::net::IfaMsghdrl>()).unwrap_or(0),
            ifam_data_off: u16::try_from(core::mem::offset_of!(abi::net::IfaMsghdrl, ifam_data))
                .unwrap_or(0),
            ifam_metric: metric,
            ifam_data: IfData {
                ifi_datalen: u16::try_from(size_of::<IfData>()).unwrap_or(0),
                ..IfData::default()
            },
            ..abi::net::IfaMsghdrl::default()
        };
        let mut v = Vec::new();
        put(&mut v, &h);
        v
    } else {
        let h = abi::net::IfaMsghdr {
            ifam_index: ifp.index,
            ifam_metric: metric,
            ..abi::net::IfaMsghdr::default()
        };
        let mut v = Vec::new();
        put(&mut v, &h);
        v
    };
    let mut addrs: [Option<Vec<u8>>; 8] = Default::default();
    addrs[5] = Some(sa_of(a.family, &a.addr, ifp.index));
    addrs[2] = Some(mask_of(a.family, a.plen));
    // `ifa_dstaddr`: a loopback IPv4 address's own.
    if let Some(d) = a.dst.as_ref().or(a.broadcast.as_ref()) {
        addrs[7] = Some(sa_of(a.family, d, ifp.index));
    }
    let (mut r, bits) = record(hdr, rtm::NEWADDR, &addrs);
    set_addrs(&mut r, bits);
    r
}

/// `NET_RT_DUMP` of family `af` (0 for all): each route as
/// `sysctl_dumpnhop` writes it.
#[must_use]
pub fn dump(n: &Net, af: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut routes: Vec<_> = n
        .routes
        .iter()
        .filter(|r| af == 0 || r.family.af() == af)
        .collect();
    // A radix walk: by family, then address.
    routes.sort_by_key(|r| (r.family, r.dst, r.plen));
    for r in routes {
        let h = abi::net::RtMsghdr {
            rtm_index: r.ifindex,
            rtm_flags: r.flags
                | rtf::UP
                | if r.plen == max_plen(r.family) {
                    rtf::HOST
                } else {
                    0
                },
            rtm_rmx: abi::net::RtMetrics {
                rmx_mtu: u64::from(r.mtu),
                rmx_weight: u64::from(r.weight),
                ..abi::net::RtMetrics::default()
            },
            ..abi::net::RtMsghdr::default()
        };
        let mut v = Vec::new();
        put(&mut v, &h);
        let ifp = n.by_index(r.ifindex);
        let mut addrs: [Option<Vec<u8>>; 8] = Default::default();
        addrs[0] = Some(sa_of(r.family, &r.dst, r.ifindex));
        addrs[1] = Some(match r.gateway {
            Gateway::Link(i) => match ifp {
                Some(f) if f.index == i => link_sockaddr(f),
                _ => alloc::vec![0u8; 8],
            },
            Gateway::Addr(g) => sa_of(r.family, &g, r.ifindex),
        });
        if r.plen != max_plen(r.family) {
            addrs[2] = Some(mask_of(r.family, r.plen));
        }
        if let Some(f) = ifp {
            addrs[4] = Some(link_sockaddr(f));
            addrs[5] = Some(sa_of(r.family, &r.ifa, r.ifindex));
        }
        let (mut rec, bits) = record(v, rtm::GET, &addrs);
        rec[core::mem::offset_of!(abi::net::RtMsghdr, rtm_addrs)..][..4]
            .copy_from_slice(&bits.to_ne_bytes());
        out.extend(rec);
    }
    let _ = (net_rt::DUMP, rta::DST);
    out
}

const fn max_plen(f: Family) -> u8 {
    match f {
        Family::Inet => 32,
        Family::Inet6 => 128,
    }
}
