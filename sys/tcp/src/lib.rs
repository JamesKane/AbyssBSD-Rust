// SPDX-License-Identifier: BSD-2-Clause
//! TCP (docs/NETWORKING.md §9.2): FreeBSD 15.1's default stack, with its
//! defaults.
//!
//! This crate holds the protocol and nothing of the kernel: a connection's
//! control block ([`Tcpcb`]), what a segment does to it (`tcp_input`),
//! what it sends (`tcp_output`), its timers, the reassembly queue, SACK,
//! congestion control (NewReno and CUBIC), the SYN cache with cookies,
//! and the host cache. The layers around it come in through traits and
//! values:
//!
//! - the socket's buffers and its state changes, through [`Sock`];
//! - the clock, the tunables, and the statistics, through [`Ctx`];
//! - the network, as segments ([`Seg`]) a call leaves in [`Ctx::out`],
//!   which the caller sends after it lets go of the connection's lock,
//!   as FreeBSD's loopback and resolution queues would defer them.
//!
//! So it runs under `cargo test` against a simulated network (`sim`),
//! where a lost SYN-ACK or a SACK hole replays exactly.
//!
//! Time is in ticks of a millisecond (FreeBSD's `hz` of 1000), which the
//! timestamps use as well.
//!
//! Behavior reference: sys/netinet/tcp_input.c, tcp_output.c,
//! tcp_timer.c, tcp_subr.c, tcp_sack.c, tcp_reass.c, tcp_syncache.c,
//! tcp_hostcache.c, tcp_ecn.c, sys/netinet/cc/.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "sequence space is modulo 2^32 (tcp_seq.h's casts), and windows, \
              MSSes and urgent pointers are 16-bit fields of 32-bit counts"
)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod cc;
pub mod ecn;
pub mod hdr;
pub mod hostcache;
mod input;
pub mod isn;
mod output;
mod reass;
pub mod sack;
pub mod seq;
#[cfg(test)]
mod sim;
pub mod stats;
pub mod syncache;
mod tcb;
#[cfg(test)]
mod tests;
pub mod timer;
mod usrreq;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

pub use abi::errno::Errno;
pub use input::{Segment, TwVerdict, reset_for};
pub use mbuf::{Chain, Packet};
pub use stats::{Stats, tcps};
pub use tcb::{Inc, Path, Tcpcb, TsoCap, build, oob, tf, tf2};
pub use usrreq::{Notify, next_mtu_down};

/// A connection's state (`t_state`, `TCPS_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    CloseWait,
    FinWait1,
    Closing,
    LastAck,
    FinWait2,
    TimeWait,
}

impl State {
    /// `TCP_NSTATES`.
    pub const COUNT: usize = 11;

    /// Its `TCPS_*`.
    #[must_use]
    pub const fn number(self) -> i32 {
        self as i32
    }

    /// `TCPS_HAVERCVDSYN`.
    #[must_use]
    pub const fn have_rcvd_syn(self) -> bool {
        self as i32 >= Self::SynReceived as i32
    }

    /// `TCPS_HAVEESTABLISHED`.
    #[must_use]
    pub const fn have_established(self) -> bool {
        self as i32 >= Self::Established as i32
    }

    /// `TCPS_HAVERCVDFIN`.
    #[must_use]
    pub const fn have_rcvd_fin(self) -> bool {
        matches!(
            self,
            Self::CloseWait | Self::Closing | Self::LastAck | Self::TimeWait
        )
    }

    /// Past `CLOSE_WAIT`: this side has sent its FIN (or will).
    #[must_use]
    pub const fn fin_sent(self) -> bool {
        self as i32 > Self::CloseWait as i32
    }

    /// `tcp_outflags`: the flags a segment sent in it carries.
    #[must_use]
    pub const fn outflags(self) -> u16 {
        use hdr::th;
        match self {
            Self::Closed => th::RST | th::ACK,
            Self::Listen => 0,
            Self::SynSent => th::SYN,
            Self::SynReceived => th::SYN | th::ACK,
            Self::Established | Self::CloseWait | Self::FinWait2 | Self::TimeWait => th::ACK,
            Self::FinWait1 | Self::Closing | Self::LastAck => th::FIN | th::ACK,
        }
    }

    /// `tcpstates[]`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::Listen => "LISTEN",
            Self::SynSent => "SYN_SENT",
            Self::SynReceived => "SYN_RCVD",
            Self::Established => "ESTABLISHED",
            Self::CloseWait => "CLOSE_WAIT",
            Self::FinWait1 => "FIN_WAIT_1",
            Self::Closing => "CLOSING",
            Self::LastAck => "LAST_ACK",
            Self::FinWait2 => "FIN_WAIT_2",
            Self::TimeWait => "TIME_WAIT",
        }
    }
}

/// `SO_*` bits TCP looks at in `so_options`.
pub mod so {
    pub const DEBUG: i32 = 0x0001;
    pub const KEEPALIVE: i32 = 0x0008;
    pub const DONTROUTE: i32 = 0x0010;
    pub const LINGER: i32 = 0x0080;
    pub const OOBINLINE: i32 = 0x0100;
}

/// What TCP needs of its socket (`struct socket` and its buffers), which
/// the caller holds locked for the call.
///
/// The send buffer holds what the user wrote and the peer has not
/// acknowledged, starting at `snd_una` (a SYN or FIN takes no room in
/// it); the receive buffer holds what arrived in order and the user has
/// not read.
pub trait Sock {
    /// `sbavail(&so->so_snd)`.
    fn snd_cc(&self) -> u32;
    /// `so_snd.sb_hiwat`.
    fn snd_hiwat(&self) -> u32;
    /// `so_snd.sb_flags & SB_AUTOSIZE`.
    fn snd_autosize(&self) -> bool;
    /// `sbreserve` of the send buffer; false if over `sb_max`.
    fn snd_reserve(&mut self, hiwat: u32) -> bool;
    /// Clears `SB_AUTOSIZE` on the send buffer.
    fn snd_autosize_off(&mut self);
    /// `m_copym` of `len` bytes from `off` in the send buffer, by
    /// reference, from the send pointer (`sbsndptr_noadv`) when it is at
    /// or before `off`; `None` out of buffers.
    fn snd_copy(&mut self, off: u32, len: u32) -> Option<Chain>;
    /// `sbdrop`: the first `n` bytes were acknowledged; wakes writers.
    fn snd_drop(&mut self, n: u32);
    /// `SBS_CANTSENDMORE` on the send buffer.
    fn cantsendmore(&self) -> bool;

    /// `sbavail(&so->so_rcv)`.
    fn rcv_cc(&self) -> u32;
    /// `so_rcv.sb_hiwat`.
    fn rcv_hiwat(&self) -> u32;
    /// `sbspace(&so->so_rcv)`: room left, which may be negative.
    fn rcv_space(&self) -> i64;
    /// `so_rcv.sb_mbcnt` and `sb_mbmax`: the mbuf budget, which the
    /// reassembly queue shares.
    fn rcv_mbcnt(&self) -> u32;
    fn rcv_mbmax(&self) -> u32;
    /// `so_rcv.sb_flags & SB_AUTOSIZE`.
    fn rcv_autosize(&self) -> bool;
    /// `sbreserve` of the receive buffer; false if over `sb_max`.
    fn rcv_reserve(&mut self, hiwat: u32) -> bool;
    /// Clears `SB_AUTOSIZE` on the receive buffer.
    fn rcv_autosize_off(&mut self);
    /// `sbappendstream`: data that arrived in order; wakes readers.
    fn rcv_append(&mut self, data: Chain);
    /// `socantrcvmore`: the peer's FIN.
    fn cantrcvmore(&mut self);
    /// `SBS_CANTRCVMORE` on the receive buffer.
    fn rcv_closed(&self) -> bool;

    /// `so_options`.
    fn options(&self) -> i32;
    /// `so_state & SS_NOFDREF`: the user closed it.
    fn nofdref(&self) -> bool;
    /// `soisconnected`.
    fn connected(&mut self);
    /// `soisdisconnecting`.
    fn disconnecting(&mut self);
    /// `soisdisconnected`.
    fn disconnected(&mut self);
    /// `so_error` (and the wakeups that report it); `None` clears it.
    fn set_error(&mut self, e: Option<Errno>);
    /// `sohasoutofband` with the urgent byte's offset in the receive
    /// buffer (`so_oobmark`) set.
    fn urgent(&mut self, oobmark: u32);
    /// The urgent byte taken out of line (`t_iobc`, `TCPOOB_HAVEDATA`).
    fn oob_byte(&mut self, c: u8);
    /// Wakes writers: room in the send buffer (`sowwakeup`).
    fn wake_writers(&mut self);
}

/// The tunables (`net.inet.tcp.*`), per VNET, as FreeBSD's GENERIC sets
/// them; read with relaxed loads, as FreeBSD reads its `V_` variables.
#[derive(Debug)]
pub struct Conf {
    pub mssdflt: AtomicI32,
    pub v6mssdflt: AtomicI32,
    pub minmss: AtomicI32,
    pub rfc1323: AtomicI32,
    pub sack_enable: AtomicI32,
    pub sack_maxholes: AtomicI32,
    pub sack_globalmaxholes: AtomicI32,
    pub ecn_enable: AtomicI32,
    pub ecn_maxretries: AtomicI32,
    pub delayed_ack: AtomicI32,
    pub delacktime: AtomicI32,
    pub rfc3042: AtomicI32,
    pub rfc3390: AtomicI32,
    pub rfc3465: AtomicI32,
    pub abc_l_var: AtomicI32,
    pub initcwnd_segments: AtomicI32,
    pub do_prr: AtomicI32,
    pub newcwv: AtomicI32,
    pub do_lrd: AtomicI32,
    pub drop_synfin: AtomicI32,
    pub log_in_vain: AtomicI32,
    pub insecure_rst: AtomicI32,
    pub insecure_syn: AtomicI32,
    pub insecure_ack: AtomicI32,
    pub tolerate_missing_ts: AtomicI32,
    pub ts_offset_per_conn: AtomicI32,
    pub path_mtu_discovery: AtomicI32,
    pub tso: AtomicI32,
    pub sendspace: AtomicI32,
    pub recvspace: AtomicI32,
    pub sendbuf_auto: AtomicI32,
    pub sendbuf_inc: AtomicI32,
    pub sendbuf_max: AtomicI32,
    pub sendbuf_auto_lowat: AtomicI32,
    pub recvbuf_auto: AtomicI32,
    pub recvbuf_max: AtomicI32,
    pub keepinit: AtomicI32,
    pub keepidle: AtomicI32,
    pub keepintvl: AtomicI32,
    pub keepcnt: AtomicI32,
    pub always_keepalive: AtomicI32,
    pub msl: AtomicI32,
    pub msl_local: AtomicI32,
    pub nolocaltimewait: AtomicI32,
    pub finwait2_timeout: AtomicI32,
    pub fast_finwait2_recycle: AtomicI32,
    pub rexmit_initial: AtomicI32,
    pub rexmit_min: AtomicI32,
    pub rexmit_max: AtomicI32,
    pub rexmit_slop: AtomicI32,
    pub retries: AtomicI32,
    pub persmin: AtomicI32,
    pub persmax: AtomicI32,
    pub maxpersistidle: AtomicI32,
    pub ack_war_timewindow: AtomicI32,
    pub ack_war_cnt: AtomicI32,
    pub syncookies: AtomicI32,
    pub syncookies_only: AtomicI32,
    pub udp_tunneling_port: AtomicI32,
    pub blackhole: AtomicI32,
    pub isn_reseed_interval: AtomicI32,
    pub icmp_may_rst: AtomicI32,
    pub pmtud_blackhole_detect: AtomicI32,
    pub pmtud_blackhole_mss: AtomicI32,
    pub v6pmtud_blackhole_mss: AtomicI32,
    pub maxunacktime: AtomicI32,
    /// `kern.ipc.maxsockbuf` (`sb_max`), which the caller keeps current.
    pub sb_max: AtomicI32,
}

impl Default for Conf {
    fn default() -> Self {
        Self::new()
    }
}

macro_rules! a {
    ($v:expr) => {
        AtomicI32::new($v)
    };
}

impl Conf {
    /// FreeBSD 15.1's defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mssdflt: a!(536),
            v6mssdflt: a!(1220),
            minmss: a!(216),
            rfc1323: a!(1),
            sack_enable: a!(1),
            sack_maxholes: a!(128),
            sack_globalmaxholes: a!(65536),
            ecn_enable: a!(2),
            ecn_maxretries: a!(1),
            delayed_ack: a!(1),
            delacktime: a!(timer::DELACK as i32),
            rfc3042: a!(1),
            rfc3390: a!(1),
            rfc3465: a!(1),
            abc_l_var: a!(2),
            initcwnd_segments: a!(10),
            do_prr: a!(1),
            newcwv: a!(0),
            do_lrd: a!(1),
            drop_synfin: a!(0),
            log_in_vain: a!(0),
            insecure_rst: a!(0),
            insecure_syn: a!(0),
            insecure_ack: a!(0),
            tolerate_missing_ts: a!(1),
            ts_offset_per_conn: a!(1),
            path_mtu_discovery: a!(1),
            tso: a!(1),
            sendspace: a!(32768),
            recvspace: a!(65536),
            sendbuf_auto: a!(1),
            sendbuf_inc: a!(8192),
            sendbuf_max: a!(8 * 1024 * 1024),
            sendbuf_auto_lowat: a!(0),
            recvbuf_auto: a!(1),
            recvbuf_max: a!(8 * 1024 * 1024),
            keepinit: a!(timer::KEEP_INIT as i32),
            keepidle: a!(timer::KEEP_IDLE as i32),
            keepintvl: a!(timer::KEEPINTVL as i32),
            keepcnt: a!(timer::KEEPCNT as i32),
            always_keepalive: a!(1),
            msl: a!(timer::MSL as i32),
            msl_local: a!(timer::MSL_LOCAL as i32),
            nolocaltimewait: a!(0),
            finwait2_timeout: a!(timer::FINWAIT2_TIMEOUT as i32),
            fast_finwait2_recycle: a!(0),
            rexmit_initial: a!(timer::RTOBASE as i32),
            rexmit_min: a!(timer::MIN as i32),
            rexmit_max: a!(timer::REXMTMAX as i32),
            rexmit_slop: a!(timer::CPU_VAR as i32),
            retries: a!(timer::MAXRXTSHIFT as i32),
            persmin: a!(timer::PERSMIN as i32),
            persmax: a!(timer::PERSMAX as i32),
            maxpersistidle: a!(timer::KEEP_IDLE as i32),
            ack_war_timewindow: a!(1000),
            ack_war_cnt: a!(5),
            syncookies: a!(1),
            syncookies_only: a!(0),
            udp_tunneling_port: a!(0),
            blackhole: a!(0),
            isn_reseed_interval: a!(0),
            icmp_may_rst: a!(1),
            pmtud_blackhole_detect: a!(0),
            pmtud_blackhole_mss: a!(1200),
            v6pmtud_blackhole_mss: a!(1220),
            maxunacktime: a!(0),
            sb_max: a!(8 * 1024 * 1024),
        }
    }
}

/// Reads a tunable.
#[inline]
#[must_use]
pub fn get(a: &AtomicI32) -> i32 {
    // Relaxed: a setting read as FreeBSD reads its `V_` variable.
    a.load(Ordering::Relaxed)
}

/// Reads a tunable that is a flag.
#[inline]
#[must_use]
pub fn on(a: &AtomicI32) -> bool {
    get(a) != 0
}

/// A segment to send: the TCP header with its options and the data
/// after it, with the checksum field zero; the caller puts the IP header
/// in front and has the checksum computed over the pseudo-header.
#[derive(Debug)]
pub struct Seg {
    pub pkt: Packet,
    /// The IP header's ECN bits (`IPTOS_ECN_*`).
    pub ecn: u8,
    /// Sent with path MTU discovery (IPv4's DF).
    pub df: bool,
    /// TSO: the segment size the interface cuts it into, or 0.
    pub tso_segsz: u16,
    /// What sending it moved, for [`Tcpcb::send_failed`] to take back if
    /// it cannot go; `None` for what `tcp_output` would not take back (a
    /// SYN, a reset, a forced byte while persisting, a reply).
    pub sent: Option<Sent>,
}

/// The sequence space a segment took (`tcp_output`'s `len`, its FIN, and
/// whether it was a SACK retransmission), as its error path backs it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sent {
    pub seq: seq::Seq,
    pub len: u32,
    pub fin: bool,
    pub sack: bool,
}

/// `IPV6_FLOWLABEL_MASK`, in host order.
pub const FLOWLABEL_MASK: u32 = 0x000f_ffff;

/// What a call into a connection runs with.
pub struct Ctx<'a> {
    /// The clock, in ticks of a millisecond.
    pub now: u64,
    pub conf: &'a Conf,
    pub stats: &'a Stats,
    /// The random source (`arc4random`).
    pub random: fn() -> u32,
    /// Segments to send once the connection's lock is let go.
    pub out: &'a mut Vec<Seg>,
    /// The VNET's host cache.
    pub hc: &'a hostcache::HostCache,
    /// What the route to a peer allows (`tcp_maxmtu`, `tcp_maxmtu6`).
    pub path: &'a dyn Fn(&Inc) -> Path,
    /// The VNET's secrets for ISNs and timestamp offsets.
    pub secrets: &'a isn::Secrets,
    /// The VNET's ISN offset.
    pub isn: &'a isn::Offset,
}

impl core::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ctx")
            .field("now", &self.now)
            .finish_non_exhaustive()
    }
}

impl Ctx<'_> {
    /// `tcp_new_ts_offset` for `inc`.
    #[must_use]
    pub fn ts_offset(&self, inc: &Inc) -> u32 {
        self.secrets
            .new_ts_offset(inc, on(&self.conf.ts_offset_per_conn))
    }

    /// `tcp_new_isn` for `inc`.
    #[must_use]
    pub fn new_isn(&self, inc: &Inc) -> u32 {
        self.secrets
            .new_isn(inc, self.now, self.isn, (self.random)())
    }

    /// `tcp_ts_getticks`: the timestamp clock.
    #[inline]
    #[must_use]
    pub fn ts_ticks(&self) -> u32 {
        self.now as u32
    }
}

/// What a call left of the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// It goes on.
    Alive,
    /// `tcp_close` (or `tcp_drop`): it ended; the caller takes it out of
    /// the lookups and frees it once its socket is gone.
    Closed,
}
