// SPDX-License-Identifier: BSD-2-Clause
//! A simulated network (docs/NETWORKING.md §16.1): two hosts, a client
//! and a server with a listener, joined by a link with a delay, a rate,
//! and a rule for which packets it loses, on a virtual clock, so a
//! scenario (a lost SYN-ACK, a SACK hole, a zero window) replays exactly.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use mbuf::Chain;

use crate::cc::Algo;
use crate::hdr::{Accept, Opts, Th, th};
use crate::hostcache::HostCache;
use crate::isn::Secrets;
use crate::syncache::{Expand, Listener, SynCache};
use crate::tcb::{Inc, Path, Tcpcb};
use crate::{Conf, Ctx, Errno, Fate, Seg, Segment, Sock, State, Stats, TwVerdict};

/// A socket's buffers and what TCP told it.
#[derive(Debug, Default)]
pub(crate) struct SimSock {
    pub(crate) snd: Chain,
    pub(crate) rcv: Chain,
    pub(crate) snd_hiwat: u32,
    pub(crate) rcv_hiwat: u32,
    pub(crate) cantsendmore: bool,
    pub(crate) cantrcvmore: bool,
    pub(crate) connected: bool,
    pub(crate) disconnected: bool,
    pub(crate) error: Option<Errno>,
    pub(crate) options: i32,
    pub(crate) oob: Option<u8>,
    pub(crate) oobmark: Option<u32>,
    pub(crate) snd_auto: bool,
    pub(crate) rcv_auto: bool,
}

impl SimSock {
    pub(crate) fn new() -> Self {
        Self {
            snd_hiwat: 32768,
            rcv_hiwat: 65536,
            snd_auto: true,
            rcv_auto: true,
            ..Self::default()
        }
    }

    /// Reads up to `n` bytes.
    pub(crate) fn read(&mut self, n: usize) -> Vec<u8> {
        let n = n.min(self.rcv.len());
        let mut b = vec![0u8; n];
        self.rcv.copy_out(0, &mut b);
        self.rcv.trim_front(n);
        b
    }
}

impl Sock for SimSock {
    fn snd_cc(&self) -> u32 {
        self.snd.len() as u32
    }
    fn snd_hiwat(&self) -> u32 {
        self.snd_hiwat
    }
    fn snd_autosize(&self) -> bool {
        self.snd_auto
    }
    fn snd_reserve(&mut self, hiwat: u32) -> bool {
        self.snd_hiwat = hiwat;
        true
    }
    fn snd_autosize_off(&mut self) {
        self.snd_auto = false;
    }
    fn snd_copy(&self, off: u32, len: u32) -> Option<Chain> {
        self.snd.copy(off as usize, len as usize).ok()
    }
    fn snd_drop(&mut self, n: u32) {
        self.snd.trim_front(n as usize);
    }
    fn cantsendmore(&self) -> bool {
        self.cantsendmore
    }
    fn rcv_cc(&self) -> u32 {
        self.rcv.len() as u32
    }
    fn rcv_hiwat(&self) -> u32 {
        self.rcv_hiwat
    }
    fn rcv_space(&self) -> i64 {
        i64::from(self.rcv_hiwat) - self.rcv.len() as i64
    }
    fn rcv_mbcnt(&self) -> u32 {
        self.rcv.cost()
    }
    fn rcv_mbmax(&self) -> u32 {
        self.rcv_hiwat * 8
    }
    fn rcv_autosize(&self) -> bool {
        self.rcv_auto
    }
    fn rcv_reserve(&mut self, hiwat: u32) -> bool {
        self.rcv_hiwat = hiwat;
        true
    }
    fn rcv_autosize_off(&mut self) {
        self.rcv_auto = false;
    }
    fn rcv_append(&mut self, data: Chain) {
        self.rcv.append(data);
    }
    fn cantrcvmore(&mut self) {
        self.cantrcvmore = true;
    }
    fn rcv_closed(&self) -> bool {
        self.cantrcvmore
    }
    fn options(&self) -> i32 {
        self.options
    }
    fn nofdref(&self) -> bool {
        false
    }
    fn connected(&mut self) {
        self.connected = true;
    }
    fn disconnecting(&mut self) {}
    fn disconnected(&mut self) {
        self.disconnected = true;
    }
    fn set_error(&mut self, e: Option<Errno>) {
        self.error = e;
    }
    fn urgent(&mut self, oobmark: u32) {
        self.oobmark = Some(oobmark);
    }
    fn oob_byte(&mut self, c: u8) {
        self.oob = Some(c);
    }
    fn wake_writers(&mut self) {}
}

/// A host's side of the connection.
#[derive(Debug)]
pub(crate) struct Host {
    pub(crate) tp: Option<Tcpcb>,
    pub(crate) so: SimSock,
    pub(crate) stats: Stats,
    pub(crate) hc: HostCache,
    /// A listener's SYN cache (the server).
    pub(crate) sc: Option<SynCache>,
    pub(crate) addr: [u8; 16],
    pub(crate) port: u16,
    pub(crate) closed: bool,
}

/// A packet in flight: when it arrives, at which host, and its bytes.
struct Flight {
    at: u64,
    to: usize,
    bytes: Vec<u8>,
    ecn: u8,
}

/// The network.
pub(crate) struct Net {
    pub(crate) now: u64,
    pub(crate) conf: Conf,
    pub(crate) hosts: [Host; 2],
    flights: VecDeque<Flight>,
    /// One-way delay, ticks.
    pub(crate) delay: u64,
    /// Bytes per tick the link carries (0: unlimited).
    pub(crate) rate: u64,
    link_free: [u64; 2],
    /// Decides a packet's loss: (sending host, its index among that
    /// host's packets, the header) -> lost.
    pub(crate) lose: fn(usize, u32, &Th) -> bool,
    sent: [u32; 2],
    pub(crate) mtu: u32,
    /// Every segment delivered, for tests to look at: (to, header,
    /// data length).
    pub(crate) log: Vec<(usize, Th, usize)>,
}

fn rnd() -> u32 {
    static X: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0x1234_5678);
    // Relaxed: a counter.
    let mut x = X.fetch_add(0x9e37_79b9, core::sync::atomic::Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 17;
    x ^ (x << 5)
}

const SECRETS: Secrets = Secrets {
    isn: [1; 16],
    ts: [2; 16],
};

impl Net {
    pub(crate) fn new() -> Self {
        let host = |a: u8, port| Host {
            tp: None,
            so: SimSock::new(),
            stats: Stats::new(),
            hc: HostCache::new(),
            sc: None,
            addr: [10, 0, 0, a, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            port,
            closed: false,
        };
        Self {
            now: 1000,
            conf: Conf::new(),
            hosts: [host(1, 40000), host(2, 80)],
            flights: VecDeque::new(),
            delay: 5,
            rate: 0,
            link_free: [0; 2],
            lose: |_, _, _| false,
            sent: [0; 2],
            mtu: 1500,
            log: Vec::new(),
        }
    }

    fn inc(&self, h: usize) -> Inc {
        let me = &self.hosts[h];
        let peer = &self.hosts[1 - h];
        Inc {
            laddr: me.addr,
            faddr: peer.addr,
            lport: me.port,
            fport: peer.port,
            v6: false,
            zone: 0,
            fib: 0,
        }
    }

    /// Runs `f` on host `h`'s connection with a context, then puts what
    /// it sent on the link.
    pub(crate) fn with<R>(
        &mut self,
        h: usize,
        f: impl FnOnce(&mut Tcpcb, &mut SimSock, &mut Ctx) -> R,
    ) -> R {
        let mut out = Vec::new();
        let mtu = self.mtu;
        let path = move |_: &Inc| Path {
            maxmtu: mtu,
            local: true,
            localip: false,
            tso: None,
        };
        let host = &mut self.hosts[h];
        let mut tp = host.tp.take().expect("a connection");
        let r = {
            let mut cx = Ctx {
                now: self.now,
                conf: &self.conf,
                stats: &host.stats,
                random: rnd,
                out: &mut out,
                hc: &host.hc,
                path: &path,
                secrets: &SECRETS,
            };
            f(&mut tp, &mut host.so, &mut cx)
        };
        host.tp = Some(tp);
        self.send(h, out);
        r
    }

    fn send(&mut self, from: usize, segs: Vec<Seg>) {
        for s in segs {
            let mut bytes = vec![0u8; s.pkt.len()];
            s.pkt.data.copy_out(0, &mut bytes);
            let t = Th::parse(&bytes).expect("a header");
            let n = self.sent[from];
            self.sent[from] += 1;
            if (self.lose)(from, n, &t) {
                continue;
            }
            // Serialization at the rate, then the delay.
            let start = self.now.max(self.link_free[from]);
            let tx = if self.rate == 0 {
                0
            } else {
                (bytes.len() as u64 + 40).div_ceil(self.rate)
            };
            self.link_free[from] = start + tx;
            let at = start + tx + self.delay;
            let f = Flight {
                at,
                to: 1 - from,
                bytes,
                ecn: s.ecn,
            };
            let pos = self
                .flights
                .iter()
                .position(|x| x.at > at)
                .unwrap_or(self.flights.len());
            self.flights.insert(pos, f);
        }
    }

    /// Makes host 1 listen and host 0 connect.
    pub(crate) fn connect(&mut self) {
        self.hosts[1].sc = Some(SynCache::new());
        let inc = self.inc(0);
        self.hosts[0].tp = Some(Tcpcb::new(inc, Algo::Cubic, &self.conf, self.now));
        let iss = SECRETS.new_isn(&inc, self.now);
        let off = SECRETS.new_ts_offset(&inc, true);
        self.with(0, |tp, so, cx| tp.connect(so, cx, iss, off));
    }

    /// A segment arrives at host `h`.
    fn deliver(&mut self, h: usize, bytes: &[u8], ecn: u8) {
        let Some(t) = Th::parse(bytes) else { return };
        let opts = bytes[crate::hdr::HDR_LEN..t.off].to_vec();
        let data = Chain::from_slice(&bytes[t.off..]).unwrap_or_default();
        self.log.push((h, t, data.len()));
        let inc = self.inc(h);
        // A listener with no connection yet: the SYN cache.
        if self.hosts[h].tp.is_none() {
            let Some(sc) = self.hosts[h].sc.take() else {
                return;
            };
            let l = Listener {
                id: 1,
                rcv_hiwat: self.hosts[h].so.rcv_hiwat,
                noopt: false,
                autoflowlabel: false,
            };
            let mtu = self.mtu;
            let path = move |_: &Inc| Path {
                maxmtu: mtu,
                local: true,
                localip: false,
                tso: None,
            };
            let mut out = Vec::new();
            let mut replies = Vec::new();
            let mut tp_new = None;
            {
                let host = &mut self.hosts[h];
                let mut cx = Ctx {
                    now: self.now,
                    conf: &self.conf,
                    stats: &host.stats,
                    random: rnd,
                    out: &mut out,
                    hc: &host.hc,
                    path: &path,
                    secrets: &SECRETS,
                };
                if t.flags & (th::RST | th::ACK | th::SYN) == th::ACK {
                    let to = Opts::parse(
                        &opts,
                        Accept {
                            syn: false,
                            sack: true,
                        },
                    );
                    match sc.expand(&cx, &inc, &to, &t, &l, &mut replies) {
                        Expand::Socket(e) => {
                            // `solisten_enqueue(so, SS_ISCONNECTED)`.
                            host.so.connected();
                            let mut tp = Tcpcb::from_syncache(
                                &e,
                                &mut host.so,
                                &mut cx,
                                Algo::Cubic,
                                0,
                                [0; 4],
                            );
                            let seg = Segment {
                                th: t,
                                opts: &opts,
                                data,
                                iptos: ecn,
                                nsegs: 1,
                            };
                            let _ = tp.do_segment(&mut host.so, &mut cx, seg);
                            tp_new = Some(tp);
                        }
                        Expand::Reset => {}
                        Expand::Drop => {}
                    }
                } else if t.flags & th::RST != 0 {
                    if let Some(r) = sc.chkrst(&cx, &inc, &t) {
                        replies.push(r);
                    }
                } else if t.flags & (th::SYN | th::ACK) == th::SYN {
                    let to = Opts::parse(
                        &opts,
                        Accept {
                            syn: true,
                            sack: true,
                        },
                    );
                    if let Some(r) = sc.add(&cx, &inc, &to, &t, ecn, &l) {
                        replies.push(r);
                    }
                }
            }
            self.hosts[h].sc = Some(sc);
            self.hosts[h].tp = tp_new;
            out.extend(replies.into_iter().map(|r| r.seg));
            self.send(h, out);
            return;
        }
        let state = self.hosts[h].tp.as_ref().map(|t| t.state);
        let seg = Segment {
            th: t,
            opts: &opts,
            data,
            iptos: ecn,
            nsegs: 1,
        };
        let fate = if state == Some(State::TimeWait) {
            match self.with(h, |tp, so, cx| tp.twcheck(so, cx, &seg)) {
                TwVerdict::Recycle => Fate::Closed,
                TwVerdict::Done => Fate::Alive,
            }
        } else if state == Some(State::Closed) {
            Fate::Closed
        } else {
            self.with(h, |tp, so, cx| tp.do_segment(so, cx, seg))
        };
        if fate == Fate::Closed {
            self.hosts[h].closed = true;
        }
    }

    /// Runs until `ticks` pass or nothing is left to do.
    pub(crate) fn run(&mut self, ticks: u64) {
        let end = self.now + ticks;
        loop {
            let next_flight = self.flights.front().map(|f| f.at);
            let next_timer = (0..2)
                .filter_map(|h| {
                    let host = &self.hosts[h];
                    let t = host.tp.as_ref().and_then(Tcpcb::next_deadline);
                    let s = host.sc.as_ref().and_then(SynCache::next_deadline);
                    match (t, s) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    }
                })
                .min();
            let next = match (next_flight, next_timer) {
                (Some(a), Some(b)) => a.min(b),
                (a, b) => match a.or(b) {
                    Some(x) => x,
                    None => break,
                },
            };
            if next > end {
                break;
            }
            self.now = self.now.max(next);
            while self.flights.front().is_some_and(|f| f.at <= self.now) {
                let f = self.flights.pop_front().expect("a flight");
                self.deliver(f.to, &f.bytes, f.ecn);
            }
            for h in 0..2 {
                if self.hosts[h].closed {
                    continue;
                }
                if self.hosts[h]
                    .tp
                    .as_ref()
                    .and_then(Tcpcb::next_deadline)
                    .is_some_and(|d| d <= self.now)
                {
                    let fate = self.with(h, |tp, so, cx| tp.timers(so, cx));
                    if fate == Fate::Closed {
                        self.hosts[h].closed = true;
                    }
                }
                if let Some(sc) = self.hosts[h].sc.take() {
                    if sc.next_deadline().is_some_and(|d| d <= self.now) {
                        let mtu = self.mtu;
                        let path = move |_: &Inc| Path {
                            maxmtu: mtu,
                            local: true,
                            localip: false,
                            tso: None,
                        };
                        let mut out = Vec::new();
                        let replies = {
                            let host = &self.hosts[h];
                            let cx = Ctx {
                                now: self.now,
                                conf: &self.conf,
                                stats: &host.stats,
                                random: rnd,
                                out: &mut out,
                                hc: &host.hc,
                                path: &path,
                                secrets: &SECRETS,
                            };
                            sc.timer(&cx)
                        };
                        out.extend(replies.into_iter().map(|r| r.seg));
                        self.send(h, out);
                    }
                    self.hosts[h].sc = Some(sc);
                }
            }
        }
        self.now = self.now.max(end.min(self.now.max(end)));
    }

    /// Host `h` writes `data` and sends.
    pub(crate) fn write(&mut self, h: usize, data: &[u8]) {
        let c = Chain::from_slice(data).expect("buffers");
        self.hosts[h].so.snd.append(c);
        let _ = self.with(h, |tp, so, cx| tp.send(so, cx, false, false));
    }

    /// Host `h` reads everything and lets TCP update the window.
    pub(crate) fn read_all(&mut self, h: usize) -> Vec<u8> {
        let n = self.hosts[h].so.rcv.len();
        let b = self.hosts[h].so.read(n);
        if self.hosts[h].tp.is_some() && !self.hosts[h].closed {
            let _ = self.with(h, |tp, so, cx| tp.rcvd(so, cx));
        }
        b
    }

    /// Host `h` shuts down its sending side.
    pub(crate) fn shutdown(&mut self, h: usize) {
        self.hosts[h].so.cantsendmore = true;
        let _ = self.with(h, |tp, so, cx| tp.shutdown(so, cx));
    }

    pub(crate) fn state(&self, h: usize) -> State {
        self.hosts[h].tp.as_ref().map_or(State::Closed, |t| t.state)
    }

    pub(crate) fn tp(&self, h: usize) -> &Tcpcb {
        self.hosts[h].tp.as_ref().expect("a connection")
    }

    /// The server reads and the client writes until `data` has crossed.
    pub(crate) fn transfer(&mut self, data: &[u8], step: u64) -> Vec<u8> {
        let mut got = Vec::new();
        let mut off = 0;
        let mut rounds = 0;
        while got.len() < data.len() {
            let room = self.hosts[0].so.snd_hiwat as usize - self.hosts[0].so.snd.len();
            let n = room.min(data.len() - off);
            if n > 0 {
                self.write(0, &data[off..off + n]);
                off += n;
            }
            self.run(step);
            got.extend(self.read_all(1));
            rounds += 1;
            assert!(rounds < 100_000, "stuck at {} of {}", got.len(), data.len());
        }
        got
    }
}
