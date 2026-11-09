// SPDX-License-Identifier: BSD-2-Clause
//! Internet sockets as open files (FreeBSD's `socketops` on an `AF_INET`
//! or `AF_INET6` socket): the [`inet`] crate's state, and the waits a
//! send, receive, or connect makes.
//!
//! A send or receive that would block registers a poller on the
//! socket's writers' or readers' `Notify` and tries again before it
//! sleeps, for as long as `SO_SNDTIMEO` or `SO_RCVTIMEO` allows, as the
//! UNIX-domain sockets' do (`super::socket::wait_until`).
//!
//! Behavior reference: sys/kern/sys_socket.c, sys/kern/uipc_socket.c
//! (`sosend_generic`, `sosend_dgram`, `soreceive_generic`,
//! `soreceive_dgram`), sys/kern/uipc_syscalls.c (`kern_connectat`'s
//! wait).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::socket::{
    MSG_DONTWAIT, MSG_EOR, MSG_NBIO, MSG_NOSIGNAL, MSG_OOB, MSG_PEEK, MSG_TLSAPPDATA, MSG_WAITALL,
};
use abi::stat::{S_IFSOCK, Stat};
use inet::{Proto, Socket};
use sched::Sleepable;
use vfs::Uio;

use super::socket::wait_until;

/// An open internet socket.
pub(crate) struct InetFile {
    pub(crate) so: Arc<Socket>,
    /// Its creator's ids (`so_cred`), for `fstat`.
    uid: u32,
    gid: u32,
}

impl InetFile {
    pub(crate) fn new(so: Arc<Socket>, uid: u32, gid: u32) -> Self {
        Self { so, uid, gid }
    }

    /// `soo_stat`.
    pub(crate) fn stat(&self) -> Stat {
        let (r, w) = self.so.stat_rw();
        let mut mode = S_IFSOCK;
        if r {
            mode |= 0o444;
        }
        if w {
            mode |= 0o222;
        }
        Stat {
            st_mode: mode,
            st_size: i64::try_from(self.so.nread()).unwrap_or(i64::MAX),
            st_uid: self.uid,
            st_gid: self.gid,
            st_blksize: i32::try_from(self.so.snd_hiwat()).unwrap_or(i32::MAX),
            // `NODEV`.
            st_dev: u64::MAX,
            ..Stat::default()
        }
    }

    /// `read(2)`: a receive with no flags.
    pub(crate) fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        receive(s, &self.so, uio, 0, nonblock).map(|g| g.count)
    }

    /// `write(2)`: a send with no flags.
    pub(crate) fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        send(
            s,
            &self.so,
            uio,
            None,
            inet::SendCtl::default(),
            0,
            nonblock,
        )
    }
}

impl Drop for InetFile {
    /// The last descriptor closed (`soo_close`).
    fn drop(&mut self) {
        self.so.close();
    }
}

/// The caller's privileges as the protocols need them.
pub(crate) fn cred() -> inet::Cred {
    use crate::cred::Priv;
    let uid = crate::proc::Process::with_current(|p| p.cred().euid()).unwrap_or(0);
    inet::Cred {
        uid,
        reservedport: super::priv_check(Priv::NetinetReservedport).is_ok(),
        reuseport: super::priv_check(Priv::NetinetReuseport).is_ok(),
        bindany: super::priv_check(Priv::NetinetBindany).is_ok(),
        raw: super::priv_check(Priv::NetinetRaw).is_ok(),
    }
}

/// The most a send or receive moves through the kernel at once.
const PIECE: usize = 64 * 1024;

/// `sosend` (and `sousrsend`'s rules): `uio` on `so`, to `to` (a
/// datagram's destination, raw). A stream send that made progress before
/// a signal or a full buffer counts what went; `EPIPE` sends `SIGPIPE`
/// unless `SO_NOSIGPIPE` or `MSG_NOSIGNAL`.
pub(crate) fn send(
    s: &Sleepable,
    so: &Arc<Socket>,
    uio: &mut dyn Uio,
    to: Option<&[u8]>,
    ctl: inet::SendCtl,
    flags: i32,
    nonblock: bool,
) -> Result<usize, Errno> {
    let nonblock = nonblock || flags & (MSG_DONTWAIT | MSG_NBIO) != 0;
    let r = match so.proto {
        Proto::Udp | Proto::Raw(_) => {
            if flags & MSG_OOB != 0 {
                Err(Errno::EOPNOTSUPP)
            } else {
                let len = uio.resid();
                if len > so.snd_hiwat() as usize {
                    Err(Errno::EMSGSIZE)
                } else {
                    // Straight into mbufs (`m_uiotombuf`), unless it fits
                    // the header's mbuf with them (`max_hdr`'s room).
                    if so.proto == Proto::Udp && len > mbuf::MHLEN as usize - mbuf::MAX_HDR as usize
                    {
                        let data = mbuf::Chain::fill(len, |b, _| uio.copy_in(b))?;
                        so.send_dgram_chain(data, to, &cred(), ctl)
                    } else {
                        let mut data = alloc::vec![0u8; len];
                        let n = uio.copy_in(&mut data)?;
                        data.truncate(n);
                        so.send_dgram_ctl(&data, to, &cred(), ctl)
                    }
                }
            }
        }
        Proto::Tcp => {
            let tls_type = ctl.tls_type;
            send_stream(s, so, uio, to, flags & MSG_OOB != 0, nonblock, tls_type)
        }
    };
    if r == Err(Errno::EPIPE)
        && flags & MSG_NOSIGNAL == 0
        && !so.nosigpipe()
        && let Some(t) = crate::uthread::UserThread::current()
    {
        let _ = crate::signal::post_thread(
            &t,
            crate::signal::user_info(abi::signal::SIGPIPE, 0, abi::signal::code::SI_KERNEL),
        );
    }
    r
}

/// A stream's send; `to`, a peer named, is connected to if the socket is
/// not yet (an implied connect), and otherwise ignored. `tls_type`, from
/// `TLS_SET_RECORD_TYPE`, makes it all records of that type, sent at once
/// or not at all.
fn send_stream(
    s: &Sleepable,
    so: &Arc<Socket>,
    uio: &mut dyn Uio,
    to: Option<&[u8]>,
    oob: bool,
    nonblock: bool,
    tls_type: Option<u8>,
) -> Result<usize, Errno> {
    let (timeo, _) = so.timeouts();
    // Plain data goes into mbufs before any lock (`sosend_generic`).
    if to.is_none() && tls_type.is_none() && so.tls_tx_layout().is_none() {
        return send_mbufs(s, so, uio, oob, nonblock, timeo);
    }
    let cred = to.map(|_| cred());
    let peer = to.zip(cred.as_ref());
    let send = |data: &[u8], last: bool| match (tls_type, peer) {
        (Some(t), _) => so.send_tls(data, t, true, last, peer),
        (None, Some((to, cred))) => so.send_stream_named(data, last, to, cred),
        (None, None) => so.send_stream(data, last),
    };
    // `sosend_generic`: with a TLS session, sending nothing is an empty
    // record where the suite has them, and otherwise an error.
    if uio.resid() == 0 && so.tls_tx_layout().is_some() {
        let t = tls_type.unwrap_or(abi::ktls::TLS_RLTYPE_APP);
        return so.send_tls(&[], t, false, false, peer);
    }
    if tls_type.is_some() && uio.resid() > so.snd_hiwat() as usize {
        return Err(Errno::EMSGSIZE);
    }
    let mut sent = 0;
    let mut buf: Vec<u8> = Vec::new();
    let mut at = 0;
    loop {
        if at == buf.len() {
            if uio.resid() == 0 {
                return Ok(sent);
            }
            let n = if tls_type.is_some() {
                uio.resid()
            } else {
                uio.resid().min(so.send_window()).min(PIECE)
            };
            buf.resize(n, 0);
            let got = uio.copy_in(&mut buf)?;
            buf.truncate(got);
            at = 0;
            if got == 0 {
                return Ok(sent);
            }
        }
        let last = oob && uio.resid() == 0;
        let r = if nonblock {
            send(&buf[at..], last)
        } else {
            let _w = sched::wait_message("sbwait");
            wait_until(s, &so.wr, timeo, || send(&buf[at..], last))
        };
        match r {
            Ok(n) => {
                at += n;
                sent += n;
            }
            // What went counts after a signal or a full buffer.
            Err(Errno::EINTR | Errno::ERESTART | Errno::EAGAIN) if sent > 0 => return Ok(sent),
            Err(e) => return Err(e),
        }
    }
}

/// [`send_stream`] of plain data: each piece copied from `uio` straight
/// into mbufs (`m_uiotombuf`), which the send buffer then takes as they
/// are, as much as it has room for each time.
fn send_mbufs(
    s: &Sleepable,
    so: &Arc<Socket>,
    uio: &mut dyn Uio,
    oob: bool,
    nonblock: bool,
    timeo: u64,
) -> Result<usize, Errno> {
    let mut sent = 0;
    let mut pending = mbuf::Chain::new();
    loop {
        if pending.is_empty() {
            if uio.resid() == 0 {
                return Ok(sent);
            }
            let n = uio.resid().min(so.send_window()).min(PIECE);
            pending = mbuf::Chain::fill(n, |b, _| uio.copy_in(b))?;
            if pending.is_empty() {
                return Ok(sent);
            }
        }
        let last = oob && uio.resid() == 0;
        let r = if nonblock {
            so.send_stream_chain(&mut pending, last)
        } else {
            let _w = sched::wait_message("sbwait");
            wait_until(s, &so.wr, timeo, || {
                so.send_stream_chain(&mut pending, last)
            })
        };
        match r {
            Ok(n) => sent += n,
            // What went counts after a signal or a full buffer.
            Err(Errno::EINTR | Errno::ERESTART | Errno::EAGAIN) if sent > 0 => return Ok(sent),
            Err(e) => return Err(e),
        }
    }
}

/// What a receive got besides its data.
pub(crate) struct Got {
    pub(crate) count: usize,
    pub(crate) from: Option<inet::Sockaddr>,
    pub(crate) flags: i32,
    /// A datagram's whole length.
    pub(crate) full: usize,
    pub(crate) meta: Option<inet::Meta>,
    /// A TLS record's `TLS_GET_RECORD`.
    pub(crate) tls: Option<inet::tls::RecordInfo>,
}

/// `soreceive` into `uio`, waiting unless `nonblock`; `MSG_WAITALL` waits
/// for all of a stream read, a partial result counting after a signal.
/// `MSG_OOB` takes the urgent byte, never waiting.
pub(crate) fn receive(
    s: &Sleepable,
    so: &Arc<Socket>,
    uio: &mut dyn Uio,
    flags: i32,
    nonblock: bool,
) -> Result<Got, Errno> {
    let nonblock = nonblock || flags & (MSG_DONTWAIT | MSG_NBIO) != 0;
    let peek = flags & MSG_PEEK != 0;
    let (_, timeo) = so.timeouts();
    let mut got = Got {
        count: 0,
        from: None,
        flags: 0,
        full: 0,
        meta: None,
        tls: None,
    };
    if so.proto.is_dgram() {
        if flags & MSG_OOB != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let max = uio.resid();
        let once = || so.recv_dgram(max, peek);
        let r = if nonblock {
            once()
        } else {
            let _w = sched::wait_message("sbwait");
            wait_until(s, &so.rd, timeo, once)
        }?;
        got.count = uio.copy_out(&r.data)?;
        got.from = r.from;
        got.flags = r.flags;
        got.full = r.full;
        got.meta = r.meta;
        return Ok(got);
    }
    if flags & MSG_OOB != 0 {
        let r = so.recv_stream(uio.resid().min(1), peek, true)?;
        got.count = uio.copy_out(&r.data)?;
        got.flags = r.flags;
        return Ok(got);
    }
    let waitall = flags & MSG_WAITALL != 0 && !peek;
    let appdata = flags & MSG_TLSAPPDATA != 0;
    loop {
        let want = uio.resid().min(PIECE);
        let first = || {
            if appdata {
                so.recv_tls_app(want, peek)
            } else {
                so.recv_stream_mbufs(want, peek)
            }
        };
        let r = if nonblock || (got.count > 0 && !waitall) {
            first()
        } else {
            let _w = sched::wait_message("sbwait");
            wait_until(s, &so.rd, timeo, first)
        };
        let r = match r {
            Ok(r) => r,
            Err(Errno::EINTR | Errno::ERESTART | Errno::EAGAIN) if got.count > 0 => return Ok(got),
            Err(e) => return Err(e),
        };
        // A TLS record comes in `data`, plain data as mbufs, copied out
        // now that the socket's lock is not held (`m_mbuftouio`).
        let n = r.data.len() + r.chain.len();
        got.count += uio.copy_out(&r.data)?;
        for seg in r.chain.segments() {
            got.count += uio.copy_out(seg)?;
        }
        got.flags |= r.flags;
        got.tls = got.tls.or(r.tls);
        // A TLS record is a read's end.
        if n == 0 || uio.resid() == 0 || peek || r.flags & MSG_EOR != 0 {
            return Ok(got);
        }
        // More waits only with `MSG_WAITALL`; without, what is there now
        // still comes (a read stops short at the urgent mark).
        if !waitall && (n < want || so.nread() == 0 || so.at_mark()) {
            return Ok(got);
        }
    }
}

/// `connect(2)` on `so`: the protocol's connect, and for a blocking TCP
/// one, the wait for it to finish (`kern_connectat`'s `soconnect` and
/// `sbwait` loop), `ETIMEDOUT` after `TCP_KEEPINIT`; an interrupted wait
/// leaves it connecting (`EINTR`, as FreeBSD's).
pub(crate) fn connect(
    s: &Sleepable,
    so: &Arc<Socket>,
    raw: &[u8],
    nonblock: bool,
) -> Result<(), Errno> {
    so.connect(raw, &cred(), nonblock)?;
    if so.proto.is_dgram() {
        return Ok(());
    }
    let _w = sched::wait_message("connec");
    match wait_until(s, &so.wr, so.connect_timeout(), || so.connect_done()) {
        Err(Errno::EAGAIN) => {
            so.connect_timed_out();
            so.take_error();
            Err(Errno::ETIMEDOUT)
        }
        Err(Errno::ERESTART) => Err(Errno::EINTR),
        r => r,
    }
}
