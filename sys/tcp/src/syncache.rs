// SPDX-License-Identifier: BSD-2-Clause
//! The SYN cache (`tcp_syncache.c`): a listener's half-open connections,
//! held apart from sockets until the handshake's last ACK, and SYN
//! cookies (`net.inet.tcp.syncookies`, on), which carry the entry in the
//! SYN-ACK's sequence number when the cache is full.
//!
//! The cache answers a SYN with a SYN-ACK, retransmits it with backoff
//! (three times, `net.inet.tcp.syncache.rexmtlimit`), and on the ACK
//! hands the entry back ([`Expand::Socket`]); the caller makes the
//! socket and its connection ([`Tcpcb::from_syncache`]). Segments it sends
//! come back as [`Reply`]s, addressed, for the caller to send.
//!
//! Behavior reference: sys/netinet/tcp_syncache.c, tcp_syncache.h.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use sync::SpinLock;

use crate::cc::Algo;
use crate::ecn::{self, scf as ecnf};
use crate::hdr::{MAX_WINSHIFT, MAXWIN, Opts, Th, th, tof};
use crate::isn::syncookie_mac;
use crate::seq::{self, Seq};
use crate::tcb::{Inc, Tcpcb, tf};
use crate::timer::{self, Tt};
use crate::{Ctx, Seg, Sock, State, build, get, on, tcps};

/// `TCP_SYNCACHE_HASHSIZE`, `TCP_SYNCACHE_BUCKETLIMIT`,
/// `SYNCACHE_MAXREXMTS`.
pub const HASHSIZE: usize = 512;
pub const BUCKETLIMIT: u32 = 30;
pub const MAXREXMTS: u32 = 3;
/// `SYNCOOKIE_LIFETIME` and `TCP_SYNCACHE_PAUSE_TIME`, seconds.
pub const SYNCOOKIE_LIFETIME: u64 = 15;
/// `TCP_SYNCACHE_MAX_BACKOFF`.
const MAX_BACKOFF: u32 = 6;

/// `SCF_*`.
pub mod scf {
    pub const NOOPT: u32 = 0x01;
    pub const WINSCALE: u32 = 0x02;
    pub const TIMESTAMP: u32 = 0x04;
    pub const UNREACH: u32 = 0x10;
    pub const SIGNATURE: u32 = 0x20;
    pub const SACK: u32 = 0x80;
}

/// `tcp_sc_msstab`, `tcp_sc_wstab`.
const MSSTAB: [u16; 8] = [216, 536, 1200, 1360, 1400, 1440, 1452, 1460];
const WSTAB: [u8; 8] = [0, 0, 1, 2, 4, 6, 7, 8];

/// An entry (`struct syncache`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Entry {
    pub inc: Inc,
    /// When to retransmit the SYN-ACK.
    pub rxttime: u64,
    pub rxmits: u32,
    pub tsreflect: u32,
    pub tsoff: u32,
    pub flowlabel: u32,
    pub irs: Seq,
    pub iss: Seq,
    pub peer_mss: u16,
    pub wnd: u16,
    pub requested_s_scale: u8,
    pub requested_r_scale: u8,
    pub flags: u32,
    pub challenge_ack_cnt: u32,
    pub challenge_ack_end: u64,
    /// The listener it belongs to (the caller's PCB id).
    pub listener: u64,
}

/// What the listener contributes to an entry.
#[derive(Clone, Copy, Debug)]
pub struct Listener {
    pub id: u64,
    /// `sol_sbrcv_hiwat`: the receive buffer new connections get.
    pub rcv_hiwat: u32,
    /// `TF_NOOPT` on the listening connection.
    pub noopt: bool,
    /// `IN6P_AUTOFLOWLABEL` on the listening socket.
    pub autoflowlabel: bool,
}

/// A segment the cache sends, with where it goes.
#[derive(Debug)]
pub struct Reply {
    /// The connection's endpoints (local is the source).
    pub inc: Inc,
    pub seg: Seg,
    /// The IPv6 flow label.
    pub flowlabel: u32,
}

/// What `syncache_expand` decided about an ACK to a listener.
#[derive(Debug)]
pub enum Expand {
    /// The handshake is done: make the connection from the entry.
    Socket(Entry),
    /// No entry or cookie fits: answer with a reset.
    Reset,
    /// Drop it silently (a challenge ACK may have gone).
    Drop,
}

#[derive(Debug, Default)]
struct Bucket {
    q: Vec<Entry>,
    /// When the bucket's timer is next due.
    nextc: u64,
    /// When it last overflowed (seconds).
    last_overflow: u64,
}

#[derive(Debug, Default)]
struct Secret {
    oddeven: u32,
    key: [[u8; 16]; 2],
    /// When to reseed (seconds).
    reseed_at: u64,
}

#[derive(Debug, Default)]
struct Pause {
    paused: bool,
    until: u64,
    backoff: u32,
}

/// A VNET's SYN cache (`V_tcp_syncache`).
#[derive(Debug)]
pub struct SynCache {
    buckets: SpinLock<Vec<Bucket>>,
    secret: SpinLock<Secret>,
    pause: SpinLock<Pause>,
    count: AtomicU32,
    /// `net.inet.tcp.syncache.bucketlimit`, `cachelimit`, `rexmtlimit`.
    pub bucket_limit: AtomicU32,
    pub cache_limit: AtomicU32,
    pub rexmt_limit: AtomicU32,
}

impl Default for SynCache {
    fn default() -> Self {
        Self::new()
    }
}

fn hash(inc: &Inc) -> usize {
    let mut h: u32 = 0x811c_9dc5;
    let n = if inc.v6 { 16 } else { 4 };
    for b in inc.faddr[..n]
        .iter()
        .chain(&inc.laddr[..n])
        .chain(&inc.fport.to_be_bytes())
        .chain(&inc.lport.to_be_bytes())
    {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h as usize % HASHSIZE
}

/// The same connection (`in_endpoints`).
fn same(a: &Inc, b: &Inc) -> bool {
    a.faddr == b.faddr
        && a.laddr == b.laddr
        && a.fport == b.fport
        && a.lport == b.lport
        && a.v6 == b.v6
}

impl SynCache {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: SpinLock::new(Vec::new()),
            secret: SpinLock::new(Secret {
                oddeven: 0,
                key: [[0; 16]; 2],
                reseed_at: 0,
            }),
            pause: SpinLock::new(Pause {
                paused: false,
                until: 0,
                backoff: 0,
            }),
            count: AtomicU32::new(0),
            bucket_limit: AtomicU32::new(BUCKETLIMIT),
            cache_limit: AtomicU32::new(HASHSIZE as u32 * BUCKETLIMIT),
            rexmt_limit: AtomicU32::new(MAXREXMTS),
        }
    }

    /// `net.inet.tcp.syncache.count`.
    #[must_use]
    pub fn count(&self) -> u32 {
        // Relaxed: a statistic.
        self.count.load(Ordering::Relaxed)
    }

    /// The entries, for `net.inet.tcp.pcblist`'s SYN_RECEIVED records.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        self.buckets
            .lock()
            .iter()
            .flat_map(|b| b.q.iter().copied())
            .collect()
    }

    /// The current cookie key and its parity, reseeding the other one
    /// every `SYNCOOKIE_LIFETIME` (`syncookie_reseed`).
    fn key(&self, cx: &Ctx, odd_even: Option<u32>) -> ([u8; 16], u32) {
        let mut s = self.secret.lock();
        let secs = cx.now / 1000;
        if s.reseed_at == 0 {
            for k in &mut s.key {
                for c in k.chunks_mut(4) {
                    c.copy_from_slice(&(cx.random)().to_ne_bytes());
                }
            }
            s.reseed_at = secs + SYNCOOKIE_LIFETIME;
        }
        while secs >= s.reseed_at {
            let next = if s.oddeven & 1 != 0 { 0 } else { 1 };
            for c in s.key[next].chunks_mut(4) {
                c.copy_from_slice(&(cx.random)().to_ne_bytes());
            }
            s.oddeven = s.oddeven.wrapping_add(1);
            s.reseed_at += SYNCOOKIE_LIFETIME;
        }
        let bit = odd_even.unwrap_or(s.oddeven & 1);
        (s.key[bit as usize], bit)
    }

    /// `syncache_cookiesonly`.
    fn cookies_only(&self, cx: &Ctx) -> bool {
        let mut p = self.pause.lock();
        // `syncache_unpause`.
        if p.paused && cx.now / 1000 >= p.until {
            p.paused = false;
        }
        (on(&cx.conf.syncookies) && p.paused) || on(&cx.conf.syncookies_only)
    }

    /// `syncache_pause`: too many SYNs; for a while only cookies.
    fn pause(&self, cx: &Ctx) {
        let mut p = self.pause.lock();
        if p.paused {
            return;
        }
        cx.stats.inc(tcps::SC_BUCKETOVERFLOW);
        if !on(&cx.conf.syncookies) {
            return;
        }
        p.paused = true;
        let now = cx.now / 1000;
        let mut delta = SYNCOOKIE_LIFETIME << p.backoff;
        if p.until + delta > now {
            if p.backoff < MAX_BACKOFF {
                delta <<= 1;
                p.backoff += 1;
            }
        } else {
            delta = SYNCOOKIE_LIFETIME;
            p.backoff = 0;
        }
        p.until = now + delta;
    }

    /// `syncookie_generate`: the ISS that encodes the entry.
    fn cookie_generate(&self, cx: &Ctx, bucket: usize, e: &Entry) -> Seq {
        let mut i = MSSTAB.len() - 1;
        while MSSTAB[i] > e.peer_mss && i > 0 {
            i -= 1;
        }
        let mut cookie = (i as u8) << 5;
        if e.flags & scf::WINSCALE != 0 {
            let mut w = WSTAB.len() - 1;
            while WSTAB[w] > e.requested_s_scale && w > 0 {
                w -= 1;
            }
            cookie |= (w as u8) << 2;
        }
        if e.flags & scf::SACK != 0 {
            cookie |= 0x02;
        }
        let (key, bit) = self.key(cx, None);
        cookie |= bit as u8;
        let h = syncookie_mac(&key, &e.inc, e.irs, cookie, bucket as u32);
        cx.stats.inc(tcps::SC_SENDCOOKIE);
        (h & !0xff) | u32::from(cookie ^ (h >> 24) as u8)
    }

    /// `syncookie_expand`: the entry a valid cookie in `t`'s ACK carries.
    fn cookie_expand(
        &self,
        cx: &Ctx,
        bucket: usize,
        inc: &Inc,
        t: &Th,
        to: &Opts,
        l: &Listener,
    ) -> Option<Entry> {
        let ack = t.ack.wrapping_sub(1);
        let sq = t.seq.wrapping_sub(1);
        let cookie = ((ack & 0xff) ^ (ack >> 24)) as u8;
        let (key, _) = self.key(cx, Some(u32::from(cookie & 1)));
        let h = syncookie_mac(&key, inc, sq, cookie, bucket as u32);
        if ack & !0xff != h & !0xff {
            return None;
        }
        let mut e = Entry {
            inc: *inc,
            irs: sq,
            iss: ack,
            listener: l.id,
            ..Entry::default()
        };
        if inc.v6 && l.autoflowlabel {
            e.flowlabel = ack & crate::FLOWLABEL_MASK;
        }
        e.peer_mss = MSSTAB[usize::from(cookie >> 5)];
        let widx = usize::from((cookie >> 2) & 7);
        if widx > 0 {
            e.requested_r_scale = r_scale(cx);
            e.requested_s_scale = WSTAB[widx];
            e.flags |= scf::WINSCALE;
        }
        e.wnd = l.rcv_hiwat.min(MAXWIN) as u16;
        if cookie & 0x02 != 0 {
            e.flags |= scf::SACK;
        }
        if to.has(tof::TS) {
            e.flags |= scf::TIMESTAMP;
            e.tsreflect = to.tsval;
            e.tsoff = cx.ts_offset(inc);
        }
        Some(e)
    }

    /// `syncache_timeout`: the next retransmission, backing off.
    fn timeout(cx: &Ctx, e: &mut Entry, b: &mut Bucket) {
        let initial = get(&cx.conf.rexmit_initial) as u32;
        let rexmt = if e.rxmits == 0 {
            initial
        } else {
            timer::rangeset(
                cx.conf,
                initial * timer::BACKOFF[(e.rxmits as usize).min(timer::BACKOFF.len() - 1)],
                get(&cx.conf.rexmit_min) as u32,
                get(&cx.conf.rexmit_max) as u32,
            )
        };
        e.rxttime = cx.now + u64::from(rexmt);
        e.rxmits += 1;
        if e.rxttime < b.nextc {
            b.nextc = e.rxttime;
        }
    }

    /// `syncache_respond`: the SYN-ACK (or, with `ACK` alone, a challenge
    /// ACK) for `e`.
    fn respond(cx: &Ctx, e: &Entry, flags: u16) -> Option<Reply> {
        let maxmtu = (cx.path)(&e.inc).maxmtu;
        let hcmtu = cx.hc.mtu(&e.inc.peer());
        let min_protoh = e.inc.min_protoh();
        let dflt = if e.inc.v6 {
            get(&cx.conf.v6mssdflt) as u32
        } else {
            get(&cx.conf.mssdflt) as u32
        };
        let mssopt = if maxmtu != 0 && hcmtu != 0 {
            maxmtu.min(hcmtu) - min_protoh
        } else if maxmtu != 0 || hcmtu != 0 {
            maxmtu.max(hcmtu) - min_protoh
        } else {
            dflt
        }
        .max(get(&cx.conf.minmss) as u32);
        let flags = ecn::syncache_respond(cx, flags, e.flags);
        let mut to = Opts::default();
        if e.flags & scf::NOOPT == 0 {
            if flags & th::SYN != 0 {
                to.mss = mssopt.min(u32::from(u16::MAX)) as u16;
                to.flags = tof::MSS;
                if e.flags & scf::WINSCALE != 0 {
                    to.wscale = e.requested_r_scale;
                    to.flags |= tof::SCALE;
                }
                if e.flags & scf::SACK != 0 {
                    to.flags |= tof::SACKPERM;
                }
            }
            if e.flags & scf::TIMESTAMP != 0 {
                to.tsval = e.tsoff.wrapping_add(cx.ts_ticks());
                to.tsecr = e.tsreflect;
                to.flags |= tof::TS;
            }
        }
        let t = Th {
            sport: e.inc.lport,
            dport: e.inc.fport,
            seq: if flags & th::SYN != 0 {
                e.iss
            } else {
                e.iss.wrapping_add(1)
            },
            ack: e.irs.wrapping_add(1),
            off: 0,
            flags,
            win: e.wnd,
            sum: 0,
            urp: 0,
        };
        let pkt = build(t, &to, None)?;
        Some(Reply {
            inc: e.inc,
            seg: Seg {
                pkt,
                ecn: ecn::NOTECT,
                df: on(&cx.conf.path_mtu_discovery) && e.flags & scf::UNREACH == 0,
                tso_segsz: 0,
            },
            flowlabel: e.flowlabel,
        })
    }

    /// `syncache_insert`: a new entry, at its bucket's head.
    fn insert(&self, cx: &Ctx, b: &mut Bucket, mut e: Entry) {
        // Relaxed: settings.
        if b.q.len() as u32 >= self.bucket_limit.load(Ordering::Relaxed) {
            self.pause(cx);
            b.last_overflow = cx.now / 1000;
            if b.q.pop().is_some() {
                cx.stats.state_dec(State::SynReceived);
                self.count.fetch_sub(1, Ordering::Relaxed);
            }
        }
        if b.q.is_empty() {
            b.nextc = u64::MAX;
        }
        Self::timeout(cx, &mut e, b);
        b.q.insert(0, e);
        self.count.fetch_add(1, Ordering::Relaxed);
        cx.stats.state_inc(State::SynReceived);
        cx.stats.inc(tcps::SC_ADDED);
    }

    fn with_bucket<R>(&self, i: usize, f: impl FnOnce(&mut Bucket) -> R) -> R {
        let mut b = self.buckets.lock();
        if b.is_empty() {
            b.resize_with(HASHSIZE, Bucket::default);
        }
        f(&mut b[i])
    }

    /// `syncache_add`: a SYN `t` with options `to` for listener `l`;
    /// returns the SYN-ACK to send.
    pub fn add(
        &self,
        cx: &Ctx,
        inc: &Inc,
        to: &Opts,
        t: &Th,
        iptos: u8,
        l: &Listener,
    ) -> Option<Reply> {
        let bi = hash(inc);
        let cookies_only = self.cookies_only(cx);
        if !cookies_only {
            // A duplicate SYN: the SYN-ACK again.
            let dup = self.with_bucket(bi, |b| {
                let i = b.q.iter().position(|e| same(&e.inc, inc))?;
                cx.stats.inc(tcps::SC_DUPSYN);
                let e = &mut b.q[i];
                if e.flags & scf::TIMESTAMP != 0 && to.has(tof::TS) {
                    e.tsreflect = to.tsval;
                } else {
                    e.flags &= !scf::TIMESTAMP;
                }
                if e.flags & ecnf::ECN_MASK != 0 {
                    e.flags &= !ecnf::ECN_MASK;
                    e.flags |= ecn::syncache_add(cx, t.flags, iptos);
                }
                let r = Self::respond(cx, e, th::SYN | th::ACK);
                if r.is_some() {
                    let mut e = *e;
                    e.rxmits = 0;
                    Self::timeout(cx, &mut e, b);
                    b.q[i] = e;
                    cx.stats.inc(tcps::SNDACKS);
                    cx.stats.inc(tcps::SNDTOTAL);
                }
                Some(r)
            });
            if let Some(r) = dup {
                return r;
            }
        }
        let mut e = Entry {
            inc: *inc,
            irs: t.seq,
            wnd: l.rcv_hiwat.min(MAXWIN) as u16,
            listener: l.id,
            ..Entry::default()
        };
        let rfc1323 = get(&cx.conf.rfc1323);
        if rfc1323 != 0 && !l.noopt {
            if to.has(tof::TS) && rfc1323 != 2 {
                e.tsreflect = to.tsval;
                e.flags |= scf::TIMESTAMP;
                e.tsoff = cx.ts_offset(inc);
            }
            if to.has(tof::SCALE) && rfc1323 != 3 {
                e.requested_r_scale = r_scale(cx);
                e.requested_s_scale = to.wscale;
                e.flags |= scf::WINSCALE;
            }
        }
        if to.has(tof::SACKPERM) {
            e.flags |= scf::SACK;
        }
        if to.has(tof::MSS) {
            e.peer_mss = to.mss;
        }
        if l.noopt {
            e.flags |= scf::NOOPT;
        }
        if get(&cx.conf.ecn_enable) != 0 {
            e.flags |= ecn::syncache_add(cx, t.flags, iptos);
        }
        e.iss = if on(&cx.conf.syncookies) || on(&cx.conf.syncookies_only) {
            self.cookie_generate(cx, bi, &e)
        } else {
            (cx.random)()
        };
        if inc.v6 && l.autoflowlabel {
            e.flowlabel = if on(&cx.conf.syncookies) || on(&cx.conf.syncookies_only) {
                e.iss
            } else {
                (cx.random)()
            } & crate::FLOWLABEL_MASK;
        }
        let r = Self::respond(cx, &e, th::SYN | th::ACK);
        if r.is_some() {
            if !cookies_only {
                self.with_bucket(bi, |b| self.insert(cx, b, e));
            }
            cx.stats.inc(tcps::SNDACKS);
            cx.stats.inc(tcps::SNDTOTAL);
        } else {
            cx.stats.inc(tcps::SC_DROPPED);
        }
        r
    }

    /// `syncache_expand`: an ACK `t` to listener `l` completes a
    /// handshake from the cache or a cookie.
    pub fn expand(
        &self,
        cx: &Ctx,
        inc: &Inc,
        to: &Opts,
        t: &Th,
        l: &Listener,
        replies: &mut Vec<Reply>,
    ) -> Expand {
        let bi = hash(inc);
        let cookies_only = self.cookies_only(cx);
        let found = if cookies_only {
            None
        } else {
            self.with_bucket(bi, |b| {
                let i = b.q.iter().position(|e| same(&e.inc, inc))?;
                let e = &mut b.q[i];
                // Checks before the entry may go.
                if e.flags & scf::TIMESTAMP != 0
                    && to.has(tof::TS)
                    && seq::ts_lt(to.tsval, e.tsreflect)
                {
                    return Some(Err(Expand::Drop));
                }
                if e.flags & scf::TIMESTAMP != 0
                    && !to.has(tof::TS)
                    && !on(&cx.conf.tolerate_missing_ts)
                {
                    return Some(Err(Expand::Drop));
                }
                if seq::leq(t.seq, e.irs) || seq::gt(t.seq, e.irs.wrapping_add(u32::from(e.wnd))) {
                    // `syncache_send_challenge_ack`.
                    if challenge_ok(cx, &mut e.challenge_ack_end, &mut e.challenge_ack_cnt)
                        && let Some(r) = Self::respond(cx, e, th::ACK)
                    {
                        cx.stats.inc(tcps::SNDACKS);
                        cx.stats.inc(tcps::SNDTOTAL);
                        replies.push(r);
                    }
                    return Some(Err(Expand::Drop));
                }
                if t.ack != e.iss.wrapping_add(1) {
                    return Some(Err(Expand::Reset));
                }
                let e = b.q.remove(i);
                self.count.fetch_sub(1, Ordering::Relaxed);
                Some(Ok(e))
            })
        };
        match found {
            Some(Ok(e)) => {
                cx.stats.inc(tcps::SC_COMPLETED);
                Expand::Socket(e)
            }
            Some(Err(x)) => x,
            None => {
                if !cookies_only {
                    if !on(&cx.conf.syncookies) {
                        cx.stats.inc(tcps::SC_SPURCOOKIE);
                        return Expand::Reset;
                    }
                    let last = self.with_bucket(bi, |b| b.last_overflow);
                    if last + SYNCOOKIE_LIFETIME < cx.now / 1000 || last == 0 {
                        cx.stats.inc(tcps::SC_SPURCOOKIE);
                        return Expand::Reset;
                    }
                }
                match self.cookie_expand(cx, bi, inc, t, to, l) {
                    Some(e) => {
                        cx.stats.inc(tcps::SC_RECVCOOKIE);
                        cx.stats.state_inc(State::SynReceived);
                        Expand::Socket(e)
                    }
                    None => {
                        cx.stats.inc(tcps::SC_FAILCOOKIE);
                        Expand::Reset
                    }
                }
            }
        }
    }

    /// `syncache_chkrst`: a reset for a half-open connection.
    pub fn chkrst(&self, cx: &Ctx, inc: &Inc, t: &Th) -> Option<Reply> {
        if self.cookies_only(cx) {
            return None;
        }
        self.with_bucket(hash(inc), |b| {
            let Some(i) = b.q.iter().position(|e| same(&e.inc, inc)) else {
                cx.stats.inc(tcps::BADRST);
                return None;
            };
            let e = &mut b.q[i];
            let nxt = e.irs.wrapping_add(1);
            if (seq::geq(t.seq, nxt) && seq::lt(t.seq, nxt.wrapping_add(u32::from(e.wnd))))
                || (e.wnd == 0 && t.seq == nxt)
            {
                if on(&cx.conf.insecure_rst) || t.seq == nxt {
                    b.q.remove(i);
                    self.count.fetch_sub(1, Ordering::Relaxed);
                    cx.stats.state_dec(State::SynReceived);
                    cx.stats.inc(tcps::SC_RESET);
                    return None;
                }
                cx.stats.inc(tcps::BADRST);
                if challenge_ok(cx, &mut e.challenge_ack_end, &mut e.challenge_ack_cnt) {
                    let r = Self::respond(cx, e, th::ACK);
                    if r.is_some() {
                        cx.stats.inc(tcps::SNDACKS);
                        cx.stats.inc(tcps::SNDTOTAL);
                    }
                    return r;
                }
                return None;
            }
            cx.stats.inc(tcps::BADRST);
            None
        })
    }

    /// `syncache_unreach`: an ICMP unreachable about our SYN-ACK
    /// (sequence number `th_seq`).
    pub fn unreach(&self, cx: &Ctx, inc: &Inc, th_seq: Seq) {
        if self.cookies_only(cx) {
            return;
        }
        self.with_bucket(hash(inc), |b| {
            let Some(i) = b.q.iter().position(|e| same(&e.inc, inc)) else {
                return;
            };
            let e = &mut b.q[i];
            if th_seq != e.iss {
                return;
            }
            if e.flags & scf::UNREACH == 0 || e.rxmits < 3 + 1 {
                e.flags |= scf::UNREACH;
                return;
            }
            b.q.remove(i);
            self.count.fetch_sub(1, Ordering::Relaxed);
            cx.stats.state_dec(State::SynReceived);
            cx.stats.inc(tcps::SC_UNREACH);
        });
    }

    /// Drops a listener's entries (its socket closed).
    pub fn drop_listener(&self, stats: &crate::Stats, id: u64) {
        let mut bs = self.buckets.lock();
        for b in bs.iter_mut() {
            let before = b.q.len();
            b.q.retain(|e| e.listener != id);
            let gone = before - b.q.len();
            for _ in 0..gone {
                stats.state_dec(State::SynReceived);
            }
            // Relaxed: a count changed under the lock.
            self.count.fetch_sub(gone as u32, Ordering::Relaxed);
        }
    }

    /// When [`SynCache::timer`] is next due.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        let bs = self.buckets.lock();
        bs.iter().filter(|b| !b.q.is_empty()).map(|b| b.nextc).min()
    }

    /// `syncache_timer`: retransmits due SYN-ACKs; entries past the limit
    /// go.
    pub fn timer(&self, cx: &Ctx) -> Vec<Reply> {
        let paused = self.cookies_only(cx) && !on(&cx.conf.syncookies_only);
        // Relaxed: settings.
        let limit = self.rexmt_limit.load(Ordering::Relaxed);
        let ecn_max = get(&cx.conf.ecn_maxretries) as u32;
        let mut out = Vec::new();
        let mut bs = self.buckets.lock();
        for b in bs
            .iter_mut()
            .filter(|b| !b.q.is_empty() && b.nextc <= cx.now)
        {
            b.nextc = u64::MAX;
            let mut i = 0;
            while i < b.q.len() {
                let gone = |b: &mut Bucket, i: usize| {
                    b.q.remove(i);
                    self.count.fetch_sub(1, Ordering::Relaxed);
                    cx.stats.state_dec(State::SynReceived);
                };
                if paused {
                    gone(b, i);
                    continue;
                }
                let mut e = b.q[i];
                if e.rxttime > cx.now {
                    b.nextc = b.nextc.min(e.rxttime);
                    i += 1;
                    continue;
                }
                if e.rxmits > ecn_max {
                    e.flags &= !ecnf::ECN_MASK;
                }
                if e.rxmits > limit {
                    gone(b, i);
                    cx.stats.inc(tcps::SC_STALE);
                    continue;
                }
                match Self::respond(cx, &e, th::SYN | th::ACK) {
                    Some(r) => {
                        out.push(r);
                        Self::timeout(cx, &mut e, b);
                        b.q[i] = e;
                        cx.stats.inc(tcps::SNDACKS);
                        cx.stats.inc(tcps::SNDTOTAL);
                        cx.stats.inc(tcps::SC_RETRANSMITTED);
                        i += 1;
                    }
                    None => {
                        gone(b, i);
                        cx.stats.inc(tcps::SC_DROPPED);
                    }
                }
            }
        }
        out
    }
}

/// `tcp_challenge_ack_check` on an entry's counters.
fn challenge_ok(cx: &Ctx, end: &mut u64, cnt: &mut u32) -> bool {
    let window = get(&cx.conf.ack_war_timewindow) as u64;
    let max = get(&cx.conf.ack_war_cnt) as u32;
    if window == 0 || max == 0 {
        return true;
    }
    if *end < cx.now {
        *cnt = 0;
        *end = cx.now + window;
    }
    if *cnt < max {
        *cnt += 1;
        true
    } else {
        false
    }
}

/// The window scale this side asks for: enough for `sb_max`.
fn r_scale(cx: &Ctx) -> u8 {
    let sb_max = get(&cx.conf.sb_max) as u64;
    let mut w = 0;
    while w < MAX_WINSHIFT && (u64::from(MAXWIN) << w) < sb_max {
        w += 1;
    }
    w
}

impl Tcpcb {
    /// `syncache_socket`: the connection a completed handshake makes, in
    /// SYN_RECEIVED, from entry `e`; `lflags` are the listener's
    /// `t_flags` and `lkeep` its keepalive settings, which it inherits.
    #[must_use]
    pub fn from_syncache<S: Sock>(
        e: &Entry,
        so: &mut S,
        cx: &mut Ctx,
        algo: Algo,
        lflags: u32,
        lkeep: [u32; 4],
    ) -> Self {
        let mut tp = Self::new(e.inc, algo, cx.conf, cx.now);
        tp.state = State::SynReceived;
        tp.iss = e.iss;
        tp.irs = e.irs;
        tp.rcvseqinit();
        tp.sendseqinit();
        tp.snd_wl1 = e.irs;
        tp.snd_max = tp.iss.wrapping_add(1);
        tp.snd_nxt = tp.iss.wrapping_add(1);
        tp.rcv_up = e.irs.wrapping_add(1);
        tp.rcv_wnd = u32::from(e.wnd);
        tp.rcv_adv = tp.rcv_adv.wrapping_add(tp.rcv_wnd);
        tp.last_ack_sent = tp.rcv_nxt;
        tp.flags = lflags & (tf::LRD | tf::NOPUSH | tf::NODELAY);
        if e.flags & scf::NOOPT != 0 {
            tp.flags |= tf::NOOPT;
        } else {
            if e.flags & scf::WINSCALE != 0 {
                tp.flags |= tf::REQ_SCALE | tf::RCVD_SCALE;
                tp.snd_scale = e.requested_s_scale;
                tp.request_r_scale = e.requested_r_scale;
            }
            if e.flags & scf::TIMESTAMP != 0 {
                tp.flags |= tf::REQ_TSTMP | tf::RCVD_TSTMP;
                tp.ts_recent = e.tsreflect;
                tp.ts_recent_age = u64::from(cx.ts_ticks());
                tp.ts_offset = e.tsoff;
            }
            if e.flags & scf::SACK != 0 {
                tp.flags |= tf::SACK_PERMIT;
            }
        }
        tp.ecn_syncache_socket(e.flags);
        let path = (cx.path)(&tp.inc);
        let hc = cx.hc;
        tp.mss(so, cx, hc, &path, i32::from(e.peer_mss));
        if e.rxmits > 1 {
            tp.snd_cwnd = 1;
        }
        tp.challenge_ack_end = e.challenge_ack_end;
        tp.challenge_ack_cnt = e.challenge_ack_cnt;
        [tp.keepinit, tp.keepidle, tp.keepintvl, tp.keepcnt] = lkeep;
        let d = tp.keepinit(cx.conf);
        tp.timer_activate(Tt::Keep, d, cx.now);
        cx.stats.inc(tcps::ACCEPTS);
        tp
    }
}
