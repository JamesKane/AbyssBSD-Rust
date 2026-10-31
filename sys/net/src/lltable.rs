// SPDX-License-Identifier: BSD-2-Clause
//! The link-layer table (`lltable`, docs/NETWORKING.md §7): for each
//! interface and family, the link-layer addresses of its neighbors (ARP's
//! for IPv4, the neighbor cache for IPv6), with the interface's own
//! addresses as permanent entries.
//!
//! Entries are added by resolution (ARP and ND), by `arp -s` and `ndp -s`
//! through the routing socket (`RTF_LLDATA`), and for each address an
//! Ethernet interface gets; `arp -a` and `ndp -a` read them through
//! `NET_RT_FLAGS` with `RTF_LLINFO`.
//!
//! Behavior reference: sys/net/if_llatbl.c (`lla_rt_output`,
//! `lltable_delete_addr`, `lltable_prefix_free`), sys/netinet/in.c
//! (`in_lltable_*`), sys/netinet6/in6.c (`in6_lltable_*`).

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::AF_LINK;
use abi::net::{IFT_ETHER, RTM_VERSION, RtMsghdr, iff, rta, rtf, rtm};
use mbuf::Packet;

use crate::Family;
use crate::addr::{In6, prefix_match};
use crate::ifnet::{Gateway, Net};

/// `LLE_*` entry flags.
pub mod lle {
    pub const DELETED: u16 = 0x0001;
    pub const STATIC: u16 = 0x0002;
    pub const IFADDR: u16 = 0x0004;
    pub const VALID: u16 = 0x0008;
    pub const REDIRECT: u16 = 0x0010;
    pub const PUB: u16 = 0x0020;
}

/// ARP's entry states (`ARP_LLINFO_*`); ND's are `ND6_LLINFO_*`.
pub mod state {
    pub const INCOMPLETE: i16 = 0;
    pub const REACHABLE: i16 = 1;
    pub const VERIFY: i16 = 2;
    pub const DELETED: i16 = 3;
    /// `ND6_LLINFO_REACHABLE`, what a static IPv6 entry is.
    pub const ND6_REACHABLE: i16 = 1;
}

/// An entry (`struct llentry`).
#[derive(Debug)]
pub struct Lle {
    pub family: Family,
    pub addr: In6,
    pub ifindex: u16,
    /// The neighbor's address, meaningful with `lle::VALID`.
    pub lladdr: [u8; 6],
    pub flags: u16,
    /// When it expires, seconds of uptime (`la_expire`); 0 for static.
    pub expire: u64,
    /// Requests sent unanswered (`la_asked`), and refreshes left while in
    /// use (`la_preempt`).
    pub asked: u16,
    pub preempt: u16,
    /// `ln_state`.
    pub state: i16,
    /// Packets waiting for resolution (`la_hold`).
    pub hold: Vec<Packet>,
    /// When its timer fires, seconds of uptime (`lle_timer`).
    pub timer: Option<u64>,
    /// Used since the timer last asked (`r_skip_req`'s feedback).
    pub used: bool,
}

impl Lle {
    /// A new, unresolved entry.
    #[must_use]
    pub fn new(family: Family, addr: In6, ifindex: u16, flags: u16) -> Self {
        Self {
            family,
            addr,
            ifindex,
            lladdr: [0; 6],
            flags,
            expire: 0,
            asked: 0,
            preempt: 0,
            state: if family == Family::Inet6 && flags & lle::STATIC != 0 {
                state::ND6_REACHABLE
            } else {
                state::INCOMPLETE
            },
            hold: Vec::new(),
            timer: None,
            used: false,
        }
    }

    /// Sets its link-layer address (`lltable_set_entry_addr`).
    pub fn set_lladdr(&mut self, ll: [u8; 6]) {
        self.lladdr = ll;
        self.flags |= lle::VALID;
    }
}

impl Net {
    /// The entry for `addr` on interface `ifindex` (`lla_lookup`).
    #[must_use]
    pub fn lle(&self, ifindex: u16, family: Family, addr: &In6) -> Option<&Lle> {
        self.lle
            .iter()
            .find(|e| e.ifindex == ifindex && e.family == family && e.addr == *addr)
    }

    /// The same, to change.
    pub fn lle_mut(&mut self, ifindex: u16, family: Family, addr: &In6) -> Option<&mut Lle> {
        self.lle
            .iter_mut()
            .find(|e| e.ifindex == ifindex && e.family == family && e.addr == *addr)
    }

    /// Links `e`, unlinking and returning the entry it replaces
    /// (`lltable_link_entry`).
    pub fn lle_link(&mut self, e: Lle) -> Option<Lle> {
        let old = self
            .lle
            .iter()
            .position(|x| x.ifindex == e.ifindex && x.family == e.family && x.addr == e.addr)
            .map(|at| self.lle.swap_remove(at));
        self.lle.push(e);
        self.bump();
        old
    }

    /// Removes the entry for `addr` (`lltable_delete_addr`); an
    /// interface's own only with `ifaddr`.
    ///
    /// # Errors
    ///
    /// `ENOENT` for none, `EPERM` for the interface's own without
    /// `ifaddr`.
    pub fn lle_delete(
        &mut self,
        ifindex: u16,
        family: Family,
        addr: &In6,
        ifaddr: bool,
    ) -> Result<Lle, Errno> {
        let at = self
            .lle
            .iter()
            .position(|x| x.ifindex == ifindex && x.family == family && x.addr == *addr)
            .ok_or(Errno::ENOENT)?;
        if self.lle[at].flags & lle::IFADDR != 0 && !ifaddr {
            return Err(Errno::EPERM);
        }
        self.bump();
        Ok(self.lle.swap_remove(at))
    }

    /// Removes the entries of every interface inside `prefix/plen`
    /// (`lltable_prefix_free` with `in_lltable_match_prefix`): dynamic
    /// ones, static ones too with `all`, and the interface address
    /// `own`'s entry with `all`.
    pub fn lle_prefix_free(
        &mut self,
        family: Family,
        prefix: &In6,
        plen: u8,
        own: &In6,
        all: bool,
    ) {
        let before = self.lle.len();
        self.lle.retain(|e| {
            if e.family != family || !prefix_match(family, &e.addr, prefix, plen) {
                return true;
            }
            if e.flags & lle::IFADDR != 0 {
                return !(e.addr == *own && all);
            }
            !(all || e.flags & lle::STATIC == 0)
        });
        if self.lle.len() != before {
            self.bump();
        }
    }

    /// Drops every entry of interface `ifindex` (`lltable_free`, as the
    /// interface goes).
    pub fn lle_purge(&mut self, ifindex: u16) {
        self.lle.retain(|e| e.ifindex != ifindex);
    }

    /// `in_lltable_rtcheck` and `in6_lltable_rtcheck`: whether `addr` is
    /// a neighbor on interface `ifindex` a route covers.
    pub(crate) fn lle_rtcheck(&self, ifindex: u16, family: Family, addr: &In6) -> bool {
        let Some(ifp) = self.by_index(ifindex) else {
            return false;
        };
        let covered_here = || {
            ifp.addrs
                .iter()
                .any(|a| a.family == family && prefix_match(family, &a.addr, addr, a.plen))
        };
        // A link-local address is looked up in its interface's zone.
        let zoned = family == Family::Inet6 && crate::addr::is_linklocal6(addr);
        let route = self
            .routes
            .iter()
            .filter(|r| {
                r.family == family
                    && r.fib == ifp.fib
                    && prefix_match(family, &r.dst, addr, r.plen)
                    && (!zoned || r.ifindex == ifindex)
            })
            .max_by_key(|r| r.plen);
        match family {
            Family::Inet => {
                let Some(r) = route else {
                    return false;
                };
                if r.flags & rtf::GATEWAY != 0 {
                    let host = r.flags & rtf::HOST != 0;
                    let gw_is_dst = r.gateway == Gateway::Addr(*addr);
                    let rifp = self.by_index(r.ifindex);
                    let ok_if = rifp.is_some_and(|i| {
                        i.ty == IFT_ETHER && i.flags & (iff::NOARP | iff::STATICARP) == 0
                    });
                    if !host || !ok_if || !gw_is_dst {
                        return false;
                    }
                }
                r.ifindex == ifindex || r.flags & rtf::HOST != 0 || covered_here()
            }
            Family::Inet6 => match route {
                Some(r) if r.flags & rtf::GATEWAY != 0 || r.ifindex != ifindex => covered_here(),
                _ => true,
            },
        }
    }

    /// An `RTF_LLDATA` request on the routing socket (`lla_rt_output`):
    /// `arp -s`'s and `ndp -s`'s `RTM_ADD`, and `RTM_DELETE`. `gw` is the
    /// request's `sockaddr_dl`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for no link-layer gateway, an unknown interface, an
    /// address no route puts on it, or another request type; `ESRCH` for
    /// an interface without the table; `EPERM` to replace or delete the
    /// interface's own address; `ENOENT` to delete what is not there.
    pub fn lla_rt_output(
        &mut self,
        hdr: &RtMsghdr,
        family: Family,
        dst: &In6,
        gw: Option<&[u8]>,
    ) -> Result<(), Errno> {
        let gw = gw.ok_or(Errno::EINVAL)?;
        if gw.len() < 8 || gw[1] != AF_LINK {
            return Err(Errno::EINVAL);
        }
        let ifindex = u16::from_ne_bytes([gw[2], gw[3]]);
        let ifty = self.by_index(ifindex).ok_or(Errno::EINVAL)?.ty;
        match hdr.rtm_type {
            rtm::ADD => {
                let flags = if hdr.rtm_rmx.rmx_expire == 0 {
                    lle::STATIC
                } else {
                    0
                };
                if !self.lle_rtcheck(ifindex, family, dst) {
                    return Err(Errno::ENOMEM);
                }
                // Only Ethernet interfaces have link-layer headers to
                // make (`lltable_calc_llheader`).
                if ifty != IFT_ETHER {
                    return Err(Errno::EINVAL);
                }
                let nlen = usize::from(gw[5]);
                let alen = usize::from(gw[6]);
                let ll: [u8; 6] = gw
                    .get(8 + nlen..8 + nlen + 6)
                    .filter(|_| alen == 6)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(Errno::EINVAL)?;
                let mut e = Lle::new(family, *dst, ifindex, flags);
                e.set_lladdr(ll);
                if hdr.rtm_flags & rtf::ANNOUNCE != 0 {
                    e.flags |= lle::PUB;
                }
                e.expire = hdr.rtm_rmx.rmx_expire;
                if self
                    .lle(ifindex, family, dst)
                    .is_some_and(|o| o.flags & lle::IFADDR != 0)
                {
                    return Err(Errno::EPERM);
                }
                let publish = e.flags & lle::PUB != 0;
                self.lle_link(e);
                // `in_lltable_post_resolved`: a published entry is
                // announced.
                if publish && family == Family::Inet {
                    let a = crate::addr::v4_of(dst);
                    let _ = crate::arp::request(self, ifindex, Some(a), a, Some(ll));
                }
                Ok(())
            }
            rtm::DELETE => self.lle_delete(ifindex, family, dst, false).map(|_| ()),
            _ => Err(Errno::EINVAL),
        }
    }

    /// The entries of `family`, as `NET_RT_FLAGS` with `RTF_LLINFO` reports
    /// them (`in_lltable_dump_entry`, `in6_lltable_dump_entry`): a header,
    /// the address, and a `sockaddr_dl`, each record. `now` and `uptime`
    /// are the realtime and uptime clocks' seconds.
    #[must_use]
    pub fn lle_dump(&self, family: Family, now: u64, uptime: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut ifs: Vec<u16> = self.ifaces.iter().map(|i| i.index).collect();
        ifs.sort_unstable();
        for index in ifs {
            let Some(ifp) = self.by_index(index) else {
                continue;
            };
            for e in self
                .lle
                .iter()
                .filter(|e| e.ifindex == index && e.family == family)
            {
                if e.flags & lle::DELETED != 0 {
                    continue;
                }
                out.extend_from_slice(&dump_entry(e, ifp.ty, now, uptime));
            }
        }
        out
    }
}

/// One entry's record.
fn dump_entry(e: &Lle, ifty: u8, now: u64, uptime: u64) -> Vec<u8> {
    let hdr = size_of::<RtMsghdr>();
    let (sa_len, pad) = match e.family {
        Family::Inet => (16usize, 0usize),
        // `ndp.c` wants the `sockaddr_dl` word-aligned.
        Family::Inet6 => (28, 4),
    };
    // `sizeof(struct sockaddr_dl)`.
    let sdl_len = 54usize;
    let total = hdr + sa_len + pad + sdl_len;
    let mut h = RtMsghdr {
        rtm_msglen: u16::try_from(total).unwrap_or(0),
        rtm_version: RTM_VERSION,
        rtm_type: rtm::GET,
        rtm_index: e.ifindex,
        rtm_flags: rtf::UP | rtf::HOST | rtf::LLDATA,
        rtm_addrs: rta::DST | rta::GATEWAY,
        ..RtMsghdr::default()
    };
    if e.flags & lle::PUB != 0 {
        h.rtm_flags |= rtf::ANNOUNCE;
    }
    if e.flags & lle::STATIC != 0 {
        h.rtm_flags |= rtf::STATIC;
    }
    if e.flags & lle::IFADDR != 0 {
        h.rtm_flags |= rtf::PINNED;
    }
    match e.family {
        Family::Inet => {
            h.rtm_rmx.rmx_expire = if e.flags & lle::STATIC != 0 {
                0
            } else {
                e.expire
            };
        }
        Family::Inet6 => {
            if e.expire != 0 {
                h.rtm_rmx.rmx_expire = (e.expire + now).saturating_sub(uptime);
            }
            h.rtm_rmx.rmx_pksent = u64::from(e.asked);
            // `rmx_state` is `rmx_weight`.
            #[allow(clippy::cast_sign_loss, reason = "as C's u_long")]
            {
                h.rtm_rmx.rmx_weight = i64::from(e.state) as u64;
            }
        }
    }
    let mut out = Vec::with_capacity(total);
    crate::rtmsg::put(&mut out, &h);
    out.truncate(hdr);
    // `sa6_recoverscope`: a link-local address with its zone.
    out.extend_from_slice(&crate::rtmsg::sa_of(e.family, &e.addr, e.ifindex));
    out.resize(hdr + sa_len + pad, 0);
    let mut sdl = [0u8; 54];
    sdl[0] = 54;
    sdl[1] = AF_LINK;
    sdl[2..4].copy_from_slice(&e.ifindex.to_ne_bytes());
    sdl[4] = ifty;
    if e.flags & lle::VALID != 0 {
        sdl[6] = 6;
        sdl[8..14].copy_from_slice(&e.lladdr);
    }
    out.extend_from_slice(&sdl);
    out
}
