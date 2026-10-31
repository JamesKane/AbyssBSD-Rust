// SPDX-License-Identifier: BSD-2-Clause
//! ARP (docs/NETWORKING.md §7): requests and replies, the IPv4 entries of
//! the link-layer table they fill, resolution with a queue of packets
//! held for it, entry aging, and the announcement an address gets.
//!
//! Everything runs under the interfaces' lock; frames to send go to the
//! lock's transmit queue and leave when it is let go.
//!
//! Behavior reference: sys/netinet/if_ether.c (`arpintr`, `in_arpinput`,
//! `arprequest_internal`, `arpresolve`, `arpresolve_full`, `arptimer`,
//! `arp_ifinit`).

use alloc::sync::Arc;

use abi::errno::Errno;
use abi::net::{IFT_ETHER, iff};
use mbuf::{Packet, flags};

use crate::Family;
use crate::Vnet;
use crate::addr::{from_v4, prefix_match, v4_of};
use crate::ether::{self, BROADCAST, Header};
use crate::ifnet::Net;
use crate::lltable::{Lle, lle, state};

/// `ARPHRD_ETHER`, `ARPHRD_IEEE802`, and the operations.
const HRD_ETHER: u16 = 1;
const HRD_IEEE802: u16 = 6;
const OP_REQUEST: u16 = 1;
const OP_REPLY: u16 = 2;
/// An Ethernet ARP packet's length: the header and two address pairs.
const LEN: usize = 8 + 2 * (6 + 4);

/// The `net.link.ether.inet` settings (per VNET).
#[derive(Clone, Copy, Debug)]
pub struct Conf {
    /// `max_age`: how long a resolved entry lasts (`arpt_keep`).
    pub max_age: u64,
    /// `maxtries`: requests before a host is down.
    pub maxtries: u16,
    pub proxyall: bool,
    /// `wait`: how long an unresolved entry lasts (`arpt_down`).
    pub wait: u64,
    /// Seconds between retransmissions (`arpt_rexmit`).
    pub rexmit: u64,
    /// `maxhold`: packets held per entry.
    pub maxhold: usize,
}

impl Conf {
    /// FreeBSD's defaults.
    pub const DEFAULT: Self = Self {
        max_age: 20 * 60,
        maxtries: 5,
        proxyall: false,
        wait: 20,
        rexmit: 1,
        maxhold: 16,
    };
}

impl Default for Conf {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `struct arpstat` (`net.link.ether.arp.stats`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    pub txrequests: u64,
    pub txreplies: u64,
    pub rxrequests: u64,
    pub rxreplies: u64,
    pub received: u64,
    pub txerrors: u64,
    pub spares: [u64; 3],
    pub dropped: u64,
    pub timeouts: u64,
    pub dupips: u64,
}

impl Stat {
    pub const ZERO: Self = Self {
        txrequests: 0,
        txreplies: 0,
        rxrequests: 0,
        rxreplies: 0,
        received: 0,
        txerrors: 0,
        spares: [0; 3],
        dropped: 0,
        timeouts: 0,
        dupips: 0,
    };
}

/// An ARP packet's fields, for Ethernet and IPv4.
#[derive(Clone, Copy, Debug)]
struct Arp {
    hrd: u16,
    pro: u16,
    hln: u8,
    pln: u8,
    op: u16,
    sha: [u8; 6],
    spa: [u8; 4],
    tha: [u8; 6],
    tpa: [u8; 4],
}

impl Arp {
    fn bytes(&self) -> [u8; LEN] {
        let mut b = [0u8; LEN];
        b[0..2].copy_from_slice(&self.hrd.to_be_bytes());
        b[2..4].copy_from_slice(&self.pro.to_be_bytes());
        b[4] = self.hln;
        b[5] = self.pln;
        b[6..8].copy_from_slice(&self.op.to_be_bytes());
        b[8..14].copy_from_slice(&self.sha);
        b[14..18].copy_from_slice(&self.spa);
        b[18..24].copy_from_slice(&self.tha);
        b[24..28].copy_from_slice(&self.tpa);
        b
    }
}

/// Queues `body` for interface `ifindex` behind an ARP Ethernet header
/// to `dst` (`ether_output` with the header prepended).
fn send(n: &mut Net, ifindex: u16, dst: [u8; 6], body: &[u8], bcast: bool) -> Result<(), Errno> {
    let ifp = n.by_index(ifindex).ok_or(Errno::ENXIO)?;
    if ifp.flags & (iff::UP | iff::RUNNING) != iff::UP | iff::RUNNING {
        return Err(Errno::ENETDOWN);
    }
    let src = ifp.lladdr.ok_or(Errno::EAFNOSUPPORT)?;
    let mut m = Packet::for_output(body.len())?;
    m.data.extend_from_slice(body)?;
    ether::encap(
        &mut m,
        &Header {
            dst,
            src,
            ty: ether::ty::ARP,
        },
    )?;
    if bcast {
        m.hdr.flags |= flags::BCAST;
    }
    n.txq.push((ifindex, m));
    Ok(())
}

/// Broadcasts a request for `tip` from `sip` (or an address of the
/// interface on `tip`'s network) and `enaddr` (or the interface's own)
/// (`arprequest_internal`).
///
/// # Errors
///
/// `EADDRNOTAVAIL` with no source address, `ENETDOWN` if the interface
/// is not running.
pub fn request(
    n: &mut Net,
    ifindex: u16,
    sip: Option<[u8; 4]>,
    tip: [u8; 4],
    enaddr: Option<[u8; 6]>,
) -> Result<(), Errno> {
    let ifp = n.by_index(ifindex).ok_or(Errno::ENXIO)?;
    let sip = match sip {
        Some(s) => s,
        None => {
            // The last IPv4 address, unless one on `tip`'s network comes
            // first.
            let mut found = None;
            for a in ifp.addrs.iter().filter(|a| a.family == Family::Inet) {
                found = Some(v4_of(&a.addr));
                if prefix_match(Family::Inet, &a.addr, &from_v4(tip), a.plen) {
                    break;
                }
            }
            found.ok_or(Errno::EADDRNOTAVAIL)?
        }
    };
    let sha = enaddr.or(ifp.lladdr).ok_or(Errno::EAFNOSUPPORT)?;
    let a = Arp {
        hrd: HRD_ETHER,
        pro: ether::ty::IP,
        hln: 6,
        pln: 4,
        op: OP_REQUEST,
        sha,
        spa: sip,
        tha: [0; 6],
        tpa: tip,
    };
    let r = send(n, ifindex, BROADCAST, &a.bytes(), true);
    n.arpstat.txrequests += 1;
    if r.is_err() {
        n.arpstat.txerrors += 1;
    }
    r
}

/// `arp_mark_lle_reachable`.
fn mark_reachable(e: &mut Lle, c: &Conf, sticky: bool, now: u64) {
    e.state = state::REACHABLE;
    if sticky {
        e.flags |= lle::STATIC;
    }
    if e.flags & lle::STATIC == 0 {
        e.expire = now + c.max_age;
        let rexmits = u64::from(c.maxtries) * c.rexmit;
        let wait = c.max_age.checked_sub(rexmits).unwrap_or(c.max_age);
        e.timer = Some(now + wait);
    }
    e.asked = 0;
    e.preempt = c.maxtries;
}

/// Sends what waited for `e`'s address (`arp_flush_holdchain`).
fn flush_hold(n: &mut Net, ifindex: u16, dst: [u8; 6], held: alloc::vec::Vec<Packet>) {
    let Some(src) = n.by_index(ifindex).and_then(|i| i.lladdr) else {
        return;
    };
    for mut m in held {
        let h = Header {
            dst,
            src,
            ty: ether::ty::IP,
        };
        if ether::encap(&mut m, &h).is_ok() {
            n.txq.push((ifindex, m));
        }
    }
}

/// A received ARP packet (`arpintr` and `in_arpinput`, by way of
/// `netisr`'s `NETISR_ARP`).
pub fn input(vnet: &Arc<Vnet>, mut m: Packet) {
    let mut b = [0u8; LEN];
    if m.data.copy_out(0, &mut b[..8]) != 8 {
        return;
    }
    let hrd = u16::from_be_bytes([b[0], b[1]]);
    let (hln, pln) = (b[4], b[5]);
    // `arpintr`: a known hardware type with its address length.
    if !matches!(hrd, HRD_ETHER | HRD_IEEE802) || hln != 6 {
        return;
    }
    let mut n = vnet.net.lock();
    n.arpstat.received += 1;
    if u16::from_be_bytes([b[2], b[3]]) != ether::ty::IP || pln != 4 {
        return;
    }
    if m.data.copy_out(0, &mut b) != LEN {
        return;
    }
    let a = Arp {
        hrd,
        pro: ether::ty::IP,
        hln,
        pln,
        op: u16::from_be_bytes([b[6], b[7]]),
        sha: b[8..14].try_into().unwrap_or_default(),
        spa: b[14..18].try_into().unwrap_or_default(),
        tha: b[18..24].try_into().unwrap_or_default(),
        tpa: b[24..28].try_into().unwrap_or_default(),
    };
    let ifindex = m.hdr.rcvif;
    let now = crate::uptime();
    in_input(&mut n, ifindex, a, now, &mut m);
}

fn in_input(n: &mut Net, ifindex: u16, mut a: Arp, now: u64, m: &mut Packet) {
    // `allow_multicast` 0.
    if a.sha[0] & 1 != 0 {
        return;
    }
    if a.op == OP_REPLY {
        n.arpstat.rxreplies += 1;
    }
    let (isaddr, mut itaddr) = (a.spa, a.tpa);
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    let v4s = || ifp.addrs.iter().filter(|x| x.family == Family::Inet);
    // The interface's address the packet is for, or from, or its first.
    let Some(myaddr) = v4s()
        .find(|x| v4_of(&x.addr) == itaddr)
        .or_else(|| v4s().find(|x| v4_of(&x.addr) == isaddr))
        .or_else(|| v4s().next())
        .map(|x| v4_of(&x.addr))
    else {
        return;
    };
    let Some(enaddr) = ifp.lladdr else {
        return;
    };
    let staticarp = ifp.flags & iff::STATICARP != 0;
    let sticky = ifp.flags & iff::STICKYARP != 0;
    // From itself, or from a broadcast address.
    if a.sha == enaddr || a.sha == BROADCAST {
        return;
    }
    let c = n.arp;
    'learn: {
        if isaddr == myaddr && myaddr != [0; 4] {
            // "is using my IP address": defend it.
            itaddr = myaddr;
            n.arpstat.dupips += 1;
            break 'learn;
        }
        if staticarp {
            break 'learn;
        }
        let ia = from_v4(isaddr);
        if let Some(e) = n.lle_mut(ifindex, Family::Inet, &ia) {
            // `arp_check_update_lle`.
            if e.flags & lle::VALID != 0 && e.lladdr != a.sha && e.flags & lle::STATIC != 0 {
                // "attempts to modify permanent entry".
                break 'learn;
            }
            e.set_lladdr(a.sha);
            mark_reachable(e, &c, sticky, now);
            let held = core::mem::take(&mut e.hold);
            flush_hold(n, ifindex, a.sha, held);
        } else if itaddr == myaddr && n.lle_rtcheck(ifindex, Family::Inet, &ia) {
            let mut e = Lle::new(Family::Inet, ia, ifindex, 0);
            e.set_lladdr(a.sha);
            mark_reachable(&mut e, &c, sticky, now);
            n.lle_link(e);
        }
    }
    if a.op != OP_REQUEST {
        return;
    }
    n.arpstat.rxrequests += 1;
    let sha = if itaddr == myaddr {
        enaddr
    } else {
        match n.lle(ifindex, Family::Inet, &from_v4(itaddr)) {
            Some(e) if e.flags & lle::PUB != 0 => e.lladdr,
            _ => {
                if !c.proxyall {
                    return;
                }
                // `proxyall`: for a host routed out of another interface,
                // asked from one routed out of this.
                let via = |d: [u8; 4]| n.lookup(Family::Inet, &from_v4(d)).ok().map(|r| r.ifindex);
                if via(itaddr).is_none_or(|i| i == ifindex) || via(isaddr) != Some(ifindex) {
                    return;
                }
                enaddr
            }
        }
    };
    a.tha = a.sha;
    a.sha = sha;
    a.tpa = a.spa;
    a.spa = itaddr;
    a.op = OP_REPLY;
    a.pro = ether::ty::IP;
    m.hdr.rcvif = 0;
    // A reply for a link-local address is broadcast (RFC 3927 §2.5); the
    // frame still goes to the asker.
    let bcast = itaddr == myaddr && itaddr[0] == 169 && itaddr[1] == 254;
    if send(n, ifindex, a.tha, &a.bytes(), bcast).is_ok() {
        n.arpstat.txreplies += 1;
    }
}

/// What resolution found.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The link-layer address to send to.
    Addr([u8; 6]),
    /// The packet waits for a reply (`EWOULDBLOCK`, which output turns
    /// into success).
    Held,
}

/// The link-layer address for IPv4 next hop `dst` on interface `ifindex`
/// (`arpresolve`), sending a request and holding `m` if it is not known.
///
/// # Errors
///
/// `EINVAL` if no entry can be made (no route puts `dst` here, or the
/// interface does not resolve), `EHOSTDOWN` (`EHOSTUNREACH` for a
/// gateway) once `maxtries` requests went unanswered, or the request's
/// error.
pub fn resolve(
    n: &mut Net,
    ifindex: u16,
    is_gw: bool,
    m: Option<Packet>,
    dst: [u8; 4],
    now: u64,
) -> Result<Resolved, Errno> {
    if let Some(m) = &m {
        if m.hdr.flags & flags::BCAST != 0 {
            return Ok(Resolved::Addr(BROADCAST));
        }
        if m.hdr.flags & flags::MCAST != 0 {
            // `ETHER_MAP_IP_MULTICAST`.
            return Ok(Resolved::Addr([1, 0, 0x5e, dst[1] & 0x7f, dst[2], dst[3]]));
        }
    }
    let a = from_v4(dst);
    let c = n.arp;
    let ifflags = n.by_index(ifindex).ok_or(Errno::ENXIO)?.flags;
    if n.lle(ifindex, Family::Inet, &a).is_none() {
        if ifflags & (iff::NOARP | iff::STATICARP) != 0 || !n.lle_rtcheck(ifindex, Family::Inet, &a)
        {
            return Err(Errno::EINVAL);
        }
        n.lle_link(Lle::new(Family::Inet, a, ifindex, 0));
    }
    let Some(e) = n.lle_mut(ifindex, Family::Inet, &a) else {
        return Err(Errno::EINVAL);
    };
    if e.flags & lle::VALID != 0 && (e.flags & lle::STATIC != 0 || e.expire > now) {
        e.used = true;
        return Ok(Resolved::Addr(e.lladdr));
    }
    let renew = e.asked == 0 || e.expire != now;
    let mut dropped = 0;
    if let Some(m) = m {
        while e.hold.len() >= c.maxhold && !e.hold.is_empty() {
            e.hold.remove(0);
            dropped += 1;
        }
        e.hold.push(m);
    }
    let r = if e.asked < c.maxtries {
        Ok(Resolved::Held)
    } else if is_gw {
        Err(Errno::EHOSTUNREACH)
    } else {
        Err(Errno::EHOSTDOWN)
    };
    if renew {
        e.expire = now;
        e.timer = Some(now + c.wait);
        e.asked += 1;
    }
    n.arpstat.dropped += dropped;
    if renew {
        request(n, ifindex, None, dst, None)?;
    }
    r
}

/// Ages the IPv4 entries whose timers have run out (`arptimer`).
pub fn tick(n: &mut Net, now: u64) {
    let c = n.arp;
    let mut ask = alloc::vec::Vec::new();
    let mut gone = 0u64;
    let mut dropped = 0u64;
    n.lle.retain_mut(|e| {
        if e.family != Family::Inet || e.flags & lle::STATIC != 0 {
            return true;
        }
        match e.timer {
            Some(t) if t <= now => {}
            _ => return true,
        }
        match e.state {
            state::REACHABLE => {
                // Ask the data path whether it is still used.
                e.used = false;
                e.state = state::VERIFY;
                e.timer = Some(now + c.rexmit);
                return true;
            }
            state::VERIFY => {
                if e.used && e.preempt > 0 {
                    e.preempt -= 1;
                    e.timer = Some(now + c.rexmit);
                    ask.push((e.ifindex, v4_of(&e.addr)));
                    return true;
                }
                if e.expire > now {
                    e.timer = Some(now + c.rexmit);
                    return true;
                }
            }
            _ => {}
        }
        gone += 1;
        dropped += e.hold.len() as u64;
        false
    });
    if gone > 0 {
        n.bump();
    }
    n.arpstat.timeouts += gone;
    n.arpstat.dropped += dropped;
    for (ifindex, dst) in ask {
        let _ = request(n, ifindex, None, dst, None);
    }
}

/// An IPv4 address `addr` was added to Ethernet interface `ifindex`
/// (`arp_ifinit`): it is announced, and gets a permanent entry.
pub fn ifinit(n: &mut Net, ifindex: u16, addr: [u8; 4]) {
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    if ifp.ty != IFT_ETHER || addr == [0; 4] {
        return;
    }
    let Some(ll) = ifp.lladdr else {
        return;
    };
    let _ = request(n, ifindex, Some(addr), addr, Some(ll));
    let mut e = Lle::new(
        Family::Inet,
        from_v4(addr),
        ifindex,
        lle::IFADDR | lle::STATIC,
    );
    e.set_lladdr(ll);
    n.lle_link(e);
}
