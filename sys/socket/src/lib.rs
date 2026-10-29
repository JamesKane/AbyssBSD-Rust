// SPDX-License-Identifier: BSD-2-Clause
//! UNIX-domain sockets (docs/MULTIUSER.md §8): the socket layer's state
//! and buffers, and the local protocol's stream, seqpacket, and datagram
//! sockets, as FreeBSD's `uipc_usrreq.c` implements them.
//!
//! The crate holds state only; the kernel copies user memory, finds
//! bound names, installs descriptors, and sleeps. An operation that would
//! block returns `EWOULDBLOCK`; the kernel registers a poller on the
//! socket's [`Socket::rd`] or [`Socket::wr`] (the `Notify` a change wakes)
//! and tries again, sleeping if it still would.
//!
//! A stream or seqpacket socket's sends go straight into its peer's
//! receive buffer, as FreeBSD's do, flow-controlled by that buffer's
//! room. A datagram socket's receive buffer holds a queue for unconnected
//! senders and one per connected sender, each charged to that sender.
//!
//! Locks: each socket's state is one spin lock; two are taken in address
//! order. Nothing sleeps or touches user memory under them.
//!
//! Behavior reference: sys/kern/uipc_usrreq.c, sys/kern/uipc_socket.c,
//! sys/kern/uipc_sockbuf.c, sys/kern/uipc_syscalls.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod buf;
mod control;
mod dgram;
mod opt;
mod poll;
#[cfg(test)]
mod tests;

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::socket::{AF_UNIX, SHUT_RD, SHUT_RDWR, SHUT_WR, SO_ACCEPTCONN, SOMAXCONN, Xucred};
use event::Notify;
use sync::SpinLock;

use buf::{Item, Sockbuf};
pub use control::{Control, Cred, InFlight, Passable, cmsgcred, gc, inflight};
use dgram::Dgrams;

/// A socket's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Stream,
    Dgram,
    Seqpacket,
}

impl Type {
    /// `PR_CONNREQUIRED`.
    const fn connreq(self) -> bool {
        !matches!(self, Self::Dgram)
    }

    /// Its `SOCK_*`.
    #[must_use]
    pub const fn number(self) -> u32 {
        match self {
            Self::Stream => abi::socket::SOCK_STREAM,
            Self::Dgram => abi::socket::SOCK_DGRAM,
            Self::Seqpacket => abi::socket::SOCK_SEQPACKET,
        }
    }
}

/// The `net.local` sysctls: buffer sizes for new sockets.
pub mod space {
    use core::sync::atomic::AtomicU64;

    pub static STREAM_SEND: AtomicU64 = AtomicU64::new(64 * 1024);
    pub static STREAM_RECV: AtomicU64 = AtomicU64::new(64 * 1024);
    pub static DGRAM_MAX: AtomicU64 = AtomicU64::new(8 * 1024);
    pub static DGRAM_RECV: AtomicU64 = AtomicU64::new(16 * 1024);
    pub static SEQPACKET_SEND: AtomicU64 = AtomicU64::new(64 * 1024);
    pub static SEQPACKET_RECV: AtomicU64 = AtomicU64::new(64 * 1024);
}

/// `so_state` bits this layer keeps.
mod state {
    pub(crate) const ISCONNECTED: u16 = 0x0002;
    pub(crate) const ISDISCONNECTED: u16 = 0x2000;
}

/// `unp_flags` bits.
mod flags {
    pub(crate) const CONNECTING: u16 = 0x0001;
    pub(crate) const BINDING: u16 = 0x0002;
    pub(crate) const HAVEPC: u16 = 0x0004;
    pub(crate) const WANTCRED_ALWAYS: u16 = 0x0008;
    pub(crate) const WANTCRED_ONESHOT: u16 = 0x0010;
    pub(crate) const WANTCRED: u16 = WANTCRED_ALWAYS | WANTCRED_ONESHOT;
}

/// A listening socket's queue and the buffer settings its connections
/// get (`sol_*`).
#[derive(Debug)]
struct Listen {
    comp: VecDeque<Arc<Socket>>,
    qlimit: u32,
}

#[derive(Debug)]
struct Inner {
    state: u16,
    flags: u16,
    options: i32,
    linger: i32,
    error: Option<Errno>,
    rcv: Sockbuf,
    snd: Sockbuf,
    /// The peer (`unp_conn`): for stream and seqpacket, each other's; for
    /// datagrams, where sends go.
    peer: Option<Arc<Socket>>,
    /// Datagram sockets connected to this one (`unp_refs`).
    refs: Vec<Weak<Socket>>,
    /// Its name (`unp_addr`), as bound: a whole `sockaddr_un`.
    addr: Option<Vec<u8>>,
    peercred: Xucred,
    /// The permissions `bind` gives its node (`unp_mode`).
    mode: u16,
    dgrams: Dgrams,
    listen: Option<Listen>,
    user_cookie: u32,
    ts_clock: i32,
    pacing: u32,
}

/// A UNIX-domain socket (FreeBSD's `struct socket` with its `unpcb`).
pub struct Socket {
    pub ty: Type,
    /// `unp_ino`, its `st_ino`.
    pub ino: u64,
    inner: SpinLock<Inner>,
    /// Readers and accepters (`so_rdsel`), and writers (`so_wrsel`).
    pub rd: Notify,
    pub wr: Notify,
    /// Its file, once one of its descriptors has been sent
    /// (`unp_file`), and how many are in flight (`unp_msgcount`).
    pub(crate) file: SpinLock<Option<Weak<dyn Passable>>>,
    pub(crate) msgcount: AtomicUsize,
}

impl core::fmt::Debug for Socket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Socket")
            .field("ty", &self.ty)
            .field("ino", &self.ino)
            .finish_non_exhaustive()
    }
}

static NEXT_INO: AtomicU64 = AtomicU64::new(1);

/// `sun_noname`: a 16-byte `struct sockaddr` with only its family.
#[must_use]
pub fn noname() -> Vec<u8> {
    let mut a = alloc::vec![0u8; 16];
    a[0] = 16;
    a[1] = AF_UNIX;
    a
}

/// Locks two sockets' state in address order (once if the same).
fn lock2<'a>(
    a: &'a Socket,
    b: &'a Socket,
) -> (
    sync::SpinGuard<'a, Inner>,
    Option<sync::SpinGuard<'a, Inner>>,
) {
    if core::ptr::eq(a, b) {
        return (a.inner.lock(), None);
    }
    if core::ptr::from_ref(a).addr() < core::ptr::from_ref(b).addr() {
        let ga = a.inner.lock();
        let gb = b.inner.lock();
        (ga, Some(gb))
    } else {
        let gb = b.inner.lock();
        let ga = a.inner.lock();
        (ga, Some(gb))
    }
}

/// A socket as process information shows it ([`Socket::info`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Info {
    /// It has a peer (`unp_conn`).
    pub connected: bool,
    /// Bytes in its send and receive buffers (`sbused`).
    pub sendq: u32,
    pub recvq: u32,
    /// `sb_state` of each (`SBS_CANTSENDMORE`, `SBS_CANTRCVMORE`).
    pub snd_state: u16,
    pub rcv_state: u16,
    /// Its peer's identity (an address, as FreeBSD's `unp_conn`).
    pub peer: Option<usize>,
}

/// What a receive got.
#[derive(Debug, Default)]
pub struct Received {
    pub data: Vec<u8>,
    pub control: Vec<Control>,
    /// The sender's name, for a datagram.
    pub from: Option<Vec<u8>>,
    /// `MSG_EOR`, `MSG_TRUNC`.
    pub flags: i32,
    /// With `MSG_TRUNC` on a datagram, its whole length.
    pub full: usize,
}

impl Socket {
    /// A new socket (`socreate` and `uipc_attach`).
    #[must_use]
    pub fn new(ty: Type) -> Arc<Self> {
        let (send, recv) = match ty {
            Type::Dgram => {
                let r = space::DGRAM_RECV.load(Ordering::Relaxed);
                (r, r)
            }
            Type::Stream => (
                space::STREAM_SEND.load(Ordering::Relaxed),
                space::STREAM_RECV.load(Ordering::Relaxed),
            ),
            Type::Seqpacket => (
                space::SEQPACKET_SEND.load(Ordering::Relaxed),
                space::SEQPACKET_RECV.load(Ordering::Relaxed),
            ),
        };
        let mut rcv = Sockbuf::new();
        let mut snd = Sockbuf::new();
        // `soreserve`: the defaults always fit.
        rcv.reserve(u32::try_from(recv).unwrap_or(u32::MAX));
        snd.reserve(u32::try_from(send).unwrap_or(u32::MAX));
        rcv.lowat = 1;
        snd.lowat = buf::MCLBYTES.min(snd.hiwat);
        let so = Arc::new(Self {
            ty,
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            inner: SpinLock::new(Inner {
                state: 0,
                flags: 0,
                options: 0,
                linger: 0,
                error: None,
                rcv,
                snd,
                peer: None,
                refs: Vec::new(),
                addr: None,
                peercred: Xucred::default(),
                mode: 0o777,
                dgrams: Dgrams::default(),
                listen: None,
                user_cookie: 0,
                ts_clock: 0,
                pacing: u32::MAX,
            }),
            rd: Notify::new(),
            wr: Notify::new(),
            file: SpinLock::new(None),
            msgcount: AtomicUsize::new(0),
        });
        control::register(&so);
        so
    }

    /// Whether it is listening (`SOLISTENING`).
    pub fn is_listening(&self) -> bool {
        self.inner.lock().listen.is_some()
    }

    /// Whether it is connected (`SS_ISCONNECTED`).
    pub fn is_connected(&self) -> bool {
        self.inner.lock().state & state::ISCONNECTED != 0
    }

    /// Its send buffer's size (`st_blksize`).
    pub fn snd_hiwat(&self) -> u32 {
        self.inner.lock().snd.hiwat
    }

    /// The timeouts of waits to send and to receive, in nanoseconds (0:
    /// none).
    pub fn timeouts(&self) -> (u64, u64) {
        let i = self.inner.lock();
        (i.snd.timeo, i.rcv.timeo)
    }

    /// Whether `SO_NOSIGPIPE` is set.
    pub fn nosigpipe(&self) -> bool {
        self.inner.lock().options & abi::socket::SO_NOSIGPIPE != 0
    }

    /// Wakes readers and writers (a change to state they wait on).
    fn wake(&self) {
        self.rd.notify(0);
        self.wr.notify(0);
    }

    // --- Naming ---

    /// `uipc_bindat`'s first half: the mode its node gets; marks it
    /// binding.
    ///
    /// # Errors
    ///
    /// `EINVAL` if bound already, `EALREADY` if being bound.
    pub fn bind_begin(&self) -> Result<u16, Errno> {
        let mut i = self.inner.lock();
        if i.addr.is_some() {
            return Err(Errno::EINVAL);
        }
        if i.flags & flags::BINDING != 0 {
            return Err(Errno::EALREADY);
        }
        i.flags |= flags::BINDING;
        Ok(i.mode)
    }

    /// The end of a bind: its name, or `None` if it failed.
    pub fn bind_end(&self, addr: Option<Vec<u8>>) {
        let mut i = self.inner.lock();
        i.flags &= !flags::BINDING;
        if let Some(a) = addr {
            i.addr = Some(a);
        }
    }

    /// Whether it has a name.
    pub fn is_bound(&self) -> bool {
        self.inner.lock().addr.is_some()
    }

    /// `uipc_chmod`: the mode `bind` will give its node.
    ///
    /// # Errors
    ///
    /// `EINVAL` for bits beyond the permissions, or once bound.
    pub fn chmod(&self, mode: u32) -> Result<(), Errno> {
        if mode & !0o777 != 0 {
            return Err(Errno::EINVAL);
        }
        let mut i = self.inner.lock();
        if i.addr.is_some() || i.flags & flags::BINDING != 0 {
            return Err(Errno::EINVAL);
        }
        i.mode = u16::try_from(mode).unwrap_or(0o777);
        Ok(())
    }

    /// What process information shows of it (`soo_fill_kinfo` for a
    /// UNIX-domain socket): its buffers' use and state while connected,
    /// and its peer, if it has one.
    #[must_use]
    pub fn info(&self) -> Info {
        let i = self.inner.lock();
        let connected = i.peer.is_some();
        let state = |b: &buf::Sockbuf| if b.cant { buf::SBS_CANTRCVMORE } else { 0 };
        Info {
            connected,
            sendq: if connected { i.snd.cc } else { 0 },
            recvq: if connected { i.rcv.cc } else { 0 },
            snd_state: if connected && i.snd.cant {
                buf::SBS_CANTSENDMORE
            } else {
                0
            },
            rcv_state: if connected { state(&i.rcv) } else { 0 },
            peer: i
                .peer
                .as_ref()
                .map(|p| core::ptr::from_ref(p.as_ref()).addr()),
        }
    }

    /// Its name (`uipc_sockaddr`).
    pub fn sockname(&self) -> Vec<u8> {
        self.inner.lock().addr.clone().unwrap_or_else(noname)
    }

    /// Its peer's name (`getpeername`).
    ///
    /// # Errors
    ///
    /// `ENOTCONN` if it is not connected.
    pub fn peername(&self) -> Result<Vec<u8>, Errno> {
        let peer = {
            let i = self.inner.lock();
            if i.state & state::ISCONNECTED == 0 {
                return Err(Errno::ENOTCONN);
            }
            i.peer.clone()
        };
        Ok(peer.map_or_else(noname, |p| p.sockname()))
    }

    // --- Connections ---

    /// `uipc_listen`.
    ///
    /// # Errors
    ///
    /// `EINVAL` if connected or connecting; `EOPNOTSUPP` for datagrams.
    pub fn listen(&self, backlog: i32, cred: &Cred) -> Result<(), Errno> {
        if !self.ty.connreq() {
            return Err(Errno::EOPNOTSUPP);
        }
        let mut i = self.inner.lock();
        if i.peer.is_some() || i.flags & flags::CONNECTING != 0 {
            return Err(Errno::EINVAL);
        }
        // FreeBSD 15's: only a named socket listens.
        if i.addr.is_none() {
            return Err(Errno::EDESTADDRREQ);
        }
        if i.state & state::ISCONNECTED != 0 {
            return Err(Errno::EINVAL);
        }
        i.peercred = xucred(cred);
        let backlog = if !(0..=SOMAXCONN).contains(&backlog) {
            SOMAXCONN
        } else {
            backlog
        };
        let qlimit = backlog.cast_unsigned();
        match &mut i.listen {
            Some(l) => l.qlimit = qlimit,
            None => {
                i.listen = Some(Listen {
                    comp: VecDeque::new(),
                    qlimit,
                });
                i.options |= SO_ACCEPTCONN;
            }
        }
        Ok(())
    }

    /// `unp_connectat`'s checks before the name is looked up: marks it
    /// connecting.
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` if listening, `EISCONN` if connected, `EALREADY` if
    /// connecting.
    pub fn connect_begin(&self) -> Result<(), Errno> {
        let mut i = self.inner.lock();
        if i.listen.is_some() {
            return Err(Errno::EOPNOTSUPP);
        }
        if i.peer.is_some() {
            return Err(Errno::EISCONN);
        }
        if i.flags & flags::CONNECTING != 0 {
            return Err(Errno::EALREADY);
        }
        i.flags |= flags::CONNECTING;
        Ok(())
    }

    /// The end of a connect that failed after [`Socket::connect_begin`].
    pub fn connect_abort(&self) {
        self.inner.lock().flags &= !flags::CONNECTING;
    }

    /// `unp_connect_peer`: connects to `to`, the socket bound to the name
    /// (for stream and seqpacket, a new socket queued on the listener
    /// `to`, which `accept` returns). `cred` is the caller's.
    ///
    /// # Errors
    ///
    /// `EPROTOTYPE` for another type; `ECONNREFUSED` if `to` is not
    /// listening or its queue is full.
    pub fn connect_to(self: &Arc<Self>, to: &Arc<Self>, cred: &Cred) -> Result<(), Errno> {
        let r = self.connect_peer(to, cred);
        self.inner.lock().flags &= !flags::CONNECTING;
        r
    }

    fn connect_peer(self: &Arc<Self>, to: &Arc<Self>, cred: &Cred) -> Result<(), Errno> {
        if self.ty != to.ty {
            return Err(Errno::EPROTOTYPE);
        }
        if !self.ty.connreq() {
            let (mut a, b) = lock2(self, to);
            if a.peer.is_some() {
                return Err(Errno::EISCONN);
            }
            a.peer = Some(Arc::clone(to));
            a.state |= state::ISCONNECTED;
            a.state &= !state::ISDISCONNECTED;
            let mut b = b;
            let refs = match &mut b {
                Some(b) => &mut b.refs,
                None => &mut a.refs,
            };
            refs.push(Arc::downgrade(self));
            drop(b);
            drop(a);
            self.wake();
            return Ok(());
        }
        // `solisten_clone`: the server's end, with the listener's buffer
        // settings, name, and credential.
        let server = Self::new(self.ty);
        {
            let mut l = to.inner.lock();
            let over = match &l.listen {
                Some(q) => q.comp.len() > 3 * q.qlimit as usize / 2,
                None => return Err(Errno::ECONNREFUSED),
            };
            if over {
                return Err(Errno::ECONNREFUSED);
            }
            let mut s = server.inner.lock();
            s.rcv.reserve(l.rcv.hiwat);
            s.snd.reserve(l.snd.hiwat);
            s.rcv.lowat = l.rcv.lowat;
            s.snd.lowat = l.snd.lowat;
            s.rcv.timeo = l.rcv.timeo;
            s.snd.timeo = l.snd.timeo;
            s.options = l.options & !SO_ACCEPTCONN;
            s.linger = l.linger;
            s.addr.clone_from(&l.addr);
            // `unp_copy_peercred`: the server sees the connecter, the
            // connecter the listener.
            s.peercred = xucred(cred);
            s.flags |= flags::HAVEPC;
            let wantcred = l.flags & flags::WANTCRED;
            let listener_cred = l.peercred;
            drop(s);
            {
                let mut c = self.inner.lock();
                c.peercred = listener_cred;
                c.flags |= flags::HAVEPC | wantcred;
            }
            Self::connect2(self, &server);
            let Some(q) = &mut l.listen else {
                return Err(Errno::ECONNREFUSED);
            };
            q.comp.push_back(Arc::clone(&server));
        }
        // `solisten_wakeup`.
        to.rd.notify(0);
        Ok(())
    }

    /// `unp_connect2` for stream and seqpacket (and `socketpair`).
    fn connect2(a: &Arc<Self>, b: &Arc<Self>) {
        {
            let (mut ga, gb) = lock2(a, b);
            ga.peer = Some(Arc::clone(b));
            ga.state = (ga.state & !state::ISDISCONNECTED) | state::ISCONNECTED;
            if let Some(mut gb) = gb {
                gb.peer = Some(Arc::clone(a));
                gb.state = (gb.state & !state::ISDISCONNECTED) | state::ISCONNECTED;
            }
        }
        a.wake();
        b.wake();
    }

    /// `socketpair`'s `soconnect2`, both ways.
    ///
    /// # Errors
    ///
    /// `EPROTOTYPE` for different types.
    pub fn pair(a: &Arc<Self>, b: &Arc<Self>, cred: &Cred) -> Result<(), Errno> {
        if a.ty != b.ty {
            return Err(Errno::EPROTOTYPE);
        }
        if a.ty.connreq() {
            // Both ends see the caller (`unp_copy_peercred`).
            for s in [a, b] {
                let mut i = s.inner.lock();
                i.peercred = xucred(cred);
                i.flags |= flags::HAVEPC;
            }
            Self::connect2(a, b);
            return Ok(());
        }
        for (x, y) in [(a, b), (b, a)] {
            let (mut gx, gy) = lock2(x, y);
            gx.peer = Some(Arc::clone(y));
            gx.state |= state::ISCONNECTED;
            match gy {
                Some(mut gy) => gy.refs.push(Arc::downgrade(x)),
                None => gx.refs.push(Arc::downgrade(x)),
            }
        }
        Ok(())
    }

    /// `solisten_dequeue`: the next connection.
    ///
    /// # Errors
    ///
    /// `EINVAL` if not listening; its error, once; `EWOULDBLOCK` if none
    /// is queued.
    pub fn accept(&self) -> Result<Arc<Self>, Errno> {
        let mut i = self.inner.lock();
        if i.listen.is_none() {
            return Err(Errno::EINVAL);
        }
        if let Some(e) = i.error.take() {
            return Err(e);
        }
        let q = i.listen.as_mut().ok_or(Errno::EINVAL)?;
        q.comp.pop_front().ok_or(Errno::EAGAIN)
    }

    /// `unp_disconnect`.
    fn disconnect(&self) {
        let Some(peer) = self.inner.lock().peer.clone() else {
            return;
        };
        let dropped = {
            let (mut a, mut b) = lock2(self, &peer);
            if !a.peer.as_ref().is_some_and(|p| Arc::ptr_eq(p, &peer)) {
                return;
            }
            a.peer = None;
            match self.ty {
                Type::Dgram => {
                    a.state &= !state::ISCONNECTED;
                    let me = core::ptr::from_ref(self).addr();
                    let mut b = b;
                    let r = match &mut b {
                        Some(b) => &mut **b,
                        None => &mut *a,
                    };
                    r.refs.retain(|w| w.as_ptr().addr() != me);
                    r.dgrams.sender_gone(me, &mut r.rcv)
                }
                Type::Stream | Type::Seqpacket => {
                    for g in [Some(&mut a), b.as_mut()].into_iter().flatten() {
                        g.state = (g.state & !state::ISCONNECTED) | state::ISDISCONNECTED;
                        g.rcv.cant = true;
                    }
                    if let Some(mut b) = b {
                        b.peer = None;
                    }
                    Vec::new()
                }
            }
        };
        drop(dropped);
        self.wake();
        peer.wake();
    }

    /// The last descriptor closed (`uipc_close`, `uipc_detach`): it
    /// disconnects; datagram sockets connected to it see `ECONNRESET`;
    /// queued connections are aborted; its buffers' contents go.
    pub fn close(&self) {
        self.disconnect();
        let (refs, queued) = {
            let mut i = self.inner.lock();
            let refs = core::mem::take(&mut i.refs);
            let queued = i.listen.as_mut().map(|l| core::mem::take(&mut l.comp));
            (refs, queued)
        };
        for r in refs.iter().filter_map(Weak::upgrade) {
            // `unp_drop`.
            r.inner.lock().error = Some(match r.ty {
                Type::Dgram => Errno::ECONNRESET,
                _ => Errno::EPIPE,
            });
            r.disconnect();
        }
        for so in queued.into_iter().flatten() {
            so.close();
        }
        drop(self.dispose());
        self.rd.clear();
        self.wr.clear();
    }

    // --- Shutting down ---

    /// `socantrcvmore`, and the writers of a stream peer see it.
    pub(crate) fn cantrcvmore(&self) {
        let peer = {
            let mut i = self.inner.lock();
            i.rcv.cant = true;
            i.peer.clone()
        };
        self.rd.notify(0);
        if self.ty.connreq()
            && let Some(p) = peer
        {
            p.wr.notify(0);
        }
    }

    /// Empties its receive buffer (`unp_dispose`); what it held is
    /// returned, to be dropped with no lock held.
    pub(crate) fn dispose(&self) -> Vec<Item> {
        let mut i = self.inner.lock();
        let mut out: Vec<Item> = i.rcv.take_all().into_iter().collect();
        for d in i.dgrams.take_all() {
            out.extend(d.control.into_iter().map(|c| {
                let n = c.len();
                Item::Control(c, n)
            }));
        }
        out
    }

    /// `uipc_shutdown`.
    ///
    /// # Errors
    ///
    /// `ENOTCONN` if listening or not connected (a datagram socket still
    /// shuts down).
    pub fn shutdown(&self, how: i32) -> Result<(), Errno> {
        let err = {
            let mut i = self.inner.lock();
            if i.listen.is_some() {
                if how != SHUT_WR {
                    i.error = Some(Errno::ECONNABORTED);
                    drop(i);
                    self.rd.notify(0);
                }
                return Err(Errno::ENOTCONN);
            }
            if i.state & state::ISCONNECTED == 0 {
                if self.ty.connreq() {
                    return Err(Errno::ENOTCONN);
                }
                Err(Errno::ENOTCONN)
            } else {
                Ok(())
            }
        };
        if how == SHUT_RD || how == SHUT_RDWR {
            self.cantrcvmore();
            drop(self.dispose());
        }
        if how == SHUT_WR || how == SHUT_RDWR {
            if self.ty.connreq() {
                let peer = self.inner.lock().peer.clone();
                if let Some(p) = peer {
                    p.cantrcvmore();
                }
            } else {
                self.inner.lock().snd.cant = true;
                self.wr.notify(0);
            }
        }
        err
    }

    // --- Credentials ---

    /// `LOCAL_PEERCRED`.
    ///
    /// # Errors
    ///
    /// `ENOTCONN` (stream, seqpacket) or `EINVAL` (datagram) if there is
    /// none.
    pub fn peercred(&self) -> Result<Xucred, Errno> {
        let i = self.inner.lock();
        if i.flags & flags::HAVEPC != 0 {
            return Ok(i.peercred);
        }
        Err(if self.ty.connreq() {
            Errno::ENOTCONN
        } else {
            Errno::EINVAL
        })
    }

    // --- Stream and seqpacket data ---

    /// `uipc_sosend_stream_or_seqpacket`, one round: as much of `data`
    /// as the peer's buffer takes now (`control` with the first of it, if
    /// it fits), the last piece marked `eor` if it is the end of `data`.
    /// Returns what was taken; `data` keeps the rest.
    ///
    /// # Errors
    ///
    /// `ENOTCONN` unconnected, `EPIPE` if the peer receives no more,
    /// `EMSGSIZE` for control larger than its buffer, `EWOULDBLOCK` when
    /// it has no room.
    pub fn send_stream(
        &self,
        data: &mut VecDeque<Mbuf>,
        control: &mut Vec<Control>,
        cred: &Cred,
    ) -> Result<usize, Errno> {
        let peer = {
            let i = self.inner.lock();
            match &i.peer {
                Some(p) => Arc::clone(p),
                None => {
                    return Err(if i.state & state::ISDISCONNECTED != 0 {
                        Errno::EPIPE
                    } else {
                        Errno::ENOTCONN
                    });
                }
            }
        };
        let mut p = peer.inner.lock();
        // A receiver asking for credentials gets them with the first send
        // (`LOCAL_CREDS`) or every one (`LOCAL_CREDS_PERSISTENT`).
        if p.flags & flags::WANTCRED != 0 {
            let persistent = p.flags & flags::WANTCRED_ALWAYS != 0;
            if !persistent {
                control.retain(|c| !c.is_creds());
            }
            control.insert(0, control::sockcred(cred, persistent));
            p.flags &= !flags::WANTCRED_ONESHOT;
        }
        let clen: u32 = control.iter().map(Control::len).sum();
        if clen > p.rcv.hiwat {
            return Err(Errno::EMSGSIZE);
        }
        if p.rcv.cant {
            return Err(Errno::EPIPE);
        }
        let space = p.rcv.stream_space();
        if space < p.rcv.lowat || space < clen {
            return Err(Errno::EAGAIN);
        }
        let space = space - clen;
        for c in control.drain(..) {
            p.rcv.push_control(c);
        }
        let total: u32 = data.iter().map(Mbuf::len).sum();
        let rest = if space < total {
            buf::split(data, space)
        } else {
            VecDeque::new()
        };
        let sent = data.iter().map(|m| m.data.len()).sum();
        let now = core::mem::replace(data, rest);
        p.rcv.push_data(now);
        drop(p);
        peer.rd.notify(0);
        Ok(sent)
    }

    /// How much a stream send may copy in next: the peer's room and this
    /// socket's send buffer size (the virtual send buffer).
    pub fn stream_send_window(&self) -> usize {
        let (peer, hiwat) = {
            let i = self.inner.lock();
            (i.peer.clone(), i.snd.hiwat)
        };
        let space = peer.map_or(0, |p| p.inner.lock().rcv.stream_space());
        (space as usize).saturating_add(hiwat as usize).max(1)
    }

    /// `uipc_soreceive_stream_or_seqpacket`, one look: up to `max` bytes
    /// (stopping after a record's end), with the control messages before
    /// them if `want_control` (else those are dropped).
    ///
    /// # Errors
    ///
    /// `ENOTCONN` if never connected, its pending error, `EWOULDBLOCK`
    /// when nothing waits; at the end of the stream, an empty result.
    ///
    /// The kernel takes a large read in pieces: `cont` says this piece
    /// continues one that ended at its size, so it stops before control
    /// (a new record of it); a peek passes `skip`, the bytes it has.
    pub fn recv_stream(
        &self,
        max: usize,
        peek: bool,
        skip: usize,
        cont: bool,
        want_control: bool,
    ) -> Result<Received, Errno> {
        let mut out = Received::default();
        let peer = {
            let mut i = self.inner.lock();
            if i.state & (state::ISCONNECTED | state::ISDISCONNECTED) == 0 {
                return Err(Errno::ENOTCONN);
            }
            if i.rcv.cc < i.rcv.lowat && (i.rcv.ctl == 0 || !want_control) {
                if let Some(e) = i.error {
                    if !peek {
                        i.error = None;
                    }
                    return Err(e);
                }
                if i.rcv.cant {
                    return Ok(out);
                }
                return Err(Errno::EAGAIN);
            }
            let rcv = &mut i.rcv;
            let mut idx = 0;
            if cont && matches!(rcv.items.front(), Some(Item::Control(..))) {
                return Ok(out);
            }
            // Leading control.
            while let Some(Item::Control(..)) = rcv.items.get(idx) {
                if peek {
                    idx += 1;
                    continue;
                }
                if let Some(Item::Control(c, n)) = rcv.items.pop_front() {
                    rcv.ctl -= n;
                    rcv.mbcnt -= c.cost();
                    out.control.push(c);
                }
            }
            let mut space = max;
            let mut skip = if peek { skip } else { 0 };
            while space > 0 {
                let Some(Item::Data(m)) = rcv.items.get_mut(idx) else {
                    break;
                };
                if skip > 0 {
                    if skip >= m.data.len() {
                        skip -= m.data.len();
                        if m.eor {
                            out.flags |= abi::socket::MSG_EOR;
                            break;
                        }
                        idx += 1;
                        continue;
                    }
                    let rest = m.data.len() - skip;
                    let n = rest.min(space);
                    out.data.extend_from_slice(&m.data[skip..skip + n]);
                    space -= n;
                    skip = 0;
                    if n < rest {
                        break;
                    }
                    if m.eor {
                        out.flags |= abi::socket::MSG_EOR;
                        break;
                    }
                    idx += 1;
                    continue;
                }
                if space >= m.data.len() {
                    space -= m.data.len();
                    out.data.extend_from_slice(&m.data);
                    let eor = m.eor;
                    if peek {
                        idx += 1;
                    } else {
                        let n = m.len();
                        let cost = m.cost();
                        rcv.items.remove(idx);
                        rcv.cc -= n;
                        rcv.mbcnt -= cost;
                    }
                    if eor {
                        out.flags |= abi::socket::MSG_EOR;
                        break;
                    }
                } else {
                    out.data.extend_from_slice(&m.data[..space]);
                    if !peek {
                        m.data.drain(..space);
                        rcv.cc -= u32::try_from(space).unwrap_or(0);
                    }
                    break;
                }
            }
            if peek {
                // A peek shows the control too, copied.
                out.control = Vec::new();
            }
            i.peer.clone()
        };
        if !want_control {
            out.control.clear();
        }
        if !peek && let Some(p) = peer {
            p.wr.notify(0);
        }
        Ok(out)
    }

    /// Puts `data` back at the front of a stream's receive buffer: a
    /// receive whose control could not be delivered leaves its data
    /// queued (`uipc_soreceive_stream_or_seqpacket`'s error path).
    pub fn unread(&self, data: &[u8], eor: bool) {
        if data.is_empty() {
            return;
        }
        let mut i = self.inner.lock();
        let c = buf::chain(data, eor);
        for m in c.into_iter().rev() {
            i.rcv.cc += m.len();
            i.rcv.mbcnt += m.cost();
            i.rcv.items.push_front(Item::Data(m));
        }
    }

    /// Bytes waiting to be read, less control (`FIONREAD`, `st_size`).
    pub fn nread(&self) -> usize {
        let i = self.inner.lock();
        i.rcv.cc.saturating_sub(i.rcv.ctl) as usize
    }

    /// Room in its send buffer (`FIONSPACE`): all of it, since sends go
    /// straight to the peer.
    pub fn send_space(&self) -> usize {
        let i = self.inner.lock();
        i.snd.hiwat.min(i.snd.mbmax) as usize
    }

    /// `soo_stat`'s permission bits: whether it can still be read and
    /// written (a listening socket, neither).
    pub fn stat_rw(&self) -> (bool, bool) {
        let i = self.inner.lock();
        if i.listen.is_some() {
            return (false, false);
        }
        (!i.rcv.cant || i.rcv.cc > 0, !i.snd.cant)
    }

    // --- Datagrams ---

    /// `uipc_sosend_dgram`: one datagram, to its peer or to `to` (a
    /// temporary connection), carrying `control`.
    ///
    /// # Errors
    ///
    /// `EMSGSIZE` over `net.local.dgram.maxdgram`, `EPIPE` after
    /// `shutdown`, its pending error, `EDESTADDRREQ` unconnected without
    /// `to`, `ENOTCONN` with neither, `ENOBUFS` when the receiver is full.
    pub fn send_dgram(
        &self,
        data: &[u8],
        mut control: Vec<Control>,
        to: Option<&Arc<Self>>,
        cred: &Cred,
    ) -> Result<(), Errno> {
        if data.len() as u64 > space::DGRAM_MAX.load(Ordering::Relaxed) {
            return Err(Errno::EMSGSIZE);
        }
        let (peer, from, snd) = {
            let mut i = self.inner.lock();
            if i.snd.cant {
                return Err(Errno::EPIPE);
            }
            if let Some(e) = i.error.take() {
                return Err(e);
            }
            if i.state & state::ISCONNECTED == 0 && to.is_none() {
                return Err(Errno::EDESTADDRREQ);
            }
            let peer = match to {
                Some(t) => Arc::clone(t),
                None => i.peer.clone().ok_or(Errno::ENOTCONN)?,
            };
            (
                peer,
                i.addr.clone().unwrap_or_else(noname),
                (i.snd.hiwat, i.snd.mbmax),
            )
        };
        if to.is_some() && peer.ty != Type::Dgram {
            return Err(Errno::EPROTOTYPE);
        }
        let me = core::ptr::from_ref(self).addr();
        let r = {
            let mut p = peer.inner.lock();
            if p.flags & flags::WANTCRED != 0 {
                let persistent = p.flags & flags::WANTCRED_ALWAYS != 0;
                if !persistent {
                    control.retain(|c| !c.is_creds());
                }
                control.insert(0, control::sockcred(cred, persistent));
            }
            let d = dgram::Dgram::new(from, control, data);
            let conn = to.is_none();
            let p = &mut *p;
            p.dgrams
                .push(d, if conn { Some((me, snd)) } else { None }, &mut p.rcv)
        };
        match r {
            Ok(()) => {
                peer.rd.notify(0);
                Ok(())
            }
            Err(d) => {
                drop(d);
                Err(Errno::ENOBUFS)
            }
        }
    }

    /// `uipc_soreceive_dgram`, one look: the next datagram, its data to
    /// `max` bytes (`MSG_TRUNC` if cut short).
    ///
    /// # Errors
    ///
    /// Its pending error, `EWOULDBLOCK` when nothing waits; after
    /// `shutdown` or for `max` 0, an empty result.
    pub fn recv_dgram(
        &self,
        max: usize,
        peek: bool,
        want_control: bool,
    ) -> Result<Received, Errno> {
        let mut i = self.inner.lock();
        let i = &mut *i;
        let Some(d) = i.dgrams.next(peek, &mut i.rcv) else {
            if let Some(e) = i.error {
                if !peek {
                    i.error = None;
                }
                return Err(e);
            }
            if i.rcv.cant || max == 0 {
                return Ok(Received::default());
            }
            return Err(Errno::EAGAIN);
        };
        let mut out = Received {
            full: d.data.len(),
            from: Some(d.from.clone()),
            ..Received::default()
        };
        let n = d.data.len().min(max);
        out.data.extend_from_slice(&d.data[..n]);
        if n < d.data.len() {
            out.flags |= abi::socket::MSG_TRUNC;
        }
        if let Some(d) = d.take()
            && want_control
        {
            out.control = d.control;
        }
        Ok(out)
    }

    /// Calls `f` on every descriptor in flight in its buffers (and its
    /// queued connections', if listening): `unp_scan`.
    pub(crate) fn scan(&self, f: &mut dyn FnMut(&InFlight)) {
        let queued: Vec<Arc<Self>> = {
            let i = self.inner.lock();
            if let Some(l) = &i.listen {
                l.comp.iter().cloned().collect()
            } else {
                for it in &i.rcv.items {
                    if let Item::Control(c, _) = it {
                        control::rights_of(c).iter().for_each(&mut *f);
                    }
                }
                i.dgrams.scan(f);
                Vec::new()
            }
        };
        for q in queued {
            q.scan(f);
        }
    }
}

/// `cru2xt`: a credential as `LOCAL_PEERCRED` gives it.
fn xucred(c: &Cred) -> Xucred {
    let mut x = Xucred {
        cr_version: abi::socket::XUCRED_VERSION,
        cr_uid: c.euid,
        cr_pid: c.pid,
        ..Xucred::default()
    };
    let n = c.groups.len().min(abi::socket::XU_NGROUPS);
    x.cr_ngroups = i16::try_from(n).unwrap_or(0);
    x.cr_groups[..n].copy_from_slice(&c.groups[..n]);
    x
}

pub use buf::{Mbuf, chain};

pub use poll::filter;

/// The `AtomicU32` the kernel bumps for each `gc` it wants run.
pub static GC_WANTED: AtomicU32 = AtomicU32::new(0);
