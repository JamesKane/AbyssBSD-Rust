// SPDX-License-Identifier: BSD-2-Clause
//! Netlink sockets as open files (`PF_NETLINK`, `SOCK_RAW` or
//! `SOCK_DGRAM`): a write is a buffer of requests the kernel answers at
//! once ([`inet::netlink::handle`]), its replies queued as one record
//! for the next read, as FreeBSD's `nl_process_nbuf` writes them to the
//! socket's receive buffer.
//!
//! `NETLINK_ROUTE` is served, and its sockets hear the link, address, and
//! route notifications of the groups they join; `NETLINK_GENERIC` has
//! its controller (`nlctrl`) and no other family (no `carp`, `pfsync`,
//! or `wlan` here).
//!
//! Routing sockets (`PF_ROUTE`, `SOCK_RAW`) are the same machinery with
//! one message per record: a write is a routing request
//! ([`net::rtsock::request`]), whose reply goes to every routing socket of
//! the VNET as its announcement, and they hear the VNET's changes.
//!
//! Behavior reference: sys/net/rtsock.c (`rts_attach`, `rts_send`,
//! `rt_dispatch`), sys/netlink/netlink_domain.c (`nl_attach`,
//! `nl_bind`, `nl_assign_port`, `nl_ctloutput`, `nl_soreceive`),
//! sys/netlink/netlink_io.c.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use abi::errno::Errno;
use abi::event::{EVFILT_READ, EVFILT_WRITE, Kevent, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM};
use abi::netlink::{
    NETLINK_ADD_MEMBERSHIP, NETLINK_CAP_ACK, NETLINK_DROP_MEMBERSHIP, NETLINK_EXT_ACK,
    NETLINK_GENERIC, NETLINK_GET_STRICT_CHK, NETLINK_LIST_MEMBERSHIPS, NETLINK_MSG_INFO,
    NETLINK_ROUTE, SOL_NETLINK,
};
use abi::socket::{
    MSG_DONTWAIT, MSG_NBIO, MSG_PEEK, MSG_TRUNC, SO_DOMAIN, SO_ERROR, SO_PROTOCOL, SO_RCVBUF,
    SO_RCVTIMEO, SO_SNDBUF, SO_SNDTIMEO, SO_TYPE, SOL_SOCKET,
};
use abi::stat::{S_IFSOCK, Stat};
use event::{Filter, Knote, Note, Notify, Wake};
use sched::Sleepable;
use sync::SpinLock;
use vfs::Uio;

use crate::fs::socket::wait_until;

/// `NLSNDQ`, `NLRCVQ`: the buffers' default sizes.
const SPACE: u32 = 65536;

#[derive(Debug)]
struct State {
    queue: VecDeque<Vec<u8>>,
    queued: usize,
    rcvbuf: u32,
    sndbuf: u32,
    rcvtimeo: u64,
    ext_ack: bool,
    cap_ack: bool,
    strict: bool,
    msg_info: bool,
    groups: u64,
    port: u32,
    /// A routing socket hears its own replies (`SO_USELOOPBACK`, on by
    /// default: `rts_attach`).
    useloopback: bool,
}

/// A netlink socket (`struct nlpcb`).
pub(crate) struct Nl {
    pub(crate) proto: i32,
    /// A routing socket, and the family it hears (0: all).
    route: Option<u8>,
    /// Its routing table (`so_fibnum`): its creator's, or `SO_SETFIB`'s.
    fib: core::sync::atomic::AtomicU16,
    /// Its creator's VNET (`so_vnet`), which its requests read and change.
    vnet: Arc<net::Vnet>,
    pub(crate) ty: u32,
    state: SpinLock<State>,
    rd: Notify,
    wr: Notify,
}

/// An open netlink socket.
pub(crate) struct NetlinkFile {
    pub(crate) nl: Arc<Nl>,
    uid: u32,
    gid: u32,
}

/// The ports taken (`nl_port`): a process's first socket takes its pid,
/// others a number of their own (`nl_assign_port`).
static PORTS: SpinLock<Vec<u32>> = SpinLock::new(Vec::new());
static NEXT_PORT: AtomicU32 = AtomicU32::new(0x8000_0000);

fn assign_port(pid: u32) -> u32 {
    let mut p = PORTS.lock();
    let port = if pid != 0 && !p.contains(&pid) {
        pid
    } else {
        // Relaxed: a counter.
        NEXT_PORT.fetch_add(1, Ordering::Relaxed)
    };
    p.push(port);
    port
}

impl NetlinkFile {
    /// `nl_attach`: a socket of protocol `proto`.
    ///
    /// # Errors
    ///
    /// `EPROTONOSUPPORT` for a protocol not served.
    pub(crate) fn new(proto: i32, ty: u32, pid: u32, uid: u32, gid: u32) -> Result<Self, Errno> {
        if proto != NETLINK_ROUTE && proto != NETLINK_GENERIC {
            return Err(Errno::EPROTONOSUPPORT);
        }
        Ok(Self::make(proto, None, ty, pid, uid, gid))
    }

    /// `rts_attach`: a routing socket hearing family `af` (0: all).
    pub(crate) fn new_route(af: u8, ty: u32, uid: u32, gid: u32) -> Self {
        Self::make(i32::from(af), Some(af), ty, 0, uid, gid)
    }

    fn make(proto: i32, route: Option<u8>, ty: u32, pid: u32, uid: u32, gid: u32) -> Self {
        let f = Self {
            nl: Arc::new(Nl {
                route,
                fib: core::sync::atomic::AtomicU16::new(
                    crate::proc::Process::current().map_or(0, |p| p.fib.load(Ordering::Relaxed)),
                ),
                proto,
                vnet: super::curvnet(),
                ty,
                state: SpinLock::new(State {
                    queue: VecDeque::new(),
                    queued: 0,
                    rcvbuf: SPACE,
                    sndbuf: SPACE,
                    rcvtimeo: 0,
                    ext_ack: false,
                    cap_ack: false,
                    strict: false,
                    msg_info: false,
                    groups: 0,
                    port: assign_port(pid),
                    useloopback: true,
                }),
                rd: Notify::new(),
                wr: Notify::new(),
            }),
            uid,
            gid,
        };
        // It hears its VNET's changes while it is open.
        let l: Arc<dyn net::announce::Listener> = Arc::clone(&f.nl) as _;
        f.nl.vnet.net.listen(&l);
        f
    }

    pub(crate) fn stat(&self) -> Stat {
        Stat {
            st_mode: S_IFSOCK | 0o666,
            st_uid: self.uid,
            st_gid: self.gid,
            st_blksize: i32::try_from(self.nl.state.lock().sndbuf).unwrap_or(i32::MAX),
            st_dev: u64::MAX,
            ..Stat::default()
        }
    }

    /// `write(2)`: a buffer of requests.
    pub(crate) fn write(&self, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let len = uio.resid();
        if len > self.nl.state.lock().sndbuf as usize {
            return Err(Errno::EMSGSIZE);
        }
        let mut buf = alloc::vec![0u8; len];
        let n = uio.copy_in(&mut buf)?;
        buf.truncate(n);
        self.nl.process(&buf)?;
        Ok(n)
    }

    /// `read(2)`: the next record of replies.
    pub(crate) fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        self.nl.receive(s, uio, 0, nonblock).map(|(n, _)| n)
    }

    pub(crate) fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if let Some(w) = w
            && events & (POLLIN | POLLRDNORM) != 0
        {
            self.nl.rd.record(w);
        }
        let mut r = events & (POLLOUT | POLLWRNORM);
        if !self.nl.state.lock().queue.is_empty() {
            r |= events & (POLLIN | POLLRDNORM);
        }
        r
    }

    pub(crate) fn kqfilter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        let read = match kev.filter {
            EVFILT_READ => true,
            EVFILT_WRITE => false,
            _ => return Err(Errno::EINVAL),
        };
        Ok(Box::new(NlFilter {
            nl: Arc::clone(&self.nl),
            read,
        }))
    }

    pub(crate) fn ioctl(&self, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
        match req {
            abi::ioctl::FIONBIO | abi::ioctl::FIOASYNC => Ok(0),
            abi::ioctl::FIONREAD => {
                let n = self.nl.state.lock().queue.front().map_or(0, Vec::len);
                arg.copy_from_slice(&i32::try_from(n).unwrap_or(i32::MAX).to_ne_bytes());
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }
}

impl Drop for NetlinkFile {
    fn drop(&mut self) {
        let port = self.nl.state.lock().port;
        PORTS.lock().retain(|&p| p != port);
        self.nl.rd.clear();
        self.nl.wr.clear();
    }
}

impl Nl {
    /// Processes requests, queueing the replies.
    pub(crate) fn process(&self, buf: &[u8]) -> Result<(), Errno> {
        use crate::cred::Priv;
        if self.route.is_some() {
            return self.route_request(buf);
        }
        let (ctx, rcvbuf) = {
            let st = self.state.lock();
            (
                inet::netlink::Ctx {
                    port: st.port,
                    ext_ack: st.ext_ack,
                    cap_ack: st.cap_ack,
                    may_addaddr: crate::fs::priv_check(Priv::NetAddifaddr).is_ok(),
                    may_deladdr: crate::fs::priv_check(Priv::NetDelifaddr).is_ok(),
                    may_route: crate::fs::priv_check(Priv::NetRoute).is_ok(),
                    fib: self.fib.load(Ordering::Relaxed),
                },
                st.rcvbuf,
            )
        };
        let replies = if self.proto == NETLINK_ROUTE {
            inet::netlink::handle(
                &ctx,
                &self.vnet.net,
                buf,
                crate::time::now_ns() / 1_000_000_000,
            )
        } else {
            inet::netlink::handle_generic(&ctx, buf)
        };
        if replies.is_empty() {
            return Ok(());
        }
        {
            let mut st = self.state.lock();
            // A reader that cannot keep up loses what does not fit
            // (`nl_send_one` sets `ENOBUFS`).
            if st.queued + replies.len() > rcvbuf as usize * 2 {
                return Err(Errno::ENOBUFS);
            }
            st.queued += replies.len();
            st.queue.push_back(replies);
        }
        self.rd.notify(0);
        Ok(())
    }

    /// `rts_send`: one routing request, its reply sent to every routing
    /// socket of the VNET (this one's only with `SO_USELOOPBACK`).
    fn route_request(&self, buf: &[u8]) -> Result<(), Errno> {
        let may = crate::fs::priv_check(crate::cred::Priv::NetRoute).is_ok();
        let pid = crate::proc::Process::current().map_or(0, |p| p.pid());
        let mut n = self.vnet.net.lock();
        let fib = self.fib.load(Ordering::Relaxed);
        let (reply, r) = net::rtsock::request(&mut n, buf, pid, may, fib);
        if !reply.is_empty() {
            let family = buf
                .get(core::mem::size_of::<abi::net::RtMsghdr>() + 1)
                .and_then(|af| net::Family::from_af(*af));
            n.outbox.push(net::announce::Event::Reply {
                family,
                msg: reply,
                from: core::ptr::from_ref(self).addr(),
                fib,
            });
        }
        drop(n);
        r
    }

    /// Queues a record for the reader, unless it is full.
    fn queue(&self, rec: Vec<u8>) {
        {
            let mut st = self.state.lock();
            if st.queued + rec.len() > st.rcvbuf as usize {
                return;
            }
            st.queued += rec.len();
            st.queue.push_back(rec);
        }
        self.rd.notify(0);
    }

    /// One look: whole messages of the next record to `max` bytes (the
    /// first cut short, `MSG_TRUNC`, if none fits); a routing socket's
    /// record is one message.
    fn take(&self, max: usize, peek: bool) -> Result<(Vec<u8>, i32), Errno> {
        let mut st = self.state.lock();
        if self.route.is_some() {
            let rec = if peek {
                st.queue.front().cloned()
            } else {
                st.queue.pop_front()
            };
            let mut rec = rec.ok_or(Errno::EAGAIN)?;
            if !peek {
                st.queued -= rec.len();
            }
            let flags = if rec.len() > max { MSG_TRUNC } else { 0 };
            rec.truncate(max);
            return Ok((rec, flags));
        }
        let Some(front) = st.queue.front_mut() else {
            return Err(Errno::EAGAIN);
        };
        let mut at = 0;
        while at + 4 <= front.len() {
            let len = u32::from_ne_bytes(front[at..at + 4].try_into().unwrap_or([0; 4])) as usize;
            let step = (len + 3) & !3;
            if len < 16 || at + step.min(front.len() - at) > max {
                break;
            }
            at += step.min(front.len() - at);
        }
        let mut flags = 0;
        let cut = if at == 0 {
            flags |= MSG_TRUNC;
            max.min(front.len())
        } else {
            at
        };
        let out = front[..cut].to_vec();
        if !peek {
            if at == 0 || at >= front.len() {
                let gone = st.queue.pop_front().map_or(0, |v| v.len());
                st.queued -= gone;
            } else {
                front.drain(..at);
                st.queued -= at;
            }
        }
        Ok((out, flags))
    }

    /// `nl_soreceive`.
    pub(crate) fn receive(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        flags: i32,
        nonblock: bool,
    ) -> Result<(usize, i32), Errno> {
        let nonblock = nonblock || flags & (MSG_DONTWAIT | MSG_NBIO) != 0;
        let peek = flags & MSG_PEEK != 0;
        let max = uio.resid();
        let timeo = self.state.lock().rcvtimeo;
        let (data, f) = if nonblock {
            self.take(max, peek)?
        } else {
            let _w = sched::wait_message("nlrecv");
            wait_until(s, &self.rd, timeo, || self.take(max, peek))?
        };
        let n = uio.copy_out(&data)?;
        Ok((n, f))
    }

    /// Its name (`sockaddr_nl`).
    pub(crate) fn name(&self, peer: bool) -> Vec<u8> {
        let st = self.state.lock();
        let (pid, groups) = if peer { (0, 0) } else { (st.port, st.groups) };
        let mut sa = alloc::vec![0u8; 12];
        sa[0] = 12;
        sa[1] = abi::inet::AF_NETLINK;
        sa[4..8].copy_from_slice(&pid.to_ne_bytes());
        sa[8..12].copy_from_slice(
            &u32::try_from(groups & 0xffff_ffff)
                .unwrap_or(0)
                .to_ne_bytes(),
        );
        sa
    }

    /// `nl_bind`: the groups to join (the port is assigned).
    pub(crate) fn bind(&self, sa: &[u8]) -> Result<(), Errno> {
        if sa.len() < 12 || sa[1] != abi::inet::AF_NETLINK {
            return Err(Errno::EINVAL);
        }
        let groups = u32::from_ne_bytes(sa[8..12].try_into().unwrap_or([0; 4]));
        self.state.lock().groups |= u64::from(groups);
        Ok(())
    }

    /// `nl_ctloutput`'s set.
    pub(crate) fn setopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), Errno> {
        let int = || -> Result<i32, Errno> {
            val.get(..4)
                .and_then(|b| b.try_into().ok())
                .map(i32::from_ne_bytes)
                .ok_or(Errno::EINVAL)
        };
        let mut st = self.state.lock();
        match (level, name) {
            (SOL_NETLINK, NETLINK_EXT_ACK) => st.ext_ack = int()? != 0,
            (SOL_NETLINK, NETLINK_CAP_ACK) => st.cap_ack = int()? != 0,
            (SOL_NETLINK, NETLINK_GET_STRICT_CHK) => st.strict = int()? != 0,
            (SOL_NETLINK, NETLINK_MSG_INFO) => st.msg_info = int()? != 0,
            (SOL_NETLINK, NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP) => {
                let g = int()?;
                if !(1..=64).contains(&g) {
                    return Err(Errno::EINVAL);
                }
                let bit = 1u64 << (g - 1);
                if name == NETLINK_ADD_MEMBERSHIP {
                    st.groups |= bit;
                } else {
                    st.groups &= !bit;
                }
            }
            (SOL_SOCKET, SO_RCVBUF | SO_SNDBUF) => {
                let v = int()?;
                if v < 1 {
                    return Err(Errno::EINVAL);
                }
                if name == SO_RCVBUF {
                    st.rcvbuf = v.cast_unsigned();
                } else {
                    st.sndbuf = v.cast_unsigned();
                }
            }
            (SOL_SOCKET, SO_RCVTIMEO | SO_SNDTIMEO) => {
                if val.len() < 16 {
                    return Err(Errno::EINVAL);
                }
                let sec = i64::from_ne_bytes(val[..8].try_into().unwrap_or([0; 8]));
                let usec = i64::from_ne_bytes(val[8..16].try_into().unwrap_or([0; 8]));
                if sec < 0 || !(0..1_000_000).contains(&usec) {
                    return Err(Errno::EDOM);
                }
                if name == SO_RCVTIMEO {
                    st.rcvtimeo = sec.cast_unsigned() * 1_000_000_000 + usec.cast_unsigned() * 1000;
                }
            }
            (SOL_SOCKET, abi::socket::SO_USELOOPBACK) => st.useloopback = int()? != 0,
            (SOL_SOCKET, abi::socket::SO_FIB) => {
                let f = u32::try_from(int()?).map_err(|_| Errno::EINVAL)?;
                if f >= net::numfibs() {
                    return Err(Errno::EINVAL);
                }
                #[allow(clippy::cast_possible_truncation, reason = "below RT_MAXFIBS")]
                self.fib.store(f as u16, Ordering::Relaxed);
            }
            (SOL_SOCKET, _) => {}
            (SOL_NETLINK, _) => return Err(Errno::ENOPROTOOPT),
            _ => return Err(Errno::ENOPROTOOPT),
        }
        Ok(())
    }

    /// `nl_ctloutput`'s get.
    pub(crate) fn getopt(&self, level: i32, name: i32) -> Result<Vec<u8>, Errno> {
        let st = self.state.lock();
        let i = |v: i32| v.to_ne_bytes().to_vec();
        let b = |v: bool| i32::from(v).to_ne_bytes().to_vec();
        Ok(match (level, name) {
            (SOL_NETLINK, NETLINK_EXT_ACK) => b(st.ext_ack),
            (SOL_NETLINK, NETLINK_CAP_ACK) => b(st.cap_ack),
            (SOL_NETLINK, NETLINK_GET_STRICT_CHK) => b(st.strict),
            (SOL_NETLINK, NETLINK_MSG_INFO) => b(st.msg_info),
            (SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS) => st.groups.to_ne_bytes()[..4].to_vec(),
            (SOL_SOCKET, SO_RCVBUF) => i(st.rcvbuf.cast_signed()),
            (SOL_SOCKET, SO_SNDBUF) => i(st.sndbuf.cast_signed()),
            (SOL_SOCKET, SO_TYPE) => i(self.ty.cast_signed()),
            (SOL_SOCKET, SO_PROTOCOL) => i(self.proto),
            (SOL_SOCKET, SO_DOMAIN) if self.route.is_some() => i(i32::from(abi::inet::AF_ROUTE)),
            (SOL_SOCKET, SO_DOMAIN) => i(i32::from(abi::inet::AF_NETLINK)),
            (SOL_SOCKET, abi::socket::SO_USELOOPBACK) => b(st.useloopback),
            (SOL_SOCKET, abi::socket::SO_FIB) => i(i32::from(self.fib.load(Ordering::Relaxed))),
            (SOL_SOCKET, SO_ERROR) => i(0),
            _ => return Err(Errno::ENOPROTOOPT),
        })
    }
}

struct NlFilter {
    nl: Arc<Nl>,
    read: bool,
}

impl Filter for NlFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        if self.read {
            self.nl.rd.add(kn);
        } else {
            self.nl.wr.add(kn);
        }
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        if self.read {
            self.nl.rd.remove(kn);
        } else {
            self.nl.wr.remove(kn);
        }
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let st = self.nl.state.lock();
        if self.read {
            note.kev.data = i64::try_from(st.queued).unwrap_or(i64::MAX);
            !st.queue.is_empty()
        } else {
            note.kev.data = i64::from(st.sndbuf);
            true
        }
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self {
            nl: Arc::clone(&self.nl),
            read: self.read,
        }))
    }
}

impl net::announce::Listener for Nl {
    fn event(&self, e: &net::announce::Event) {
        let Some(af) = self.route else {
            // A netlink socket hears the groups it joined.
            let groups = self.state.lock().groups;
            if self.proto == NETLINK_ROUTE
                && groups != 0
                && let Some((g, msg)) =
                    inet::netlink::notify(e, crate::time::now_ns() / 1_000_000_000)
                && groups & (1u64 << (g - 1)) != 0
            {
                self.queue(msg);
            }
            return;
        };
        if let net::announce::Event::Reply { from, .. } = e
            && *from == core::ptr::from_ref(self).addr()
            && !self.state.lock().useloopback
        {
            return;
        }
        // Route messages reach the sockets of their table only.
        let fib = self.fib.load(Ordering::Relaxed);
        match e {
            net::announce::Event::Route { route, .. } if route.fib != fib => return,
            net::announce::Event::Reply { fib: f, .. } if *f != fib => return,
            _ => {}
        }
        let Some((family, msg)) = net::rtsock::announce(e) else {
            return;
        };
        if af != 0 && family.is_some_and(|f| f.af() != af) {
            return;
        }
        self.queue(msg);
    }
}
