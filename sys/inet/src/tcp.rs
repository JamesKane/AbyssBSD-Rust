// SPDX-License-Identifier: BSD-2-Clause
//! TCP sockets on the wire (docs/NETWORKING.md §9.2): the socket layer of
//! FreeBSD's TCP around `sys/tcp`'s protocol.
//!
//! A socket's connection is a [`::tcp::Tcpcb`] kept with the socket's
//! state, under its lock (FreeBSD's INP lock); its buffers are mbuf
//! chains the protocol reads and appends to through [`::tcp::Sock`].
//! What a call into the protocol sends is put on the wire once the lock
//! is let go, with the IP header the connection's addresses and options
//! give it. Arriving segments find their connection in [`TCB`] (or a
//! listener, whose half-open connections the VNET's SYN cache holds);
//! a closed port answers with a reset.
//!
//! A socket closed while its connection still runs (`SS_NOFDREF`) stays
//! in its VNET's orphans until the connection ends (`INP_SOCKREF`), so
//! TIME_WAIT is a socket like any other.
//!
//! Timers: every connection's earliest deadline is in one queue, which
//! the kernel's protocol timer thread runs ([`run_timers`]), with each
//! VNET's SYN cache and host cache.
//!
//! Behavior reference: sys/netinet/tcp_usrreq.c, sys/netinet/tcp_input.c
//! (`tcp_input_with_port`: the lookup, the listener, `tcp_dropwithreset`),
//! sys/netinet/tcp_output.c and tcp_subr.c (the IP headers,
//! `tcpip_fillheaders`), sys/netinet/tcp_syncache.c (`syncache_socket`),
//! sys/kern/uipc_socket.c (`solisten_proto`, `sonewconn`, `soabort`,
//! `soreceive_generic`'s out-of-band mark, `sosend_generic`).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use ::tcp::cc::Algo;
use ::tcp::hdr::{Accept, Opts, Th, th};
use ::tcp::hostcache::HostCache;
use ::tcp::isn::Secrets;
use ::tcp::syncache::{Expand, Listener, Reply, SynCache};
use ::tcp::{Chain, Conf, Ctx, Fate, Inc, Path, Seg, Segment, Sock, Stats, Tcpcb, TwVerdict, tf};
use abi::errno::Errno;
use abi::inet::{INP_IPV4, INP_IPV6, inp, ss};
use abi::socket::{SHUT_RD, SHUT_RDWR, SHUT_WR, SO_ACCEPTCONN, SO_LINGER, SO_OOBINLINE};
use mbuf::{Packet, csum};
use net::Vnet;
use sync::SpinLock;

use crate::addr::{self, is_broadcast4, is_multicast4, is_multicast6, is_v4mapped, v4_of};
use crate::ip::{self, Ip};
use crate::pcb::{Pcb, TCB};
use crate::{Cred, Family, Inner, Proto, Socket, cksum, state, tune};

pub use ::tcp::State as Tstate;

/// A VNET's TCP (`V_tcbinfo`'s neighbors): tunables, statistics, the
/// host cache, the SYN cache, the secrets, and the closed sockets whose
/// connections still run.
#[derive(Debug)]
pub struct Vtcp {
    pub conf: Conf,
    pub stats: Stats,
    pub hc: HostCache,
    pub syncache: SynCache,
    pub(crate) secrets: sync::Once<Secrets>,
    /// The ISN offset (`V_isn_offset` and its clock).
    pub(crate) isn: ::tcp::isn::Offset,
    /// `INP_SOCKREF`: sockets the connection holds after their close.
    orphans: SpinLock<BTreeMap<u64, Arc<Socket>>>,
    /// `net.inet.tcp.cc.algorithm` (an [`Algo`] index).
    pub default_cc: AtomicU8,
    /// When the host cache is next purged (ns).
    hc_purge_at: AtomicU64,
}

impl Default for Vtcp {
    fn default() -> Self {
        Self::new()
    }
}

impl Vtcp {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            conf: Conf::new(),
            stats: Stats::new(),
            hc: HostCache::new(),
            syncache: SynCache::new(),
            secrets: sync::Once::new(),
            isn: ::tcp::isn::Offset::new(),
            orphans: SpinLock::new(BTreeMap::new()),
            default_cc: AtomicU8::new(1),
            hc_purge_at: AtomicU64::new(0),
        }
    }

    /// The secrets, made from the random source on first use.
    fn secrets(&self) -> Secrets {
        if let Some(k) = self.secrets.get() {
            return *k;
        }
        let mut k = Secrets::default();
        for b in k.isn.chunks_mut(4).chain(k.ts.chunks_mut(4)) {
            b.copy_from_slice(&crate::random().to_ne_bytes());
        }
        // A racing first use may set them first: theirs stand.
        let _ = self.secrets.set(k);
        self.secrets.get().copied().unwrap_or(k)
    }

    /// The algorithm new connections use.
    #[must_use]
    pub fn cc_algo(&self) -> Algo {
        // Relaxed: a setting.
        Algo::ALL[usize::from(self.default_cc.load(Ordering::Relaxed)).min(Algo::ALL.len() - 1)]
    }

    /// Connections whose sockets were closed (`net.inet.tcp.pcblist`'s
    /// unowned ones).
    #[must_use]
    pub fn orphan_count(&self) -> usize {
        self.orphans.lock().len()
    }
}

/// `sonewconn`'s queue of a listener: complete connections not yet
/// accepted, and the backlog.
#[derive(Debug)]
pub(crate) struct Listen {
    pub(crate) comp: VecDeque<Arc<Socket>>,
    pub(crate) qlimit: u32,
}

impl Listen {
    /// `sonewconn`'s limit: past half again the backlog, no more.
    fn over(&self) -> bool {
        self.comp.len() > 3 * self.qlimit as usize / 2
    }
}

/// `TCPOOB_HAVEDATA`, `TCPOOB_HADDATA` with the byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Oob {
    None,
    Have(u8),
    Had,
}

/// What the socket keeps of its connection besides the control block.
#[derive(Debug)]
pub(crate) struct Side {
    /// `SS_NOFDREF`: its descriptor is closed.
    nofdref: bool,
    oob: Oob,
    /// `so_oobmark`: bytes before the urgent mark; `SBS_RCVATMARK`.
    oobmark: usize,
    atmark: bool,
    /// The route's limits when the connection began (`inp_route`).
    path: Path,
    /// The IPv6 flow label (`inp_flow`).
    flow: u32,
    /// Readers or writers to wake once the lock goes.
    wake: bool,
    /// Whole TLS records to open once the lock goes.
    tls: bool,
}

/// A TCP socket's protocol state (`struct tcpcb` and what of `struct
/// socket` it drives).
#[derive(Debug)]
pub(crate) struct Tcb {
    pub(crate) tp: Tcpcb,
    pub(crate) listen: Option<Listen>,
    /// `INP_DROPPED`: the connection is over; it connects no more.
    pub(crate) dropped: bool,
    pub(crate) side: Side,
    vnet: Arc<Vnet>,
}

impl Tcb {
    pub(crate) fn new(vnet: &Arc<Vnet>, family: Family) -> Self {
        let st = state::of(vnet);
        let inc = Inc {
            v6: family == Family::Inet6,
            ..Inc::default()
        };
        let tp = Tcpcb::new(inc, st.tcp.cc_algo(), &st.tcp.conf, ms(crate::now()));
        // `tcp_usr_attach`.
        st.tcp.stats.state_inc(Tstate::Closed);
        Self {
            tp,
            listen: None,
            dropped: false,
            side: Side {
                nofdref: false,
                oob: Oob::None,
                oobmark: 0,
                atmark: false,
                path: Path::default(),
                flow: 0,
                wake: false,
                tls: false,
            },
            vnet: Arc::clone(vnet),
        }
    }

    /// The connection's state.
    pub(crate) fn state(&self) -> Tstate {
        self.tp.state
    }
}

impl Drop for Tcb {
    /// `tcp_discardcb`: what the connection learned goes to the host
    /// cache.
    fn drop(&mut self) {
        let st = state::of(&self.vnet);
        let mut out = Vec::new();
        let path = |_: &Inc| Path::default();
        let secrets = Secrets::default();
        let cx = Ctx {
            now: ms(crate::now()),
            conf: &st.tcp.conf,
            stats: &st.tcp.stats,
            random: crate::random,
            out: &mut out,
            hc: &st.tcp.hc,
            path: &path,
            secrets: &secrets,
            isn: &st.tcp.isn,
        };
        self.tp.free_sackholes(&st.tcp.stats);
        self.tp.discard(&cx, &st.tcp.hc, 0);
    }
}

/// The protocols' clock (ns) in TCP's ticks of a millisecond.
fn ms(ns: u64) -> u64 {
    ns / 1_000_000
}

/// The socket as `sys/tcp` sees it: its buffers and state, locked.
struct View<'a> {
    i: &'a mut Inner,
    side: &'a mut Side,
    options: i32,
}

impl Sock for View<'_> {
    fn snd_cc(&self) -> u32 {
        self.i.snd.cc
    }
    fn snd_hiwat(&self) -> u32 {
        self.i.snd.hiwat
    }
    fn snd_autosize(&self) -> bool {
        self.i.snd.auto
    }
    fn snd_reserve(&mut self, hiwat: u32) -> bool {
        self.i.snd.reserve(hiwat)
    }
    fn snd_autosize_off(&mut self) {
        self.i.snd.auto = false;
    }
    fn snd_copy(&mut self, off: u32, len: u32) -> Option<Chain> {
        self.i.sndq.copy_seq(off as usize, len as usize).ok()
    }
    fn snd_drop(&mut self, n: u32) {
        let n = n.min(self.i.snd.cc);
        self.i.sndq.trim_front(n as usize);
        self.i.snd.cc -= n;
        self.i.snd.mbcnt = self.i.sndq.cost();
        self.side.wake = true;
    }
    fn cantsendmore(&self) -> bool {
        self.i.snd.cant
    }
    fn rcv_cc(&self) -> u32 {
        self.i.rcv.cc + self.i.tls_pending()
    }
    fn rcv_hiwat(&self) -> u32 {
        self.i.rcv.hiwat
    }
    fn rcv_space(&self) -> i64 {
        let r = &self.i.rcv;
        let cc = i64::from(r.cc) + i64::from(self.i.tls_pending());
        (i64::from(r.hiwat) - cc).min(i64::from(r.mbmax) - i64::from(r.mbcnt))
    }
    fn rcv_mbcnt(&self) -> u32 {
        self.i.rcv.mbcnt
    }
    fn rcv_mbmax(&self) -> u32 {
        self.i.rcv.mbmax
    }
    fn rcv_autosize(&self) -> bool {
        self.i.rcv.auto
    }
    fn rcv_reserve(&mut self, hiwat: u32) -> bool {
        self.i.rcv.reserve(hiwat)
    }
    fn rcv_autosize_off(&mut self) {
        self.i.rcv.auto = false;
    }
    fn rcv_append(&mut self, data: Chain) {
        if self.i.rcv.cant {
            return;
        }
        if self.i.tls_rx.is_some() {
            self.side.tls |= self.i.tls_append(data);
            return;
        }
        self.i.rcv.cc += u32::try_from(data.len()).unwrap_or(u32::MAX);
        self.i.stream.append(data);
        self.i.rcv.mbcnt = self.i.stream.cost();
        self.side.wake = true;
    }
    fn cantrcvmore(&mut self) {
        self.i.rcv.cant = true;
        self.side.tls |= self.i.tls_check_rx();
        self.side.wake = true;
    }
    fn rcv_closed(&self) -> bool {
        self.i.rcv.cant
    }
    fn options(&self) -> i32 {
        self.options
    }
    fn nofdref(&self) -> bool {
        self.side.nofdref
    }
    fn connected(&mut self) {
        self.i.state = (self.i.state & !(ss::ISCONNECTING | ss::ISDISCONNECTING)) | ss::ISCONNECTED;
        self.side.wake = true;
    }
    fn disconnecting(&mut self) {
        self.i.state = (self.i.state & !ss::ISCONNECTING) | ss::ISDISCONNECTING;
        self.i.rcv.cant = true;
        self.i.snd.cant = true;
        self.side.tls |= self.i.tls_check_rx();
        self.side.wake = true;
    }
    fn disconnected(&mut self) {
        self.i.state = (self.i.state & !(ss::ISCONNECTING | ss::ISCONNECTED | ss::ISDISCONNECTING))
            | ss::ISDISCONNECTED;
        self.i.rcv.cant = true;
        self.i.snd.cant = true;
        self.side.tls |= self.i.tls_check_rx();
        // `sbdrop` of the send buffer.
        self.i.sndq = Chain::default();
        self.i.snd.cc = 0;
        self.i.snd.mbcnt = 0;
        self.side.wake = true;
    }
    fn set_error(&mut self, e: Option<Errno>) {
        self.i.error = e;
        self.side.wake = true;
    }
    fn urgent(&mut self, oobmark: u32) {
        self.side.oobmark = oobmark as usize;
        self.side.atmark = oobmark == 0;
        self.side.wake = true;
    }
    fn oob_byte(&mut self, c: u8) {
        self.side.oob = Oob::Have(c);
    }
    fn wake_writers(&mut self) {
        self.side.wake = true;
    }
}

/// A segment waiting to go, with what its IP header takes.
#[derive(Debug)]
pub(crate) struct Tx {
    inc: Inc,
    info: TxInfo,
    seg: Seg,
}

/// What the IP header of a connection's segments takes from its socket.
#[derive(Clone, Copy, Debug)]
struct TxInfo {
    ttl: u8,
    tos: u8,
    hops: i16,
    tclass: i16,
    flow: u32,
    dontroute: bool,
    fib: u16,
}

impl TxInfo {
    fn of(i: &Inner, options: i32, flow: u32) -> Self {
        Self {
            ttl: i.ip.ttl,
            tos: i.ip.tos,
            hops: i.ip.hops,
            tclass: i.ip.tclass,
            flow,
            dontroute: options & abi::socket::SO_DONTROUTE != 0,
            fib: 0,
        }
    }

    /// A segment sent for no socket (a reset): the defaults.
    fn default_for() -> Self {
        Self {
            ttl: u8::try_from(tune::IP_TTL.load(Ordering::Relaxed)).unwrap_or(64),
            tos: 0,
            hops: -1,
            tclass: -1,
            flow: 0,
            dontroute: false,
            fib: 0,
        }
    }
}

// --- Addresses ---

/// A PCB's endpoints as `sys/tcp` keeps them (IPv4 in the first four
/// bytes).
fn inc_of(p: &Pcb) -> Inc {
    let v6 = p.vflag & INP_IPV6 != 0;
    let conv = |a: &[u8; 16]| {
        if v6 {
            *a
        } else {
            let mut b = [0u8; 16];
            b[..4].copy_from_slice(&v4_of(a));
            b
        }
    };
    Inc {
        laddr: conv(&p.laddr),
        faddr: conv(&p.faddr),
        lport: p.lport,
        fport: p.fport,
        v6,
        zone: p.zone,
        fib: 0,
    }
}

fn a4(a: &[u8; 16]) -> [u8; 4] {
    [a[0], a[1], a[2], a[3]]
}

fn wide4(a: [u8; 4]) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&a);
    b
}

/// `tcp_maxmtu`, `tcp_maxmtu6`, `in_localaddr`, `in_localip`: what the
/// route to `inc`'s peer allows.
fn path_for(vnet: &Vnet, inc: &Inc) -> Path {
    let n = vnet.net.lock();
    let (family, dst) = if inc.v6 {
        (Family::Inet6, inc.faddr)
    } else {
        (Family::Inet, addr::from_v4(a4(&inc.faddr)))
    };
    let route = if inc.v6 {
        n.route6(0, &dst, u16::try_from(inc.zone).unwrap_or(0))
    } else {
        n.route_in(0, family, &dst)
    };
    let Some(r) = route else {
        return Path::default();
    };
    let ifmtu = n.by_index(r.ifindex).map_or(0, |i| i.mtu);
    let maxmtu = if r.mtu != 0 { r.mtu.min(ifmtu) } else { ifmtu };
    let localip = n.is_local(family, &dst, inc.zone);
    Path {
        maxmtu,
        local: matches!(r.gateway, net::ifnet::Gateway::Link(_)) || localip,
        localip,
        tso: None,
    }
}

// --- Output ---

fn transmit_one(vnet: &Arc<Vnet>, inc: &Inc, tx: &TxInfo, s: Seg) -> Result<(), Errno> {
    let mut m = s.pkt;
    let tlen = m.len();
    if inc.v6 {
        let (src, dst) = (inc.laddr, inc.faddr);
        let p = cksum::fold(crate::ip6::pseudo(&src, &dst, ip::proto::TCP, tlen));
        m.data
            .copy_in(16, &p.to_be_bytes())
            .map_err(|_| Errno::ENOBUFS)?;
        m.hdr.csum_flags = csum::IP6_TCP;
        m.hdr.csum_data = 16;
        if s.tso_segsz != 0 {
            m.hdr.csum_flags |= csum::IP6_TSO;
            m.hdr.tso_segsz = s.tso_segsz;
        }
        let hlim = if tx.hops >= 0 {
            u8::try_from(tx.hops).unwrap_or(64)
        } else {
            crate::ip6::DEFHLIM
        };
        let tclass = if tx.tclass >= 0 {
            u8::try_from(tx.tclass).unwrap_or(0)
        } else {
            0
        };
        let plen = u16::try_from(tlen).map_err(|_| Errno::EMSGSIZE)?;
        let mut h = crate::ip6::Ip6::new(
            ip::proto::TCP,
            src,
            dst,
            plen,
            hlim,
            (tclass & !::tcp::ecn::MASK) | s.ecn,
        );
        h.flow |= tx.flow & ::tcp::FLOWLABEL_MASK;
        m.prepend(crate::ip6::HDR_LEN)
            .map_err(|_| Errno::ENOBUFS)?
            .copy_from_slice(&h.bytes());
        let zone = u16::try_from(inc.zone).unwrap_or(0);
        let mut o = crate::ip6::Opts6 {
            flags: 0,
            mcast: None,
            fib: tx.fib,
            pcp: None,
            flowid: 0,
            hlim: None,
            tclass: None,
            ifindex: 0,
            nexthop: None,
            zone,
            src_zone: zone,
            router_alert: false,
            minmtu: -1,
        };
        if ip::hash_outbound(vnet) {
            o.flowid = crate::udp::flow_hash6(&src, &dst, inc.lport, inc.fport, ip::proto::TCP);
        }
        crate::ip6::output(vnet, m, &o)
    } else {
        let (s4, d4) = (a4(&inc.laddr), a4(&inc.faddr));
        let p = cksum::fold(cksum::pseudo4(s4, d4, ip::proto::TCP, tlen));
        m.data
            .copy_in(16, &p.to_be_bytes())
            .map_err(|_| Errno::ENOBUFS)?;
        m.hdr.csum_flags = csum::IP_TCP;
        m.hdr.csum_data = 16;
        if s.tso_segsz != 0 {
            m.hdr.csum_flags |= csum::IP_TSO;
            m.hdr.tso_segsz = s.tso_segsz;
        }
        let len = u16::try_from(tlen + ip::HDR_LEN).map_err(|_| Errno::EMSGSIZE)?;
        let mut h = Ip::new(
            ip::proto::TCP,
            s4,
            d4,
            len,
            tx.ttl,
            (tx.tos & !::tcp::ecn::MASK) | s.ecn,
        );
        if s.df {
            h.off = ip::DF;
        }
        m.prepend(ip::HDR_LEN)
            .map_err(|_| Errno::ENOBUFS)?
            .copy_from_slice(&h.bytes());
        let mut o = ip::Opts {
            flags: 0,
            mcast: None,
            fib: tx.fib,
            pcp: None,
            router_alert: false,
            flowid: 0,
        };
        if tx.dontroute {
            o.flags |= ip::out::ROUTETOIF;
        }
        if ip::hash_outbound(vnet) {
            o.flowid = ip::flow_hash(s4, d4, inc.lport, inc.fport, ip::proto::TCP);
        }
        ip::output(vnet, m, &o)
    }
}

/// The SYN cache's segments.
fn transmit_replies(vnet: &Arc<Vnet>, rs: Vec<Reply>) {
    for r in rs {
        let tx = TxInfo {
            flow: r.flowlabel,
            ..TxInfo::default_for()
        };
        let _ = transmit_one(vnet, &r.inc, &tx, r.seg);
    }
}

// --- Timers ---

/// Every connection's next deadline (ns), by time and by socket.
#[derive(Debug)]
struct TimerQ {
    by_time: BTreeMap<(u64, u64), Weak<Socket>>,
    by_id: BTreeMap<u64, u64>,
}

static TIMERS: SpinLock<TimerQ> = SpinLock::new(TimerQ {
    by_time: BTreeMap::new(),
    by_id: BTreeMap::new(),
});

/// The earliest TCP deadline (ns), for the timer thread.
static NEXT: AtomicU64 = AtomicU64::new(u64::MAX);

/// Sets `so`'s timer to `at` (TCP's ticks), or clears it.
fn schedule(so: &Arc<Socket>, at: Option<u64>) {
    let want = at.map_or(u64::MAX, |t| t.saturating_mul(1_000_000));
    // Relaxed: the queue's lock orders the entry; an unchanged deadline
    // (often: many segments a tick) leaves it be.
    if so.timer_at.load(Ordering::Relaxed) == want {
        return;
    }
    let mut q = TIMERS.lock();
    if let Some(old) = q.by_id.remove(&so.id) {
        q.by_time.remove(&(old, so.id));
    }
    so.timer_at.store(want, Ordering::Relaxed);
    if at.is_some() {
        let ns = want;
        q.by_id.insert(so.id, ns);
        q.by_time.insert((ns, so.id), Arc::downgrade(so));
        drop(q);
        // AcqRel: an earlier deadline wins, and the thread sees it.
        if NEXT.fetch_min(ns, Ordering::AcqRel) > ns {
            crate::timer_kick();
        }
    }
}

/// When TCP's timers (connections, SYN caches, host caches) next need
/// to run (ns), `u64::MAX` for never.
#[must_use]
pub(crate) fn next_deadline() -> u64 {
    // Acquire: pairs with `schedule`.
    NEXT.load(Ordering::Acquire)
}

/// `tcp_timer_enter` for every connection due at `now` (ns), each VNET's
/// `syncache_timer` and host cache purge; returns the next deadline.
pub(crate) fn run_timers(now: u64) -> u64 {
    // Release: a deadline set while these run lowers it again.
    NEXT.store(u64::MAX, Ordering::Release);
    loop {
        let due = {
            let mut q = TIMERS.lock();
            let Some((&(t, id), _)) = q.by_time.iter().next() else {
                break;
            };
            if t > now {
                break;
            }
            q.by_id.remove(&id);
            // Taken out under the lock, and dropped after it (a last
            // reference's drop may schedule).
            let so = q.by_time.remove(&(t, id)).and_then(|w| w.upgrade());
            if let Some(so) = &so {
                // Relaxed: under the queue's lock, as in `schedule`.
                so.timer_at.store(u64::MAX, Ordering::Relaxed);
            }
            so
        };
        if let Some(so) = due {
            so.tcp_timers();
        }
    }
    let mut next = TIMERS
        .lock()
        .by_time
        .keys()
        .next()
        .map_or(u64::MAX, |&(t, _)| t);
    for v in Vnet::all() {
        if v.inet.get().is_none() {
            continue;
        }
        let st = state::of(&v);
        let replies = {
            let secrets = st.tcp.secrets();
            let path = |inc: &Inc| path_for(&v, inc);
            let mut out = Vec::new();
            let cx = Ctx {
                now: ms(now),
                conf: &st.tcp.conf,
                stats: &st.tcp.stats,
                random: crate::random,
                out: &mut out,
                hc: &st.tcp.hc,
                path: &path,
                secrets: &secrets,
                isn: &st.tcp.isn,
            };
            if st
                .tcp
                .syncache
                .next_deadline()
                .is_some_and(|d| d <= ms(now))
            {
                st.tcp.syncache.timer(&cx)
            } else {
                Vec::new()
            }
        };
        transmit_replies(&v, replies);
        if let Some(d) = st.tcp.syncache.next_deadline() {
            next = next.min(d.saturating_mul(1_000_000));
        }
        // `tcp_hc_purge`, every `prune` seconds.
        // Relaxed: the timer thread alone moves it.
        let at = st.tcp.hc_purge_at.load(Ordering::Relaxed);
        let prune = u64::try_from(st.tcp.hc.prune.load(Ordering::Relaxed)).unwrap_or(300);
        if at == 0 || at <= now {
            if at != 0 {
                st.tcp.hc.purge(false);
            }
            st.tcp
                .hc_purge_at
                .store(now + prune * 1_000_000_000, Ordering::Relaxed);
        }
        if st.tcp.hc.count() > 0 {
            next = next.min(st.tcp.hc_purge_at.load(Ordering::Relaxed));
        }
    }
    // AcqRel: as in `schedule`.
    NEXT.fetch_min(next, Ordering::AcqRel);
    next_deadline()
}

/// The SYN cache's timer is due sooner than the thread knows.
fn syncache_armed(vnet: &Vnet) {
    let st = state::of(vnet);
    if let Some(d) = st.tcp.syncache.next_deadline() {
        let ns = d.saturating_mul(1_000_000);
        if NEXT.fetch_min(ns, Ordering::AcqRel) > ns {
            crate::timer_kick();
        }
    }
}

// --- Running the protocol ---

/// What a call into a connection left to do once the lock is gone.
/// `sosend_generic`'s checks before a round, and `tcp_usr_send`'s of a
/// named peer: the peer to connect to first, if any.
fn send_check<'a>(
    i: &mut Inner,
    to: Option<(&'a [u8], &'a Cred)>,
) -> Result<Option<(&'a [u8], &'a Cred)>, Errno> {
    if i.snd.cant {
        return Err(Errno::EPIPE);
    }
    if let Some(e) = i.error.take() {
        return Err(e);
    }
    if i.state & ss::ISCONNECTED == 0 && to.is_none() {
        return Err(Errno::ENOTCONN);
    }
    // `tcp_usr_send`: a peer named before SYN_SENT is connected to.
    match (to, i.tcp.as_ref()) {
        (Some(_), Some(t)) if t.dropped => Err(Errno::ECONNRESET),
        (Some((raw, cred)), Some(t)) if t.tp.state < ::tcp::State::SynSent => {
            if t.listen.is_some() {
                return Err(Errno::EINVAL);
            }
            Ok(Some((raw, cred)))
        }
        _ => Ok(None),
    }
}

/// How much of `len` bytes the send buffer takes now.
///
/// # Errors
///
/// `EAGAIN` with not all the room and less than `least`.
fn send_room(i: &Inner, len: usize, least: usize) -> Result<usize, Errno> {
    let room = i.snd.space() as usize;
    if room < len && room < least.max(1) {
        return Err(Errno::EAGAIN);
    }
    Ok(len.min(room))
}

struct Done {
    fate: Fate,
    deadline: Option<u64>,
    wake: bool,
    tls: bool,
    nofdref: bool,
}

impl Socket {
    /// Runs `f` on the connection with its socket locked, then sends what
    /// it sent, wakes whoever waits, ends the connection if it ended,
    /// and sets its timer. `None` if it has no connection.
    fn run_tcp<R>(
        self: &Arc<Self>,
        path: Option<Path>,
        f: impl FnOnce(&mut Tcpcb, &mut View, &mut Ctx) -> (R, Fate),
    ) -> Option<R> {
        let st = state::of(&self.vnet);
        let options = self.so_options();
        let secrets = st.tcp.secrets();
        let mut out = Vec::new();
        let (r, done) = {
            let mut i = self.inner.lock();
            let mut tcb = i.tcp.take()?;
            let tx = TxInfo::of(&i, options, tcb.side.flow);
            let p = path.unwrap_or(tcb.side.path);
            let pathf = move |_: &Inc| p;
            let Tcb { tp, side, .. } = &mut *tcb;
            let (r, fate) = {
                let mut v = View {
                    i: &mut i,
                    side,
                    options,
                };
                let mut cx = Ctx {
                    now: ms(crate::now()),
                    conf: &st.tcp.conf,
                    stats: &st.tcp.stats,
                    random: crate::random,
                    out: &mut out,
                    hc: &st.tcp.hc,
                    path: &pathf,
                    secrets: &secrets,
                    isn: &st.tcp.isn,
                };
                f(tp, &mut v, &mut cx)
            };
            if fate == Fate::Closed {
                tcb.dropped = true;
            }
            // In order: queued under the lock, sent after it.
            let inc = tcb.tp.inc;
            i.txq
                .extend(out.drain(..).map(|seg| Tx { inc, info: tx, seg }));
            let done = Done {
                fate,
                deadline: if fate == Fate::Closed {
                    None
                } else {
                    tcb.tp.next_deadline()
                },
                wake: core::mem::take(&mut tcb.side.wake),
                tls: core::mem::take(&mut tcb.side.tls),
                nofdref: tcb.side.nofdref,
            };
            i.tcp = Some(tcb);
            (r, done)
        };
        self.finish(done);
        Some(r)
    }

    /// After a call: segments out, wakeups, the end, the timer.
    fn finish(self: &Arc<Self>, d: Done) {
        self.tcp_send_queued();
        if d.tls {
            crate::tls::enqueue(self);
        }
        if d.wake {
            self.wake();
        }
        if d.fate == Fate::Closed {
            self.tcp_ended(d.nofdref);
        } else {
            schedule(self, d.deadline);
        }
    }

    /// Sends the queued segments in order. One caller at a time does; a
    /// caller that finds another at it (perhaps itself, further up the
    /// stack, as a looped segment's answer comes back) leaves its own to
    /// it.
    fn tcp_send_queued(self: &Arc<Self>) {
        loop {
            if self.txbusy.swap(true, Ordering::Acquire) {
                return;
            }
            // All that is queued, under one hold of the lock.
            let batch = core::mem::take(&mut self.inner.lock().txq);
            for t in batch {
                let _ = transmit_one(&self.vnet, &t.inc, &t.info, t.seg);
            }
            // Release: what was sent before is seen by the next sender.
            self.txbusy.store(false, Ordering::Release);
            if self.inner.lock().txq.is_empty() {
                return;
            }
        }
    }

    /// `tcp_close`'s `in_pcbdrop`, and `INP_SOCKREF`'s release: out of
    /// the lookups; freed if its socket was closed.
    fn tcp_ended(self: &Arc<Self>, nofdref: bool) {
        schedule(self, None);
        {
            let mut t = TCB.lock();
            if nofdref {
                t.free(self.id);
            } else {
                t.drop_hash(self.id);
            }
        }
        let st = state::of(&self.vnet);
        let gone = st.tcp.orphans.lock().remove(&self.id);
        drop(gone);
    }

    /// `tcp_output`, for an option that lets held data go.
    pub(crate) fn tcp_output(self: &Arc<Self>) {
        let _ = self.run_tcp(None, |tp, so, cx| ((), tp.output(so, cx)));
    }

    /// `tcp_drop` for `e`.
    pub(crate) fn tcp_drop_for(self: &Arc<Self>, e: Errno) {
        let _ = self.run_tcp(None, |tp, so, cx| ((), tp.drop(so, cx, e)));
    }

    /// The connection's timers are due.
    fn tcp_timers(self: &Arc<Self>) {
        let _ = self.run_tcp(None, |tp, so, cx| ((), tp.timers(so, cx)));
    }

    /// The connection's state.
    pub fn tcp_state(&self) -> Tstate {
        self.inner
            .lock()
            .tcp
            .as_ref()
            .map_or(Tstate::Closed, |t| t.state())
    }

    /// `tcp_usr_listen` and `tcp6_usr_listen` with `solisten_proto`:
    /// binds an ephemeral port if unbound; a backlog out of range is
    /// `somaxconn`. Listening again sets the backlog anew.
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` for UDP; `EINVAL` connected, connecting, or reset;
    /// the implicit bind's.
    pub fn listen(&self, backlog: i32, cred: &Cred) -> Result<(), Errno> {
        if self.proto != Proto::Tcp {
            return Err(Errno::EOPNOTSUPP);
        }
        {
            let i = self.inner.lock();
            if i.state & (ss::ISCONNECTED | ss::ISCONNECTING | ss::ISDISCONNECTING) != 0
                || i.tcp.as_ref().is_some_and(|t| t.dropped)
            {
                return Err(Errno::EINVAL);
            }
            // `solisten_proto_check`.
            if i.tls_tx.is_some() || i.tls_rx.is_some() {
                return Err(Errno::EINVAL);
            }
        }
        {
            let mut t = TCB.lock();
            if t.get(self.id).is_some_and(|p| p.lport == 0) {
                t.bind_anon(self, cred)?;
            }
        }
        let max = tune::SOMAXCONN.load(Ordering::Relaxed);
        let backlog = if backlog < 0 || backlog > max {
            max
        } else {
            backlog
        };
        let st = state::of(&self.vnet);
        let mut i = self.inner.lock();
        let t = i.tcp.as_mut().ok_or(Errno::EINVAL)?;
        if t.tp.state != Tstate::Listen {
            t.tp.listen(&st.tcp.stats);
        }
        match &mut t.listen {
            Some(l) => l.qlimit = backlog.cast_unsigned(),
            None => {
                t.listen = Some(Listen {
                    comp: VecDeque::new(),
                    qlimit: backlog.cast_unsigned(),
                });
            }
        }
        self.options.fetch_or(SO_ACCEPTCONN, Ordering::Relaxed);
        Ok(())
    }

    /// `tcp_usr_connect` and `tcp6_usr_connect`: the SYN goes; a blocking
    /// caller then waits with [`Socket::connect_done`]; `nonblock`
    /// returns `EINPROGRESS`.
    pub(crate) fn tcp_connect(
        self: &Arc<Self>,
        raw: &[u8],
        cred: &Cred,
        nonblock: bool,
    ) -> Result<(), Errno> {
        {
            let mut i = self.inner.lock();
            let t = i.tcp.as_ref().ok_or(Errno::EINVAL)?;
            if t.listen.is_some() {
                return Err(Errno::EOPNOTSUPP);
            }
            // `kern_connectat` and `soconnectat`, then `tcp_usr_connect`'s
            // `INP_DROPPED`, then `tcp_connect`'s state check.
            if i.state & ss::ISCONNECTING != 0 {
                return Err(Errno::EALREADY);
            }
            if i.state & (ss::ISCONNECTED | ss::ISDISCONNECTING) != 0 {
                return Err(Errno::EISCONN);
            }
            if t.dropped {
                return Err(Errno::ECONNREFUSED);
            }
            if i.state & ss::ISDISCONNECTED != 0 {
                return Err(Errno::EISCONN);
            }
            i.error = None;
        }
        self.tcp_start(raw, cred, false)?;
        if nonblock {
            return Err(Errno::EINPROGRESS);
        }
        // An answer that came at once (a reset over loopback) is the
        // connect's; otherwise the caller waits with `connect_done`.
        match self.connect_done() {
            Err(Errno::EAGAIN) => Ok(()),
            r => r,
        }
    }

    /// `tcp_connect` and `tcp6_connect`, after `tcp_usr_connect`'s (or
    /// `tcp_usr_send`'s) checks of the peer's address `raw`: the
    /// connection bound to it, SYN_SENT, and the SYN. `implied`:
    /// `tcp_usr_send`'s, the data already queued.
    fn tcp_start(self: &Arc<Self>, raw: &[u8], cred: &Cred, implied: bool) -> Result<(), Errno> {
        let sa = match self.family {
            Family::Inet => addr::parse_in(raw, false)?,
            Family::Inet6 => addr::parse_in6(raw)?,
        };
        match self.family {
            Family::Inet => {
                let a = v4_of(&sa.addr);
                if is_multicast4(&a) {
                    return Err(Errno::EAFNOSUPPORT);
                }
                if is_broadcast4(&a) {
                    return Err(Errno::EACCES);
                }
            }
            Family::Inet6 => {
                if is_multicast6(&sa.addr) {
                    return Err(Errno::EAFNOSUPPORT);
                }
                if is_v4mapped(&sa.addr) {
                    let a = v4_of(&sa.addr);
                    if is_multicast4(&a) {
                        return Err(Errno::EAFNOSUPPORT);
                    }
                    if is_broadcast4(&a) {
                        return Err(Errno::EACCES);
                    }
                }
            }
        }
        let (inc, flow) = {
            let mut t = TCB.lock();
            t.connect(self, &sa, cred)?;
            let p = t.get(self.id).ok_or(Errno::EINVAL)?;
            (inc_of(p), p.flow)
        };
        let path = path_for(&self.vnet, &inc);
        {
            let mut i = self.inner.lock();
            i.state = (i.state & !ss::ISDISCONNECTED) | ss::ISCONNECTING;
            if let Some(t) = i.tcp.as_mut() {
                t.tp.inc = inc;
                t.side.path = path;
                t.side.flow = flow;
            }
        }
        let st = state::of(&self.vnet);
        let secrets = st.tcp.secrets();
        let iss = secrets.new_isn(&inc, ms(crate::now()), &st.tcp.isn, crate::random());
        let off = secrets.new_ts_offset(&inc, ::tcp::on(&st.tcp.conf.ts_offset_per_conn));
        self.run_tcp(Some(path), |tp, so, cx| {
            let f = if implied {
                tp.connect_implied(so, cx, iss, off)
            } else {
                tp.connect(so, cx, iss, off)
            };
            ((), f)
        });
        Ok(())
    }

    /// Where a connect stands, for a caller waiting on it.
    ///
    /// # Errors
    ///
    /// Its error if it failed (taken); `EWOULDBLOCK` while it waits.
    pub fn connect_done(&self) -> Result<(), Errno> {
        let mut i = self.inner.lock();
        if i.state & ss::ISCONNECTED != 0 {
            return Ok(());
        }
        if let Some(e) = i.error.take() {
            return Err(e);
        }
        if i.state & ss::ISCONNECTING != 0 {
            return Err(Errno::EAGAIN);
        }
        Err(Errno::ECONNREFUSED)
    }

    /// How long a connect may wait, ns: no limit of its own; the
    /// connection's `TCP_KEEPINIT` timer ends it.
    pub fn connect_timeout(&self) -> u64 {
        0
    }

    /// A connect whose wait ended without a result: nothing more to do
    /// (the connection's timer ends it).
    pub fn connect_timed_out(self: &Arc<Self>) {}

    /// `solisten_dequeue`: the next connection.
    ///
    /// # Errors
    ///
    /// `EINVAL` if not listening; its error, once; `EWOULDBLOCK` with
    /// none queued.
    pub fn accept(self: &Arc<Self>) -> Result<Arc<Self>, Errno> {
        let mut i = self.inner.lock();
        if i.tcp.as_ref().is_none_or(|t| t.listen.is_none()) {
            return Err(Errno::EINVAL);
        }
        if let Some(e) = i.error.take() {
            return Err(e);
        }
        let q = i
            .tcp
            .as_mut()
            .and_then(|t| t.listen.as_mut())
            .ok_or(Errno::EINVAL)?;
        q.comp.pop_front().ok_or(Errno::EAGAIN)
    }

    /// `soabort`: a queued connection the listener let go of; it resets.
    fn abort(self: &Arc<Self>) {
        {
            let mut i = self.inner.lock();
            if let Some(t) = i.tcp.as_mut() {
                t.side.nofdref = true;
            }
        }
        let r = self.run_tcp(None, |tp, so, cx| ((), tp.abort(so, cx)));
        if r.is_none() {
            TCB.lock().free(self.id);
        }
    }

    /// `tcp_usr_shutdown`.
    pub(crate) fn tcp_shutdown(self: &Arc<Self>, how: i32) -> Result<(), Errno> {
        {
            let mut i = self.inner.lock();
            if i.tcp.as_ref().is_some_and(|t| t.listen.is_some()) {
                if how != SHUT_WR {
                    i.error = Some(Errno::ECONNABORTED);
                    drop(i);
                    self.rd.notify(0);
                }
                return Err(Errno::ENOTCONN);
            }
            if i.state & (ss::ISCONNECTED | ss::ISCONNECTING | ss::ISDISCONNECTING) == 0 {
                return Err(Errno::ENOTCONN);
            }
            if how == SHUT_RD || how == SHUT_RDWR {
                // `sorflush`.
                i.rcv.cant = true;
                i.rcv_flush();
            }
        }
        if how == SHUT_WR || how == SHUT_RDWR {
            let dropped = self.inner.lock().tcp.as_ref().is_none_or(|t| t.dropped);
            if dropped {
                self.wake();
                return Err(Errno::ECONNRESET);
            }
            self.run_tcp(None, |tp, so, cx| {
                so.i.snd.cant = true;
                ((), tp.shutdown(so, cx))
            });
        }
        self.wake();
        Ok(())
    }

    /// `tcp_usr_close` with `soclose`: a listener's queue is aborted and
    /// its half-open connections dropped; a connection closes in order
    /// (unread data flushed, a FIN after what is queued) or, with
    /// `SO_LINGER` of zero, resets. The socket stays with its connection
    /// until that ends.
    pub(crate) fn tcp_close(self: &Arc<Self>) {
        let linger0 = self.so_options() & SO_LINGER != 0 && self.inner.lock().linger == 0;
        let (listen, dropped) = {
            let mut i = self.inner.lock();
            let Some(t) = i.tcp.as_mut() else {
                return;
            };
            t.side.nofdref = true;
            (t.listen.take(), t.dropped)
        };
        let st = state::of(&self.vnet);
        if let Some(q) = listen {
            st.tcp.syncache.drop_listener(&st.tcp.stats, self.id);
            let _ = self.run_tcp(None, |tp, so, cx| ((), tp.close(so, cx)));
            TCB.lock().free(self.id);
            for s in q.comp {
                s.abort();
            }
            return;
        }
        if dropped {
            schedule(self, None);
            TCB.lock().free(self.id);
            return;
        }
        // `sbflush` of what was not read.
        self.inner.lock().rcv_flush();
        // Held until the connection ends (`INP_SOCKREF`).
        st.tcp.orphans.lock().insert(self.id, Arc::clone(self));
        let r = self.run_tcp(None, |tp, so, cx| ((), tp.usr_close(so, cx, linger0)));
        if r.is_none() {
            let gone = st.tcp.orphans.lock().remove(&self.id);
            drop(gone);
            TCB.lock().free(self.id);
        }
    }

    /// `sosend_generic` with `tcp_usr_send`, one round: as much of `data`
    /// as the send buffer takes now. `oob` marks its last byte urgent
    /// (`MSG_OOB`). Returns the bytes taken.
    ///
    /// # Errors
    ///
    /// `EPIPE` once it cannot send; its pending error; `ENOTCONN`
    /// unconnected; `EWOULDBLOCK` with less room than the low water mark.
    pub fn send_stream(self: &Arc<Self>, data: &[u8], oob: bool) -> Result<usize, Errno> {
        self.send_stream_to(data, oob, None, None)
    }

    /// [`Socket::send_stream`] naming the peer (`sendto`): an unconnected
    /// socket connects to `to` first, its data going after the handshake
    /// (TCP's `PR_IMPLOPCL`); a connecting or connected one sends as if
    /// unnamed.
    ///
    /// # Errors
    ///
    /// As [`Socket::send_stream`], but not `ENOTCONN`; `EINVAL` listening;
    /// as `connect(2)` for `to` (the data is then not sent).
    pub fn send_stream_named(
        self: &Arc<Self>,
        data: &[u8],
        oob: bool,
        to: &[u8],
        cred: &Cred,
    ) -> Result<usize, Errno> {
        self.send_stream_to(data, oob, None, Some((to, cred)))
    }

    /// [`Socket::send_stream`]; with `spliced`, a splice's send, which
    /// takes whatever fits, its low water mark aside, and counts what it
    /// took in `spliced`.
    #[inline]
    pub(crate) fn send_stream_to(
        self: &Arc<Self>,
        data: &[u8],
        oob: bool,
        spliced: Option<&core::sync::atomic::AtomicU64>,
        to: Option<(&[u8], &Cred)>,
    ) -> Result<usize, Errno> {
        let (n, implied) = {
            let mut i = self.inner.lock();
            if i.tls_tx.is_some() {
                drop(i);
                return self.send_tls(data, abi::ktls::TLS_RLTYPE_APP, false, oob, to);
            }
            let implied = send_check(&mut i, to)?;
            if data.is_empty() && implied.is_none() {
                return Ok(0);
            }
            let least = if spliced.is_some() {
                1
            } else {
                i.snd.lowat as usize
            };
            let n = send_room(&i, data.len(), least)?;
            if n > 0 {
                let c = Chain::from_slice(&data[..n]).map_err(|_| Errno::ENOBUFS)?;
                i.sndq.append(c);
                i.snd.cc += u32::try_from(n).unwrap_or(u32::MAX);
                i.snd.mbcnt = i.sndq.cost();
            }
            (n, implied)
        };
        self.send_queued(n, implied, spliced, oob && n == data.len())
    }

    /// [`Socket::send_stream`] of mbufs the caller filled before taking any
    /// lock (`sosend_generic`'s `m_uiotombuf`): as much of `data`'s front
    /// as the buffer takes now goes to the send buffer as it is; the rest
    /// stays in `data`. A sending TLS session takes a copy of the bytes.
    ///
    /// # Errors
    ///
    /// As [`Socket::send_stream`].
    pub fn send_stream_chain(
        self: &Arc<Self>,
        data: &mut Chain,
        oob: bool,
    ) -> Result<usize, Errno> {
        let (n, implied) = {
            let mut i = self.inner.lock();
            if i.tls_tx.is_some() {
                drop(i);
                let mut b = vec![0u8; data.len()];
                data.copy_out(0, &mut b);
                let n = self.send_tls(&b, abi::ktls::TLS_RLTYPE_APP, false, oob, None)?;
                data.trim_front(n);
                return Ok(n);
            }
            let implied = send_check(&mut i, None)?;
            if data.is_empty() && implied.is_none() {
                return Ok(0);
            }
            let n = send_room(&i, data.len(), i.snd.lowat as usize)?;
            if n > 0 {
                let c = data.take_front(n).map_err(|_| Errno::ENOBUFS)?;
                i.sndq.append(c);
                i.snd.cc += u32::try_from(n).unwrap_or(u32::MAX);
                i.snd.mbcnt = i.sndq.cost();
            }
            (n, implied)
        };
        self.send_queued(n, implied, None, oob && data.is_empty())
    }

    /// `sosend_generic` with a sending TLS session, one round: as much of
    /// `data` as the buffer takes now, or with `atomic` all of it or none,
    /// in records of type `rtype`. Returns the bytes taken.
    ///
    /// # Errors
    ///
    /// As [`Socket::send_stream`]; `EINVAL` with no session, or nothing to
    /// send where an empty record is not allowed.
    pub fn send_tls(
        self: &Arc<Self>,
        data: &[u8],
        rtype: u8,
        atomic: bool,
        oob: bool,
        to: Option<(&[u8], &Cred)>,
    ) -> Result<usize, Errno> {
        let (n, implied, layout) = {
            let mut i = self.inner.lock();
            let layout = i.tls_tx.ok_or(Errno::EINVAL)?;
            if data.is_empty() && !layout.permit_empty() {
                return Err(Errno::EINVAL);
            }
            let implied = send_check(&mut i, to)?;
            let least = if atomic {
                data.len()
            } else {
                i.snd.lowat as usize
            };
            (send_room(&i, data.len(), least)?, implied, layout)
        };
        if n > 0 || data.is_empty() {
            self.tls_frame(&data[..n], rtype, layout)?;
        }
        self.send_queued(n, implied, None, oob && n == data.len())
    }

    /// What `sosend_generic` does after a round is in the buffer: connects
    /// if `implied` (the data then waits for the handshake), or sends.
    fn send_queued(
        self: &Arc<Self>,
        n: usize,
        implied: Option<(&[u8], &Cred)>,
        spliced: Option<&core::sync::atomic::AtomicU64>,
        urgent: bool,
    ) -> Result<usize, Errno> {
        if let Some((raw, cred)) = implied {
            if let Err(e) = self.tcp_start(raw, cred, true) {
                // `sbflush`.
                let mut i = self.inner.lock();
                i.sndq = Chain::default();
                i.snd.cc = 0;
                i.snd.mbcnt = 0;
                return Err(e);
            }
            // The SYN went; the data waits for the handshake.
            return Ok(n);
        }
        if let Some(c) = spliced {
            // Relaxed: counted before the receiver can see it.
            c.fetch_add(n as u64, Ordering::Relaxed);
        }
        self.run_tcp(None, |tp, so, cx| {
            let f = if urgent {
                tp.send_oob(so, cx)
            } else {
                tp.send(so, cx, false, false)
            };
            ((), f)
        });
        Ok(n)
    }

    /// How much a send may copy in at once: the send buffer's room.
    pub fn send_window(&self) -> usize {
        (self.inner.lock().snd.space() as usize).max(1)
    }

    /// `sendfile_wait_generic`: whether `sendfile` may queue more of the
    /// `need` bytes it has left, raising an automatic send low-water mark
    /// first (to half the buffer, and at least a page) so a nonblocking
    /// sender is woken for big pieces. `Some(room)` to go on; `None` to
    /// wait for room.
    ///
    /// # Errors
    ///
    /// `EPIPE` after the send side shut; the pending error (taken);
    /// `ENOTCONN` unconnected; `EAGAIN` instead of waiting when
    /// `nonblock`.
    pub fn sendfile_room(&self, need: usize, nonblock: bool) -> Result<Option<usize>, Errno> {
        const PAGE: u32 = 4096;
        let mut i = self.inner.lock();
        if i.snd.autolowat {
            i.snd.lowat = i.snd.lowat.max(i.snd.hiwat / 2);
            if i.snd.lowat < PAGE && i.snd.hiwat >= PAGE {
                i.snd.lowat = PAGE;
            }
        }
        if i.snd.cant {
            return Err(Errno::EPIPE);
        }
        if let Some(e) = i.error.take() {
            return Err(e);
        }
        if i.state & ss::ISCONNECTED == 0 {
            return Err(Errno::ENOTCONN);
        }
        let room = i.snd.space() as usize;
        if room < need && (room == 0 || room < i.snd.lowat as usize) {
            return if nonblock {
                Err(Errno::EAGAIN)
            } else {
                Ok(None)
            };
        }
        Ok(Some(room))
    }

    /// `tcp_usr_send` of a chain `sendfile` built (a header, file pages):
    /// queued whole, then sent as the connection allows.
    ///
    /// # Errors
    ///
    /// `ECONNRESET` if the connection was dropped.
    pub fn send_chain(self: &Arc<Self>, c: Chain) -> Result<(), Errno> {
        let tls = {
            let i = self.inner.lock();
            if i.tcp.as_ref().is_some_and(|t| t.dropped) {
                return Err(Errno::ECONNRESET);
            }
            i.tls_tx
        };
        if let Some(layout) = tls {
            // Into new buffers: the pages of a file stay as they are.
            if !c.is_empty() {
                let mut b = vec![0; c.len()];
                c.copy_out(0, &mut b);
                self.tls_frame(&b, abi::ktls::TLS_RLTYPE_APP, layout)?;
            }
        } else {
            let mut i = self.inner.lock();
            i.snd.cc = i
                .snd
                .cc
                .saturating_add(u32::try_from(c.len()).unwrap_or(u32::MAX));
            i.sndq.append(c);
            i.snd.mbcnt = i.sndq.cost();
        }
        self.run_tcp(None, |tp, so, cx| ((), tp.send(so, cx, false, false)));
        Ok(())
    }

    /// `soreceive_generic` on a stream, one look: up to `max` bytes,
    /// stopping at the urgent mark. With `oob`, the urgent byte
    /// (`tcp_usr_rcvoob`).
    ///
    /// # Errors
    ///
    /// With nothing to read: its pending error (taken unless `peek`),
    /// `ENOTCONN` if never connected, else `EWOULDBLOCK`; at the end of
    /// the stream, an empty result. `MSG_OOB`: `EINVAL` with no urgent
    /// data to read, `EWOULDBLOCK` before it arrives.
    pub fn recv_stream(
        self: &Arc<Self>,
        max: usize,
        peek: bool,
        oob: bool,
    ) -> Result<crate::Received, Errno> {
        let mut out = crate::Received::default();
        if oob {
            let mut i = self.inner.lock();
            let inline = self.so_options() & SO_OOBINLINE != 0;
            let t = i.tcp.as_mut().ok_or(Errno::EINVAL)?;
            let s = &mut t.side;
            if (s.oobmark == 0 && !s.atmark) || inline || s.oob == Oob::Had {
                return Err(Errno::EINVAL);
            }
            let Oob::Have(b) = s.oob else {
                return Err(Errno::EAGAIN);
            };
            if !peek {
                s.oob = Oob::Had;
                t.tp.oobflags = ::tcp::oob::HADDATA;
            }
            out.data.push(b);
            out.flags = abi::socket::MSG_OOB;
            return Ok(out);
        }
        self.recv_data(max, peek, false, false, |_| true)
            .map(Option::unwrap_or_default)
    }

    /// [`Socket::recv_stream`] of in-band data, which comes as the socket
    /// buffer's own mbufs (`soreceive_stream`'s `sbcut`, or by reference
    /// with `peek`) for the caller to copy out once the lock is let go;
    /// a receiving TLS session's records come in `data`.
    ///
    /// # Errors
    ///
    /// As [`Socket::recv_stream`].
    pub fn recv_stream_mbufs(
        self: &Arc<Self>,
        max: usize,
        peek: bool,
    ) -> Result<crate::Received, Errno> {
        self.recv_data(max, peek, false, true, |_| true)
            .map(Option::unwrap_or_default)
    }

    /// [`Socket::recv_stream`] of data with `MSG_TLSAPPDATA`: on a socket
    /// with a receiving TLS session, stops before an alert record.
    ///
    /// # Errors
    ///
    /// As [`Socket::recv_stream`]; `ENXIO` at an alert.
    pub fn recv_tls_app(
        self: &Arc<Self>,
        max: usize,
        peek: bool,
    ) -> Result<crate::Received, Errno> {
        self.recv_data(max, peek, true, false, |_| true)
            .map(Option::unwrap_or_default)
    }

    /// A splice's receive: [`Socket::recv_stream`] of up to `max` bytes,
    /// if `still` holds of the socket's state (`None` if not).
    pub(crate) fn recv_stream_if(
        self: &Arc<Self>,
        max: usize,
        still: impl FnOnce(&Inner) -> bool,
    ) -> Result<Option<crate::Received>, Errno> {
        self.recv_data(max, false, false, false, still)
    }

    /// `soreceive_generic`'s data path, if `still` holds under the lock;
    /// what is read opens the window (`tcp_usr_rcvd`). With `mbufs`, the
    /// data comes as mbufs in `chain`.
    #[inline]
    fn recv_data(
        self: &Arc<Self>,
        max: usize,
        peek: bool,
        appdata: bool,
        mbufs: bool,
        still: impl FnOnce(&Inner) -> bool,
    ) -> Result<Option<crate::Received>, Errno> {
        let mut out = crate::Received::default();
        {
            let mut i = self.inner.lock();
            if !still(&i) {
                return Ok(None);
            }
            if Self::tls_rx_on(&i) {
                out = Self::tls_recv(&mut i, max, peek, appdata)?;
                let read = !peek && !out.data.is_empty();
                drop(i);
                if read {
                    self.tcp_rcvd();
                }
                return Ok(Some(out));
            }
            if i.stream.is_empty() {
                if let Some(e) = i.error {
                    if !peek {
                        i.error = None;
                    }
                    return Err(e);
                }
                if i.rcv.cant {
                    return Ok(Some(out));
                }
                if i.state & (ss::ISCONNECTED | ss::ISCONNECTING) == 0 {
                    return Err(Errno::ENOTCONN);
                }
                if max == 0 {
                    return Ok(Some(out));
                }
                return Err(Errno::EAGAIN);
            }
            let mark = i.tcp.as_ref().map_or(0, |t| t.side.oobmark);
            let limit = if mark > 0 { mark } else { usize::MAX };
            let n = max.min(limit).min(i.stream.len());
            if mbufs {
                if peek {
                    out.chain = i.stream.copy(0, n)?;
                    return Ok(Some(out));
                }
                out.chain = i.stream.take_front(n)?;
            } else {
                out.data = vec![0u8; n];
                i.stream.copy_out(0, &mut out.data);
                if peek {
                    return Ok(Some(out));
                }
                i.stream.trim_front(n);
            }
            i.rcv.cc -= u32::try_from(n).unwrap_or(0);
            i.rcv.mbcnt = i.stream.cost();
            if let Some(t) = i.tcp.as_mut() {
                let s = &mut t.side;
                s.atmark = false;
                if s.oobmark > 0 {
                    s.oobmark -= n;
                    if s.oobmark == 0 {
                        s.atmark = true;
                    }
                }
            }
        }
        self.tcp_rcvd();
        Ok(Some(out))
    }

    /// `tcp_usr_rcvd`.
    fn tcp_rcvd(self: &Arc<Self>) {
        self.run_tcp(None, |tp, so, cx| {
            if tp.state.have_established() {
                ((), tp.rcvd(so, cx))
            } else {
                ((), Fate::Alive)
            }
        });
    }

    /// `SIOCATMARK`.
    pub fn at_mark(&self) -> bool {
        self.inner
            .lock()
            .tcp
            .as_ref()
            .is_some_and(|t| t.side.atmark)
    }

    /// Bytes before the urgent mark (`so_oobmark`).
    pub fn oobmark(&self) -> usize {
        self.inner.lock().tcp.as_ref().map_or(0, |t| t.side.oobmark)
    }
}

/// Whether urgent data is ahead or reading is at its mark (`so_oobmark`
/// or `SBS_RCVATMARK`).
pub(crate) fn urgent_pending(i: &Inner) -> bool {
    i.tcp
        .as_ref()
        .is_some_and(|t| t.side.oobmark > 0 || t.side.atmark)
}

/// What `net.inet.tcp.pcblist` shows of a connection.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Snapshot {
    pub(crate) state: i32,
    pub(crate) flags: u32,
    pub(crate) maxseg: u32,
    pub(crate) rcv_wnd: u32,
    pub(crate) snd_wnd: u32,
    pub(crate) snd_cwnd: u32,
    pub(crate) snd_ssthresh: u32,
    /// Ticks since it last received.
    pub(crate) idle: u64,
    pub(crate) sndzerowin: u32,
    pub(crate) sndrexmitpack: u32,
    pub(crate) rcvoopack: u32,
    /// The timers' ticks left, 0 if stopped: rexmt, persist, keep, 2msl,
    /// delack.
    pub(crate) timers: [u64; 5],
    pub(crate) cc: &'static str,
    pub(crate) ecn: u32,
    pub(crate) dsack_bytes: u32,
    pub(crate) dsack_pack: u32,
}

pub(crate) fn snapshot(so: &Socket) -> Snapshot {
    let i = so.inner.lock();
    let Some(t) = i.tcp.as_ref() else {
        return Snapshot::default();
    };
    let tp = &t.tp;
    let now = ms(crate::now());
    let left = |w: ::tcp::timer::Tt| {
        tp.timer_deadline(w)
            .map_or(0, |d| d.saturating_sub(now).max(1))
    };
    use ::tcp::timer::Tt;
    Snapshot {
        state: tp.state.number(),
        flags: tp.flags,
        maxseg: tp.t_maxseg,
        rcv_wnd: tp.rcv_wnd,
        snd_wnd: tp.snd_wnd,
        snd_cwnd: tp.snd_cwnd,
        snd_ssthresh: tp.snd_ssthresh,
        idle: now.saturating_sub(tp.rcvtime),
        sndzerowin: tp.sndzerowin,
        sndrexmitpack: tp.sndrexmitpack,
        rcvoopack: tp.rcvoopack,
        timers: [
            left(Tt::Rexmt),
            left(Tt::Persist),
            left(Tt::Keep),
            left(Tt::TwoMsl),
            left(Tt::Delack),
        ],
        cc: tp.cc.algo().name(),
        ecn: u32::from(tp.flags2 & ::tcp::tf2::ECN_PERMIT != 0),
        dsack_bytes: tp.dsack_bytes,
        dsack_pack: tp.dsack_pack,
    }
}

// --- Input ---

/// A segment that arrived, its addresses in `sys/tcp`'s form.
struct Arrival {
    family: Family,
    /// The local (destination) and foreign (source) addresses.
    local: [u8; 16],
    foreign: [u8; 16],
    zone: u32,
    iptos: u8,
    /// `M_BCAST` or `M_MCAST`, or a multicast or broadcast address.
    multi: bool,
}

/// `tcp_input` for IPv4: `m` holds the IP header (`ip`) and the segment.
pub(crate) fn input4(vnet: &Arc<Vnet>, mut m: Packet, ip: &Ip) {
    let st = state::of(vnet);
    let hlen = usize::from(ip.hl);
    let tlen = usize::from(ip.len).saturating_sub(hlen);
    st.tcp.stats.inc(::tcp::tcps::RCVTOTAL);
    if tlen < ::tcp::hdr::HDR_LEN || m.len() < hlen + tlen {
        st.tcp.stats.inc(::tcp::tcps::RCVSHORT);
        return;
    }
    if m.len() > hlen + tlen {
        let _ = m.data.split_off(hlen + tlen);
    }
    // The checksum, by the device or here.
    let f = m.hdr.csum_flags;
    let bad = if f & csum::DATA_VALID != 0 {
        #[allow(clippy::cast_possible_truncation, reason = "a 16-bit sum")]
        let s = if f & csum::PSEUDO_HDR != 0 {
            m.hdr.csum_data as u16
        } else {
            cksum::fold(
                cksum::pseudo4(ip.src, ip.dst, ip::proto::TCP, tlen) + u64::from(m.hdr.csum_data),
            )
        };
        s ^ 0xffff != 0
    } else if f & csum::IP_TCP != 0 {
        false
    } else {
        let s = cksum::pseudo4(ip.src, ip.dst, ip::proto::TCP, tlen)
            + cksum::sum_chain(&m.data, hlen, tlen);
        !cksum::fold(s) != 0
    };
    if bad {
        st.tcp.stats.inc(::tcp::tcps::RCVBADSUM);
        return;
    }
    let multi = m.hdr.flags & (mbuf::flags::BCAST | mbuf::flags::MCAST) != 0
        || is_multicast4(&ip.dst)
        || is_multicast4(&ip.src)
        || ip.src == [0xff; 4];
    m.data.trim_front(hlen);
    let a = Arrival {
        family: Family::Inet,
        local: wide4(ip.dst),
        foreign: wide4(ip.src),
        zone: 0,
        iptos: ip.tos,
        multi,
    };
    input(vnet, st, m, &a);
}

/// `tcp6_input`: the segment at `off` in `m`, IPv6 header `h`.
pub(crate) fn input6(vnet: &Arc<Vnet>, mut m: Packet, off: usize, h: &crate::ip6::Ip6) {
    let st = state::of(vnet);
    st.tcp.stats.inc(::tcp::tcps::RCVTOTAL);
    let tlen = m.len().saturating_sub(off);
    if tlen < ::tcp::hdr::HDR_LEN {
        st.tcp.stats.inc(::tcp::tcps::RCVSHORT);
        return;
    }
    let f = m.hdr.csum_flags;
    let bad = if f & csum::DATA_VALID != 0 {
        #[allow(clippy::cast_possible_truncation, reason = "a 16-bit sum")]
        let s = if f & csum::PSEUDO_HDR != 0 {
            m.hdr.csum_data as u16
        } else {
            cksum::fold(
                crate::ip6::pseudo(&h.src, &h.dst, ip::proto::TCP, tlen)
                    + u64::from(m.hdr.csum_data),
            )
        };
        s ^ 0xffff != 0
    } else if f & csum::IP6_TCP != 0 {
        false
    } else {
        crate::ip6::cksum6(&m, ip::proto::TCP, off, tlen) != 0
    };
    if bad {
        st.tcp.stats.inc(::tcp::tcps::RCVBADSUM);
        return;
    }
    let multi = is_multicast6(&h.dst) || is_multicast6(&h.src);
    let zone = if crate::addr::is_linklocal6(&h.src) {
        u32::from(m.hdr.rcvif)
    } else {
        0
    };
    let iptos = ((h.flow >> 20) & 0xff) as u8;
    m.data.trim_front(off);
    let a = Arrival {
        family: Family::Inet6,
        local: h.dst,
        foreign: h.src,
        zone,
        iptos,
        multi,
    };
    input(vnet, st, m, &a);
}

/// `tcp_input_with_port` after IP: the segment (TCP header first) to its
/// connection, its listener, or a reset.
fn input(vnet: &Arc<Vnet>, st: &state::State, m: Packet, a: &Arrival) {
    let mut hb = [0u8; 60];
    let n = m.data.copy_out(0, &mut hb[..m.len().min(60)]);
    let Some(t) = Th::parse(&hb[..n]) else {
        st.tcp.stats.inc(::tcp::tcps::RCVBADOFF);
        return;
    };
    if t.off > m.len() {
        st.tcp.stats.inc(::tcp::tcps::RCVBADOFF);
        return;
    }
    let opts = hb[::tcp::hdr::HDR_LEN..t.off].to_vec();
    let mut data = m.data;
    data.trim_front(t.off);
    let tlen = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let inc = Inc {
        laddr: a.local,
        faddr: a.foreign,
        lport: t.dport,
        fport: t.sport,
        v6: a.family == Family::Inet6,
        zone: a.zone,
        fib: 0,
    };
    // Lookups, more than once after a TIME_WAIT gives way.
    for _ in 0..2 {
        let found = {
            let tb = TCB.lock();
            let p = match a.family {
                Family::Inet => tb.lookup4(
                    vnet.id,
                    a4(&a.foreign),
                    t.sport,
                    a4(&a.local),
                    t.dport,
                    true,
                ),
                Family::Inet6 => tb.lookup6(vnet.id, &a.foreign, t.sport, &a.local, t.dport, true),
            };
            p.and_then(|p| p.socket().map(|s| (s, p.flags & inp::DROPPED != 0)))
        };
        let Some((so, dropped)) = found else {
            closed_port(vnet, st, &inc, &t, tlen, a);
            return;
        };
        if dropped {
            closed_port(vnet, st, &inc, &t, tlen, a);
            return;
        }
        let listening = so
            .inner
            .lock()
            .tcp
            .as_ref()
            .is_some_and(|x| x.listen.is_some());
        if listening {
            listen_input(vnet, st, &so, &inc, &t, &opts, data, a);
            return;
        }
        let path = so
            .inner
            .lock()
            .tcp
            .as_ref()
            .map(|x| x.side.path)
            .unwrap_or_default();
        let mut seg = Some(Segment {
            th: t,
            opts: &opts,
            data: core::mem::take(&mut data),
            iptos: a.iptos,
            nsegs: 1,
        });
        let mut reset = false;
        let mut recycle = false;
        let ran = so.run_tcp(Some(path), |tp, sk, cx| match tp.state {
            Tstate::TimeWait => {
                let v = seg.as_ref().map(|s| tp.twcheck(sk, cx, s));
                if v == Some(TwVerdict::Recycle) {
                    recycle = true;
                    ((), Fate::Closed)
                } else {
                    ((), Fate::Alive)
                }
            }
            Tstate::Closed | Tstate::Listen => {
                reset = true;
                ((), Fate::Alive)
            }
            _ => match seg.take() {
                Some(s) => ((), tp.do_segment(sk, cx, s)),
                None => ((), Fate::Alive),
            },
        });
        if ran.is_none() || reset {
            closed_port(vnet, st, &inc, &t, tlen, a);
            return;
        }
        if !recycle {
            return;
        }
        // A new SYN ended the TIME_WAIT: it looks again, now for the
        // listener (`goto findpcb`).
        if let Some(s) = seg.take() {
            data = s.data;
        }
    }
}

/// `dropwithreset` for a segment no connection takes: a reset, unless
/// the segment is one or `net.inet.tcp.blackhole` says not to.
fn closed_port(vnet: &Arc<Vnet>, st: &state::State, inc: &Inc, t: &Th, tlen: u32, a: &Arrival) {
    let bh = ::tcp::get(&st.tcp.conf.blackhole);
    if (bh == 1 && t.has(th::SYN)) || bh > 1 {
        return;
    }
    if a.multi {
        return;
    }
    if !crate::icmp::bandlim(st, crate::state::bandlim::TCP_RST) {
        return;
    }
    let Some(r) = ::tcp::reset_for(t, tlen) else {
        return;
    };
    let Some(pkt) = ::tcp::build(r, &Opts::default(), None) else {
        return;
    };
    let seg = Seg {
        pkt,
        ecn: 0,
        df: false,
        tso_segsz: 0,
    };
    let _ = transmit_one(vnet, inc, &TxInfo::default_for(), seg);
}

/// A segment for a listener: the SYN cache's (a SYN, the handshake's
/// ACK, a reset), and a new connection when the handshake is done.
#[allow(clippy::too_many_arguments)]
fn listen_input(
    vnet: &Arc<Vnet>,
    st: &state::State,
    lso: &Arc<Socket>,
    inc: &Inc,
    t: &Th,
    opts: &[u8],
    data: Chain,
    a: &Arrival,
) {
    // The table's lock before the socket's, never with it.
    let autoflow = TCB
        .lock()
        .get(lso.id)
        .is_some_and(|p| p.flags & inp::IN6P_AUTOFLOWLABEL != 0);
    let (l, lflags, lkeep, algo, over, lrcv, lsnd, linger, lip) = {
        let i = lso.inner.lock();
        let Some(x) = i.tcp.as_ref() else {
            return;
        };
        let Some(q) = x.listen.as_ref() else {
            return;
        };
        (
            Listener {
                id: lso.id,
                rcv_hiwat: i.rcv.hiwat,
                noopt: x.tp.flags & tf::NOOPT != 0,
                autoflowlabel: autoflow,
            },
            x.tp.flags,
            [x.tp.keepinit, x.tp.keepidle, x.tp.keepintvl, x.tp.keepcnt],
            x.tp.cc.algo(),
            q.over(),
            i.rcv,
            i.snd,
            i.linger,
            i.ip,
        )
    };
    let path = path_for(vnet, inc);
    let secrets = st.tcp.secrets();
    let pathf = move |_: &Inc| path;
    let mut out = Vec::new();
    let mut replies = Vec::new();
    let cx = Ctx {
        now: ms(crate::now()),
        conf: &st.tcp.conf,
        stats: &st.tcp.stats,
        random: crate::random,
        out: &mut out,
        hc: &st.tcp.hc,
        path: &pathf,
        secrets: &secrets,
        isn: &st.tcp.isn,
    };
    let sack = ::tcp::on(&st.tcp.conf.sack_enable);
    let tlen = u32::try_from(data.len()).unwrap_or(u32::MAX);
    if t.flags & (th::RST | th::ACK | th::SYN) == th::ACK {
        let to = Opts::parse(opts, Accept { syn: false, sack });
        match st.tcp.syncache.expand(&cx, inc, &to, t, &l, &mut replies) {
            Expand::Socket(e) => {
                transmit_replies(vnet, replies);
                if over {
                    // `sonewconn` refused: no socket, a reset.
                    st.tcp.stats.inc(::tcp::tcps::LISTENDROP);
                    closed_port(vnet, st, inc, t, tlen, a);
                    return;
                }
                accept_new(
                    st,
                    lso,
                    &e,
                    path,
                    lflags,
                    lkeep,
                    algo,
                    (lrcv, lsnd, linger, lip),
                    t,
                    opts,
                    data,
                    a.iptos,
                );
            }
            Expand::Reset => {
                transmit_replies(vnet, replies);
                closed_port(vnet, st, inc, t, tlen, a);
            }
            Expand::Drop => {
                transmit_replies(vnet, replies);
            }
        }
        return;
    }
    if t.has(th::RST) {
        if let Some(r) = st.tcp.syncache.chkrst(&cx, inc, t) {
            replies.push(r);
        }
        transmit_replies(vnet, replies);
        return;
    }
    if !t.has(th::SYN) || t.has(th::ACK) {
        st.tcp.stats.inc(::tcp::tcps::BADSYN);
        if t.has(th::ACK) {
            closed_port(vnet, st, inc, t, tlen, a);
        }
        return;
    }
    if t.has(th::FIN) && ::tcp::on(&st.tcp.conf.drop_synfin) {
        st.tcp.stats.inc(::tcp::tcps::BADSYN);
        return;
    }
    // No connections from or to broadcast, multicast, or itself.
    if a.multi || (t.dport == t.sport && a.local == a.foreign) {
        return;
    }
    let to = Opts::parse(opts, Accept { syn: true, sack });
    if let Some(r) = st.tcp.syncache.add(&cx, inc, &to, t, a.iptos, &l) {
        replies.push(r);
    }
    transmit_replies(vnet, replies);
    syncache_armed(vnet);
}

/// `syncache_socket` and `solisten_enqueue`: the listener's new socket
/// for a completed handshake, which inherits its options, its first
/// segment, and its place on the accept queue.
#[allow(clippy::too_many_arguments)]
fn accept_new(
    st: &state::State,
    lso: &Arc<Socket>,
    e: &::tcp::syncache::Entry,
    path: Path,
    lflags: u32,
    lkeep: [u32; 4],
    algo: Algo,
    inherit: (crate::Sb, crate::Sb, i32, crate::IpOpts),
    t: &Th,
    opts: &[u8],
    data: Chain,
    iptos: u8,
) {
    let (mut rcv, mut snd, linger, ip) = inherit;
    rcv.cc = 0;
    rcv.mbcnt = 0;
    rcv.cant = false;
    snd.cc = 0;
    snd.mbcnt = 0;
    snd.cant = false;
    let (uid, pflags) = TCB.lock().get(lso.id).map_or((0, 0), |p| (p.uid, p.flags));
    let cred = Cred {
        uid,
        ..Cred::default()
    };
    let s = Socket::with_buffers(&lso.vnet, lso.family, Proto::Tcp, &cred, rcv, snd);
    s.options
        .store(lso.so_options() & !SO_ACCEPTCONN, Ordering::Relaxed);
    let v6 = e.inc.v6;
    {
        let mut tb = TCB.lock();
        if let Some(mut p) = tb.get_mut(s.id) {
            p.vflag = (p.vflag & !(INP_IPV4 | INP_IPV6)) | if v6 { INP_IPV6 } else { INP_IPV4 };
            p.flags =
                (pflags & !(inp::ANONPORT | inp::INHASHLIST | inp::DROPPED)) | inp::INHASHLIST;
            if v6 {
                p.laddr = e.inc.laddr;
                p.faddr = e.inc.faddr;
            } else {
                p.laddr = addr::from_v4(a4(&e.inc.laddr));
                p.faddr = addr::from_v4(a4(&e.inc.faddr));
            }
            p.lport = e.inc.lport;
            p.fport = e.inc.fport;
            p.zone = e.inc.zone;
            p.flow = e.flowlabel;
            p.hashed = true;
        }
    }
    {
        let mut i = s.inner.lock();
        i.linger = linger;
        i.ip = ip;
        // `solisten_enqueue(so, SS_ISCONNECTED)`.
        i.state = ss::ISCONNECTED;
        if let Some(x) = i.tcp.as_mut() {
            x.side.path = path;
            x.side.flow = e.flowlabel;
        }
    }
    let seg = Segment {
        th: *t,
        opts,
        data,
        iptos,
        nsegs: 1,
    };
    let e = *e;
    let made = s.run_tcp(Some(path), |tp, so, cx| {
        // The control block `tcp_usr_attach` made goes; the cache's
        // connection takes its place, already counted in SYN_RECEIVED.
        cx.stats.state_dec(Tstate::Closed);
        *tp = Tcpcb::from_syncache(&e, so, cx, algo, lflags, lkeep);
        let f = tp.do_segment(so, cx, seg);
        ((), f)
    });
    let _ = (made, st);
    let queued = {
        let mut li = lso.inner.lock();
        match li.tcp.as_mut().and_then(|x| x.listen.as_mut()) {
            Some(q) => {
                q.comp.push_back(Arc::clone(&s));
                true
            }
            None => false,
        }
    };
    if !queued {
        s.abort();
        return;
    }
    // `solisten_wakeup`.
    lso.rd.notify(0);
}

// --- ICMP errors ---

/// The connection (or the SYN cache's entry) an ICMP error is about,
/// told (`tcp_ctlinput`, `tcp6_ctlinput`).
fn ctlinput(vnet: &Arc<Vnet>, inc: &Inc, th_seq: u32, n: ::tcp::Notify) {
    let found = {
        let tb = TCB.lock();
        let p = if inc.v6 {
            tb.lookup6(vnet.id, &inc.faddr, inc.fport, &inc.laddr, inc.lport, false)
        } else {
            tb.lookup4(
                vnet.id,
                a4(&inc.faddr),
                inc.fport,
                a4(&inc.laddr),
                inc.lport,
                false,
            )
        };
        p.and_then(|p| p.socket().map(|s| (s, p.flags & inp::DROPPED != 0)))
    };
    match found {
        Some((so, false)) => {
            let _ = so.run_tcp(None, |tp, sk, cx| {
                if tp.state == Tstate::Listen {
                    return ((), Fate::Alive);
                }
                ((), tp.ctlinput(sk, cx, th_seq, n))
            });
        }
        Some((_, true)) => {}
        None => {
            let st = state::of(vnet);
            let secrets = st.tcp.secrets();
            let path = |_: &Inc| Path::default();
            let mut out = Vec::new();
            let cx = Ctx {
                now: ms(crate::now()),
                conf: &st.tcp.conf,
                stats: &st.tcp.stats,
                random: crate::random,
                out: &mut out,
                hc: &st.tcp.hc,
                path: &path,
                secrets: &secrets,
                isn: &st.tcp.isn,
            };
            st.tcp.syncache.unreach(&cx, inc, th_seq);
        }
    }
}

/// `tcp_ctlinput`: ICMP type `t`, code `c` about a segment this host
/// sent (inner header `oip`, the first 8 bytes of its TCP header in
/// `th8`), with the next-hop MTU of a "fragmentation needed".
pub(crate) fn ctlinput4(vnet: &Arc<Vnet>, t: u8, c: u8, oip: &Ip, th8: &[u8; 8], nextmtu: u16) {
    let Some(e) = crate::icmp::errmap(t, c) else {
        return;
    };
    let st = state::of(vnet);
    let may_rst = ::tcp::on(&st.tcp.conf.icmp_may_rst);
    let n = match e {
        Errno::EMSGSIZE => {
            // `tcp_next_pmtu`.
            let mut mtu = u32::from(nextmtu);
            if mtu == 0 {
                mtu = ::tcp::next_mtu_down(u32::from(oip.len));
            }
            let least = ::tcp::get(&st.tcp.conf.minmss).cast_unsigned() + 40;
            ::tcp::Notify::Mtu(mtu.max(least))
        }
        Errno::ECONNREFUSED if may_rst => ::tcp::Notify::DropSynSent(e),
        Errno::EHOSTUNREACH if may_rst && t == crate::icmp::ty::TIMXCEED => {
            ::tcp::Notify::DropSynSent(e)
        }
        _ => ::tcp::Notify::Soft(e),
    };
    let inc = Inc {
        laddr: wide4(oip.src),
        faddr: wide4(oip.dst),
        lport: u16::from_be_bytes([th8[0], th8[1]]),
        fport: u16::from_be_bytes([th8[2], th8[3]]),
        v6: false,
        zone: 0,
        fib: 0,
    };
    let th_seq = u32::from_be_bytes([th8[4], th8[5], th8[6], th8[7]]);
    ctlinput(vnet, &inc, th_seq, n);
}

/// `tcp6_ctlinput`: an ICMPv6 error about a segment this host sent.
pub(crate) fn ctlinput6(vnet: &Arc<Vnet>, m: &Packet, ctl: &crate::icmp6::Ctl, mtu: u32) {
    let Some(e) = crate::icmp6::errmap(ctl.t, ctl.c) else {
        return;
    };
    let mut th8 = [0u8; 8];
    if m.data.copy_out(ctl.off, &mut th8) != 8 {
        return;
    }
    let st = state::of(vnet);
    let may_rst = ::tcp::on(&st.tcp.conf.icmp_may_rst);
    let n = match e {
        Errno::EMSGSIZE => {
            if mtu < crate::ip6::MMTU {
                return;
            }
            ::tcp::Notify::Mtu(mtu)
        }
        Errno::ECONNREFUSED if may_rst => ::tcp::Notify::DropSynSent(e),
        Errno::EHOSTUNREACH if may_rst && ctl.t == crate::icmp6::ty::TIME_EXCEEDED => {
            ::tcp::Notify::DropSynSent(e)
        }
        _ => ::tcp::Notify::Soft(e),
    };
    let zone = if crate::addr::is_linklocal6(&ctl.dst) {
        u32::from(m.hdr.rcvif)
    } else {
        0
    };
    let inc = Inc {
        laddr: ctl.src,
        faddr: ctl.dst,
        lport: u16::from_be_bytes([th8[0], th8[1]]),
        fport: u16::from_be_bytes([th8[2], th8[3]]),
        v6: true,
        zone,
        fib: 0,
    };
    let th_seq = u32::from_be_bytes([th8[4], th8[5], th8[6], th8[7]]);
    ctlinput(vnet, &inc, th_seq, n);
}

/// `net.inet.tcp.hostcache.list` (`sysctl_tcp_hc_list`).
#[must_use]
pub fn hostcache_list(st: &state::State) -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::from(
        "\nIP address        MTU  SSTRESH      RTT   RTTVAR     CWND SENDPIPE RECVPIPE EXP\n",
    );
    // `msec(rtt * (RTM_RTTUNIT / (hz * TCP_RTT_SCALE)))`.
    let msec = |u: u64| (u + 500) / 1000;
    for e in st.tcp.hc.entries() {
        let a = if e.peer.v6 {
            crate::nd6::fmt6(&e.peer.addr)
        } else {
            net::addr::fmt4(a4(&e.peer.addr))
        };
        let _ = writeln!(
            s,
            "{a:<15} {:5} {:8} {:6}ms {:6}ms {:8} {:8} {:8} {:4}",
            e.m.mtu,
            e.m.ssthresh,
            msec(u64::from(e.m.rtt) * (1_000_000 / (1000 * 32))),
            msec(u64::from(e.m.rttvar) * (1_000_000 / (1000 * 16))),
            e.m.cwnd,
            e.m.sendpipe,
            e.m.recvpipe,
            e.expire
        );
    }
    s
}
