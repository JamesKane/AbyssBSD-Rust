// SPDX-License-Identifier: BSD-2-Clause
//! TCP's statistics (`struct tcpstat`, `net.inet.tcp.stats`), and the
//! connections in each state (`net.inet.tcp.states`).
//!
//! Behavior reference: sys/netinet/tcp_var.h.

use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

/// `struct tcpstat`'s fields, by index (its `uint64_t` words in order).
pub mod tcps {
    pub const CONNATTEMPT: usize = 0;
    pub const ACCEPTS: usize = 1;
    pub const CONNECTS: usize = 2;
    pub const DROPS: usize = 3;
    pub const CONNDROPS: usize = 4;
    pub const MINMSSDROPS: usize = 5;
    pub const CLOSED: usize = 6;
    pub const SEGSTIMED: usize = 7;
    pub const RTTUPDATED: usize = 8;
    pub const DELACK: usize = 9;
    pub const TIMEOUTDROP: usize = 10;
    pub const REXMTTIMEO: usize = 11;
    pub const PERSISTTIMEO: usize = 12;
    pub const KEEPTIMEO: usize = 13;
    pub const KEEPPROBE: usize = 14;
    pub const KEEPDROPS: usize = 15;
    pub const PROGDROPS: usize = 16;
    pub const SNDTOTAL: usize = 17;
    pub const SNDPACK: usize = 18;
    pub const SNDBYTE: usize = 19;
    pub const SNDREXMITPACK: usize = 20;
    pub const SNDREXMITBYTE: usize = 21;
    pub const SNDREXMITBAD: usize = 22;
    pub const SNDACKS: usize = 23;
    pub const SNDPROBE: usize = 24;
    pub const SNDURG: usize = 25;
    pub const SNDWINUP: usize = 26;
    pub const SNDCTRL: usize = 27;
    pub const RCVTOTAL: usize = 28;
    pub const RCVPACK: usize = 29;
    pub const RCVBYTE: usize = 30;
    pub const RCVBADSUM: usize = 31;
    pub const RCVBADOFF: usize = 32;
    pub const RCVREASSFULL: usize = 33;
    pub const RCVSHORT: usize = 34;
    pub const RCVDUPPACK: usize = 35;
    pub const RCVDUPBYTE: usize = 36;
    pub const RCVPARTDUPPACK: usize = 37;
    pub const RCVPARTDUPBYTE: usize = 38;
    pub const RCVOOPACK: usize = 39;
    pub const RCVOOBYTE: usize = 40;
    pub const RCVPACKAFTERWIN: usize = 41;
    pub const RCVBYTEAFTERWIN: usize = 42;
    pub const RCVAFTERCLOSE: usize = 43;
    pub const RCVWINPROBE: usize = 44;
    pub const RCVDUPACK: usize = 45;
    pub const RCVACKTOOMUCH: usize = 46;
    pub const RCVACKPACK: usize = 47;
    pub const RCVACKBYTE: usize = 48;
    pub const RCVWINUPD: usize = 49;
    pub const PAWSDROP: usize = 50;
    pub const PREDACK: usize = 51;
    pub const PREDDAT: usize = 52;
    pub const PCBCACHEMISS: usize = 53;
    pub const CACHEDRTT: usize = 54;
    pub const CACHEDRTTVAR: usize = 55;
    pub const CACHEDSSTHRESH: usize = 56;
    pub const USEDRTT: usize = 57;
    pub const USEDRTTVAR: usize = 58;
    pub const USEDSSTHRESH: usize = 59;
    pub const PERSISTDROP: usize = 60;
    pub const BADSYN: usize = 61;
    pub const MTURESENT: usize = 62;
    pub const LISTENDROP: usize = 63;
    pub const BADRST: usize = 64;
    pub const SC_ADDED: usize = 65;
    pub const SC_RETRANSMITTED: usize = 66;
    pub const SC_DUPSYN: usize = 67;
    pub const SC_DROPPED: usize = 68;
    pub const SC_COMPLETED: usize = 69;
    pub const SC_BUCKETOVERFLOW: usize = 70;
    pub const SC_CACHEOVERFLOW: usize = 71;
    pub const SC_RESET: usize = 72;
    pub const SC_STALE: usize = 73;
    pub const SC_ABORTED: usize = 74;
    pub const SC_BADACK: usize = 75;
    pub const SC_UNREACH: usize = 76;
    pub const SC_ZONEFAIL: usize = 77;
    pub const SC_SENDCOOKIE: usize = 78;
    pub const SC_RECVCOOKIE: usize = 79;
    pub const SC_SPURCOOKIE: usize = 80;
    pub const SC_FAILCOOKIE: usize = 81;
    pub const HC_ADDED: usize = 82;
    pub const HC_BUCKETOVERFLOW: usize = 83;
    pub const FINWAIT2_DROPS: usize = 84;
    pub const SACK_RECOVERY_EPISODE: usize = 85;
    pub const SACK_REXMITS: usize = 86;
    pub const SACK_REXMITS_TSO: usize = 87;
    pub const SACK_REXMIT_BYTES: usize = 88;
    pub const SACK_RCV_BLOCKS: usize = 89;
    pub const SACK_SEND_BLOCKS: usize = 90;
    pub const SACK_LOSTREXMT: usize = 91;
    pub const SACK_SBOVERFLOW: usize = 92;
    pub const ECN_RCVCE: usize = 93;
    pub const ECN_RCVECT0: usize = 94;
    pub const ECN_RCVECT1: usize = 95;
    pub const ECN_SHS: usize = 96;
    pub const ECN_RCWND: usize = 97;
    pub const SIG_RCVGOODSIG: usize = 98;
    pub const SIG_RCVBADSIG: usize = 99;
    pub const SIG_ERR_BUILDSIG: usize = 100;
    pub const SIG_ERR_SIGOPT: usize = 101;
    pub const SIG_ERR_NOSIGOPT: usize = 102;
    pub const PMTUD_BLACKHOLE_ACTIVATED: usize = 103;
    pub const PMTUD_BLACKHOLE_ACTIVATED_MIN_MSS: usize = 104;
    pub const PMTUD_BLACKHOLE_FAILED: usize = 105;
    pub const TUNNELED_PKTS: usize = 106;
    pub const TUNNELED_ERRS: usize = 107;
    pub const DSACK_COUNT: usize = 108;
    pub const DSACK_BYTES: usize = 109;
    pub const DSACK_TLP_BYTES: usize = 110;
    pub const TW_RECYCLES: usize = 111;
    pub const TW_RESETS: usize = 112;
    pub const TW_RESPONDS: usize = 113;
    pub const ACE_NECT: usize = 114;
    pub const ACE_ECT1: usize = 115;
    pub const ACE_ECT0: usize = 116;
    pub const ACE_CE: usize = 117;
    pub const ECN_SNDECT0: usize = 118;
    pub const ECN_SNDECT1: usize = 119;
    pub const TLPRESENDS: usize = 120;
    pub const TLPRESEND_BYTES: usize = 121;
    pub const RCVGHOSTACK: usize = 122;
    pub const RCVACKTOOOLD: usize = 123;
    /// The words in `struct tcpstat` (with its one pad word).
    pub const N: usize = 125;
}

/// A VNET's TCP statistics.
#[derive(Debug)]
pub struct Stats {
    /// Per CPU, as `VNET_PCPUSTAT_DEFINE`'s.
    words: sync::PerCpuStats<{ tcps::N }>,
    /// `tcps_states`: connections in each `TCPS_*`.
    states: [AtomicU64; crate::State::COUNT],
    /// `V_tcp_sack_globalholes` (`net.inet.tcp.sack.globalholes`).
    pub sack_globalholes: AtomicI32,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    #[must_use]
    pub fn new() -> Self {
        Self {
            words: sync::PerCpuStats::new(),
            states: [const { AtomicU64::new(0) }; crate::State::COUNT],
            sack_globalholes: AtomicI32::new(0),
        }
    }

    /// `TCPSTAT_INC`.
    pub fn inc(&self, i: usize) {
        self.add(i, 1);
    }

    /// `TCPSTAT_ADD`.
    pub fn add(&self, i: usize, n: u64) {
        self.words.add(i, n);
    }

    #[must_use]
    pub fn get(&self, i: usize) -> u64 {
        self.words.get(i)
    }

    /// The statistics as `struct tcpstat`'s words.
    #[must_use]
    pub fn snapshot(&self) -> [u64; tcps::N] {
        core::array::from_fn(|i| self.words.get(i))
    }

    /// Zeroes them (a write to `net.inet.tcp.stats`).
    pub fn clear(&self) {
        self.words.clear();
    }

    /// `TCPSTATES_INC`.
    pub fn state_inc(&self, s: crate::State) {
        // Relaxed: a statistic.
        self.states[s.number() as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// `TCPSTATES_DEC`.
    pub fn state_dec(&self, s: crate::State) {
        // Relaxed: a statistic.
        self.states[s.number() as usize].fetch_sub(1, Ordering::Relaxed);
    }

    /// `net.inet.tcp.states`.
    #[must_use]
    pub fn states(&self) -> [u64; crate::State::COUNT] {
        // Relaxed: statistics.
        core::array::from_fn(|i| self.states[i].load(Ordering::Relaxed))
    }
}
