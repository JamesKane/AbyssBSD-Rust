// SPDX-License-Identifier: BSD-2-Clause
//! UDP: datagrams from one socket straight to the one its destination's
//! lookup finds, with what the receiver asked to learn about each
//! (`IP_RECVDSTADDR`, `IPV6_RECVPKTINFO`, ...), and the ICMP port
//! unreachable a connected sender sees as `ECONNREFUSED`.
//!
//! Behavior reference: sys/netinet/udp_usrreq.c (`udp_send`,
//! `udp_append`, `udp_connect`, `udp_disconnect`, `udp_shutdown`,
//! `udp_notify`), sys/netinet6/udp6_usrreq.c, sys/kern/uipc_socket.c
//! (`soreceive_dgram`, `sosend_dgram`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::{inp, ss};
use abi::socket::{MSG_TRUNC, SHUT_RD, SHUT_RDWR, SHUT_WR, SO_BINTIME, SO_BROADCAST, SO_TIMESTAMP};

use crate::addr::{self, Sockaddr, is_broadcast4, is_unspecified, is_v4mapped, v4_of};
use crate::pcb::UDB;
use crate::{Cred, Family, MSIZE, Socket, mbuf_cost};

/// What a receiver asked to be told about a datagram, decided when it
/// arrives (`udp_append`'s `ip_savecontrol`, `ip6_savecontrol`).
pub mod want {
    pub const DSTADDR: u32 = 1 << 0;
    pub const TTL: u32 = 1 << 1;
    pub const TOS: u32 = 1 << 2;
    pub const IF: u32 = 1 << 3;
    pub const ORIGDSTADDR: u32 = 1 << 4;
    pub const PKTINFO: u32 = 1 << 5;
    pub const HOPLIMIT: u32 = 1 << 6;
    pub const TCLASS: u32 = 1 << 7;
    pub const TIMESTAMP: u32 = 1 << 8;
    pub const BINTIME: u32 = 1 << 9;
}

/// A datagram's facts, for the control messages its receiver wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// [`want`] bits.
    pub want: u32,
    /// Its destination, as the receiver names addresses.
    pub dst: Sockaddr,
    /// TTL or hop limit, TOS or traffic class.
    pub ttl: u8,
    pub tos: u8,
    /// The interface it came in on.
    pub ifindex: u16,
    /// When it arrived (the hooks' clock).
    pub stamp: u64,
}

/// A queued datagram (`MT_SONAME` mbuf, control, and data).
#[derive(Debug)]
pub(crate) struct Dgram {
    pub(crate) from: Sockaddr,
    pub(crate) data: Vec<u8>,
    pub(crate) meta: Meta,
    /// What it is charged (`sb_ccc`, `sb_mbcnt`).
    cc: u32,
    mb: u32,
}

/// What a receive got.
#[derive(Debug, Default)]
pub struct Received {
    pub data: Vec<u8>,
    /// The sender, for a datagram.
    pub from: Option<Sockaddr>,
    /// `MSG_TRUNC`, `MSG_OOB`.
    pub flags: i32,
    /// With `MSG_TRUNC`, a datagram's whole length.
    pub full: usize,
    pub meta: Option<Meta>,
}

impl Socket {
    /// `udp_connect` and `udp6_connect` (after `soconnectat` has
    /// disconnected a connected socket, which is how `AF_UNSPEC`
    /// disconnects).
    pub(crate) fn udp_connect(&self, raw: &[u8], cred: &Cred) -> Result<(), Errno> {
        if self.inner.lock().state & ss::ISCONNECTED != 0 {
            self.udp_disconnect();
        }
        let sa = match self.family {
            Family::Inet => addr::parse_in(raw, false)?,
            Family::Inet6 => addr::parse_in6(raw)?,
        };
        let mut t = UDB.lock();
        if t.get(self.id).is_some_and(|p| !is_unspecified(&p.faddr)) {
            return Err(Errno::EISCONN);
        }
        t.connect(self, &sa, cred)?;
        drop(t);
        let mut i = self.inner.lock();
        i.state = (i.state | ss::ISCONNECTED) & !ss::ISDISCONNECTED;
        i.error = None;
        drop(i);
        self.wake();
        Ok(())
    }

    /// `udp_disconnect`.
    fn udp_disconnect(&self) {
        UDB.lock().disconnect(self.id);
        self.inner.lock().state &= !ss::ISCONNECTED;
    }

    /// `udp_shutdown`: `ENOTCONN` unconnected, but the buffers shut all
    /// the same.
    pub(crate) fn udp_shutdown(&self, how: i32) -> Result<(), Errno> {
        let mut i = self.inner.lock();
        let r = if i.state & ss::ISCONNECTED == 0 {
            Err(Errno::ENOTCONN)
        } else {
            Ok(())
        };
        if how == SHUT_RD || how == SHUT_RDWR {
            // `sorflush`.
            i.rcv.cant = true;
            i.dgrams.clear();
            i.rcv.cc = 0;
            i.rcv.mbcnt = 0;
        }
        if how == SHUT_WR || how == SHUT_RDWR {
            i.snd.cant = true;
        }
        drop(i);
        self.wake();
        r
    }

    /// `udp_close` and `udp_detach`.
    pub(crate) fn udp_close(&self) {
        UDB.lock().free(self.id);
        let mut i = self.inner.lock();
        i.dgrams.clear();
        i.rcv.cc = 0;
        i.rcv.mbcnt = 0;
        i.state &= !ss::ISCONNECTED;
    }

    /// `sosend_dgram` and `udp_send`: one datagram, to the connected
    /// peer or `to` (raw), binding an ephemeral port first if it has
    /// none. A datagram nobody is bound to is dropped; a connected
    /// sender then sees `ECONNREFUSED` on its next call (the ICMP port
    /// unreachable). Returns the bytes sent.
    ///
    /// # Errors
    ///
    /// `EPIPE` after `shutdown`; its pending error; `EMSGSIZE` over
    /// `net.inet.udp.maxdgram`; `EISCONN` with `to` while connected;
    /// `EDESTADDRREQ` with neither; address errors (`EAFNOSUPPORT`,
    /// `EINVAL`, `EADDRNOTAVAIL`); `EACCES` to a broadcast address
    /// without `SO_BROADCAST`; `ENETUNREACH` or `EHOSTUNREACH`.
    pub fn send_dgram(&self, data: &[u8], to: Option<&[u8]>, cred: &Cred) -> Result<usize, Errno> {
        let connected = {
            let mut i = self.inner.lock();
            if i.snd.cant {
                return Err(Errno::EPIPE);
            }
            if let Some(e) = i.error.take() {
                return Err(e);
            }
            if data.len() > i.snd.hiwat as usize {
                return Err(Errno::EMSGSIZE);
            }
            i.state & ss::ISCONNECTED != 0
        };
        let (src, dst, family) = {
            let mut t = UDB.lock();
            let dst = match to {
                Some(raw) => {
                    if connected {
                        return Err(Errno::EISCONN);
                    }
                    let sa = match self.family {
                        Family::Inet => addr::parse_in(raw, false)?,
                        Family::Inet6 => addr::parse_in6(raw)?,
                    };
                    let p = t.get(self.id).ok_or(Errno::EINVAL)?;
                    let sa = if self.family == Family::Inet6 && is_v4mapped(&sa.addr) {
                        // `udp6_send` hands a mapped destination to IPv4,
                        // unless the socket is IPv6-only.
                        if p.flags & inp::IPV6_V6ONLY != 0 {
                            return Err(Errno::EINVAL);
                        }
                        sa.unmapped()
                    } else if self.family == Family::Inet6 && p.vflag & abi::inet::INP_IPV6 == 0 {
                        // Bound to a mapped address: IPv4 only.
                        return Err(Errno::EAFNOSUPPORT);
                    } else {
                        sa
                    };
                    if sa.port == 0 {
                        return Err(Errno::EADDRNOTAVAIL);
                    }
                    sa
                }
                None => {
                    if !connected {
                        return Err(Errno::EDESTADDRREQ);
                    }
                    let p = t.get(self.id).ok_or(Errno::ENOTCONN)?;
                    if p.vflag & abi::inet::INP_IPV6 == 0 {
                        Sockaddr::v4(v4_of(&p.faddr), p.fport)
                    } else {
                        Sockaddr::v6(p.faddr, p.fport, p.zone)
                    }
                }
            };
            if dst.family == Family::Inet {
                let a = v4_of(&dst.addr);
                if is_broadcast4(&a) && self.so_options() & SO_BROADCAST == 0 {
                    return Err(Errno::EACCES);
                }
            }
            if t.get(self.id).is_some_and(|p| p.lport == 0) {
                t.bind_anon(self, cred)?;
            }
            let src = t.source_for(self.id, &dst)?;
            let family = dst.family;
            (src, dst, family)
        };
        let hdr = match family {
            Family::Inet => 28,
            Family::Inet6 => 48,
        };
        self.vnet.lo.looped(data.len() + hdr);
        let (ttl, tos) = {
            let i = self.inner.lock();
            match family {
                Family::Inet => (i.ip.ttl, i.ip.tos),
                Family::Inet6 => (
                    hops(i.ip.hops),
                    u8::try_from(i.ip.tclass.max(0)).unwrap_or(0),
                ),
            }
        };
        let target = UDB.lock().deliver(family, &src, &dst);
        match target {
            Some((rcvr, flags, flags2, rfamily)) => {
                let orig = flags2 & inp::ORIGDSTADDR != 0;
                rcvr.udp_append(&src, &dst, data, flags, orig, rfamily, ttl, tos);
            }
            None if connected => {
                // `udp_notify`: the error waits for the next call.
                self.inner.lock().error = Some(Errno::ECONNREFUSED);
                self.wake();
            }
            None => {}
        }
        Ok(data.len())
    }

    /// `udp_append`: queues a datagram from `src` to `dst` if there is
    /// room (else it is dropped, as `udps_fullsock`), with the facts its
    /// PCB flags ask for.
    #[allow(clippy::too_many_arguments, reason = "udp_append's")]
    fn udp_append(
        self: &Arc<Self>,
        src: &Sockaddr,
        dst: &Sockaddr,
        data: &[u8],
        flags: i32,
        orig: bool,
        family: Family,
        ttl: u8,
        tos: u8,
    ) {
        let mut wanted = if orig { want::ORIGDSTADDR } else { 0 };
        let v4 = src.family == Family::Inet;
        let bit = |f: i32, w: u32| if flags & f != 0 { w } else { 0 };
        if v4 {
            wanted |= bit(inp::RECVDSTADDR, want::DSTADDR)
                | bit(inp::RECVTTL, want::TTL)
                | bit(inp::RECVTOS, want::TOS)
                | bit(inp::RECVIF, want::IF);
        } else {
            wanted |= bit(inp::IN6P_PKTINFO, want::PKTINFO)
                | bit(inp::IN6P_HOPLIMIT, want::HOPLIMIT)
                | bit(inp::IN6P_TCLASS, want::TCLASS);
        }
        let so = self.so_options();
        if so & SO_TIMESTAMP != 0 {
            wanted |= want::TIMESTAMP;
        }
        if so & SO_BINTIME != 0 {
            wanted |= want::BINTIME;
        }
        // A mapped receiver sees IPv4 peers as `::ffff:a.b.c.d`.
        let (from, to) = if family == Family::Inet6 && v4 {
            (src.mapped(), dst.mapped())
        } else {
            (*src, *dst)
        };
        let meta = Meta {
            want: wanted,
            dst: to,
            ttl,
            tos,
            ifindex: 1,
            stamp: crate::now(),
        };
        let salen: u32 = match from.family {
            Family::Inet => 16,
            Family::Inet6 => 28,
        };
        let cc = salen + u32::try_from(data.len()).unwrap_or(u32::MAX);
        let mb = MSIZE + mbuf_cost(data.len());
        {
            let mut i = self.inner.lock();
            if cc > i.rcv.space() {
                self.vnet
                    .lo
                    .iqdrops
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                return;
            }
            i.rcv.cc += cc;
            i.rcv.mbcnt += mb;
            i.dgrams.push_back(Dgram {
                from,
                data: data.to_vec(),
                meta,
                cc,
                mb,
            });
        }
        self.rd.notify(0);
    }

    /// `soreceive_dgram`, one look: the next datagram, its data to
    /// `max` bytes (`MSG_TRUNC` if cut short).
    ///
    /// # Errors
    ///
    /// With nothing queued: its pending error, else `EWOULDBLOCK`; after
    /// `shutdown` an empty result.
    pub fn recv_dgram(&self, max: usize, peek: bool) -> Result<Received, Errno> {
        let mut i = self.inner.lock();
        if peek {
            let Some(d) = i.dgrams.front() else {
                return empty(&mut i, max, peek);
            };
            let mut r = cut(
                d.from,
                d.data[..d.data.len().min(max)].to_vec(),
                d.meta,
                max,
            );
            r.full = d.data.len();
            if r.full > max {
                r.flags |= MSG_TRUNC;
            }
            return Ok(r);
        }
        let Some(d) = i.dgrams.pop_front() else {
            return empty(&mut i, max, peek);
        };
        i.rcv.cc -= d.cc;
        i.rcv.mbcnt -= d.mb;
        drop(i);
        Ok(cut(d.from, d.data, d.meta, max))
    }
}

/// A receive's result with nothing queued.
fn empty(i: &mut crate::Inner, max: usize, peek: bool) -> Result<Received, Errno> {
    if let Some(e) = i.error {
        if !peek {
            i.error = None;
        }
        return Err(e);
    }
    if i.rcv.cant || max == 0 {
        return Ok(Received::default());
    }
    Err(Errno::EAGAIN)
}

fn cut(from: Sockaddr, mut data: Vec<u8>, meta: Meta, max: usize) -> Received {
    let full = data.len();
    let mut flags = 0;
    if data.len() > max {
        data.truncate(max);
        flags |= MSG_TRUNC;
    }
    Received {
        data,
        from: Some(from),
        flags,
        full,
        meta: Some(meta),
    }
}

/// The hop limit a send uses: the socket's, or the default
/// (`in6_selecthlim`).
pub(crate) fn hops(h: i16) -> u8 {
    if h < 0 {
        u8::try_from(
            crate::tune::IP6_HLIM
                .load(core::sync::atomic::Ordering::Relaxed)
                .clamp(0, 255),
        )
        .unwrap_or(64)
    } else {
        u8::try_from(h).unwrap_or(64)
    }
}
