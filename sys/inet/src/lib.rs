// SPDX-License-Identifier: BSD-2-Clause
//! Internet sockets over loopback (docs/MULTIUSER.md §8.4): `AF_INET` and
//! `AF_INET6` UDP and TCP sockets, their protocol control blocks and the
//! bind and connect rules FreeBSD's `in_pcb.c` and `in6_pcb.c` apply, the
//! loopback interface `lo0` with its addresses and routes, and the
//! records `netstat` and `sockstat` read (`net.inet.*.pcblist`).
//!
//! No packet is built: a datagram goes straight to the socket its
//! destination's lookup finds, and a TCP connection is two sockets whose
//! sends land in each other's receive buffers, flow-controlled by their
//! room, with what does not fit waiting in the sender's send buffer, as
//! FreeBSD's do between acknowledgements. What a program can see of the
//! protocol (connection states, `ECONNREFUSED` from a closed port, an
//! ICMP port unreachable on a connected UDP socket, half-close, resets,
//! `TIME_WAIT` holding a port, the listen queue's limit, urgent data) is
//! FreeBSD's. There is no wire format, retransmission, or congestion
//! control; those are P7's, with the interfaces that need them.
//!
//! As the UNIX-domain crate does, this one holds state only: the kernel
//! copies user memory and sleeps. An operation that would block returns
//! `EWOULDBLOCK`; the kernel waits on [`Socket::rd`] or [`Socket::wr`]
//! and tries again.
//!
//! Locks: the PCB tables ([`pcb::UDB`], [`pcb::TCB`]) hold every
//! address and port, under one spin lock each (FreeBSD's hash lock);
//! each socket's state is its own spin lock, two taken in address order.
//! A table's lock is never taken with a socket's held, and nothing
//! sleeps or touches user memory under any of them.
//!
//! Behavior reference: sys/netinet/in_pcb.c, sys/netinet6/in6_pcb.c,
//! sys/netinet/udp_usrreq.c, sys/netinet6/udp6_usrreq.c,
//! sys/netinet/tcp_usrreq.c, sys/netinet/tcp_input.c (the states a
//! segment moves a connection through), sys/netinet/tcp_subr.c,
//! sys/netinet/ip_output.c (`ip_ctloutput`), sys/netinet6/ip6_output.c
//! (`ip6_ctloutput`), sys/kern/uipc_socket.c, sys/kern/uipc_sockbuf.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod export;
mod opt;
pub mod pcb;
mod poll;
pub mod splice;
mod tcp;
#[cfg(test)]
mod tests;
mod udp;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::inet::{IPPROTO_TCP, IPPROTO_UDP, ss};
use event::Notify;
use sync::SpinLock;

pub use net::{Family, Vnet, addr, ifnet, netlink, rtmsg};

pub use addr::Sockaddr;
pub use poll::filter;
pub use tcp::Tstate;
pub use udp::{Meta, Received, want};

/// A socket's protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Udp,
    Tcp,
}

impl Proto {
    /// Its `IPPROTO_*`.
    #[must_use]
    pub const fn number(self) -> i32 {
        match self {
            Self::Udp => IPPROTO_UDP,
            Self::Tcp => IPPROTO_TCP,
        }
    }

    /// Its `SOCK_*`.
    #[must_use]
    pub const fn sock_type(self) -> u32 {
        match self {
            Self::Udp => abi::socket::SOCK_DGRAM,
            Self::Tcp => abi::socket::SOCK_STREAM,
        }
    }
}

/// What the caller may do that needs privilege, decided by the kernel's
/// `priv_check` once per call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cred {
    pub uid: u32,
    /// `PRIV_NETINET_RESERVEDPORT`.
    pub reservedport: bool,
    /// `PRIV_NETINET_REUSEPORT`.
    pub reuseport: bool,
    /// `PRIV_NETINET_BINDANY`.
    pub bindany: bool,
}

impl Cred {
    /// The superuser's.
    #[must_use]
    pub const fn root() -> Self {
        Self {
            uid: 0,
            reservedport: true,
            reuseport: true,
            bindany: true,
        }
    }
}

/// The tunables (`net.inet.*`), as sysctls read and set them.
pub mod tune {
    use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64};

    /// `net.inet.ip.portrange.*`.
    pub static LOWFIRST: AtomicI32 = AtomicI32::new(1023);
    pub static LOWLAST: AtomicI32 = AtomicI32::new(600);
    pub static FIRST: AtomicI32 = AtomicI32::new(10000);
    pub static LAST: AtomicI32 = AtomicI32::new(65535);
    pub static HIFIRST: AtomicI32 = AtomicI32::new(49152);
    pub static HILAST: AtomicI32 = AtomicI32::new(65535);
    pub static RESERVEDHIGH: AtomicI32 = AtomicI32::new(1023);
    pub static RESERVEDLOW: AtomicI32 = AtomicI32::new(0);
    pub static RANDOMIZED: AtomicI32 = AtomicI32::new(1);
    /// `net.inet.ip.ttl`, `net.inet6.ip6.hlim`.
    pub static IP_TTL: AtomicI32 = AtomicI32::new(64);
    pub static IP6_HLIM: AtomicI32 = AtomicI32::new(64);
    /// `net.inet6.ip6.v6only`.
    pub static V6ONLY: AtomicI32 = AtomicI32::new(1);
    /// `net.inet.udp.maxdgram`, `net.inet.udp.recvspace`:
    /// `40 * (1024 + sizeof(struct sockaddr_in6))`.
    pub static UDP_SENDSPACE: AtomicU64 = AtomicU64::new(9216);
    pub static UDP_RECVSPACE: AtomicU64 = AtomicU64::new(40 * (1024 + 28));
    /// `net.inet.tcp.sendspace`, `net.inet.tcp.recvspace`.
    pub static TCP_SENDSPACE: AtomicU64 = AtomicU64::new(32 * 1024);
    pub static TCP_RECVSPACE: AtomicU64 = AtomicU64::new(64 * 1024);
    /// `net.inet.tcp.mssdflt`, `net.inet.tcp.v6mssdflt`.
    pub static TCP_MSSDFLT: AtomicU32 = AtomicU32::new(536);
    pub static TCP_V6MSSDFLT: AtomicU32 = AtomicU32::new(1220);
    /// `net.inet.tcp.msl`, ms: `TIME_WAIT` lasts twice it.
    pub static TCP_MSL: AtomicU32 = AtomicU32::new(30_000);
    /// `net.inet.tcp.msl_local`, ms: the MSL between local addresses
    /// (`tcp_eff_msl`), which every connection here is.
    pub static TCP_MSL_LOCAL: AtomicU32 = AtomicU32::new(10);
    /// `net.inet.tcp.keepinit`, `keepidle`, `keepintvl` (ms) and
    /// `keepcnt`.
    pub static TCP_KEEPINIT: AtomicU32 = AtomicU32::new(75_000);
    pub static TCP_KEEPIDLE: AtomicU32 = AtomicU32::new(7_200_000);
    pub static TCP_KEEPINTVL: AtomicU32 = AtomicU32::new(75_000);
    pub static TCP_KEEPCNT: AtomicU32 = AtomicU32::new(8);
    /// `kern.ipc.somaxconn`.
    pub static SOMAXCONN: AtomicI32 = AtomicI32::new(128);
}

/// The clock `TIME_WAIT` and the timestamps run on (nanoseconds, any
/// epoch), and the random source of ephemeral ports; the kernel sets
/// them at boot.
pub struct Hooks {
    pub now: fn() -> u64,
    pub random: fn() -> u32,
}

impl core::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

fn default_now() -> u64 {
    static T: AtomicU64 = AtomicU64::new(0);
    // Relaxed: a counter.
    T.fetch_add(1, Ordering::Relaxed)
}

fn default_random() -> u32 {
    static X: AtomicU32 = AtomicU32::new(0x2545_f491);
    // Relaxed: a counter; xorshift on its value.
    let mut x = X.fetch_add(0x9e37_79b9, Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 17;
    x ^ (x << 5)
}

static HOOKS: SpinLock<Hooks> = SpinLock::new(Hooks {
    now: default_now,
    random: default_random,
});

/// Installs the kernel's clock and random source.
pub fn set_hooks(h: Hooks) {
    *HOOKS.lock() = h;
}

pub(crate) fn now() -> u64 {
    let f = HOOKS.lock().now;
    f()
}

pub(crate) fn random() -> u32 {
    let f = HOOKS.lock().random;
    f()
}

/// `MSIZE`, `MCLBYTES`, `MLEN`, `MINCLSIZE`: what a buffer charges its
/// memory count per mbuf.
const MSIZE: u32 = 256;
const MCLBYTES: u32 = 2048;
const MJUMPAGESIZE: u32 = 4096;
const MINCLSIZE: u32 = 169;
/// `sb_efficiency`, `sb_max` (`kern.ipc.maxsockbuf`) and `sb_max_adj`.
const SB_EFFICIENCY: u32 = 8;
const SB_MAX: u32 = 8 * 1024 * 1024;
#[allow(clippy::cast_possible_truncation, reason = "the quotient fits")]
const SB_MAX_ADJ: u32 = (SB_MAX as u64 * MCLBYTES as u64 / (MSIZE + MCLBYTES) as u64) as u32;

/// The memory `len` bytes of data take in mbufs (`m_uiotombuf`'s
/// clusters, each with its header).
pub(crate) fn mbuf_cost(len: usize) -> u32 {
    let mut left = u32::try_from(len).unwrap_or(u32::MAX);
    let mut cost = 0u32;
    loop {
        let (room, ext) = if left > MCLBYTES {
            (MJUMPAGESIZE, MJUMPAGESIZE)
        } else if left >= MINCLSIZE {
            (MCLBYTES, MCLBYTES)
        } else {
            (224, 0)
        };
        cost = cost.saturating_add(MSIZE + ext);
        left = left.saturating_sub(room);
        if left == 0 {
            return cost;
        }
    }
}

/// A socket buffer's limits and counts (`struct sockbuf`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Sb {
    pub(crate) hiwat: u32,
    pub(crate) lowat: u32,
    pub(crate) mbmax: u32,
    /// Timeout of a wait on it, ns; 0 for none.
    pub(crate) timeo: u64,
    /// Bytes held (`sb_ccc`) and their memory (`sb_mbcnt`).
    pub(crate) cc: u32,
    pub(crate) mbcnt: u32,
    /// `SBS_CANTRCVMORE` (receive) or `SBS_CANTSENDMORE` (send).
    pub(crate) cant: bool,
}

impl Sb {
    /// `sbreserve_locked`.
    pub(crate) fn reserve(&mut self, cc: u32) -> bool {
        if cc > SB_MAX_ADJ {
            return false;
        }
        self.hiwat = cc;
        self.mbmax = cc.saturating_mul(SB_EFFICIENCY).min(SB_MAX);
        if self.lowat > self.hiwat {
            self.lowat = self.hiwat;
        }
        true
    }

    /// `sbspace`.
    pub(crate) fn space(&self) -> u32 {
        let bytes = self.hiwat.saturating_sub(self.cc);
        let mem = self.mbmax.saturating_sub(self.mbcnt);
        bytes.min(mem)
    }
}

/// `IPPROTO_IP` and `IPPROTO_IPV6` state a socket keeps outside its PCB.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IpOpts {
    pub(crate) ttl: u8,
    pub(crate) tos: u8,
    pub(crate) minttl: u8,
    pub(crate) mttl: u8,
    pub(crate) mloop: u8,
    pub(crate) mif: [u8; 4],
    /// `in6p_hops`: -1 for the default.
    pub(crate) hops: i16,
    pub(crate) mhops: i16,
    pub(crate) mloop6: u8,
    pub(crate) mif6: u32,
    pub(crate) tclass: i16,
    pub(crate) prefer_tempaddr: i32,
}

impl IpOpts {
    fn new() -> Self {
        Self {
            ttl: u8::try_from(tune::IP_TTL.load(Ordering::Relaxed)).unwrap_or(64),
            tos: 0,
            minttl: 0,
            mttl: 1,
            mloop: 1,
            mif: [0; 4],
            hops: -1,
            mhops: 1,
            mloop6: 1,
            mif6: 0,
            tclass: -1,
            prefer_tempaddr: -1,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Inner {
    /// `so_state` (`SS_*`).
    pub(crate) state: i16,
    pub(crate) error: Option<Errno>,
    pub(crate) linger: i32,
    pub(crate) rcv: Sb,
    pub(crate) snd: Sb,
    pub(crate) ip: IpOpts,
    /// A UDP socket's received datagrams.
    pub(crate) dgrams: VecDeque<udp::Dgram>,
    /// A TCP socket's received bytes and the ones waiting to go.
    pub(crate) stream: VecDeque<u8>,
    pub(crate) sndq: VecDeque<u8>,
    pub(crate) tcp: Option<alloc::boxed::Box<tcp::Tcb>>,
    pub(crate) user_cookie: u32,
    pub(crate) ts_clock: i32,
    /// The splice this socket is the source of (`so_splice`), and the
    /// one it is the sink of (`so_splice_back`).
    pub(crate) splice: Option<Arc<splice::Splice>>,
    pub(crate) splice_back: Option<Arc<splice::Splice>>,
}

/// An internet socket (FreeBSD's `struct socket` with its `inpcb` and,
/// for TCP, `tcpcb`).
pub struct Socket {
    /// The VNET it was made in (`so_vnet`).
    pub vnet: Arc<Vnet>,
    pub family: Family,
    pub proto: Proto,
    /// Its PCB in its protocol's table.
    pub id: u64,
    /// `so_options` (`SO_*` bits), read by other sockets' binds.
    pub(crate) options: AtomicI32,
    pub(crate) inner: SpinLock<Inner>,
    /// Readers and accepters (`so_rdsel`), and writers (`so_wrsel`).
    pub rd: Notify,
    pub wr: Notify,
    /// `SB_SPLICED` on either buffer ([`splice::RCV`], [`splice::SND`]),
    /// for the wakeups to look at without the lock.
    pub(crate) spliced: core::sync::atomic::AtomicU8,
    /// Bytes spliced from it since its last splice (`so_splice_sent`).
    pub(crate) splice_sent: AtomicU64,
}

impl core::fmt::Debug for Socket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Socket")
            .field("family", &self.family)
            .field("proto", &self.proto)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Locks two sockets' state in address order (once if the same). Bind
/// the [`IrqOff`](sync::IrqOff) first, so it outlives both guards.
pub(crate) fn lock2<'a>(
    a: &'a Socket,
    b: &'a Socket,
) -> (
    sync::IrqOff,
    sync::SpinGuard<'a, Inner>,
    Option<sync::SpinGuard<'a, Inner>>,
) {
    let irq = sync::IrqOff::new();
    if core::ptr::eq(a, b) {
        return (irq, a.inner.lock(), None);
    }
    if core::ptr::from_ref(a).addr() < core::ptr::from_ref(b).addr() {
        let ga = a.inner.lock();
        let gb = b.inner.lock();
        (irq, ga, Some(gb))
    } else {
        let gb = b.inner.lock();
        let ga = a.inner.lock();
        (irq, ga, Some(gb))
    }
}

impl Socket {
    /// A new socket in the system's VNET.
    #[must_use]
    pub fn new(family: Family, proto: Proto, cred: &Cred) -> Arc<Self> {
        Self::new_in(net::vnet0(), family, proto, cred)
    }

    /// A new socket in `vnet` (`socreate` with `udp_attach` or
    /// `tcp_usr_attach`, the creator's VNET as `so_vnet`): its buffers
    /// reserved, its PCB in the table, unbound.
    #[must_use]
    pub fn new_in(vnet: &Arc<Vnet>, family: Family, proto: Proto, cred: &Cred) -> Arc<Self> {
        let (send, recv) = match proto {
            Proto::Udp => (
                tune::UDP_SENDSPACE.load(Ordering::Relaxed),
                tune::UDP_RECVSPACE.load(Ordering::Relaxed),
            ),
            Proto::Tcp => (
                tune::TCP_SENDSPACE.load(Ordering::Relaxed),
                tune::TCP_RECVSPACE.load(Ordering::Relaxed),
            ),
        };
        let mut rcv = Sb::default();
        let mut snd = Sb::default();
        rcv.reserve(u32::try_from(recv).unwrap_or(u32::MAX));
        snd.reserve(u32::try_from(send).unwrap_or(u32::MAX));
        rcv.lowat = 1;
        snd.lowat = MCLBYTES.min(snd.hiwat);
        Self::with_buffers(vnet, family, proto, cred, rcv, snd)
    }

    pub(crate) fn with_buffers(
        vnet: &Arc<Vnet>,
        family: Family,
        proto: Proto,
        cred: &Cred,
        rcv: Sb,
        snd: Sb,
    ) -> Arc<Self> {
        let table = pcb::table(proto);
        let id = table.lock().reserve_id();
        let so = Arc::new(Self {
            vnet: Arc::clone(vnet),
            family,
            proto,
            id,
            options: AtomicI32::new(0),
            inner: SpinLock::new(Inner {
                state: 0,
                error: None,
                linger: 0,
                rcv,
                snd,
                ip: IpOpts::new(),
                dgrams: VecDeque::new(),
                stream: VecDeque::new(),
                sndq: VecDeque::new(),
                tcp: (proto == Proto::Tcp).then(|| alloc::boxed::Box::new(tcp::Tcb::new())),
                user_cookie: 0,
                ts_clock: 0,
                splice: None,
                splice_back: None,
            }),
            rd: Notify::new(),
            wr: Notify::new(),
            spliced: core::sync::atomic::AtomicU8::new(0),
            splice_sent: AtomicU64::new(0),
        });
        table.lock().alloc(&so, cred.uid);
        so
    }

    /// `so_options`.
    pub fn so_options(&self) -> i32 {
        // Relaxed: a word other binds read as FreeBSD's `atomic_load_int`.
        self.options.load(Ordering::Relaxed)
    }

    /// Wakes readers and writers (`sorwakeup`, `sowwakeup`); a spliced
    /// buffer's wakeup is its splice's work instead.
    pub(crate) fn wake(&self) {
        // Relaxed: set under the socket's lock, which whoever changed the
        // buffer this wakeup is for has taken since.
        let spliced = self.spliced.load(Ordering::Relaxed);
        if spliced & splice::RCV == 0 {
            self.rd.notify(0);
        }
        if spliced & splice::SND == 0 {
            self.wr.notify(0);
        }
        if spliced != 0 {
            self.splice_wake();
        }
    }

    /// Whether it is listening (`SOLISTENING`).
    pub fn is_listening(&self) -> bool {
        self.inner
            .lock()
            .tcp
            .as_ref()
            .is_some_and(|t| t.listen.is_some())
    }

    /// Whether it is connected (`SS_ISCONNECTED`).
    pub fn is_connected(&self) -> bool {
        self.inner.lock().state & ss::ISCONNECTED != 0
    }

    /// Whether `SO_NOSIGPIPE` is set.
    pub fn nosigpipe(&self) -> bool {
        self.so_options() & abi::socket::SO_NOSIGPIPE != 0
    }

    /// The timeouts of waits to send and to receive, ns (0: none).
    pub fn timeouts(&self) -> (u64, u64) {
        let i = self.inner.lock();
        (i.snd.timeo, i.rcv.timeo)
    }

    /// Its send buffer's size (`st_blksize`).
    pub fn snd_hiwat(&self) -> u32 {
        self.inner.lock().snd.hiwat
    }

    /// Bytes waiting to be read (`FIONREAD`): a stream's bytes, or every
    /// datagram's.
    pub fn nread(&self) -> usize {
        let i = self.inner.lock();
        match self.proto {
            Proto::Tcp => i.stream.len(),
            Proto::Udp => i.dgrams.iter().map(|d| d.data.len()).sum(),
        }
    }

    /// Bytes in its send buffer (`FIONWRITE`).
    pub fn nwrite(&self) -> usize {
        self.inner.lock().sndq.len()
    }

    /// Room in its send buffer (`FIONSPACE`).
    pub fn send_space(&self) -> usize {
        self.inner.lock().snd.space() as usize
    }

    /// `soo_stat`'s permission bits: whether it can still be read and
    /// written (a listening socket, neither).
    pub fn stat_rw(&self) -> (bool, bool) {
        let i = self.inner.lock();
        if i.tcp.as_ref().is_some_and(|t| t.listen.is_some()) {
            return (false, false);
        }
        (!i.rcv.cant || i.rcv.cc > 0, !i.snd.cant)
    }

    /// Takes its pending error (`SO_ERROR`).
    pub fn take_error(&self) -> Option<Errno> {
        self.inner.lock().error.take()
    }

    /// Its name (`in_getsockaddr`, `in6_mapped_sockaddr`): the bound
    /// address and port, wildcard and 0 before bind.
    pub fn sockname(&self) -> Sockaddr {
        pcb::table(self.proto).lock().sockname(self)
    }

    /// Its peer's name (`in_getpeeraddr`, `in6_mapped_peeraddr`).
    ///
    /// # Errors
    ///
    /// `ENOTCONN` if it is not connected.
    pub fn peername(&self) -> Result<Sockaddr, Errno> {
        if self.inner.lock().state & ss::ISCONNECTED == 0 {
            return Err(Errno::ENOTCONN);
        }
        pcb::table(self.proto).lock().peername(self)
    }

    /// `bind` (`udp_bind`, `udp6_bind`, `tcp_usr_bind`, `tcp6_usr_bind`)
    /// to the address in `raw`.
    ///
    /// # Errors
    ///
    /// `EINVAL` if bound already or `raw` is malformed; `EAFNOSUPPORT`
    /// for another family or a multicast address on TCP;
    /// `EADDRNOTAVAIL` for an address not local; `EACCES` for a reserved
    /// port without privilege; `EADDRINUSE`.
    pub fn bind(&self, raw: &[u8], cred: &Cred) -> Result<(), Errno> {
        if self.proto == Proto::Tcp && self.inner.lock().tcp.as_ref().is_some_and(|t| t.dropped) {
            return Err(Errno::EINVAL);
        }
        let sa = match self.family {
            Family::Inet => addr::parse_in(raw, true)?,
            Family::Inet6 => addr::parse_in6(raw)?,
        };
        pcb::table(self.proto)
            .lock()
            .bind(self, &sa, cred, self.so_options())
    }

    /// The last descriptor closed (`soclose`): what each protocol does,
    /// then its buffers' contents go.
    pub fn close(self: &Arc<Self>) {
        // Relaxed: as in `wake`.
        if self.spliced.load(Ordering::Relaxed) != 0 {
            self.splice_close();
        }
        match self.proto {
            Proto::Udp => self.udp_close(),
            Proto::Tcp => self.tcp_close(),
        }
        self.rd.clear();
        self.wr.clear();
    }

    /// `shutdown`.
    ///
    /// # Errors
    ///
    /// `ENOTCONN` unconnected (a UDP socket still shuts down).
    pub fn shutdown(self: &Arc<Self>, how: i32) -> Result<(), Errno> {
        match self.proto {
            Proto::Udp => self.udp_shutdown(how),
            Proto::Tcp => self.tcp_shutdown(how),
        }
    }

    /// `connect` to the address in `raw`; for TCP, `nonblock` returns
    /// `EINPROGRESS` while the connection is made, as FreeBSD's does
    /// before its handshake ends.
    ///
    /// # Errors
    ///
    /// Each protocol's (`EISCONN`, `EALREADY`, `EAFNOSUPPORT`,
    /// `EINVAL`, `ENETUNREACH`, `EADDRINUSE`, `ECONNREFUSED`, ...).
    pub fn connect(self: &Arc<Self>, raw: &[u8], cred: &Cred, nonblock: bool) -> Result<(), Errno> {
        match self.proto {
            Proto::Udp => self.udp_connect(raw, cred),
            Proto::Tcp => self.tcp_connect(raw, cred, nonblock),
        }
    }

    /// The flags `recvmsg` reports about addresses (`MSG_*`) do not
    /// apply; this returns what `getsockopt(SO_TYPE)` would.
    #[must_use]
    pub fn sock_type(&self) -> u32 {
        self.proto.sock_type()
    }

    /// Process information (`soo_fill_kinfo`): send and receive queue
    /// bytes, and the PCB's identity.
    #[must_use]
    pub fn queues(&self) -> (u32, u32) {
        let i = self.inner.lock();
        (u32::try_from(i.sndq.len()).unwrap_or(u32::MAX), i.rcv.cc)
    }
}

/// An internet socket's local and foreign names, for `kinfo_file`.
#[must_use]
pub fn names(so: &Socket) -> (Vec<u8>, Vec<u8>) {
    let t = pcb::table(so.proto).lock();
    let local = t.sockname(so).encode();
    let peer = t.peer_any(so).encode();
    (local, peer)
}
