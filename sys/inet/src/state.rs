// SPDX-License-Identifier: BSD-2-Clause
//! The internet protocols' state in each VNET (their `VNET_DEFINE`s):
//! statistics, the `net.inet.*` settings FreeBSD keeps per VNET, the
//! reassembly queues, the IP identification counter, and ICMP's rate
//! limits.
//!
//! Behavior reference: sys/netinet/ip_var.h (`struct ipstat`),
//! sys/netinet/icmp_var.h (`struct icmpstat`), sys/netinet/udp_var.h
//! (`struct udpstat`), sys/kern/subr_counter.c (`counter_ratecheck`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use net::Vnet;
use sync::SpinLock;

/// Statistics counters (a `struct *stat` of `uint64_t`s).
#[derive(Debug)]
pub struct Counters<const N: usize>([AtomicU64; N]);

impl<const N: usize> Counters<N> {
    #[must_use]
    pub const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; N])
    }

    pub fn inc(&self, i: usize) {
        self.add(i, 1);
    }

    pub fn add(&self, i: usize, n: u64) {
        if let Some(c) = self.0.get(i) {
            // Relaxed: a statistic.
            c.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Takes one back (`IPSTAT_DEC`).
    pub fn dec(&self, i: usize) {
        if let Some(c) = self.0.get(i) {
            // Relaxed: a statistic.
            c.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn get(&self, i: usize) -> u64 {
        // Relaxed: a statistic.
        self.0.get(i).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// The structure's bytes, as `sysctl` returns it.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        self.0
            .iter()
            // Relaxed: a snapshot of statistics.
            .flat_map(|c| c.load(Ordering::Relaxed).to_ne_bytes())
            .collect()
    }

    /// Zeroes them (a write of zeros to the `stats` sysctl).
    pub fn clear(&self) {
        for c in &self.0 {
            // Relaxed: a statistic.
            c.store(0, Ordering::Relaxed);
        }
    }
}

impl<const N: usize> Default for Counters<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// `struct ipstat`'s fields.
pub mod ips {
    pub const TOTAL: usize = 0;
    pub const BADSUM: usize = 1;
    pub const TOOSHORT: usize = 2;
    pub const TOOSMALL: usize = 3;
    pub const BADHLEN: usize = 4;
    pub const BADLEN: usize = 5;
    pub const FRAGMENTS: usize = 6;
    pub const FRAGDROPPED: usize = 7;
    pub const FRAGTIMEOUT: usize = 8;
    pub const FORWARD: usize = 9;
    pub const FASTFORWARD: usize = 10;
    pub const CANTFORWARD: usize = 11;
    pub const REDIRECTSENT: usize = 12;
    pub const NOPROTO: usize = 13;
    pub const DELIVERED: usize = 14;
    pub const LOCALOUT: usize = 15;
    pub const ODROPPED: usize = 16;
    pub const REASSEMBLED: usize = 17;
    pub const FRAGMENTED: usize = 18;
    pub const OFRAGMENTS: usize = 19;
    pub const CANTFRAG: usize = 20;
    pub const BADOPTIONS: usize = 21;
    pub const NOROUTE: usize = 22;
    pub const BADVERS: usize = 23;
    pub const RAWOUT: usize = 24;
    pub const TOOLONG: usize = 25;
    pub const NOTMEMBER: usize = 26;
    pub const NOGIF: usize = 27;
    pub const BADADDR: usize = 28;
    pub const N: usize = 29;
}

/// `struct icmpstat`'s fields; the histograms have `ICMP_MAXTYPE + 1`
/// entries.
pub mod icps {
    pub const ERROR: usize = 0;
    pub const OLDSHORT: usize = 1;
    pub const OLDICMP: usize = 2;
    pub const OUTHIST: usize = 3;
    pub const BADCODE: usize = 44;
    pub const TOOSHORT: usize = 45;
    pub const CHECKSUM: usize = 46;
    pub const BADLEN: usize = 47;
    pub const REFLECT: usize = 48;
    pub const INHIST: usize = 49;
    pub const BMCASTECHO: usize = 90;
    pub const BMCASTTSTAMP: usize = 91;
    pub const BADADDR: usize = 92;
    pub const NOROUTE: usize = 93;
    pub const N: usize = 94;
}

/// `struct udpstat`'s fields.
pub mod udps {
    pub const IPACKETS: usize = 0;
    pub const HDROPS: usize = 1;
    pub const BADSUM: usize = 2;
    pub const NOSUM: usize = 3;
    pub const BADLEN: usize = 4;
    pub const NOPORT: usize = 5;
    pub const NOPORTBCAST: usize = 6;
    pub const FULLSOCK: usize = 7;
    pub const OPACKETS: usize = 10;
    pub const NOPORTMCAST: usize = 12;
    pub const FILTERMCAST: usize = 13;
    pub const N: usize = 14;
}

/// The per-VNET settings (`net.inet.ip.*`, `net.inet.icmp.*`,
/// `net.inet.udp.*`), FreeBSD's defaults.
#[derive(Debug)]
pub struct Conf {
    pub forwarding: AtomicI32,
    /// `net.inet.ip.redirect` (`ipsendredirects`).
    pub redirect: AtomicI32,
    pub random_id: AtomicI32,
    /// `net.inet.ip.rfc6864`: no identification on atomic datagrams.
    pub rfc6864: AtomicI32,
    pub strong_es: AtomicI32,
    /// `net.inet.ip.source_address_validation`.
    pub sav: AtomicI32,
    pub maxfragsperpacket: AtomicI32,
    /// `net.inet.ip.fragttl`, seconds.
    pub fragttl: AtomicI32,
    pub icmplim: AtomicU32,
    pub icmplim_jitter: AtomicU32,
    pub icmplim_output: AtomicI32,
    pub bmcastecho: AtomicI32,
    pub tstamprepl: AtomicI32,
    pub maskrepl: AtomicI32,
    pub drop_redirect: AtomicI32,
    pub log_redirect: AtomicI32,
    pub quotelen: AtomicI32,
    /// `net.inet.icmp.reply_from_interface`.
    pub rfi: AtomicI32,
    pub udp_blackhole: AtomicI32,
    pub udp_blackhole_local: AtomicI32,
    pub udp_cksum: AtomicI32,
    pub udp_log_in_vain: AtomicI32,
    /// `net.inet.igmp.default_version`, `sendra`, `sendlocal`.
    pub igmp_version: AtomicI32,
    pub igmp_sendra: AtomicI32,
    pub igmp_sendlocal: AtomicI32,
    /// `net.route.multipath` (`ROUTE_MPATH`, on as in GENERIC).
    pub route_multipath: AtomicI32,
}

impl Conf {
    const fn new() -> Self {
        Self {
            forwarding: AtomicI32::new(0),
            redirect: AtomicI32::new(1),
            random_id: AtomicI32::new(0),
            rfc6864: AtomicI32::new(1),
            strong_es: AtomicI32::new(0),
            sav: AtomicI32::new(1),
            maxfragsperpacket: AtomicI32::new(16),
            fragttl: AtomicI32::new(30),
            icmplim: AtomicU32::new(200),
            icmplim_jitter: AtomicU32::new(16),
            icmplim_output: AtomicI32::new(1),
            bmcastecho: AtomicI32::new(0),
            tstamprepl: AtomicI32::new(1),
            maskrepl: AtomicI32::new(0),
            drop_redirect: AtomicI32::new(0),
            log_redirect: AtomicI32::new(0),
            quotelen: AtomicI32::new(548),
            rfi: AtomicI32::new(0),
            udp_blackhole: AtomicI32::new(0),
            udp_blackhole_local: AtomicI32::new(0),
            udp_cksum: AtomicI32::new(1),
            udp_log_in_vain: AtomicI32::new(0),
            igmp_version: AtomicI32::new(3),
            igmp_sendra: AtomicI32::new(1),
            igmp_sendlocal: AtomicI32::new(1),
            route_multipath: AtomicI32::new(1),
        }
    }
}

/// Reads a setting.
pub(crate) fn on(a: &AtomicI32) -> bool {
    // Relaxed: a setting read as FreeBSD reads its `V_` variable.
    a.load(Ordering::Relaxed) != 0
}

/// ICMP's rate limits (`BANDLIM_*`).
pub mod bandlim {
    pub const UNREACH: usize = 0;
    pub const ECHO: usize = 1;
    pub const TSTAMP: usize = 2;
    pub const N: usize = 3;
    /// `icmp_rate_descrs`.
    pub const NAMES: [&str; N] = ["icmp unreach", "icmp ping", "icmp tstamp"];
}

/// One rate limit (`struct counter_rate`) with its jitter.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Rate {
    /// When its second began (the hooks' clock), and the count in it.
    start: u64,
    count: u64,
    over: bool,
    pub(crate) jitter: i32,
}

impl Rate {
    /// `counter_ratecheck`: -1 if this one is over `limit` in the current
    /// second; at the start of a second, the last one's count if it was
    /// over, else 0.
    pub(crate) fn check(&mut self, now: u64, limit: u64) -> i64 {
        let mut val = if self.over { -1 } else { 0 };
        if now.saturating_sub(self.start) >= 1_000_000_000 {
            let prev = self.count;
            self.count = 0;
            self.over = false;
            self.start = now;
            val = if prev > limit {
                i64::try_from(prev).unwrap_or(i64::MAX)
            } else {
                0
            };
        }
        self.count += 1;
        if self.over {
            return -1;
        }
        if self.count > limit {
            self.over = true;
            val = -1;
        }
        val
    }
}

/// A VNET's internet state.
#[derive(Debug)]
pub struct State {
    pub ipstat: Counters<{ ips::N }>,
    pub icmpstat: Counters<{ icps::N }>,
    pub udpstat: Counters<{ udps::N }>,
    pub conf: Conf,
    pub(crate) reass: SpinLock<crate::reass::Queues>,
    /// `V_ip_id`.
    pub(crate) id: AtomicU32,
    pub(crate) rates: SpinLock<[Rate; bandlim::N]>,
    /// The IPv4 groups joined on each interface, with their sockets'
    /// count (`struct in_multi`).
    pub(crate) groups: SpinLock<Vec<(u16, [u8; 4], u32)>>,
}

/// `vnet`'s state, made on first use.
#[must_use]
pub fn of(vnet: &Vnet) -> Arc<State> {
    let any = match vnet.inet.get() {
        Some(a) => Arc::clone(a),
        None => {
            let s: Arc<dyn core::any::Any + Send + Sync> = Arc::new(State {
                ipstat: Counters::new(),
                icmpstat: Counters::new(),
                udpstat: Counters::new(),
                conf: Conf::new(),
                reass: SpinLock::new(crate::reass::Queues::new()),
                id: AtomicU32::new(crate::random()),
                rates: SpinLock::new([Rate::default(); bandlim::N]),
                groups: SpinLock::new(Vec::new()),
            });
            // A racing first use makes one too; one of them is kept.
            let _ = vnet.inet.set(s);
            vnet.inet
                .get()
                .map(Arc::clone)
                .unwrap_or_else(|| unreachable!("just set"))
        }
    };
    any.downcast::<State>()
        .unwrap_or_else(|_| unreachable!("only this crate sets it"))
}
