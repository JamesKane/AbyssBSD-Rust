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

/// `struct ip6stat`'s fields; the arrays' first entries.
pub mod ip6s {
    pub const TOTAL: usize = 0;
    pub const TOOSHORT: usize = 1;
    pub const TOOSMALL: usize = 2;
    pub const FRAGMENTS: usize = 3;
    pub const FRAGDROPPED: usize = 4;
    pub const FRAGTIMEOUT: usize = 5;
    pub const FRAGOVERFLOW: usize = 6;
    pub const FORWARD: usize = 7;
    pub const CANTFORWARD: usize = 8;
    pub const REDIRECTSENT: usize = 9;
    pub const DELIVERED: usize = 10;
    pub const LOCALOUT: usize = 11;
    pub const ODROPPED: usize = 12;
    pub const REASSEMBLED: usize = 13;
    pub const ATOMICFRAGS: usize = 14;
    pub const FRAGMENTED: usize = 15;
    pub const OFRAGMENTS: usize = 16;
    pub const CANTFRAG: usize = 17;
    pub const BADOPTIONS: usize = 18;
    pub const NOROUTE: usize = 19;
    pub const BADVERS: usize = 20;
    pub const RAWOUT: usize = 21;
    pub const BADSCOPE: usize = 22;
    pub const NOTMEMBER: usize = 23;
    /// `ip6s_nxthist[256]`.
    pub const NXTHIST: usize = 24;
    pub const M1: usize = 280;
    /// `ip6s_m2m[32]`.
    pub const M2M: usize = 281;
    pub const MEXT1: usize = 313;
    pub const MEXT2M: usize = 314;
    pub const EXTHDRTOOLONG: usize = 315;
    pub const NOGIF: usize = 316;
    pub const TOOMANYHDR: usize = 317;
    pub const SOURCES_NONE: usize = 318;
    /// `ip6s_sources_sameif[16]` and the other source selection arrays.
    pub const SOURCES_SAMEIF: usize = 319;
    pub const SOURCES_OTHERIF: usize = 335;
    pub const SOURCES_SAMESCOPE: usize = 351;
    pub const SOURCES_OTHERSCOPE: usize = 367;
    pub const SOURCES_DEPRECATED: usize = 383;
    pub const SOURCES_RULE: usize = 399;
    pub const N: usize = 415;
}

/// `struct icmp6stat`'s fields; the histograms have 256 entries.
pub mod icp6s {
    pub const ERROR: usize = 0;
    pub const CANTERROR: usize = 1;
    pub const TOOFREQ: usize = 2;
    pub const OUTHIST: usize = 3;
    pub const BADCODE: usize = 259;
    pub const TOOSHORT: usize = 260;
    pub const CHECKSUM: usize = 261;
    pub const BADLEN: usize = 262;
    pub const DROPPED: usize = 263;
    pub const REFLECT: usize = 264;
    pub const INHIST: usize = 265;
    pub const ND_TOOMANYOPT: usize = 521;
    pub const ODST_UNREACH_NOROUTE: usize = 522;
    pub const ODST_UNREACH_ADMIN: usize = 523;
    pub const ODST_UNREACH_BEYONDSCOPE: usize = 524;
    pub const ODST_UNREACH_ADDR: usize = 525;
    pub const ODST_UNREACH_NOPORT: usize = 526;
    pub const OPACKET_TOO_BIG: usize = 527;
    pub const OTIME_EXCEED_TRANSIT: usize = 528;
    pub const OTIME_EXCEED_REASSEMBLY: usize = 529;
    pub const OPARAMPROB_HEADER: usize = 530;
    pub const OPARAMPROB_NEXTHEADER: usize = 531;
    pub const OPARAMPROB_OPTION: usize = 532;
    pub const OREDIRECT: usize = 533;
    pub const OUNKNOWN: usize = 534;
    pub const PMTUCHG: usize = 535;
    pub const ND_BADOPT: usize = 536;
    pub const BADNS: usize = 537;
    pub const BADNA: usize = 538;
    pub const BADRS: usize = 539;
    pub const BADRA: usize = 540;
    pub const BADREDIRECT: usize = 541;
    pub const OVERFLOWDEFRTR: usize = 542;
    pub const OVERFLOWPRFX: usize = 543;
    pub const OVERFLOWNNDP: usize = 544;
    pub const OVERFLOWREDIRECT: usize = 545;
    pub const INVLHLIM: usize = 546;
    /// With `icp6s_spare[32]`.
    pub const N: usize = 579;
}

/// `struct rip6stat` (`net.inet6.ip6.rip6stats`).
pub mod rip6s {
    pub const IPACKETS: usize = 0;
    pub const ISUM: usize = 1;
    pub const BADSUM: usize = 2;
    pub const NOSOCK: usize = 3;
    pub const NOSOCKMCAST: usize = 4;
    pub const FULLSOCK: usize = 5;
    pub const OPACKETS: usize = 6;
    pub const N: usize = 7;
}

/// The per-VNET IPv6 settings (`net.inet6.ip6.*`, `net.inet6.icmp6.*`,
/// `net.inet6.mld.*`), FreeBSD's defaults.
#[derive(Debug)]
pub struct Conf6 {
    pub forwarding: AtomicI32,
    /// `net.inet6.ip6.redirect` (`ip6_sendredirects`).
    pub redirect: AtomicI32,
    pub defmcasthlim: AtomicI32,
    pub accept_rtadv: AtomicI32,
    pub no_radr: AtomicI32,
    pub norbit_raif: AtomicI32,
    pub rfc6204w3: AtomicI32,
    pub hdrnestlimit: AtomicI32,
    pub dad_count: AtomicI32,
    pub auto_flowlabel: AtomicI32,
    pub use_deprecated: AtomicI32,
    pub use_tempaddr: AtomicI32,
    pub prefer_tempaddr: AtomicI32,
    /// `net.inet6.ip6.source_address_validation`.
    pub sav: AtomicI32,
    pub log_cannot_forward: AtomicI32,
    pub maxfragpackets: AtomicI32,
    pub maxfragbucketsize: AtomicI32,
    pub maxfragsperpacket: AtomicI32,
    /// `net.inet6.ip6.fraglifetime_ms`.
    pub fraglifetime_ms: AtomicI32,
    /// `net.inet6.icmp6.*`.
    pub rediraccept: AtomicI32,
    pub redirtimeout: AtomicI32,
    pub nodeinfo: AtomicI32,
    pub errppslimit: AtomicU32,
    pub icmp6lim_jitter: AtomicU32,
    pub icmp6lim_output: AtomicI32,
    pub nd6_prune: AtomicI32,
    pub nd6_delay: AtomicI32,
    pub nd6_umaxtries: AtomicI32,
    pub nd6_mmaxtries: AtomicI32,
    pub nd6_useloopback: AtomicI32,
    pub nd6_gctimer: AtomicI32,
    pub nd6_maxndopt: AtomicI32,
    pub nd6_maxqueuelen: AtomicI32,
    pub nd6_debug: AtomicI32,
    pub nd6_onlink_ns_rfc4861: AtomicI32,
    pub dad_enhanced: AtomicI32,
    pub dad_maxtry: AtomicI32,
    pub nd6_ignore_ipv6_only_ra: AtomicI32,
    /// `net.inet6.mld.v2enable`, `v1enable`, `use_allow`.
    pub mld_v2enable: AtomicI32,
    pub mld_v1enable: AtomicI32,
    pub mld_use_allow: AtomicI32,
}

impl Conf6 {
    const fn new() -> Self {
        Self {
            forwarding: AtomicI32::new(0),
            redirect: AtomicI32::new(1),
            defmcasthlim: AtomicI32::new(1),
            accept_rtadv: AtomicI32::new(0),
            no_radr: AtomicI32::new(0),
            norbit_raif: AtomicI32::new(0),
            rfc6204w3: AtomicI32::new(0),
            hdrnestlimit: AtomicI32::new(15),
            dad_count: AtomicI32::new(1),
            auto_flowlabel: AtomicI32::new(1),
            use_deprecated: AtomicI32::new(1),
            use_tempaddr: AtomicI32::new(0),
            prefer_tempaddr: AtomicI32::new(0),
            sav: AtomicI32::new(1),
            log_cannot_forward: AtomicI32::new(1),
            maxfragpackets: AtomicI32::new(0),
            maxfragbucketsize: AtomicI32::new(1),
            maxfragsperpacket: AtomicI32::new(64),
            fraglifetime_ms: AtomicI32::new(60_000),
            rediraccept: AtomicI32::new(1),
            redirtimeout: AtomicI32::new(600),
            nodeinfo: AtomicI32::new(0),
            errppslimit: AtomicU32::new(100),
            icmp6lim_jitter: AtomicU32::new(8),
            icmp6lim_output: AtomicI32::new(1),
            nd6_prune: AtomicI32::new(1),
            nd6_delay: AtomicI32::new(5),
            nd6_umaxtries: AtomicI32::new(3),
            nd6_mmaxtries: AtomicI32::new(3),
            nd6_useloopback: AtomicI32::new(1),
            nd6_gctimer: AtomicI32::new(86_400),
            nd6_maxndopt: AtomicI32::new(10),
            nd6_maxqueuelen: AtomicI32::new(16),
            nd6_debug: AtomicI32::new(0),
            nd6_onlink_ns_rfc4861: AtomicI32::new(0),
            dad_enhanced: AtomicI32::new(1),
            dad_maxtry: AtomicI32::new(15),
            nd6_ignore_ipv6_only_ra: AtomicI32::new(1),
            mld_v2enable: AtomicI32::new(1),
            mld_v1enable: AtomicI32::new(1),
            mld_use_allow: AtomicI32::new(1),
        }
    }
}

/// Reads a setting's value (for the kernel's handlers).
#[must_use]
pub fn val_of(a: &AtomicI32) -> i32 {
    val(a)
}

/// Reads a setting's value.
pub(crate) fn val(a: &AtomicI32) -> i32 {
    // Relaxed: a setting read as FreeBSD reads its `V_` variable.
    a.load(Ordering::Relaxed)
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
    pub const TCP_RST: usize = 3;
    pub const N: usize = 4;
    /// `icmp_rate_descrs`.
    pub const NAMES: [&str; N] = ["icmp unreach", "icmp ping", "icmp tstamp", "tcp reset"];
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
    pub ip6stat: Counters<{ ip6s::N }>,
    pub icmp6stat: Counters<{ icp6s::N }>,
    pub rip6stat: Counters<{ rip6s::N }>,
    pub conf6: Conf6,
    /// IPv6's reassembly queues.
    pub(crate) frag6: SpinLock<crate::frag6::Queues>,
    /// ICMPv6's rate limits (`icmp6_rates`).
    pub(crate) rates6: SpinLock<[Rate; crate::icmp6::RATELIM_MAX]>,
    /// Neighbor discovery's and MLD's state, taken after the interfaces'
    /// lock and never before it.
    pub(crate) nd6: SpinLock<crate::nd6::Nd6>,
    /// TCP's tunables, statistics, host cache, SYN cache, and secrets.
    pub tcp: crate::tcp::Vtcp,
    /// The address selection policy table (`addrsel_policytab`), which
    /// `ip6addrctl(8)` installs.
    pub(crate) policy: SpinLock<Vec<abi::net::In6Addrpolicy>>,
}

impl State {
    /// `frag6_nfragpackets`: the packets IPv6 is reassembling.
    #[must_use]
    pub fn frag6_len(&self) -> usize {
        self.frag6.lock().len()
    }
}

// Names every field, so one added to `State` must be added to
// `new_state` too.
const _: fn(State) = |State {
                          ipstat: _,
                          icmpstat: _,
                          udpstat: _,
                          conf: _,
                          reass: _,
                          id: _,
                          rates: _,
                          groups: _,
                          ip6stat: _,
                          icmp6stat: _,
                          rip6stat: _,
                          conf6: _,
                          frag6: _,
                          rates6: _,
                          nd6: _,
                          tcp: _,
                          policy: _,
                      }| {};

/// A new VNET's state, made in place: at 11 KB it is too big for the
/// kernel stacks of the paths that first use it.
#[inline(never)]
fn new_state() -> Arc<State> {
    let mut a = Arc::<State>::new_uninit();
    let p = Arc::get_mut(&mut a)
        .unwrap_or_else(|| unreachable!("just made"))
        .as_mut_ptr();
    // SAFETY: `p` is the new allocation's, aligned and unique; each
    // field is written once, so every one is initialized below.
    unsafe {
        (&raw mut (*p).ipstat).write(Counters::new());
        (&raw mut (*p).icmpstat).write(Counters::new());
        (&raw mut (*p).udpstat).write(Counters::new());
        (&raw mut (*p).conf).write(Conf::new());
        (&raw mut (*p).reass).write(SpinLock::new(crate::reass::Queues::new()));
        (&raw mut (*p).id).write(AtomicU32::new(crate::random()));
        (&raw mut (*p).rates).write(SpinLock::new([Rate::default(); bandlim::N]));
        (&raw mut (*p).groups).write(SpinLock::new(Vec::new()));
        (&raw mut (*p).ip6stat).write(Counters::new());
        (&raw mut (*p).icmp6stat).write(Counters::new());
        (&raw mut (*p).rip6stat).write(Counters::new());
        (&raw mut (*p).conf6).write(Conf6::new());
        (&raw mut (*p).frag6).write(SpinLock::new(crate::frag6::Queues::new()));
        (&raw mut (*p).rates6).write(SpinLock::new([Rate::default(); crate::icmp6::RATELIM_MAX]));
        (&raw mut (*p).nd6).write(SpinLock::new(crate::nd6::Nd6::new()));
        (&raw mut (*p).tcp).write(crate::tcp::Vtcp::new());
        (&raw mut (*p).policy).write(SpinLock::new(Vec::new()));
    }
    // SAFETY: every field was written above.
    unsafe { a.assume_init() }
}

/// `vnet`'s state, made on first use, borrowed from it: no reference
/// count to touch on a path every packet takes.
#[must_use]
pub fn of(vnet: &Vnet) -> &State {
    let any = match vnet.inet.get() {
        Some(a) => a,
        None => {
            let s = new_state();
            crate::frag6::init_limits(&s);
            let s: Arc<dyn core::any::Any + Send + Sync> = s;
            // A racing first use makes one too; one of them is kept.
            let _ = vnet.inet.set(s);
            vnet.inet.get().unwrap_or_else(|| unreachable!("just set"))
        }
    };
    any.downcast_ref::<State>()
        .unwrap_or_else(|| unreachable!("only this crate sets it"))
}

/// [`of`], counted: for a holder that outlives the borrow.
#[must_use]
pub fn of_arc(vnet: &Vnet) -> Arc<State> {
    let _ = of(vnet);
    let any = vnet
        .inet
        .get()
        .map(Arc::clone)
        .unwrap_or_else(|| unreachable!("made above"));
    any.downcast::<State>()
        .unwrap_or_else(|_| unreachable!("only this crate sets it"))
}
