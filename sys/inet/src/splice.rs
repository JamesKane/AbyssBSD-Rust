// SPDX-License-Identifier: BSD-2-Clause
//! Socket splicing (`SO_SPLICE`): what a TCP socket (the source)
//! receives goes out another (the sink) without passing through user
//! space, up to a byte limit, until it is unspliced, the limit is
//! reached, either side can move no more, or a timeout set at splicing
//! passes.
//!
//! As FreeBSD's, the data moves on a worker thread, not in the call that
//! brought it: a wakeup of a spliced buffer (data in the source's receive
//! buffer, room in the sink's send buffer) queues the splice instead of
//! waking readers or writers, and the kernel's worker runs what is
//! queued ([`run`]), one transfer per turn, queueing it again while there
//! is more. Splices can make loops; the worker running one forever is
//! the kernel's to keep from starving anything.
//!
//! The states are FreeBSD's less the wait an unsplice makes for a
//! running transfer: a transfer takes data from the source only while
//! the source is still spliced to it (under the source's lock, which an
//! unsplice takes), so once an unsplice returns nothing more leaves the
//! source; at most what was already taken is still on its way out.
//!
//! Behavior reference: sys/kern/uipc_socket.c (`so_splice`,
//! `so_unsplice`, `so_splice_xfer`, `so_splice_xfer_data`,
//! `so_splice_dispatch`, `splice_work_thread`, `SO_SPLICE` in `sosetopt`
//! and `sogetopt`, `filt_soread`, `sopoll_generic`),
//! sys/kern/uipc_sockbuf.c (`sorwakeup_locked`, `sowwakeup_locked`),
//! sys/sys/socketvar.h (`struct so_splice`).

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};

use abi::errno::Errno;
use abi::inet::ss;
use sync::SpinLock;

use crate::{Inner, Proto, Socket};

/// [`Socket::spliced`] bits: `SB_SPLICED` on the receive buffer (it is
/// a source) and on the send buffer (it is a sink).
pub(crate) const RCV: u8 = 1;
pub(crate) const SND: u8 = 2;

/// A splice's state (`enum so_splice_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum State {
    /// Being set up: wakeups queue nothing yet.
    Init,
    /// Waiting for work.
    Idle,
    /// On the worker's queue.
    Queued,
    /// Transferring.
    Running,
    /// Unspliced: the end.
    Closed,
}

impl State {
    const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Init,
            1 => Self::Idle,
            2 => Self::Queued,
            3 => Self::Running,
            _ => Self::Closed,
        }
    }
}

/// One splice (`struct so_splice`): the source, the sink, the limit.
pub struct Splice {
    src: Arc<Socket>,
    dst: Arc<Socket>,
    /// The most bytes to move; `None` for no limit.
    max: Option<u64>,
    state: AtomicU8,
    /// What a transfer took from the source and the sink had no room for
    /// after all (a writer to the sink took it meanwhile): the next
    /// transfer sends it first. Only the transfer running touches it.
    carry: SpinLock<Vec<u8>>,
}

impl core::fmt::Debug for Splice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Splice")
            .field("src", &self.src.id)
            .field("dst", &self.dst.id)
            .field("max", &self.max)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

/// The splices waiting for the worker (`splice_wq`).
static QUEUE: SpinLock<VecDeque<Arc<Splice>>> = SpinLock::new(VecDeque::new());

fn no_worker() {}

/// How the kernel's worker is woken.
static WORKER: SpinLock<fn()> = SpinLock::new(no_worker);

/// Installs the wakeup of the kernel's worker, which calls [`run`]
/// whenever [`pending`] holds.
pub fn set_worker(wake: fn()) {
    *WORKER.lock() = wake;
}

/// Whether splices wait for the worker.
#[must_use]
pub fn pending() -> bool {
    !QUEUE.lock().is_empty()
}

/// The worker's turn (`splice_work_thread`): one transfer for each
/// splice queued now (each may queue itself again). Returns how many
/// ran.
pub fn run() -> usize {
    let batch = core::mem::take(&mut *QUEUE.lock());
    let n = batch.len();
    for sp in batch {
        sp.xfer();
    }
    n
}

/// `so_splice_dispatch_async`.
fn enqueue(sp: Arc<Splice>) {
    QUEUE.lock().push_back(sp);
    let wake = *WORKER.lock();
    wake();
}

impl Splice {
    fn state(&self) -> State {
        State::from_u8(self.state.load(Ordering::Acquire))
    }

    fn cas(&self, from: State, to: State) -> bool {
        self.state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// `so_splice_dispatch`: queues it unless it is queued or running (a
    /// running transfer looks again when it ends).
    fn dispatch(self: &Arc<Self>) {
        if self.cas(State::Idle, State::Queued) {
            enqueue(Arc::clone(self));
        }
    }

    /// `so_splice_timeout`: the splice's time is up; it is unspliced if
    /// it is still in place.
    pub fn expire(self: &Arc<Self>) {
        let _ = self.src.unsplice_if(Some(self));
    }

    /// `so_splice_xfer`: one transfer, then an unsplice if it failed or
    /// reached the limit, or another turn if there is more.
    fn xfer(self: &Arc<Self>) {
        if !self.cas(State::Queued, State::Running) {
            // Unspliced while it waited.
            return;
        }
        let r = self.move_some();
        let done = self.max.is_some_and(|m| {
            // Relaxed: only transfers add to it, and this one is the only
            // one running.
            self.src.splice_sent.load(Ordering::Relaxed) >= m
        });
        if r.is_err() || done {
            let _ = self.src.unsplice_if(Some(self));
            return;
        }
        if !self.cas(State::Running, State::Idle) {
            return;
        }
        // Data that came while it ran saw it running and queued nothing:
        // look again now that it is idle (FreeBSD's look under its mutex).
        if self.more() {
            self.dispatch();
        }
    }

    /// Whether another transfer would move something.
    fn more(&self) -> bool {
        let have = !self.carry.lock().is_empty() || self.src.nread() > 0;
        have && self.dst.splice_room().is_none_or(|r| r > 0)
    }

    /// `so_splice_xfer_data`: as much as the sink has room for, of what
    /// the source has, within the limit.
    ///
    /// # Errors
    ///
    /// `EPIPE` once the source can receive or the sink send no more;
    /// either's pending error.
    fn move_some(&self) -> Result<(), Errno> {
        let mut buf = core::mem::take(&mut *self.carry.lock());
        if buf.is_empty() {
            // Relaxed: as in `xfer`.
            let sent = self.src.splice_sent.load(Ordering::Relaxed);
            let left = self.max.map_or(usize::MAX, |m| {
                usize::try_from(m.saturating_sub(sent)).unwrap_or(usize::MAX)
            });
            let room = self.dst.splice_room().ok_or(Errno::EPIPE)?;
            let want = room.min(left);
            if want == 0 {
                return if self.src.rcv_cant() {
                    Err(Errno::EPIPE)
                } else {
                    Ok(())
                };
            }
            match self.src.splice_take(self, want) {
                Ok(Some(data)) if data.is_empty() => return Err(Errno::EPIPE),
                Ok(Some(data)) => buf = data,
                // Unspliced meanwhile.
                Ok(None) => return Ok(()),
                Err(Errno::EAGAIN) => {
                    return if self.dst.splice_room().is_none() {
                        Err(Errno::EPIPE)
                    } else {
                        Ok(())
                    };
                }
                Err(e) => return Err(e),
            }
        }
        match self
            .dst
            .send_stream_to(&buf, false, Some(&self.src.splice_sent))
        {
            Ok(n) => {
                buf.drain(..n);
            }
            Err(Errno::EAGAIN) => {}
            Err(e) => return Err(e),
        }
        if !buf.is_empty() {
            *self.carry.lock() = buf;
        }
        Ok(())
    }
}

/// `SOLISTENING`.
fn listening(i: &Inner) -> bool {
    i.tcp.as_ref().is_some_and(|t| t.listen.is_some())
}

impl Socket {
    /// `so_splice`: splices this socket's input to `sink`'s output, at
    /// most `max` bytes (0: no limit), counting anew; what it has
    /// already received starts on its way. The caller arms the timeout,
    /// if any, to call [`Splice::expire`].
    ///
    /// # Errors
    ///
    /// `EINVAL` for a negative limit or a listening socket;
    /// `EPROTONOSUPPORT` unless both are TCP; `ENOTCONN` for a socket
    /// neither connected nor connecting; `EBUSY` for a source already
    /// spliced or a sink already spliced to.
    pub fn splice(self: &Arc<Self>, sink: &Arc<Self>, max: i64) -> Result<Arc<Splice>, Errno> {
        if max < 0 {
            return Err(Errno::EINVAL);
        }
        if self.proto != Proto::Tcp || sink.proto != Proto::Tcp {
            return Err(Errno::EPROTONOSUPPORT);
        }
        let sp = Arc::new(Splice {
            src: Arc::clone(self),
            dst: Arc::clone(sink),
            max: u64::try_from(max).ok().filter(|&m| m > 0),
            state: AtomicU8::new(State::Init as u8),
            carry: SpinLock::new(Vec::new()),
        });
        // Relaxed: only transfers of this splice, not yet begun, add to it.
        self.splice_sent.store(0, Ordering::Relaxed);
        {
            let mut i = self.inner.lock();
            if listening(&i) {
                return Err(Errno::EINVAL);
            }
            if i.state & (ss::ISCONNECTED | ss::ISCONNECTING) == 0 {
                return Err(Errno::ENOTCONN);
            }
            if i.splice.is_some() {
                return Err(Errno::EBUSY);
            }
            i.splice = Some(Arc::clone(&sp));
            // Relaxed: under the lock (see `wake`).
            self.spliced.fetch_or(RCV, Ordering::Relaxed);
        }
        let refused = {
            let mut d = sink.inner.lock();
            if listening(&d) {
                Some(Errno::EINVAL)
            } else if d.state & (ss::ISCONNECTED | ss::ISCONNECTING) == 0 {
                Some(Errno::ENOTCONN)
            } else if d.splice_back.is_some() {
                Some(Errno::EBUSY)
            } else {
                d.splice_back = Some(Arc::clone(&sp));
                // Relaxed: as above.
                sink.spliced.fetch_or(SND, Ordering::Relaxed);
                None
            }
        };
        if let Some(e) = refused {
            self.detach(&sp);
            return Err(e);
        }
        // What is there already goes now.
        sp.state.store(State::Queued as u8, Ordering::Release);
        sp.xfer();
        Ok(sp)
    }

    /// `so_unsplice` from `setsockopt`: ends this socket's splice.
    ///
    /// # Errors
    ///
    /// `EINVAL` listening; `ENOTCONN` if it is not spliced (or its splice
    /// is still being set up).
    pub fn unsplice(self: &Arc<Self>) -> Result<(), Errno> {
        self.unsplice_if(None)
    }

    /// `so_unsplice`: ends this socket's splice if it is `which` (any,
    /// with `None`).
    pub(crate) fn unsplice_if(self: &Arc<Self>, which: Option<&Arc<Splice>>) -> Result<(), Errno> {
        let sp = {
            let i = self.inner.lock();
            if listening(&i) {
                return Err(Errno::EINVAL);
            }
            let Some(sp) = i.splice.as_ref() else {
                return Err(Errno::ENOTCONN);
            };
            if which.is_some_and(|w| !Arc::ptr_eq(w, sp)) {
                return Err(Errno::ENOTCONN);
            }
            if sp.state() == State::Init {
                return Err(Errno::ENOTCONN);
            }
            Arc::clone(sp)
        };
        if self.detach(&sp) {
            Ok(())
        } else {
            Err(Errno::ENOTCONN)
        }
    }

    /// Takes splice `sp` off its source (this socket) and its sink, and
    /// wakes their readers and writers; false if another unsplice got
    /// there first.
    fn detach(self: &Arc<Self>, sp: &Arc<Splice>) -> bool {
        let mine = {
            let mut i = self.inner.lock();
            if i.splice.as_ref().is_some_and(|s| Arc::ptr_eq(s, sp)) {
                i.splice = None;
                // Relaxed: under the lock.
                self.spliced.fetch_and(!RCV, Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        if !mine {
            return false;
        }
        {
            let mut d = sp.dst.inner.lock();
            if d.splice_back.as_ref().is_some_and(|s| Arc::ptr_eq(s, sp)) {
                d.splice_back = None;
                // Relaxed: as above.
                sp.dst.spliced.fetch_and(!SND, Ordering::Relaxed);
            }
        }
        sp.state.store(State::Closed as u8, Ordering::Release);
        self.wake();
        sp.dst.wake();
        true
    }

    /// The wakeup of a spliced buffer (`splice_push`, `splice_pull`):
    /// its splice's work.
    #[cold]
    pub(crate) fn splice_wake(&self) {
        let (fwd, back) = {
            let i = self.inner.lock();
            (i.splice.clone(), i.splice_back.clone())
        };
        if let Some(sp) = fwd {
            sp.dispatch();
        }
        if let Some(sp) = back {
            sp.dispatch();
        }
    }

    /// The last descriptor closed: its splices end, as FreeBSD's do on
    /// the transfer that finds the connection gone.
    #[cold]
    pub(crate) fn splice_close(self: &Arc<Self>) {
        let (fwd, back) = {
            let i = self.inner.lock();
            (i.splice.clone(), i.splice_back.clone())
        };
        if let Some(sp) = fwd {
            let _ = self.unsplice_if(Some(&sp));
        }
        if let Some(sp) = back {
            let _ = sp.src.unsplice_if(Some(&sp));
        }
    }

    /// `getsockopt(SO_SPLICE)`: the bytes spliced since the last splice
    /// (0 listening).
    #[must_use]
    pub fn splice_sent(&self) -> i64 {
        if self.is_listening() {
            return 0;
        }
        // Relaxed: a count.
        i64::try_from(self.splice_sent.load(Ordering::Relaxed)).unwrap_or(i64::MAX)
    }

    /// Whether it is spliced (`isspliced`).
    #[must_use]
    pub fn is_spliced(&self) -> bool {
        // Relaxed: a snapshot.
        self.spliced.load(Ordering::Relaxed) & RCV != 0
    }

    /// Whether it can receive no more (`SBS_CANTRCVMORE`).
    fn rcv_cant(&self) -> bool {
        self.inner.lock().rcv.cant
    }

    /// Room for a splice to send (`sbspace` of the send buffer, with the
    /// peer's window as a send's); `None` once it can send no more.
    fn splice_room(&self) -> Option<usize> {
        let (peer, space, queued) = {
            let i = self.inner.lock();
            if i.snd.cant {
                return None;
            }
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
        Some(direct + space)
    }

    /// A splice's receive (`soreceive_stream_locked`): up to `max` bytes,
    /// if this socket is still `sp`'s source (`None` otherwise); empty at
    /// the end of the stream.
    ///
    /// # Errors
    ///
    /// As [`Socket::recv_stream`]'s.
    fn splice_take(self: &Arc<Self>, sp: &Splice, max: usize) -> Result<Option<Vec<u8>>, Errno> {
        self.recv_stream_if(max, |i| {
            i.splice
                .as_ref()
                .is_some_and(|s| core::ptr::eq(Arc::as_ptr(s), sp))
        })
        .map(|r| r.map(|r| r.data))
    }
}
