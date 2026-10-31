// SPDX-License-Identifier: BSD-2-Clause
//! UNIX-domain sockets as open files (FreeBSD's `socketops`): the
//! [`socket`] crate's state, the names they are bound to, and the waits
//! a send or receive makes.
//!
//! A send that finds no room, or a receive nothing, registers a poller on
//! the socket's writers' or readers' `Notify` and tries again before it
//! sleeps (no wakeup is lost), for as long as the socket's `SO_SNDTIMEO`
//! or `SO_RCVTIMEO` allows.
//!
//! Behavior reference: sys/kern/sys_socket.c, sys/kern/uipc_syscalls.c,
//! sys/kern/uipc_usrreq.c (`unp_connectat`, `vfs_unp_reclaim`).

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use abi::errno::Errno;
use abi::socket::{MSG_DONTWAIT, MSG_EOR, MSG_NBIO, MSG_NOSIGNAL, MSG_OOB, MSG_PEEK, MSG_WAITALL};
use abi::stat::{S_IFSOCK, Stat};
use event::Wake;
use sched::Sleepable;
use socket::{Control, Cred, Socket, Type};
use sync::SpinLock;
use vfs::{Uio, Vnode, VnodeUse};

use crate::event::Poller;
use crate::signal::Woke;

/// The sockets bound to names, by node (FreeBSD's `v_unpcb`).
static BOUND: SpinLock<BTreeMap<(u64, u64), Weak<Socket>>> = SpinLock::new(BTreeMap::new());

fn key(v: &Vnode) -> (u64, u64) {
    (v.mount().id, v.ino())
}

/// The socket bound to node `v`, if one still is.
pub(crate) fn bound_to(v: &Vnode) -> Option<Arc<Socket>> {
    BOUND.lock().get(&key(v)).and_then(Weak::upgrade)
}

/// An open UNIX-domain socket.
pub(crate) struct SocketFile {
    pub(crate) so: Arc<Socket>,
    /// The node `bind` made, held while the socket is open.
    bound: SpinLock<Option<VnodeUse>>,
    /// Its creator's ids (`so_cred`), for `fstat`.
    uid: u32,
    gid: u32,
}

impl SocketFile {
    pub(crate) fn new(so: Arc<Socket>, uid: u32, gid: u32) -> Self {
        Self {
            so,
            bound: SpinLock::new(None),
            uid,
            gid,
        }
    }

    /// Records the node `bind` made for it.
    pub(crate) fn set_bound(&self, v: &Arc<Vnode>) {
        BOUND.lock().insert(key(v), Arc::downgrade(&self.so));
        *self.bound.lock() = Some(v.use_ref());
    }

    /// `soo_stat` and `uipc_sense`.
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
            st_ino: self.so.ino,
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
        receive(s, &self.so, uio, 0, nonblock, None).map(|g| g.count)
    }

    /// `write(2)`: a send with no flags.
    pub(crate) fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        send(s, &self.so, uio, Vec::new(), None, 0, nonblock)
    }
}

impl Drop for SocketFile {
    /// The last descriptor closed (`soo_close`): the name goes, the
    /// socket disconnects, and the collector runs if descriptors are in
    /// flight (`uipc_fdclose`).
    fn drop(&mut self) {
        if let Some(v) = self.bound.lock().take() {
            let mut b = BOUND.lock();
            if b.get(&key(v.vnode()))
                .is_some_and(|w| w.as_ptr() == Arc::as_ptr(&self.so))
            {
                b.remove(&key(v.vnode()));
            }
        }
        self.so.close();
        collect();
    }
}

/// Runs the collector, once at a time (a socket it frees closes and asks
/// again, which the running pass picks up).
pub(crate) fn collect() {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    static AGAIN: AtomicBool = AtomicBool::new(false);
    if socket::inflight() == 0 {
        return;
    }
    AGAIN.store(true, Ordering::SeqCst);
    while AGAIN.swap(false, Ordering::SeqCst) {
        if RUNNING.swap(true, Ordering::SeqCst) {
            return;
        }
        socket::gc();
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Waits for a change `notify` tells of: `retry` first, then, if it
/// would block, again after registering (so a change meanwhile is seen),
/// and if it still would, until woken, `timeout` (0: none) passes, or a
/// signal.
/// `EAGAIN` when the time runs out.
pub(crate) fn wait_until<R>(
    s: &Sleepable,
    notify: &event::Notify,
    timeout: u64,
    mut retry: impl FnMut() -> Result<R, Errno>,
) -> Result<R, Errno> {
    // Once without waiting: most calls need not (`sbwait` only when the
    // buffer has no room or nothing to read).
    match retry() {
        Err(Errno::EAGAIN) => {}
        r => return r,
    }
    let poller = Poller::new();
    let w: Arc<dyn Wake> = Arc::clone(&poller) as Arc<dyn Wake>;
    let deadline = (timeout != 0).then(|| crate::time::now_ns().saturating_add(timeout));
    loop {
        poller.arm();
        notify.record(&w);
        match retry() {
            Err(Errno::EAGAIN) => {}
            r => return r,
        }
        let left = deadline.map(|d| d.saturating_sub(crate::time::now_ns()));
        if left == Some(0) {
            return Err(Errno::EAGAIN);
        }
        if poller.wait(s, left)? == Woke::TimedOut {
            return Err(Errno::EAGAIN);
        }
    }
}

/// The caller's credential as messages carry it.
pub(crate) fn cred() -> Cred {
    let Some(p) = crate::proc::Process::current() else {
        return Cred::default();
    };
    let c = crate::fs::cred();
    let mut groups = Vec::with_capacity(c.groups().len() + 1);
    groups.push(c.egid());
    groups.extend(c.groups().iter().copied().filter(|&g| g != c.egid()));
    Cred {
        pid: p.pid(),
        ruid: c.ruid,
        euid: c.euid(),
        rgid: c.rgid,
        egid: c.egid(),
        groups,
    }
}

/// `sosend` (and `sousrsend`'s rules): `uio` on `so`, with `control`, to
/// `to` (a datagram's destination). A stream send that made progress
/// before a signal or a full buffer counts what went; `EPIPE` sends
/// `SIGPIPE` unless `SO_NOSIGPIPE` or `MSG_NOSIGNAL`.
pub(crate) fn send(
    s: &Sleepable,
    so: &Arc<Socket>,
    uio: &mut dyn Uio,
    mut control: Vec<Control>,
    to: Option<Arc<Socket>>,
    flags: i32,
    nonblock: bool,
) -> Result<usize, Errno> {
    if flags & MSG_OOB != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let nonblock = nonblock || flags & (MSG_DONTWAIT | MSG_NBIO) != 0;
    // Built only if the receiver wants it (`LOCAL_CREDS`).
    let cred: &dyn Fn() -> Cred = &cred;
    let r = if so.ty == Type::Dgram {
        let len = uio.resid();
        if len as u64 > socket::space::DGRAM_MAX.load(Ordering::Relaxed) {
            Err(Errno::EMSGSIZE)
        } else {
            // Copied in straight to the mbufs (`m_uiotombuf`).
            let data = socket::Chain::fill(len, |b, _| uio.copy_in(b))?;
            let n = data.len();
            so.send_dgram(data, control, to.as_ref(), cred).map(|()| n)
        }
    } else {
        send_stream(s, so, uio, &mut control, flags, nonblock, cred)
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

fn send_stream(
    s: &Sleepable,
    so: &Arc<Socket>,
    uio: &mut dyn Uio,
    control: &mut Vec<Control>,
    flags: i32,
    nonblock: bool,
    cred: &dyn Fn() -> Cred,
) -> Result<usize, Errno> {
    let eor = flags & MSG_EOR != 0;
    let (timeo, _) = so.timeouts();
    // Found once; each round then takes only its buffer's lock.
    let peer = so.stream_peer()?;
    // The first copy-in is sized to the send buffer, the later ones to
    // that and the room the last round left.
    let mut window = so.snd_hiwat() as usize;
    let mut sent = 0;
    let mut chain = socket::Chain::new();
    loop {
        if chain.is_empty() && (uio.resid() > 0 || !control.is_empty()) {
            let n = uio.resid().min(window.max(1));
            // Copied in straight to the mbufs.
            let last = eor && uio.resid() == n;
            chain = socket::Chain::fill(n, |b, _| uio.copy_in(b))?;
            // A short copy ends the send there, not the record.
            chain.set_eor(last && chain.len() == n);
        }
        if chain.is_empty() && control.is_empty() {
            return Ok(sent);
        }
        let r = if nonblock {
            so.send_stream_to(&peer, &mut chain, control, cred)
        } else {
            let _w = sched::wait_message("sbwait");
            wait_until(s, &so.wr, timeo, || {
                so.send_stream_to(&peer, &mut chain, control, cred)
            })
        };
        match r {
            Ok((n, left)) => {
                sent += n;
                window = (left as usize).saturating_add(so.snd_hiwat() as usize);
            }
            // What went counts after a signal or a full buffer.
            Err(Errno::EINTR | Errno::ERESTART | Errno::EAGAIN) if sent > 0 => return Ok(sent),
            Err(e) => return Err(e),
        }
        if chain.is_empty() && uio.resid() == 0 {
            return Ok(sent);
        }
    }
}

/// Copies out each mbuf's data, as far as `uio` has room.
fn copy_chain(uio: &mut dyn Uio, c: &socket::Chain) -> Result<usize, Errno> {
    let mut n = 0;
    for seg in c.segments() {
        if uio.resid() == 0 {
            break;
        }
        n += uio.copy_out(seg)?;
    }
    Ok(n)
}

/// The most a peek copies out at once.
const PIECE: usize = 16 * 1024;

/// What one look at a stream takes: all `uio` wants, or a peek's piece.
fn piece(uio: &dyn Uio, peek: bool) -> usize {
    if peek {
        uio.resid().min(PIECE)
    } else {
        uio.resid()
    }
}

/// What a receive got besides its data, which went to the caller's
/// buffers.
pub(crate) struct Got {
    pub(crate) count: usize,
    pub(crate) from: Option<Vec<u8>>,
    pub(crate) flags: i32,
    /// A datagram's whole length.
    pub(crate) full: usize,
}

/// `soreceive` into `uio`: what waits, to its size (and the control
/// before it, if `want_control`), waiting unless `nonblock`;
/// `MSG_WAITALL` waits for all of it on a stream, a partial result
/// counting after a signal. A receive unlinks all it can take at once
/// and wakes the writer once (`uipc_soreceive_stream_or_seqpacket`); a
/// peek, which copies under the lock, moves pieces of [`PIECE`].
///
/// Control goes to `ext` (which installs descriptors) before the data it
/// came with is copied; if that fails, a stream's data goes back, as
/// FreeBSD's does.
pub(crate) fn receive(
    s: &Sleepable,
    so: &Arc<Socket>,
    uio: &mut dyn Uio,
    flags: i32,
    nonblock: bool,
    mut ext: Option<&mut dyn FnMut(Vec<Control>) -> Result<(), Errno>>,
) -> Result<Got, Errno> {
    let want_control = ext.is_some();
    if flags & MSG_OOB != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let nonblock = nonblock || flags & (MSG_DONTWAIT | MSG_NBIO) != 0;
    let peek = flags & MSG_PEEK != 0;
    let (_, timeo) = so.timeouts();
    let mut got = Got {
        count: 0,
        from: None,
        flags: 0,
        full: 0,
    };
    if so.ty == Type::Dgram {
        let max = uio.resid();
        let once = || so.recv_dgram(max, peek, want_control);
        let r = if nonblock {
            once()
        } else {
            let _w = sched::wait_message("sbwait");
            wait_until(s, &so.rd, timeo, once)
        }?;
        if let Some(ext) = ext.as_mut()
            && !r.control.is_empty()
        {
            ext(r.control)?;
        }
        got.count = uio.copy_out(&r.data)?;
        got.count += copy_chain(uio, &r.chain)?;
        got.from = r.from;
        got.flags = r.flags;
        got.full = r.full;
        return Ok(got);
    }
    let waitall = flags & MSG_WAITALL != 0 && !peek;
    loop {
        // A look that may wait, then what more is there without waiting.
        let want = piece(uio, peek);
        let skip = got.count;
        let first = || so.recv_stream_parts(want, peek, skip, false, want_control);
        let r = if nonblock {
            first()
        } else {
            let _w = sched::wait_message("sbwait");
            wait_until(s, &so.rd, timeo, first)
        };
        let mut r = match r {
            Ok(r) => r,
            Err(Errno::EINTR | Errno::ERESTART | Errno::EAGAIN) if got.count > 0 => return Ok(got),
            Err(e) => return Err(e),
        };
        loop {
            let asked = piece(uio, peek);
            let n = r.data.len() + r.chain.len();
            if let Some(ext) = ext.as_mut()
                && !r.control.is_empty()
            {
                let c = core::mem::take(&mut r.control);
                if let Err(e) = ext(c) {
                    if !peek {
                        so.unread(core::mem::take(&mut r.chain));
                    }
                    return Err(e);
                }
            }
            // Each mbuf's bytes straight out (a peek's copy is in `data`).
            got.count += uio.copy_out(&r.data)?;
            got.count += copy_chain(uio, &r.chain)?;
            got.flags |= r.flags;
            if n == 0 || n < asked || r.flags & abi::socket::MSG_EOR != 0 || uio.resid() == 0 {
                break;
            }
            let more = piece(uio, peek);
            match so.recv_stream_parts(more, peek, got.count, true, want_control) {
                Ok(m) => r = m,
                Err(_) => break,
            }
        }
        let empty = r.data.is_empty() && r.chain.is_empty();
        if !waitall || got.flags & abi::socket::MSG_EOR != 0 || uio.resid() == 0 || empty {
            return Ok(got);
        }
    }
}
