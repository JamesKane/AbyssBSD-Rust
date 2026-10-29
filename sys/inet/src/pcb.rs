// SPDX-License-Identifier: BSD-2-Clause
//! Protocol control blocks: each socket's addresses and ports, and the
//! rules that decide what may bind or connect where and which socket a
//! datagram or connection reaches, as FreeBSD's `in_pcb.c` and
//! `in6_pcb.c` decide them.
//!
//! A table holds every PCB of one protocol, by id. A PCB is "hashed"
//! (`INP_INHASHLIST`) once it has a port; lookups see hashed ones only.
//! Unconnected hashed PCBs are FreeBSD's wildcard hash, connected ones
//! its exact hash.
//!
//! Behavior reference: sys/netinet/in_pcb.c (`in_pcbbind_avail`,
//! `in_pcbbind_setup`, `in_pcb_lport_dest`, `in_pcbconnect`,
//! `in_pcbdisconnect`, `in_pcblookup_local`, `in_pcblookup_hash_locked`),
//! sys/netinet6/in6_pcb.c (the same for IPv6, `in6_pcbsetport`).

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};

use abi::errno::Errno;
use abi::inet::{INP_IPV4, INP_IPV6, INP_IPV6PROTO, inp};
use abi::socket::{SO_REUSEADDR, SO_REUSEPORT, SO_REUSEPORT_LB};
use sync::SpinLock;

use crate::addr::{
    ANY, In6, LOOPBACK6, Sockaddr, from_v4, is_broadcast4, is_multicast4, is_multicast6,
    is_unspecified, is_v4mapped, v4_of,
};
use crate::ifnet::NET;
use crate::{Cred, Family, Proto, Socket, tune};

/// One PCB (`struct inpcb`).
#[derive(Debug)]
pub struct Pcb {
    pub(crate) so: Weak<Socket>,
    /// The socket's domain.
    pub family: Family,
    /// `inp_vflag`.
    pub vflag: u8,
    /// `inp_flags`, `inp_flags2`.
    pub flags: i32,
    pub flags2: i32,
    /// PCB form: IPv4 in the last four bytes.
    pub laddr: In6,
    pub faddr: In6,
    pub lport: u16,
    pub fport: u16,
    /// The zone of a link-local address (`ie6_zoneid`).
    pub zone: u32,
    /// `INP_INHASHLIST`.
    pub hashed: bool,
    /// The creator's uid (`inp_cred->cr_uid`).
    pub uid: u32,
    /// `inp_gencnt`.
    pub gencnt: u64,
    /// A TCP connection in `TIME_WAIT` after its socket went: when it
    /// ends (the table's clock).
    pub tw_until: Option<u64>,
}

impl Pcb {
    fn v4(&self) -> bool {
        self.vflag & INP_IPV4 != 0
    }

    fn v6(&self) -> bool {
        self.vflag & INP_IPV6 != 0
    }

    fn laddr4(&self) -> [u8; 4] {
        v4_of(&self.laddr)
    }

    fn faddr4(&self) -> [u8; 4] {
        v4_of(&self.faddr)
    }

    /// Unconnected (`inp_faddr` the wildcard).
    fn wild(&self) -> bool {
        is_unspecified(&self.faddr)
    }

    /// The socket, if it is still there.
    #[must_use]
    pub fn socket(&self) -> Option<Arc<Socket>> {
        self.so.upgrade()
    }
}

/// A protocol's PCBs (`struct inpcbinfo`).
#[derive(Debug)]
pub struct Table {
    pub(crate) pcbs: BTreeMap<u64, Pcb>,
    next_id: u64,
    /// `ipi_gencnt`.
    pub gencnt: u64,
    /// `ipi_lastport`, `ipi_lasthi`, `ipi_lastlow`.
    last: [u16; 3],
    /// The earliest `TIME_WAIT` end, to reap lazily.
    next_reap: u64,
}

impl Table {
    const fn new() -> Self {
        Self {
            pcbs: BTreeMap::new(),
            next_id: 1,
            gencnt: 0,
            last: [0; 3],
            next_reap: u64::MAX,
        }
    }
}

/// The UDP and TCP tables (`V_udbinfo`, `V_tcbinfo`).
pub static UDB: SpinLock<Table> = SpinLock::new(Table::new());
pub static TCB: SpinLock<Table> = SpinLock::new(Table::new());

/// The table of `proto`.
#[must_use]
pub fn table(proto: Proto) -> &'static SpinLock<Table> {
    match proto {
        Proto::Udp => &UDB,
        Proto::Tcp => &TCB,
    }
}

/// Which range an ephemeral port comes from (`IP_PORTRANGE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Range {
    Default,
    High,
    Low,
}

/// The wildcard cost of a mapped PCB in an IPv4 lookup
/// (`INP_LOOKUP_MAPPED_PCB_COST`).
const MAPPED_COST: u32 = 3;

impl Table {
    pub(crate) fn reserve_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// `in_pcballoc`: `so`'s PCB, unbound.
    pub(crate) fn alloc(&mut self, so: &Arc<Socket>, uid: u32) {
        let (vflag, flags) = match so.family {
            Family::Inet => (INP_IPV4, 0),
            Family::Inet6 => {
                // Relaxed: a tunable.
                if tune::V6ONLY.load(core::sync::atomic::Ordering::Relaxed) != 0 {
                    (INP_IPV6PROTO | INP_IPV6, inp::IPV6_V6ONLY)
                } else {
                    (INP_IPV6PROTO | INP_IPV6 | INP_IPV4, 0)
                }
            }
        };
        self.gencnt += 1;
        self.pcbs.insert(
            so.id,
            Pcb {
                so: Arc::downgrade(so),
                family: so.family,
                vflag,
                flags,
                flags2: 0,
                laddr: ANY,
                faddr: ANY,
                lport: 0,
                fport: 0,
                zone: 0,
                hashed: false,
                uid,
                gencnt: self.gencnt,
                tw_until: None,
            },
        );
    }

    /// `in_pcbfree`.
    pub(crate) fn free(&mut self, id: u64) {
        if self.pcbs.remove(&id).is_some() {
            self.gencnt += 1;
        }
    }

    /// The PCB `id`.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Pcb> {
        self.pcbs.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: u64) -> Option<&mut Pcb> {
        self.pcbs.get_mut(&id)
    }

    /// Every PCB, newest first (`ipi_listhead`'s order), expired
    /// `TIME_WAIT` ones reaped first.
    pub fn iter(&mut self) -> impl Iterator<Item = (&u64, &Pcb)> {
        self.reap();
        // A socket gone without a close (a kernel bug, or a test) leaves
        // its PCB behind; it goes here.
        self.pcbs
            .retain(|_, p| p.tw_until.is_some() || p.so.strong_count() > 0);
        self.pcbs.iter().rev()
    }

    /// Drops the PCBs whose `TIME_WAIT` has ended.
    pub(crate) fn reap(&mut self) {
        let now = crate::now();
        if now < self.next_reap {
            return;
        }
        self.pcbs.retain(|_, p| p.tw_until.is_none_or(|t| t > now));
        self.next_reap = self
            .pcbs
            .values()
            .filter_map(|p| p.tw_until)
            .min()
            .unwrap_or(u64::MAX);
        self.gencnt += 1;
    }

    /// Starts `TIME_WAIT` on `id` for twice the MSL.
    pub(crate) fn time_wait(&mut self, id: u64) {
        // Relaxed: a tunable.
        let msl = u64::from(tune::TCP_MSL_LOCAL.load(core::sync::atomic::Ordering::Relaxed));
        let until = crate::now().saturating_add(2 * msl * 1_000_000);
        if let Some(p) = self.pcbs.get_mut(&id) {
            p.tw_until = Some(until);
            self.next_reap = self.next_reap.min(until);
        }
    }

    /// `in_pcbdrop` and `in_pcbremhash`: out of the lookups, its port
    /// free, listed until its socket goes.
    pub(crate) fn drop_hash(&mut self, id: u64) {
        if let Some(p) = self.pcbs.get_mut(&id) {
            p.hashed = false;
            p.flags |= inp::DROPPED;
        }
    }

    // --- Lookups ---

    /// `in_pcblookup_local`: a PCB bound to `laddr:lport` (IPv4); with
    /// `wildcard`, the best match counting wildcards (other than
    /// `exclude`).
    fn local4(&self, laddr: [u8; 4], lport: u16, wildcard: bool, exclude: u64) -> Option<&Pcb> {
        let laddr_any = laddr == [0; 4];
        if !wildcard {
            return self.pcbs.iter().find_map(|(&id, p)| {
                (id != exclude
                    && p.hashed
                    && p.v4()
                    && p.wild()
                    && p.laddr4() == laddr
                    && p.lport == lport)
                    .then_some(p)
            });
        }
        let mut best = None;
        let mut matchwild = 3 + MAPPED_COST;
        for (&id, p) in &self.pcbs {
            if id == exclude || !p.hashed || p.lport != lport || !p.v4() {
                continue;
            }
            let mut w = 0;
            if p.v6() {
                w += MAPPED_COST;
            }
            if p.faddr4() != [0; 4] {
                w += 1;
            }
            if p.laddr4() != [0; 4] {
                if laddr_any {
                    w += 1;
                } else if p.laddr4() != laddr {
                    continue;
                }
            } else if !laddr_any {
                w += 1;
            }
            if w < matchwild {
                best = Some(p);
                matchwild = w;
                if w == 0 {
                    break;
                }
            }
        }
        best
    }

    /// `in6_pcblookup_local`.
    fn local6(&self, laddr: &In6, lport: u16, wildcard: bool, exclude: u64) -> Option<&Pcb> {
        let laddr_any = is_unspecified(laddr);
        if !wildcard {
            return self.pcbs.iter().find_map(|(&id, p)| {
                (id != exclude
                    && p.hashed
                    && p.v6()
                    && p.wild()
                    && p.laddr == *laddr
                    && p.lport == lport)
                    .then_some(p)
            });
        }
        let mut best = None;
        let mut matchwild = 3;
        for (&id, p) in &self.pcbs {
            if id == exclude || !p.hashed || p.lport != lport || !p.v6() {
                continue;
            }
            let mut w = 0;
            if !p.wild() {
                w += 1;
            }
            if !is_unspecified(&p.laddr) {
                if laddr_any {
                    w += 1;
                } else if p.laddr != *laddr {
                    continue;
                }
            } else if !laddr_any {
                w += 1;
            }
            if w < matchwild {
                best = Some(p);
                matchwild = w;
                if w == 0 {
                    break;
                }
            }
        }
        best
    }

    /// `in_pcblookup_hash_locked` for IPv4: the connected PCB for the
    /// four-tuple, else (with `wildcard`) the unconnected one bound to
    /// `laddr` or the wildcard, an IPv4 one before a mapped IPv6 one.
    #[must_use]
    pub fn lookup4(
        &self,
        faddr: [u8; 4],
        fport: u16,
        laddr: [u8; 4],
        lport: u16,
        wildcard: bool,
    ) -> Option<&Pcb> {
        let exact = self.pcbs.values().find(|p| {
            p.hashed
                && p.v4()
                && !p.wild()
                && p.faddr4() == faddr
                && p.fport == fport
                && p.laddr4() == laddr
                && p.lport == lport
        });
        if exact.is_some() || !wildcard {
            return exact;
        }
        let (mut exact_l, mut wild, mut wild_mapped) = (None, None, None);
        for p in self.pcbs.values() {
            if !p.hashed || !p.v4() || !p.wild() || p.lport != lport {
                continue;
            }
            if p.laddr4() == laddr {
                if exact_l.is_none() {
                    exact_l = Some(p);
                }
            } else if p.laddr4() == [0; 4] {
                if p.vflag & INP_IPV6PROTO != 0 {
                    wild_mapped = Some(p);
                } else {
                    wild = Some(p);
                }
            }
        }
        exact_l.or(wild).or(wild_mapped)
    }

    /// `in6_pcblookup_hash_locked`.
    #[must_use]
    pub fn lookup6(
        &self,
        faddr: &In6,
        fport: u16,
        laddr: &In6,
        lport: u16,
        wildcard: bool,
    ) -> Option<&Pcb> {
        let exact = self.pcbs.values().find(|p| {
            p.hashed
                && p.v6()
                && !p.wild()
                && p.faddr == *faddr
                && p.fport == fport
                && p.laddr == *laddr
                && p.lport == lport
        });
        if exact.is_some() || !wildcard {
            return exact;
        }
        let (mut exact_l, mut wild) = (None, None);
        for p in self.pcbs.values() {
            if !p.hashed || !p.v6() || !p.wild() || p.lport != lport {
                continue;
            }
            if p.laddr == *laddr {
                if exact_l.is_none() {
                    exact_l = Some(p);
                }
            } else if is_unspecified(&p.laddr) {
                wild = Some(p);
            }
        }
        exact_l.or(wild)
    }

    /// The socket a packet from `src` to `dst` reaches (`family` the
    /// packet's), and its PCB's `inp_flags`.
    #[must_use]
    pub fn deliver(
        &self,
        family: Family,
        src: &Sockaddr,
        dst: &Sockaddr,
    ) -> Option<(Arc<Socket>, i32, i32, Family)> {
        let p = match family {
            Family::Inet => {
                self.lookup4(v4_of(&src.addr), src.port, v4_of(&dst.addr), dst.port, true)
            }
            Family::Inet6 => self.lookup6(&src.addr, src.port, &dst.addr, dst.port, true),
        }?;
        if p.tw_until.is_some() {
            return None;
        }
        p.socket().map(|s| (s, p.flags, p.flags2, p.family))
    }

    // --- Binding ---

    /// `in_pcbbind_avail`: whether `me` may bind `laddr:lport`.
    #[allow(clippy::too_many_arguments, reason = "in_pcbbind_avail's")]
    fn avail4(
        &self,
        me: &Pcb,
        id: u64,
        laddr: [u8; 4],
        lport: u16,
        sooptions: i32,
        wildcard: bool,
        cred: &Cred,
        so_type_stream: bool,
    ) -> Result<(), Errno> {
        let mut reuseport = sooptions & SO_REUSEPORT;
        let mut reuseport_lb = sooptions & SO_REUSEPORT_LB;
        if is_multicast4(&laddr) {
            if sooptions & (SO_REUSEADDR | SO_REUSEPORT) != 0 {
                reuseport = SO_REUSEADDR | SO_REUSEPORT;
            }
            if sooptions & (SO_REUSEADDR | SO_REUSEPORT_LB) != 0 {
                reuseport_lb = SO_REUSEADDR | SO_REUSEPORT_LB;
            }
        } else if laddr != [0; 4]
            && me.flags & inp::BINDANY == 0
            && !NET.lock().is_local(Family::Inet, &from_v4(laddr), 0)
        {
            return Err(Errno::EADDRNOTAVAIL);
        }
        if lport == 0 {
            return Ok(());
        }
        reserved(lport, cred)?;
        if !is_multicast4(&laddr) && !cred.reuseport {
            // Another user's binding of the port is refused, unless it
            // is a connected stream's (a unique four-tuple).
            if let Some(t) = self.local4(laddr, lport, true, id)
                && (!so_type_stream || t.wild())
                && me.uid != t.uid
            {
                return Err(Errno::EADDRINUSE);
            }
        }
        if let Some(t) = self.local4(laddr, lport, wildcard, id)
            && (reuseport | reuseport_lb) & so_options_of(t) == 0
            && (laddr != [0; 4]
                || t.laddr4() != [0; 4]
                || me.vflag & INP_IPV6PROTO == 0
                || t.vflag & INP_IPV6PROTO == 0)
        {
            return Err(Errno::EADDRINUSE);
        }
        Ok(())
    }

    /// `in6_pcbbind_avail`.
    #[allow(clippy::too_many_arguments, reason = "in6_pcbbind_avail's")]
    fn avail6(
        &self,
        me: &Pcb,
        id: u64,
        laddr: &In6,
        zone: u32,
        lport: u16,
        sooptions: i32,
        wildcard: bool,
        cred: &Cred,
        so_type_stream: bool,
    ) -> Result<(), Errno> {
        let mut reuseport = sooptions & SO_REUSEPORT;
        let mut reuseport_lb = sooptions & SO_REUSEPORT_LB;
        if is_multicast6(laddr) {
            if sooptions & (SO_REUSEADDR | SO_REUSEPORT) != 0 {
                reuseport = SO_REUSEADDR | SO_REUSEPORT;
            }
            if sooptions & (SO_REUSEADDR | SO_REUSEPORT_LB) != 0 {
                reuseport_lb = SO_REUSEADDR | SO_REUSEPORT_LB;
            }
        } else if !is_unspecified(laddr)
            && me.flags & inp::BINDANY == 0
            && !NET.lock().is_local(Family::Inet6, laddr, zone)
        {
            return Err(Errno::EADDRNOTAVAIL);
        }
        if lport == 0 {
            return Ok(());
        }
        reserved(lport, cred)?;
        let dual = me.flags & inp::IPV6_V6ONLY == 0 && is_unspecified(laddr);
        if !is_multicast6(laddr) && !cred.reuseport {
            if let Some(t) = self.local6(laddr, lport, true, id)
                && (!so_type_stream || t.wild())
                && me.uid != t.uid
            {
                return Err(Errno::EADDRINUSE);
            }
            if dual
                && let Some(t) = self.local4([0; 4], lport, true, id)
                && (!so_type_stream || t.wild())
                && me.uid != t.uid
            {
                return Err(Errno::EADDRINUSE);
            }
        }
        if let Some(t) = self.local6(laddr, lport, wildcard, id)
            && (reuseport | reuseport_lb) & so_options_of(t) == 0
        {
            return Err(Errno::EADDRINUSE);
        }
        if dual
            && let Some(t) = self.local4([0; 4], lport, wildcard, id)
            && (reuseport | reuseport_lb) & so_options_of(t) == 0
            && (t.laddr4() != [0; 4] || t.vflag & INP_IPV6PROTO != 0)
        {
            return Err(Errno::EADDRINUSE);
        }
        Ok(())
    }

    /// `in_pcb_lport_dest`: an ephemeral port for `id`, from the range
    /// its `IP_PORTRANGE` names, unused with `laddr` (and, for a
    /// connect, with the destination `dst`).
    fn ephemeral(
        &mut self,
        id: u64,
        laddr: &In6,
        dst: Option<&Sockaddr>,
        wildcard: bool,
        cred: &Cred,
    ) -> Result<u16, Errno> {
        use core::sync::atomic::Ordering::Relaxed;
        let me = self.pcbs.get(&id).ok_or(Errno::EINVAL)?;
        let range = if me.flags & inp::HIGHPORT != 0 {
            Range::High
        } else if me.flags & inp::LOWPORT != 0 {
            Range::Low
        } else {
            Range::Default
        };
        let (first, last, slot) = match range {
            Range::High => (tune::HIFIRST.load(Relaxed), tune::HILAST.load(Relaxed), 1),
            Range::Low => {
                if !cred.reservedport {
                    return Err(Errno::EACCES);
                }
                (tune::LOWFIRST.load(Relaxed), tune::LOWLAST.load(Relaxed), 2)
            }
            Range::Default => (tune::FIRST.load(Relaxed), tune::LAST.load(Relaxed), 0),
        };
        let clamp = |v: i32| u16::try_from(v.clamp(1, 65535)).unwrap_or(1);
        let (first, last) = (clamp(first), clamp(last));
        let (first, last) = if first > last {
            (last, first)
        } else {
            (first, last)
        };
        let v4only = me.vflag & (INP_IPV4 | INP_IPV6) == INP_IPV4;
        let v6 = me.vflag & INP_IPV6 != 0;
        let dual = me.vflag & INP_IPV4 != 0;
        let span = u32::from(last - first);
        if tune::RANDOMIZED.load(Relaxed) != 0 && span > 0 {
            let off = u16::try_from(crate::random() % span).unwrap_or(0);
            self.last[slot] = first + off;
        }
        let mut count = i64::from(span);
        loop {
            if count < 0 {
                return Err(Errno::EADDRNOTAVAIL);
            }
            count -= 1;
            let next = self.last[slot].wrapping_add(1);
            self.last[slot] = if next < first || next > last {
                first
            } else {
                next
            };
            let lport = self.last[slot];
            let taken = match dst {
                Some(d) if v4only || d.family == Family::Inet => self
                    .lookup4(v4_of(&d.addr), d.port, v4_of(laddr), lport, wildcard)
                    .is_some(),
                Some(d) => self
                    .lookup6(&d.addr, d.port, laddr, lport, wildcard)
                    .is_some(),
                None if v6 => {
                    self.local6(laddr, lport, wildcard, id).is_some()
                        || (dual && self.local4(v4_of(laddr), lport, wildcard, id).is_some())
                }
                None => self.local4(v4_of(laddr), lport, wildcard, id).is_some(),
            };
            if !taken {
                return Ok(lport);
            }
        }
    }

    /// `in_pcbbind` and `in6_pcbbind` for the socket `so`: to `sa`'s
    /// address and port, a port of 0 an ephemeral one. An `AF_INET6`
    /// socket bound to a mapped address becomes an IPv4 one; bound to
    /// the wildcard without `IPV6_V6ONLY`, it takes IPv4 too.
    ///
    /// # Errors
    ///
    /// See [`Socket::bind`].
    pub(crate) fn bind(
        &mut self,
        so: &Socket,
        sa: &Sockaddr,
        cred: &Cred,
        sooptions: i32,
    ) -> Result<(), Errno> {
        self.reap();
        let id = so.id;
        let tcp = so.proto == Proto::Tcp;
        let me = self.pcbs.get(&id).ok_or(Errno::EINVAL)?;
        if me.lport != 0 || !is_unspecified(&me.laddr) || me.flags & inp::DROPPED != 0 {
            return Err(Errno::EINVAL);
        }
        let wildcard = sooptions & (SO_REUSEADDR | SO_REUSEPORT | SO_REUSEPORT_LB) == 0;
        let saved = me.vflag;
        match so.family {
            Family::Inet => {
                let a = v4_of(&sa.addr);
                if tcp && is_multicast4(&a) {
                    return Err(Errno::EAFNOSUPPORT);
                }
                self.bind4(id, a, sa.port, sooptions, wildcard, cred, tcp)
            }
            Family::Inet6 => {
                if tcp && is_multicast6(&sa.addr) {
                    return Err(Errno::EAFNOSUPPORT);
                }
                let v6only = me.flags & inp::IPV6_V6ONLY != 0;
                let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
                p.vflag = (p.vflag & !INP_IPV4) | INP_IPV6;
                let r = if !v6only && is_unspecified(&sa.addr) {
                    p.vflag |= INP_IPV4;
                    self.bind6(id, sa, sooptions, wildcard, cred, tcp)
                } else if !v6only && is_v4mapped(&sa.addr) {
                    let a = v4_of(&sa.addr);
                    if tcp && is_multicast4(&a) {
                        Err(Errno::EAFNOSUPPORT)
                    } else {
                        p.vflag = (p.vflag | INP_IPV4) & !INP_IPV6;
                        self.bind4(id, a, sa.port, sooptions, wildcard, cred, tcp)
                    }
                } else if is_v4mapped(&sa.addr) {
                    // An IPv6-only socket has no mapped addresses.
                    Err(Errno::EADDRNOTAVAIL)
                } else {
                    self.bind6(id, sa, sooptions, wildcard, cred, tcp)
                };
                if r.is_err()
                    && let Some(p) = self.pcbs.get_mut(&id)
                {
                    p.vflag = saved;
                }
                r
            }
        }
    }

    #[allow(clippy::too_many_arguments, reason = "in_pcbbind_setup's")]
    fn bind4(
        &mut self,
        id: u64,
        laddr: [u8; 4],
        port: u16,
        sooptions: i32,
        wildcard: bool,
        cred: &Cred,
        tcp: bool,
    ) -> Result<(), Errno> {
        let me = self.pcbs.get(&id).ok_or(Errno::EINVAL)?;
        self.avail4(me, id, laddr, port, sooptions, wildcard, cred, tcp)?;
        let lport = if port == 0 {
            self.ephemeral(id, &from_v4(laddr), None, wildcard, cred)?
        } else {
            port
        };
        let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
        p.laddr = from_v4(laddr);
        p.lport = lport;
        p.hashed = true;
        if port == 0 {
            p.flags |= inp::ANONPORT;
        }
        p.flags |= inp::INHASHLIST;
        Ok(())
    }

    fn bind6(
        &mut self,
        id: u64,
        sa: &Sockaddr,
        sooptions: i32,
        wildcard: bool,
        cred: &Cred,
        tcp: bool,
    ) -> Result<(), Errno> {
        let zone = if crate::addr::is_linklocal6(&sa.addr) {
            if sa.scope == 0 {
                // `sa6_embedscope` with `ip6_use_defzone` 0.
                return Err(Errno::EINVAL);
            }
            sa.scope
        } else {
            0
        };
        let me = self.pcbs.get(&id).ok_or(Errno::EINVAL)?;
        self.avail6(
            me, id, &sa.addr, zone, sa.port, sooptions, wildcard, cred, tcp,
        )?;
        let lport = if sa.port == 0 {
            self.ephemeral(id, &sa.addr, None, wildcard, cred)?
        } else {
            sa.port
        };
        let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
        p.laddr = sa.addr;
        p.zone = zone;
        p.lport = lport;
        p.hashed = true;
        if sa.port == 0 {
            p.flags |= inp::ANONPORT;
        }
        p.flags |= inp::INHASHLIST;
        Ok(())
    }

    /// `in_pcbbind(inp, NULL)`: an ephemeral port on the wildcard address
    /// (an implicit bind before a send or `listen`). An `AF_INET6` socket
    /// takes IPv4 too unless `IPV6_V6ONLY`.
    pub(crate) fn bind_anon(&mut self, so: &Socket, cred: &Cred) -> Result<(), Errno> {
        self.reap();
        let id = so.id;
        let sooptions = so.so_options();
        let wildcard = sooptions & (SO_REUSEADDR | SO_REUSEPORT | SO_REUSEPORT_LB) == 0;
        let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
        if p.lport != 0 {
            return Ok(());
        }
        if so.family == Family::Inet6 && is_unspecified(&p.laddr) && p.vflag & INP_IPV6 != 0 {
            p.vflag &= !INP_IPV4;
            if p.flags & inp::IPV6_V6ONLY == 0 {
                p.vflag |= INP_IPV4;
            }
        }
        let laddr = p.laddr;
        let lport = self.ephemeral(id, &laddr, None, wildcard, cred)?;
        let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
        p.lport = lport;
        p.hashed = true;
        p.flags |= inp::ANONPORT | inp::INHASHLIST;
        Ok(())
    }

    // --- Connecting ---

    /// `in_pcbconnect` and `in6_pcbconnect` (with the `AF_INET6`
    /// protocols' mapped-address handling): sets the foreign address,
    /// choosing the local one by route and an ephemeral port if unbound.
    /// Returns the destination as the PCB now holds it (IPv4 for a mapped
    /// one).
    ///
    /// # Errors
    ///
    /// `EINVAL` (mapped with `IPV6_V6ONLY`), `EAFNOSUPPORT` (a family the
    /// PCB cannot take), `EADDRNOTAVAIL` (port 0), `ENETUNREACH` or
    /// `EHOSTUNREACH` (no route), `EADDRINUSE` (the four-tuple taken).
    pub(crate) fn connect(
        &mut self,
        so: &Socket,
        dst: &Sockaddr,
        cred: &Cred,
    ) -> Result<Sockaddr, Errno> {
        self.reap();
        let id = so.id;
        let me = self.pcbs.get(&id).ok_or(Errno::EINVAL)?;
        let saved = me.vflag;
        let unbound = me.lport == 0;
        let r = match so.family {
            Family::Inet => self.connect4(id, dst, cred),
            Family::Inet6 => {
                if is_v4mapped(&dst.addr) {
                    if me.flags & inp::IPV6_V6ONLY != 0 {
                        return Err(Errno::EINVAL);
                    }
                    if me.vflag & INP_IPV4 == 0 {
                        return Err(Errno::EAFNOSUPPORT);
                    }
                    let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
                    p.vflag = (p.vflag | INP_IPV4) & !INP_IPV6;
                    self.connect4(id, &dst.unmapped(), cred)
                } else {
                    if me.vflag & INP_IPV6 == 0 {
                        return Err(Errno::EAFNOSUPPORT);
                    }
                    let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
                    p.vflag = (p.vflag & !INP_IPV4) | INP_IPV6;
                    self.connect6(id, dst, cred)
                }
            }
        };
        if r.is_err()
            && unbound
            && let Some(p) = self.pcbs.get_mut(&id)
            && p.lport == 0
        {
            p.vflag = saved;
        }
        r
    }

    fn connect4(&mut self, id: u64, dst: &Sockaddr, cred: &Cred) -> Result<Sockaddr, Errno> {
        if dst.port == 0 {
            return Err(Errno::EADDRNOTAVAIL);
        }
        let faddr = v4_of(&dst.addr);
        // `in_broadcast` covers the wildcard too; without
        // `connect_inaddr_wild` (0 since 15.0) neither is reachable.
        if faddr == [0; 4] || is_broadcast4(&faddr) {
            return Err(Errno::ENETUNREACH);
        }
        let me = self.pcbs.get(&id).ok_or(Errno::EINVAL)?;
        let laddr = if me.laddr4() == [0; 4] {
            let (src, _) = NET.lock().source(Family::Inet, &from_v4(faddr), 0)?;
            v4_of(&src)
        } else {
            me.laddr4()
        };
        let lport = if me.lport == 0 {
            self.ephemeral(id, &from_v4(laddr), Some(dst), true, cred)?
        } else {
            if self
                .lookup4(faddr, dst.port, laddr, me.lport, false)
                .is_some()
            {
                return Err(Errno::EADDRINUSE);
            }
            me.lport
        };
        let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
        if p.lport == 0 {
            p.flags |= inp::ANONPORT;
        }
        p.faddr = from_v4(faddr);
        p.fport = dst.port;
        p.laddr = from_v4(laddr);
        p.lport = lport;
        p.hashed = true;
        p.flags |= inp::INHASHLIST;
        Ok(Sockaddr::v4(faddr, dst.port))
    }

    fn connect6(&mut self, id: u64, dst: &Sockaddr, cred: &Cred) -> Result<Sockaddr, Errno> {
        if dst.port == 0 {
            return Err(Errno::EADDRNOTAVAIL);
        }
        // `connect_in6addr_wild`: the unspecified address means `::1`.
        let faddr = if is_unspecified(&dst.addr) {
            LOOPBACK6
        } else {
            dst.addr
        };
        let zone = if crate::addr::is_linklocal6(&faddr) {
            dst.scope
        } else {
            0
        };
        let target = Sockaddr::v6(faddr, dst.port, zone);
        let me = self.pcbs.get(&id).ok_or(Errno::EINVAL)?;
        let (laddr, lzone) = if is_unspecified(&me.laddr) {
            NET.lock().source(Family::Inet6, &faddr, zone)?
        } else {
            (me.laddr, me.zone)
        };
        if self
            .lookup6(&faddr, dst.port, &laddr, me.lport, false)
            .is_some()
            && me.lport != 0
        {
            return Err(Errno::EADDRINUSE);
        }
        let lport = if me.lport == 0 {
            self.ephemeral(id, &laddr, Some(&target), true, cred)?
        } else {
            me.lport
        };
        let p = self.pcbs.get_mut(&id).ok_or(Errno::EINVAL)?;
        if p.lport == 0 {
            p.flags |= inp::ANONPORT;
        }
        p.laddr = laddr;
        p.zone = lzone.max(zone);
        p.faddr = faddr;
        p.fport = dst.port;
        p.lport = lport;
        p.hashed = true;
        p.flags |= inp::INHASHLIST;
        Ok(target)
    }

    /// `in_pcbdisconnect`, `in6_pcbdisconnect`: out of the lookups, its
    /// addresses the wildcard (it keeps its port).
    pub(crate) fn disconnect(&mut self, id: u64) {
        if let Some(p) = self.pcbs.get_mut(&id) {
            p.hashed = false;
            p.flags &= !inp::INHASHLIST;
            p.laddr = ANY;
            p.faddr = ANY;
            p.fport = 0;
        }
    }

    /// The source a send from `id` to `dst` uses, and its port: the
    /// bound address, or the route's (`in_pcbladdr`, `in6_selectsrc`).
    pub(crate) fn source_for(&self, id: u64, dst: &Sockaddr) -> Result<Sockaddr, Errno> {
        let p = self.pcbs.get(&id).ok_or(Errno::EINVAL)?;
        match dst.family {
            Family::Inet => {
                let a = if p.laddr4() == [0; 4] {
                    v4_of(&NET.lock().source(Family::Inet, &dst.addr, 0)?.0)
                } else {
                    // A send from a bound address still needs a route.
                    let _ = NET.lock().lookup(Family::Inet, &dst.addr)?;
                    p.laddr4()
                };
                Ok(Sockaddr::v4(a, p.lport))
            }
            Family::Inet6 => {
                let (a, z) = if is_unspecified(&p.laddr) {
                    NET.lock().source(Family::Inet6, &dst.addr, dst.scope)?
                } else {
                    let n = NET.lock();
                    if !crate::addr::is_linklocal6(&dst.addr) {
                        let _ = n.lookup(Family::Inet6, &dst.addr)?;
                    }
                    (p.laddr, p.zone)
                };
                Ok(Sockaddr::v6(a, p.lport, z))
            }
        }
    }

    /// `so`'s name: its family's form of the bound address (an
    /// `AF_INET6` socket's IPv4 one mapped).
    pub(crate) fn sockname(&self, so: &Socket) -> Sockaddr {
        let Some(p) = self.pcbs.get(&so.id) else {
            return blank(so.family);
        };
        name_of(so.family, p, &p.laddr, p.lport, p.zone)
    }

    /// `so`'s peer's name.
    pub(crate) fn peername(&self, so: &Socket) -> Result<Sockaddr, Errno> {
        let p = self.pcbs.get(&so.id).ok_or(Errno::ENOTCONN)?;
        Ok(name_of(so.family, p, &p.faddr, p.fport, p.zone))
    }

    /// The peer's name, the wildcard if none.
    pub(crate) fn peer_any(&self, so: &Socket) -> Sockaddr {
        self.peername(so).unwrap_or_else(|_| blank(so.family))
    }
}

/// The wildcard name of `family`.
fn blank(family: Family) -> Sockaddr {
    match family {
        Family::Inet => Sockaddr::v4([0; 4], 0),
        Family::Inet6 => Sockaddr::v6(ANY, 0, 0),
    }
}

/// `a:port` as a socket of `family` names it.
fn name_of(family: Family, p: &Pcb, a: &In6, port: u16, zone: u32) -> Sockaddr {
    match family {
        Family::Inet => Sockaddr::v4(v4_of(a), port),
        Family::Inet6 if p.vflag & INP_IPV6 == 0 && p.vflag & INP_IPV4 != 0 => {
            // `in6_mapped_sockaddr`.
            Sockaddr::v4(v4_of(a), port).mapped()
        }
        Family::Inet6 => Sockaddr::v6(
            *a,
            port,
            if crate::addr::is_linklocal6(a) {
                zone
            } else {
                0
            },
        ),
    }
}

/// The `so_options` of a PCB's socket (none once it is gone, a
/// `TIME_WAIT` one's).
fn so_options_of(p: &Pcb) -> i32 {
    p.socket().map_or(0, |s| s.so_options())
}

/// A reserved port without `PRIV_NETINET_RESERVEDPORT` is `EACCES`
/// (`net.inet.ip.portrange.reservedlow` to `reservedhigh`).
fn reserved(lport: u16, cred: &Cred) -> Result<(), Errno> {
    use core::sync::atomic::Ordering::Relaxed;
    let p = i32::from(lport);
    if p <= tune::RESERVEDHIGH.load(Relaxed)
        && p >= tune::RESERVEDLOW.load(Relaxed)
        && !cred.reservedport
    {
        return Err(Errno::EACCES);
    }
    Ok(())
}

/// Sets or clears `inp_flags` bits.
pub(crate) fn set_flag(proto: Proto, id: u64, bit: i32, on: bool) {
    if let Some(p) = table(proto).lock().get_mut(id) {
        if on {
            p.flags |= bit;
        } else {
            p.flags &= !bit;
        }
    }
}
