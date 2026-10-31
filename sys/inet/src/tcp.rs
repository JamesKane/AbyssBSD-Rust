// SPDX-License-Identifier: BSD-2-Clause
//! TCP between local sockets: a connection is two sockets, each the
//! other's peer. A send lands in the peer's receive buffer as far as its
//! room allows (the window), the rest waiting in the sender's send
//! buffer until the receiver reads; a FIN follows the last of it. The
//! states a connection passes through are FreeBSD's, each segment's
//! effect taken at once (its acknowledgement too), so the side that
//! closes first ends in `TIME_WAIT` for twice the MSL, holding its port,
//! and the other goes straight from `LAST_ACK` to `CLOSED`.
//!
//! A connection to a listener is queued on it complete (no syncache);
//! past the listen queue's limit a connect waits, as a client
//! retransmitting its SYN would, until `accept` makes room or
//! `TCP_KEEPINIT` passes.
//!
//! Behavior reference: sys/netinet/tcp_usrreq.c (`tcp_usr_listen`,
//! `tcp_usr_connect`, `tcp_usr_shutdown`, `tcp_disconnect`,
//! `tcp_usrclosed`, `tcp_usr_rcvoob`, `tcp_usr_send` with `PRUS_OOB`),
//! sys/netinet/tcp_input.c (the FIN and RST transitions, data after
//! close, `tcp_pulloutofband`), sys/netinet/tcp_syncache.c
//! (`syncache_socket`: what a new connection inherits),
//! sys/kern/uipc_socket.c (`solisten_proto`, `sonewconn`,
//! `soreceive_generic`'s out-of-band mark, `sosend_generic`).

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::inet::{INP_IPV4, INP_IPV6, inp, ss, tcp as tcpopt, tcps};
use abi::socket::{SHUT_RD, SHUT_RDWR, SHUT_WR, SO_ACCEPTCONN, SO_LINGER, SO_OOBINLINE};

use crate::addr::{
    self, Sockaddr, is_broadcast4, is_multicast4, is_multicast6, is_v4mapped, v4_of,
};
use crate::ifnet::LOMTU;
use crate::pcb::TCB;
use crate::{Cred, Family, Inner, Proto, Socket, lock2, tune};

/// A connection's state (`t_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tstate {
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

impl Tstate {
    /// Its `TCPS_*`.
    #[must_use]
    pub const fn number(self) -> i32 {
        match self {
            Self::Closed => tcps::CLOSED,
            Self::Listen => tcps::LISTEN,
            Self::SynSent => tcps::SYN_SENT,
            Self::SynReceived => tcps::SYN_RECEIVED,
            Self::Established => tcps::ESTABLISHED,
            Self::CloseWait => tcps::CLOSE_WAIT,
            Self::FinWait1 => tcps::FIN_WAIT_1,
            Self::Closing => tcps::CLOSING,
            Self::LastAck => tcps::LAST_ACK,
            Self::FinWait2 => tcps::FIN_WAIT_2,
            Self::TimeWait => tcps::TIME_WAIT,
        }
    }

    /// Past `CLOSE_WAIT`: this side has sent its FIN.
    const fn fin_sent(self) -> bool {
        matches!(
            self,
            Self::FinWait1 | Self::Closing | Self::LastAck | Self::FinWait2 | Self::TimeWait
        )
    }
}

/// A listener's queue of complete connections (`sol_comp`), its limit,
/// and the connects waiting for room.
#[derive(Debug)]
pub(crate) struct Listen {
    pub(crate) comp: VecDeque<Arc<Socket>>,
    pub(crate) qlimit: u32,
    pending: VecDeque<Weak<Socket>>,
}

impl Listen {
    /// `sonewconn`'s limit: past half again the backlog, no more.
    fn over(&self) -> bool {
        self.comp.len() > 3 * self.qlimit as usize / 2
    }
}

/// `TCPOOB_HAVEDATA`, `TCPOOB_HADDATA`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Oob {
    None,
    Have(u8),
    Had,
}

/// The TCP state of a socket (`struct tcpcb`).
#[derive(Debug)]
pub(crate) struct Tcb {
    pub(crate) state: Tstate,
    pub(crate) peer: Option<Arc<Socket>>,
    pub(crate) listen: Option<Listen>,
    /// The listener a waiting connect is queued on.
    target: Option<Weak<Socket>>,
    /// `INP_DROPPED`: reset or refused; it connects no more.
    pub(crate) dropped: bool,
    /// `TF_NEEDFIN`: a FIN goes once the send buffer drains.
    fin_wanted: bool,
    /// `SS_NOFDREF`: its descriptor is closed.
    nofdref: bool,
    /// `t_flags` the options set (`TF_NODELAY`, `TF_NOPUSH`,
    /// `TF_NOOPT`), and `TF_SENTFIN`.
    pub(crate) flags: u32,
    pub(crate) maxseg: u32,
    /// `TCP_KEEPINIT` and the keepalive parameters, ms and count.
    pub(crate) keepinit: u32,
    pub(crate) keepidle: u32,
    pub(crate) keepintvl: u32,
    pub(crate) keepcnt: u32,
    /// The urgent byte (`t_iobc`), how many bytes come before the mark
    /// (`so_oobmark`), and whether reading is at it (`SBS_RCVATMARK`).
    oob: Oob,
    oobmark: usize,
    atmark: bool,
    /// When it last received (`t_rcvtime`, the hooks' clock).
    pub(crate) rcvtime: u64,
}

impl Tcb {
    pub(crate) fn new() -> Self {
        Self {
            state: Tstate::Closed,
            peer: None,
            listen: None,
            target: None,
            dropped: false,
            fin_wanted: false,
            nofdref: false,
            flags: 0,
            maxseg: tune::TCP_MSSDFLT.load(Ordering::Relaxed),
            keepinit: tune::TCP_KEEPINIT.load(Ordering::Relaxed),
            keepidle: tune::TCP_KEEPIDLE.load(Ordering::Relaxed),
            keepintvl: tune::TCP_KEEPINTVL.load(Ordering::Relaxed),
            keepcnt: tune::TCP_KEEPCNT.load(Ordering::Relaxed),
            oob: Oob::None,
            oobmark: 0,
            atmark: false,
            rcvtime: 0,
        }
    }
}

/// What to do once the sockets' locks are dropped: PCB changes and
/// wakeups.
#[derive(Debug, Default)]
struct After {
    /// PCBs to free, to unhash, and to put in `TIME_WAIT`.
    free: Vec<u64>,
    unhash: Vec<u64>,
    time_wait: Vec<u64>,
    wake: Vec<Arc<Socket>>,
    /// References to let go of with no lock held.
    drop: Vec<Arc<Socket>>,
}

impl After {
    fn run(self) {
        if !self.free.is_empty() || !self.unhash.is_empty() || !self.time_wait.is_empty() {
            let mut t = TCB.lock();
            for id in self.free {
                t.free(id);
            }
            for id in self.unhash {
                t.drop_hash(id);
            }
            for id in self.time_wait {
                t.time_wait(id);
            }
        }
        for s in self.wake {
            s.wake();
        }
        drop(self.drop);
    }
}

fn tcb(i: &mut Inner) -> &mut Tcb {
    // Every TCP socket has its control block from `Socket::new`.
    i.tcp.as_mut().expect("a TCP socket's tcpcb")
}

fn tcb_ref(i: &Inner) -> &Tcb {
    i.tcp.as_ref().expect("a TCP socket's tcpcb")
}

/// `soisdisconnected`, with the error a reset or refusal leaves.
fn disconnected(i: &mut Inner, err: Option<Errno>) {
    i.state = (i.state & !(ss::ISCONNECTED | ss::ISCONNECTING | ss::ISDISCONNECTING))
        | ss::ISDISCONNECTED;
    i.rcv.cant = true;
    i.snd.cant = true;
    if let Some(e) = err {
        i.error = Some(e);
    }
}

/// `tcp_drop` of the side `s` (locked as `i`): it is reset with `err`,
/// leaves the lookups, and lets go of its peer.
fn drop_side(s: &Arc<Socket>, i: &mut Inner, err: Errno, fx: &mut After) {
    disconnected(i, Some(err));
    // Unsent data goes with the connection (`tcp_discardcb`).
    i.sndq.clear();
    i.snd.cc = 0;
    let t = tcb(i);
    t.state = Tstate::Closed;
    t.dropped = true;
    t.fin_wanted = false;
    if let Some(p) = t.peer.take() {
        fx.drop.push(p);
    }
    if t.nofdref {
        fx.free.push(s.id);
    } else {
        fx.unhash.push(s.id);
    }
    fx.wake.push(Arc::clone(s));
}

/// The FIN from `a` (whose send buffer has drained) reaching `b`, and
/// its acknowledgement (`tcp_usrclosed`, then `tcp_input`'s `TH_FIN`
/// handling on the other side).
fn fin(a_so: &Arc<Socket>, a: &mut Inner, b_so: &Arc<Socket>, b: &mut Inner, fx: &mut After) {
    let at = tcb(a);
    at.fin_wanted = false;
    at.flags |= tcpopt::TF_SENTFIN;
    at.state = match at.state {
        Tstate::CloseWait => Tstate::LastAck,
        // Established: FIN_WAIT_1, acknowledged at once.
        _ => Tstate::FinWait2,
    };
    // `socantrcvmore` at the receiver: the end follows its data.
    b.rcv.cant = true;
    let bt = tcb(b);
    bt.rcvtime = crate::now();
    let b_had_fin = bt.state.fin_sent();
    bt.state = match bt.state {
        Tstate::Established => Tstate::CloseWait,
        s if s.fin_sent() => Tstate::TimeWait,
        s => s,
    };
    fx.wake.push(Arc::clone(b_so));
    fx.wake.push(Arc::clone(a_so));
    if b_had_fin {
        // Both FINs are in: `b` (the active closer) waits in
        // `TIME_WAIT`, `a` goes from `LAST_ACK` to `CLOSED` on its ACK.
        let at = tcb(a);
        at.state = Tstate::Closed;
        if let Some(p) = at.peer.take() {
            fx.drop.push(p);
        }
        let a_gone = at.nofdref;
        disconnected(a, None);
        if a_gone {
            fx.free.push(a_so.id);
        } else {
            fx.unhash.push(a_so.id);
        }
        let bt = tcb(b);
        if let Some(p) = bt.peer.take() {
            fx.drop.push(p);
        }
        disconnected(b, None);
        fx.time_wait.push(b_so.id);
    }
}

/// Moves what `a` has queued into `b`'s receive buffer as far as its room
/// goes, then `a`'s FIN if it was waiting for that.
fn push(a_so: &Arc<Socket>, a: &mut Inner, b_so: &Arc<Socket>, b: &mut Inner, fx: &mut After) {
    if !a.sndq.is_empty() {
        if b.rcv.cant {
            // `SHUT_RD` at the receiver: what comes is acknowledged and
            // dropped.
            a.snd.cc = 0;
            a.sndq.clear();
        } else {
            let n = a.sndq.len().min(b.rcv.space() as usize);
            if n > 0 {
                b.stream.extend(a.sndq.drain(..n));
                let n32 = u32::try_from(n).unwrap_or(u32::MAX);
                a.snd.cc -= n32;
                b.rcv.cc += n32;
                b.rcv.mbcnt = crate::mbuf_cost(b.stream.len());
                a.snd.mbcnt = crate::mbuf_cost(a.sndq.len());
                a_so.vnet.lo.looped(n + 40);
                fx.wake.push(Arc::clone(b_so));
                fx.wake.push(Arc::clone(a_so));
            }
        }
    }
    if a.sndq.is_empty() && tcb_ref(a).fin_wanted {
        fin(a_so, a, b_so, b, fx);
    }
}

impl Socket {
    fn peer(&self) -> Option<Arc<Socket>> {
        self.inner.lock().tcp.as_ref().and_then(|t| t.peer.clone())
    }

    /// The connection's state.
    pub fn tcp_state(&self) -> Tstate {
        self.inner
            .lock()
            .tcp
            .as_ref()
            .map_or(Tstate::Closed, |t| t.state)
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
            let mut i = self.inner.lock();
            if i.state & (ss::ISCONNECTED | ss::ISCONNECTING | ss::ISDISCONNECTING) != 0
                || tcb(&mut i).dropped
            {
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
        let mut i = self.inner.lock();
        let t = tcb(&mut i);
        t.state = Tstate::Listen;
        match &mut t.listen {
            Some(l) => l.qlimit = backlog.cast_unsigned(),
            None => {
                t.listen = Some(Listen {
                    comp: VecDeque::new(),
                    qlimit: backlog.cast_unsigned(),
                    pending: VecDeque::new(),
                });
            }
        }
        self.options.fetch_or(SO_ACCEPTCONN, Ordering::Relaxed);
        Ok(())
    }

    /// `tcp_usr_connect` and `tcp6_usr_connect`: the connection is made
    /// at once if a listener has room, or waits for room. A blocking
    /// caller then waits with [`Socket::connect_done`]; `nonblock`
    /// returns `EINPROGRESS` either way, as FreeBSD's does before its
    /// handshake ends.
    pub(crate) fn tcp_connect(
        self: &Arc<Self>,
        raw: &[u8],
        cred: &Cred,
        nonblock: bool,
    ) -> Result<(), Errno> {
        {
            let mut i = self.inner.lock();
            let t = tcb(&mut i);
            if t.listen.is_some() {
                return Err(Errno::EOPNOTSUPP);
            }
            if i.state & ss::ISCONNECTING != 0 {
                return Err(Errno::EALREADY);
            }
            if i.state & ss::ISCONNECTED != 0 {
                return Err(Errno::EISCONN);
            }
            if tcb(&mut i).dropped {
                return Err(Errno::ECONNREFUSED);
            }
            i.error = None;
        }
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
        let (src, dst, family, listener) = {
            let mut t = TCB.lock();
            let dst = t.connect(self, &sa, cred)?;
            let p = t.get(self.id).ok_or(Errno::EINVAL)?;
            let src = if p.vflag & INP_IPV6 != 0 {
                Sockaddr::v6(p.laddr, p.lport, p.zone)
            } else {
                Sockaddr::v4(v4_of(&p.laddr), p.lport)
            };
            let family = dst.family;
            let l = t.deliver(family, &src, &dst).map(|(s, _, _, _)| s);
            (src, dst, family, l)
        };
        let mss = match family {
            Family::Inet => LOMTU - 40,
            Family::Inet6 => LOMTU - 60,
        };
        {
            let mut i = self.inner.lock();
            i.state |= ss::ISCONNECTING;
            i.state &= !ss::ISDISCONNECTED;
            let t = tcb(&mut i);
            t.state = Tstate::SynSent;
            t.maxseg = mss;
        }
        self.vnet
            .lo
            .looped(if family == Family::Inet { 60 } else { 80 });
        let made = match listener {
            Some(l) if l.is_listening() => self.join(&l, &src, &dst, family),
            _ => Err(Errno::ECONNREFUSED),
        };
        match made {
            Ok(()) => {}
            Err(e) => {
                // The RST: refused, and done with (`tcp_drop`).
                let mut fx = After::default();
                {
                    let mut i = self.inner.lock();
                    drop_side(self, &mut i, e, &mut fx);
                }
                fx.run();
                if !nonblock {
                    self.inner.lock().error = None;
                    return Err(e);
                }
            }
        }
        if nonblock {
            return Err(Errno::EINPROGRESS);
        }
        Ok(())
    }

    /// Queues the connection on listener `l`: made now if it has room,
    /// else waiting for it.
    fn join(
        self: &Arc<Self>,
        l: &Arc<Self>,
        src: &Sockaddr,
        dst: &Sockaddr,
        family: Family,
    ) -> Result<(), Errno> {
        let room = {
            let mut li = l.inner.lock();
            let lt = tcb(&mut li);
            match &mut lt.listen {
                None => return Err(Errno::ECONNREFUSED),
                Some(q) if q.over() => {
                    q.pending.push_back(Arc::downgrade(self));
                    false
                }
                Some(_) => true,
            }
        };
        if !room {
            tcb(&mut self.inner.lock()).target = Some(Arc::downgrade(l));
            return Ok(());
        }
        self.establish(l, src, dst, family)
    }

    /// `syncache_socket` and `soisconnected`: the listener's new socket
    /// for this connection, which inherits its options, and the two
    /// joined.
    fn establish(
        self: &Arc<Self>,
        l: &Arc<Self>,
        src: &Sockaddr,
        dst: &Sockaddr,
        family: Family,
    ) -> Result<(), Errno> {
        let (rcv, snd, linger, ip, tflags, keep, uid, lflags) = {
            let li = l.inner.lock();
            let lt = tcb_ref(&li);
            let mut rcv = li.rcv;
            let mut snd = li.snd;
            rcv.cc = 0;
            rcv.mbcnt = 0;
            rcv.cant = false;
            snd.cc = 0;
            snd.mbcnt = 0;
            snd.cant = false;
            let p = TCB.lock();
            let (uid, flags) = p.get(l.id).map_or((0, 0), |p| (p.uid, p.flags));
            drop(p);
            (
                rcv,
                snd,
                li.linger,
                li.ip,
                lt.flags & (tcpopt::TF_NODELAY | tcpopt::TF_NOPUSH | tcpopt::TF_NOOPT),
                (lt.keepinit, lt.keepidle, lt.keepintvl, lt.keepcnt),
                uid,
                flags,
            )
        };
        let cred = Cred {
            uid,
            ..Cred::default()
        };
        let s = Socket::with_buffers(&l.vnet, l.family, Proto::Tcp, &cred, rcv, snd);
        s.options
            .store(l.so_options() & !SO_ACCEPTCONN, Ordering::Relaxed);
        {
            let mut t = TCB.lock();
            if let Some(p) = t.get_mut(s.id) {
                p.vflag = (p.vflag & !(INP_IPV4 | INP_IPV6))
                    | if family == Family::Inet {
                        INP_IPV4
                    } else {
                        INP_IPV6
                    };
                p.flags =
                    (lflags & !(inp::ANONPORT | inp::INHASHLIST | inp::DROPPED)) | inp::INHASHLIST;
                p.laddr = dst.addr;
                p.lport = dst.port;
                p.faddr = src.addr;
                p.fport = src.port;
                p.zone = dst.scope.max(src.scope);
                p.hashed = true;
            }
        }
        let now = crate::now();
        let mss = tcb_ref(&self.inner.lock()).maxseg;
        {
            let (_irq, mut a, b) = lock2(self, &s);
            let mut b = b.expect("a new socket is another");
            b.linger = linger;
            b.ip = ip;
            b.state = ss::ISCONNECTED;
            let bt = tcb(&mut b);
            bt.state = Tstate::Established;
            bt.flags = tflags;
            (bt.keepinit, bt.keepidle, bt.keepintvl, bt.keepcnt) = keep;
            bt.maxseg = mss;
            bt.peer = Some(Arc::clone(self));
            bt.rcvtime = now;
            a.state = (a.state & !ss::ISCONNECTING) | ss::ISCONNECTED;
            let at = tcb(&mut a);
            at.state = Tstate::Established;
            at.peer = Some(Arc::clone(&s));
            at.target = None;
            at.rcvtime = now;
        }
        let queued = {
            let mut li = l.inner.lock();
            match &mut tcb(&mut li).listen {
                Some(q) => {
                    q.comp.push_back(Arc::clone(&s));
                    true
                }
                None => false,
            }
        };
        if !queued {
            // The listener closed meanwhile: its new socket is aborted.
            s.abort(Errno::ECONNREFUSED);
            return Ok(());
        }
        // `solisten_wakeup`, and the connecter's `soisconnected`.
        l.rd.notify(0);
        self.wake();
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

    /// How long a connect may wait (`TCP_KEEPINIT`), ns.
    pub fn connect_timeout(&self) -> u64 {
        self.inner
            .lock()
            .tcp
            .as_ref()
            .map_or(0, |t| u64::from(t.keepinit) * 1_000_000)
    }

    /// A connect that waited too long: `ETIMEDOUT` (`tcp_timer_keep` in
    /// `SYN_SENT`), and the socket is done with.
    pub fn connect_timed_out(self: &Arc<Self>) {
        let mut fx = After::default();
        {
            let mut i = self.inner.lock();
            if i.state & ss::ISCONNECTING == 0 {
                return;
            }
            drop_side(self, &mut i, Errno::ETIMEDOUT, &mut fx);
            tcb(&mut i).target = None;
        }
        fx.run();
    }

    /// `solisten_dequeue`: the next connection, letting a waiting
    /// connect in.
    ///
    /// # Errors
    ///
    /// `EINVAL` if not listening; its error, once; `EWOULDBLOCK` with
    /// none queued.
    pub fn accept(self: &Arc<Self>) -> Result<Arc<Self>, Errno> {
        let (s, waiting) = {
            let mut i = self.inner.lock();
            if tcb(&mut i).listen.is_none() {
                return Err(Errno::EINVAL);
            }
            if let Some(e) = i.error.take() {
                return Err(e);
            }
            let q = tcb(&mut i).listen.as_mut().ok_or(Errno::EINVAL)?;
            let s = q.comp.pop_front().ok_or(Errno::EAGAIN)?;
            let mut w = None;
            while let Some(c) = q.pending.pop_front() {
                if let Some(c) = c.upgrade() {
                    w = Some(c);
                    break;
                }
            }
            (s, w)
        };
        if let Some(c) = waiting {
            c.promote(self);
        }
        Ok(s)
    }

    /// A waiting connect let in by an `accept` on `l`.
    fn promote(self: &Arc<Self>, l: &Arc<Self>) {
        if self.inner.lock().state & ss::ISCONNECTING == 0 {
            return;
        }
        let (src, dst, family) = {
            let t = TCB.lock();
            let Some(p) = t.get(self.id) else {
                return;
            };
            if p.vflag & INP_IPV6 != 0 {
                (
                    Sockaddr::v6(p.laddr, p.lport, p.zone),
                    Sockaddr::v6(p.faddr, p.fport, p.zone),
                    Family::Inet6,
                )
            } else {
                (
                    Sockaddr::v4(v4_of(&p.laddr), p.lport),
                    Sockaddr::v4(v4_of(&p.faddr), p.fport),
                    Family::Inet,
                )
            }
        };
        let _ = self.establish(l, &src, &dst, family);
    }

    /// `soabort`: a queued connection the listener let go of; its peer
    /// is reset.
    fn abort(self: &Arc<Self>, err: Errno) {
        let mut fx = After::default();
        let peer = self.peer();
        match peer {
            Some(p) => {
                let (_irq, mut a, b) = lock2(self, &p);
                let tcb_a = tcb(&mut a);
                tcb_a.nofdref = true;
                drop_side(self, &mut a, Errno::ECONNABORTED, &mut fx);
                if let Some(mut b) = b {
                    drop_side(&p, &mut b, err, &mut fx);
                }
            }
            None => {
                let mut a = self.inner.lock();
                tcb(&mut a).nofdref = true;
                drop_side(self, &mut a, Errno::ECONNABORTED, &mut fx);
            }
        }
        fx.run();
    }

    /// `tcp_usr_shutdown`.
    pub(crate) fn tcp_shutdown(self: &Arc<Self>, how: i32) -> Result<(), Errno> {
        {
            let mut i = self.inner.lock();
            if tcb(&mut i).listen.is_some() {
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
                i.stream.clear();
                i.rcv.cc = 0;
                i.rcv.mbcnt = 0;
            }
            if (how == SHUT_WR || how == SHUT_RDWR) && !i.snd.cant {
                i.snd.cant = true;
                i.state |= ss::ISDISCONNECTING;
                let t = tcb(&mut i);
                if matches!(t.state, Tstate::Established | Tstate::CloseWait) {
                    t.fin_wanted = true;
                }
            }
        }
        self.flush();
        self.wake();
        Ok(())
    }

    /// Sends what can go now: queued data, then a FIN waiting on it.
    fn flush(self: &Arc<Self>) {
        let Some(p) = self.peer() else {
            return;
        };
        let mut fx = After::default();
        {
            let (_irq, mut a, b) = lock2(self, &p);
            if let Some(mut b) = b {
                push(self, &mut a, &p, &mut b, &mut fx);
            }
        }
        fx.run();
    }

    /// `tcp_usr_close` and `tcp_disconnect`: a listener's queue is
    /// aborted; an embryonic connection goes; `SO_LINGER` with no time
    /// resets; otherwise the connection closes in order (unread data
    /// flushed, a FIN after what is queued), and data the peer still
    /// sends resets it.
    pub(crate) fn tcp_close(self: &Arc<Self>) {
        let mut fx = After::default();
        let linger0 = self.so_options() & SO_LINGER != 0 && self.inner.lock().linger == 0;
        let (listen, target, state) = {
            let mut i = self.inner.lock();
            let t = tcb(&mut i);
            t.nofdref = true;
            let listen = t.listen.take();
            let target = t.target.take();
            let state = t.state;
            if listen.is_some() {
                t.state = Tstate::Closed;
            }
            (listen, target, state)
        };
        drop(target);
        if let Some(q) = listen {
            TCB.lock().free(self.id);
            for s in q.comp {
                s.abort(Errno::ECONNRESET);
            }
            for c in q.pending.into_iter().filter_map(|w| w.upgrade()) {
                let mut i = c.inner.lock();
                if i.state & ss::ISCONNECTING != 0 {
                    drop_side(&c, &mut i, Errno::ECONNREFUSED, &mut fx);
                }
            }
            fx.run();
            return;
        }
        match state {
            Tstate::Closed | Tstate::SynSent | Tstate::Listen => {
                TCB.lock().free(self.id);
                let mut i = self.inner.lock();
                i.state &= !ss::ISCONNECTING;
                return;
            }
            Tstate::TimeWait => {
                // The PCB waits out its time without the socket.
                return;
            }
            _ => {}
        }
        let Some(peer) = self.peer() else {
            TCB.lock().free(self.id);
            return;
        };
        {
            let (_irq, mut a, b) = lock2(self, &peer);
            let Some(mut b) = b else {
                return;
            };
            // `sbflush` of what was not read.
            a.stream.clear();
            a.rcv.cc = 0;
            a.rcv.mbcnt = 0;
            a.rcv.cant = true;
            if linger0 || !b.sndq.is_empty() {
                // `tcp_drop`'s RST, or the peer's data arriving after
                // the close.
                drop_side(self, &mut a, Errno::ECONNRESET, &mut fx);
                drop_side(&peer, &mut b, Errno::ECONNRESET, &mut fx);
            } else {
                if !a.snd.cant {
                    a.snd.cant = true;
                    a.state |= ss::ISDISCONNECTING;
                    let t = tcb(&mut a);
                    if matches!(t.state, Tstate::Established | Tstate::CloseWait) {
                        t.fin_wanted = true;
                    }
                }
                push(self, &mut a, &peer, &mut b, &mut fx);
            }
        }
        fx.run();
    }

    /// `sosend_generic` with `tcp_usr_send`, one round: as much of `data`
    /// as the peer's window and this socket's send buffer take now.
    /// `oob` marks its last byte urgent (`MSG_OOB`). Returns the bytes
    /// taken.
    ///
    /// # Errors
    ///
    /// `EPIPE` once it cannot send; its pending error; `ENOTCONN`
    /// unconnected; `EWOULDBLOCK` with less room than the low water mark.
    pub fn send_stream(self: &Arc<Self>, data: &[u8], oob: bool) -> Result<usize, Errno> {
        self.send_stream_to(data, oob, None)
    }

    /// [`Socket::send_stream`]; with `spliced`, a splice's send
    /// (`sosend_generic_locked` from `so_splice_xfer_data`), which takes
    /// whatever fits, its low water mark aside, and counts what it took
    /// in `spliced` before the receiver can see it.
    #[inline]
    pub(crate) fn send_stream_to(
        self: &Arc<Self>,
        data: &[u8],
        oob: bool,
        spliced: Option<&core::sync::atomic::AtomicU64>,
    ) -> Result<usize, Errno> {
        let peer = {
            let mut i = self.inner.lock();
            if i.snd.cant {
                return Err(Errno::EPIPE);
            }
            if let Some(e) = i.error.take() {
                return Err(e);
            }
            if i.state & ss::ISCONNECTED == 0 {
                return Err(Errno::ENOTCONN);
            }
            tcb(&mut i).peer.clone()
        };
        let Some(peer) = peer else {
            return Err(Errno::ENOTCONN);
        };
        if data.is_empty() {
            return Ok(0);
        }
        let mut fx = After::default();
        let r = {
            let (_irq, mut a, b) = lock2(self, &peer);
            let Some(mut b) = b else {
                return Err(Errno::ENOTCONN);
            };
            let bt = tcb_ref(&b);
            if bt.nofdref && bt.state.fin_sent() {
                // Data after the peer closed: it resets (`tcp_input`'s
                // `SS_NOFDREF` check), and this send has gone.
                drop_side(&peer, &mut b, Errno::ECONNRESET, &mut fx);
                drop_side(self, &mut a, Errno::ECONNRESET, &mut fx);
                count(spliced, data.len());
                Ok(data.len())
            } else if b.rcv.cant {
                count(spliced, data.len());
                Ok(data.len())
            } else {
                let direct = if a.sndq.is_empty() {
                    b.rcv.space() as usize
                } else {
                    0
                };
                let room = direct + a.snd.space() as usize;
                let least = if spliced.is_some() {
                    1
                } else {
                    a.snd.lowat as usize
                };
                if room < data.len() && room < least {
                    Err(Errno::EAGAIN)
                } else {
                    let n = data.len().min(room);
                    let now = direct.min(n);
                    if now > 0 {
                        b.stream.extend(&data[..now]);
                        let n32 = u32::try_from(now).unwrap_or(u32::MAX);
                        b.rcv.cc += n32;
                        b.rcv.mbcnt = crate::mbuf_cost(b.stream.len());
                        tcb(&mut b).rcvtime = crate::now();
                        self.vnet.lo.looped(now + 40);
                        fx.wake.push(Arc::clone(&peer));
                    }
                    if n > now {
                        a.sndq.extend(&data[now..n]);
                        a.snd.cc += u32::try_from(n - now).unwrap_or(u32::MAX);
                        a.snd.mbcnt = crate::mbuf_cost(a.sndq.len());
                    }
                    if oob && n == data.len() && now == n {
                        urgent(&mut b, peer.so_options() & SO_OOBINLINE != 0);
                    }
                    count(spliced, n);
                    Ok(n)
                }
            }
        };
        fx.run();
        r
    }

    /// How much a send may copy in at once: the peer's window and this
    /// socket's send buffer room.
    pub fn send_window(&self) -> usize {
        let (peer, space, queued) = {
            let i = self.inner.lock();
            (
                i.tcp.as_ref().and_then(|t| t.peer.clone()),
                i.snd.space() as usize,
                !i.sndq.is_empty(),
            )
        };
        let direct = match (peer, queued) {
            (Some(p), false) => p.inner.lock().rcv.space() as usize,
            _ => 0,
        };
        (direct + space).max(1)
    }

    /// `soreceive_generic` on a stream, one look: up to `max` bytes,
    /// stopping at the urgent mark. Reading lets the peer's queued data
    /// in. With `oob`, the urgent byte (`tcp_usr_rcvoob`).
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
            let t = tcb(&mut i);
            if (t.oobmark == 0 && !t.atmark) || inline || t.oob == Oob::Had {
                return Err(Errno::EINVAL);
            }
            let Oob::Have(b) = t.oob else {
                return Err(Errno::EAGAIN);
            };
            if !peek {
                t.oob = Oob::Had;
            }
            out.data.push(b);
            out.flags = abi::socket::MSG_OOB;
            return Ok(out);
        }
        self.recv_data(max, peek, |_| true)
            .map(Option::unwrap_or_default)
    }

    /// A splice's receive: [`Socket::recv_stream`] of up to `max` bytes,
    /// if `still` holds of the socket's state (`None` if not).
    pub(crate) fn recv_stream_if(
        self: &Arc<Self>,
        max: usize,
        still: impl FnOnce(&Inner) -> bool,
    ) -> Result<Option<crate::Received>, Errno> {
        self.recv_data(max, false, still)
    }

    /// `soreceive_generic`'s data path, if `still` holds under the lock.
    #[inline]
    fn recv_data(
        self: &Arc<Self>,
        max: usize,
        peek: bool,
        still: impl FnOnce(&Inner) -> bool,
    ) -> Result<Option<crate::Received>, Errno> {
        let mut out = crate::Received::default();
        {
            let mut i = self.inner.lock();
            if !still(&i) {
                return Ok(None);
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
            let t = tcb(&mut i);
            let limit = if t.oobmark > 0 { t.oobmark } else { usize::MAX };
            let n = max.min(limit);
            let n = n.min(i.stream.len());
            if peek {
                out.data.extend(i.stream.iter().take(n));
                return Ok(Some(out));
            }
            out.data.extend(i.stream.drain(..n));
            i.rcv.cc -= u32::try_from(n).unwrap_or(0);
            i.rcv.mbcnt = crate::mbuf_cost(i.stream.len());
            let t = tcb(&mut i);
            t.atmark = false;
            if t.oobmark > 0 {
                t.oobmark -= n;
                if t.oobmark == 0 {
                    t.atmark = true;
                }
            }
        }
        // `tcp_usr_rcvd`: the window opens.
        if let Some(p) = self.peer() {
            let mut fx = After::default();
            {
                let (_irq, mut a, b) = lock2(&p, self);
                if let Some(mut b) = b {
                    push(&p, &mut a, self, &mut b, &mut fx);
                }
            }
            fx.run();
        }
        Ok(Some(out))
    }

    /// `SIOCATMARK`.
    pub fn at_mark(&self) -> bool {
        self.inner.lock().tcp.as_ref().is_some_and(|t| t.atmark)
    }

    /// Bytes before the urgent mark (`so_oobmark`).
    pub fn oobmark(&self) -> usize {
        self.inner.lock().tcp.as_ref().map_or(0, |t| t.oobmark)
    }
}

/// Counts `n` bytes a splice sent, with the receiver still locked.
#[inline]
fn count(spliced: Option<&core::sync::atomic::AtomicU64>, n: usize) {
    if let Some(c) = spliced {
        // Relaxed: the receiver's lock, held, publishes it to whoever
        // reads what was sent.
        c.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Whether urgent data is ahead or reading is at its mark (`so_oobmark`
/// or `SBS_RCVATMARK`).
pub(crate) fn urgent_pending(i: &Inner) -> bool {
    i.tcp.as_ref().is_some_and(|t| t.oobmark > 0 || t.atmark)
}

/// What `net.inet.tcp.pcblist` shows of a connection: its state, `t_flags`,
/// MSS, and windows (its receive room, its peer's).
pub(crate) fn snapshot(so: &Socket) -> (Tstate, u32, u32, u32, u32, u64) {
    let (state, flags, mss, rwnd, peer, rcvtime) = {
        let i = so.inner.lock();
        let Some(t) = i.tcp.as_ref() else {
            return (Tstate::Closed, 0, 0, 0, 0, 0);
        };
        (
            t.state,
            t.flags,
            t.maxseg,
            i.rcv.space(),
            t.peer.clone(),
            t.rcvtime,
        )
    };
    let swnd = peer.map_or(0, |p| p.inner.lock().rcv.space());
    (state, flags, mss, rwnd, swnd, rcvtime)
}

/// `tcp_pulloutofband` and the urgent mark: the last byte that came is
/// urgent; unless `SO_OOBINLINE` it leaves the stream for `t_iobc`.
fn urgent(b: &mut Inner, inline: bool) {
    let Some(&last) = b.stream.back() else {
        return;
    };
    if !inline {
        b.stream.pop_back();
        b.rcv.cc -= 1;
    }
    let mark = if inline {
        b.stream.len() - 1
    } else {
        b.stream.len()
    };
    let t = tcb(b);
    t.oob = Oob::Have(last);
    t.oobmark = mark;
    t.atmark = mark == 0;
}
